//! `drum` instrument — 8 procedural pads (kick, snare, clap, closed/open hat,
//! lo tom, perc, crash), port of `makeDrum()` in src/audio/devices.ts.
//!
//! A pad with an injected sample (`set_sample` slot 0–7, from
//! TrackJSON.inst.padSamples) plays that sample instead — port of the browser
//! `samplePad()`: one-shot, the FULL sample always plays (`decay` is a no-op),
//! `tune` repitches by 2^(tune/12) read at trigger time, `level` is the pad
//! gain, 1 ms attack. Sample-rate mismatch is handled by stepping the
//! `SampleBuf` linear-interp reader at `src_sr / engine_sr × 2^(tune/12)`.
//! The browser samplePad is polyphonic (Tone.Sampler); here it shares the
//! pad's mono voice, so a retrigger restarts the sample (per-pad steal, like
//! the synth pads). Pads without samples keep the synthesized sound:
//! - membrane pads (kick @ MIDI 36, lo tom @ 48): sine with an exponential
//!   pitch drop — Tone.MembraneSynth `setNote` starts at `hz * octaves` (6
//!   here) and ramps exponentially to `hz` over `pitchDecay` (0.05 / 0.03 s).
//! - noise pads: white noise through a bandpass (snare 1800 Hz Q 0.8, clap
//!   1100 Hz Q 1) or highpass (closed hat 9 kHz, open hat 8 kHz, crash
//!   6.5 kHz on pink noise). `tune` retunes the filter live.
//! - clap: three quick bursts (t, +12 ms, +26 ms at 0.7/0.85/1.0 × vel) on
//!   ONE mono noise voice, retriggering the envelope from its current level.
//! - perc: short triangle blip at 1100 Hz.
//!
//! Envelopes are attack 1 ms, exponential decay to 0 (sustain 0), so a pad
//! always rings out on its own; note_off is a no-op like the browser device.
//! Params are the frozen `p{i}_{tune,decay,level}` keys (schema.ts
//! `drumParams()`): tune ±12 st, decay 0.03–2 s, level −24…+6 dB.
//!
//! Voice pool: the browser device is one mono Tone synth per pad (a retrigger
//! steals that pad's own voice), so the pool is 8 fixed mono voices — one per
//! pad — instead of the default 12 (RUST_ARCH.md "unless the browser device
//! implies otherwise").

use crate::sample::SampleBuf;
use crate::traits::InstrumentDsp;
use crate::util::{db_to_lin, midi_hz, PolyBlepOsc, Smoother, Svf, SvfMode, Wave};
use std::sync::Arc;

const PAD_COUNT: usize = 8;
const ATTACK_S: f32 = 0.001;
/// Exp decay reaches ~e⁻⁶·² (−54 dB) at the nominal decay time — same
/// approach-time convention as `util::Adsr`.
const ENV_EXP_MULT: f32 = 6.2;
const ENV_FLOOR: f32 = 1e-4; // ≈ −80 dB: the voice is over
/// Tone.MembraneSynth `octaves` as passed by membranePad: sweep starts at
/// `hz * 6` (a linear multiplier in Tone's setNote, not 2^6).
const MEMBRANE_OCTAVES: f32 = 6.0;
const CLAP_VELS: [f32; 3] = [0.7, 0.85, 1.0];
/// Bursts land at t, t+12 ms, t+26 ms → inter-burst gaps.
const CLAP_GAPS_S: [f32; 2] = [0.012, 0.014];

#[derive(Clone, Copy)]
enum PadKind {
    Membrane { base_midi: f32, pitch_decay: f32 },
    Noise { center_hz: f32, mode: SvfMode, q: f32, pink: bool, clap: bool },
    Blip { base_hz: f32 },
}

const PAD_KINDS: [PadKind; PAD_COUNT] = [
    PadKind::Membrane { base_midi: 36.0, pitch_decay: 0.05 }, // kick
    PadKind::Noise { center_hz: 1800.0, mode: SvfMode::Bandpass, q: 0.8, pink: false, clap: false }, // snare
    PadKind::Noise { center_hz: 1100.0, mode: SvfMode::Bandpass, q: 1.0, pink: false, clap: true }, // clap
    PadKind::Noise { center_hz: 9000.0, mode: SvfMode::Highpass, q: 1.0, pink: false, clap: false }, // closed hat
    PadKind::Noise { center_hz: 8000.0, mode: SvfMode::Highpass, q: 1.0, pink: false, clap: false }, // open hat
    PadKind::Membrane { base_midi: 48.0, pitch_decay: 0.03 }, // lo tom
    PadKind::Blip { base_hz: 1100.0 },                        // perc
    PadKind::Noise { center_hz: 6500.0, mode: SvfMode::Highpass, q: 1.0, pink: true, clap: false }, // crash
];

/// Schema decay defaults per pad (schema.ts `PAD_DECAYS`).
const PAD_DECAY_DEF: [f32; PAD_COUNT] = [0.42, 0.18, 0.28, 0.06, 0.4, 0.32, 0.12, 1.3];

#[derive(Clone, Copy, PartialEq, Eq)]
enum EnvStage {
    Idle,
    Attack,
    Decay,
}

struct Voice {
    stage: EnvStage,
    env: f32,
    attack_step: f32,
    attack_target: f32,
    decay_coeff: f32,
    vel: f32,
    // membrane / blip
    osc: PolyBlepOsc,
    freq: f32,
    freq_end: f32,
    sweep_ratio: f32,
    sweep_left: u32,
    // noise
    rng: u32,
    pink: [f32; 3],
    filt: Svf,
    // clap burst scheduling (bursts_fired == 3 ⇒ none pending)
    bursts_fired: u8,
    next_burst_in: u32,
    // sample playback (browser samplePad); pos/rate are in SOURCE frames
    sample_active: bool,
    pos: f64,
    rate: f64,
}

impl Voice {
    fn new(kind: PadKind, seed_idx: usize) -> Self {
        let (wave, mode) = match kind {
            PadKind::Membrane { .. } => (Wave::Sine, SvfMode::Lowpass),
            PadKind::Blip { .. } => (Wave::Triangle, SvfMode::Lowpass),
            PadKind::Noise { mode, .. } => (Wave::Sine, mode),
        };
        Self {
            stage: EnvStage::Idle,
            env: 0.0,
            attack_step: 0.0,
            attack_target: 0.0,
            decay_coeff: 0.0,
            vel: 0.0,
            osc: PolyBlepOsc::new(wave),
            freq: 0.0,
            freq_end: 0.0,
            sweep_ratio: 1.0,
            sweep_left: 0,
            rng: 0x9E37_79B9u32.wrapping_mul(seed_idx as u32 + 1) | 1,
            pink: [0.0; 3],
            filt: Svf::new(mode),
            bursts_fired: 3,
            next_burst_in: 0,
            sample_active: false,
            pos: 0.0,
            rate: 1.0,
        }
    }

    fn reset(&mut self) {
        self.stage = EnvStage::Idle;
        self.env = 0.0;
        self.sweep_left = 0;
        self.bursts_fired = 3;
        self.next_burst_in = 0;
        self.pink = [0.0; 3];
        self.filt.reset();
        self.sample_active = false;
        self.pos = 0.0;
    }

    /// Linear 1 ms attack from the CURRENT level (Tone retrigger semantics —
    /// the clap's overlapping bursts must not click).
    #[inline]
    fn start_attack(&mut self, target: f32, sr: f32) {
        let n = (ATTACK_S * sr).max(1.0);
        self.attack_target = target;
        self.attack_step = (target - self.env) / n;
        self.stage = EnvStage::Attack;
    }

    #[inline]
    fn env_next(&mut self) -> f32 {
        match self.stage {
            EnvStage::Idle => 0.0,
            EnvStage::Attack => {
                self.env += self.attack_step;
                let done = if self.attack_step >= 0.0 {
                    self.env >= self.attack_target
                } else {
                    self.env <= self.attack_target
                };
                if done {
                    self.env = self.attack_target;
                    self.stage = EnvStage::Decay;
                }
                self.env
            }
            EnvStage::Decay => {
                self.env *= self.decay_coeff;
                if self.env < ENV_FLOOR {
                    self.env = 0.0;
                    self.stage = EnvStage::Idle;
                }
                self.env
            }
        }
    }

    /// xorshift32 white noise in [-1, 1).
    #[inline]
    fn white(&mut self) -> f32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as f32 * (2.0 / 16_777_216.0) - 1.0
    }

    /// Paul Kellet economy pink filter (crash uses Tone's pink noise).
    #[inline]
    fn pink_next(&mut self, w: f32) -> f32 {
        self.pink[0] = 0.99765 * self.pink[0] + w * 0.099_046;
        self.pink[1] = 0.963 * self.pink[1] + w * 0.296_516_4;
        self.pink[2] = 0.57 * self.pink[2] + w * 1.052_691_3;
        (self.pink[0] + self.pink[1] + self.pink[2] + w * 0.1848) * 0.25
    }
}

struct Pad {
    kind: PadKind,
    tune: f32,     // semitones, applied at trigger (membrane/blip)
    decay: f32,    // seconds, applied at trigger
    tune_sm: Smoother,  // smoothed for the live noise-filter retune
    level_sm: Smoother, // linear gain (level is a dB param)
    /// Injected one-shot override (browser samplePad): when set, this pad
    /// plays the sample instead of its synthesized sound.
    sample: Option<Arc<SampleBuf>>,
    voice: Voice,
}

impl Pad {
    fn new(i: usize) -> Self {
        let kind = PAD_KINDS[i];
        Self {
            kind,
            tune: 0.0,
            decay: PAD_DECAY_DEF[i],
            tune_sm: Smoother::new(0.0),
            level_sm: Smoother::new(1.0), // 0 dB default
            sample: None,
            voice: Voice::new(kind, i),
        }
    }
}

pub struct DrumInst {
    sample_rate: f32,
    pads: [Pad; PAD_COUNT],
}

impl DrumInst {
    pub fn new() -> Self {
        Self {
            sample_rate: 44_100.0,
            pads: core::array::from_fn(Pad::new),
        }
    }

    fn trig(&mut self, pitch: u8, vel: f32) {
        let sr = self.sample_rate;
        let pad = &mut self.pads[pitch as usize % PAD_COUNT];
        let v = &mut pad.voice;
        v.vel = vel.clamp(0.0, 1.0);
        v.decay_coeff = libm::expf(-ENV_EXP_MULT / (pad.decay * sr));
        v.bursts_fired = 3;
        if let Some(buf) = &pad.sample {
            // browser samplePad: one-shot from the top, tune → playbackRate
            // 2^(tune/12) read at trigger time, decay is a no-op (the full
            // sample plays). decay_coeff 1.0 holds the env at vel after the
            // 1 ms attack; the voice ends when the read position passes the
            // end of the buffer. Source-rate mismatch folds into the step.
            v.sample_active = true;
            v.pos = 0.0;
            v.rate = (buf.sample_rate as f64 / sr as f64)
                * libm::pow(2.0, pad.tune as f64 / 12.0);
            v.decay_coeff = 1.0;
            v.start_attack(v.vel, sr);
            return;
        }
        v.sample_active = false;
        match pad.kind {
            PadKind::Membrane { base_midi, pitch_decay } => {
                let base = midi_hz(base_midi + pad.tune);
                let n = (pitch_decay * sr).max(1.0);
                v.freq = base * MEMBRANE_OCTAVES;
                v.freq_end = base;
                v.sweep_left = n as u32;
                v.sweep_ratio = libm::powf(1.0 / MEMBRANE_OCTAVES, 1.0 / n);
                v.osc.set_freq(v.freq);
                v.start_attack(v.vel, sr);
            }
            PadKind::Blip { base_hz } => {
                v.osc.set_freq(base_hz * libm::powf(2.0, pad.tune / 12.0));
                v.start_attack(v.vel, sr);
            }
            PadKind::Noise { center_hz, q, clap, .. } => {
                let cutoff = center_hz * libm::powf(2.0, pad.tune_sm.current() / 12.0);
                v.filt.set_cutoff_q(cutoff, q);
                if clap {
                    v.bursts_fired = 1;
                    v.next_burst_in = (CLAP_GAPS_S[0] * sr) as u32;
                    v.start_attack(v.vel * CLAP_VELS[0], sr);
                } else {
                    v.start_attack(v.vel, sr);
                }
            }
        }
    }
}

impl Default for DrumInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for DrumInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for pad in &mut self.pads {
            pad.tune_sm.prepare(self.sample_rate);
            pad.level_sm.prepare(self.sample_rate);
            pad.voice.osc.prepare(self.sample_rate);
            pad.voice.filt.prepare(self.sample_rate);
            pad.voice.reset();
        }
    }

    fn set_param(&mut self, key: &str, value: f32) {
        // frozen keys: p{0..7}_{tune|decay|level} (schema.ts drumParams)
        let b = key.as_bytes();
        if b.len() < 4 || b[0] != b'p' || b[2] != b'_' {
            return;
        }
        let i = (b[1] as usize).wrapping_sub(b'0' as usize);
        if i >= PAD_COUNT {
            return;
        }
        let pad = &mut self.pads[i];
        match &b[3..] {
            b"tune" => {
                pad.tune = value.clamp(-12.0, 12.0);
                pad.tune_sm.set_target(pad.tune);
            }
            b"decay" => pad.decay = value.clamp(0.03, 2.0),
            b"level" => pad.level_sm.set_target(db_to_lin(value.clamp(-24.0, 6.0))),
            _ => {}
        }
    }

    fn set_sample(&mut self, slot: u32, buf: Option<Arc<SampleBuf>>) {
        // control path (graph build / padSamples injection): swap the Arc and
        // silence the pad so a ringing voice never reads across a swap
        if let Some(pad) = self.pads.get_mut(slot as usize) {
            pad.sample = buf;
            pad.voice.reset();
        }
    }

    fn note_on(&mut self, pitch: u8, vel: f32) {
        self.trig(pitch, vel);
    }

    fn note_off(&mut self, _pitch: u8) {
        // one-shot pads ring out on their own decay (browser noteOff is a no-op)
    }

    fn trigger(&mut self, pitch: u8, _dur_sec: f32, vel: f32) {
        // duration is ignored: each pad plays its own decay (browser trigger)
        self.trig(pitch, vel);
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let sr = self.sample_rate;
        // block-rate: retune ringing noise filters from the smoothed tune
        for pad in &mut self.pads {
            if let PadKind::Noise { center_hz, q, .. } = pad.kind {
                if pad.sample.is_none() && pad.voice.stage != EnvStage::Idle {
                    let cutoff = center_hz * libm::powf(2.0, pad.tune_sm.current() / 12.0);
                    pad.voice.filt.set_cutoff_q(cutoff, q);
                }
            }
        }
        for (ls, rs) in l[..n].iter_mut().zip(r[..n].iter_mut()) {
            let mut mix_l = 0.0f32;
            let mut mix_r = 0.0f32;
            for pad in &mut self.pads {
                pad.tune_sm.next();
                let gain = pad.level_sm.next();
                let v = &mut pad.voice;
                // pending clap bursts retrigger the envelope mid-decay
                if v.bursts_fired < 3 {
                    if v.next_burst_in == 0 {
                        let k = v.bursts_fired as usize;
                        v.start_attack(v.vel * CLAP_VELS[k], sr);
                        v.bursts_fired += 1;
                        if k < CLAP_GAPS_S.len() {
                            v.next_burst_in = (CLAP_GAPS_S[k] * sr) as u32;
                        }
                    } else {
                        v.next_burst_in -= 1;
                    }
                }
                if v.stage == EnvStage::Idle {
                    continue;
                }
                let (raw_l, raw_r) = if v.sample_active {
                    // one-shot sample read; the voice ends at buffer end
                    // (decay never gates it — browser samplePad semantics)
                    match &pad.sample {
                        Some(buf) if v.pos < buf.frames() as f64 => {
                            let s = buf.read_lin(v.pos);
                            v.pos += v.rate;
                            s
                        }
                        _ => {
                            v.env = 0.0;
                            v.stage = EnvStage::Idle;
                            v.sample_active = false;
                            continue;
                        }
                    }
                } else {
                    let m = match pad.kind {
                        PadKind::Membrane { .. } => {
                            if v.sweep_left > 0 {
                                v.freq *= v.sweep_ratio;
                                v.sweep_left -= 1;
                                if v.sweep_left == 0 {
                                    v.freq = v.freq_end;
                                }
                                v.osc.set_freq(v.freq);
                            }
                            v.osc.next()
                        }
                        PadKind::Blip { .. } => v.osc.next(),
                        PadKind::Noise { q, mode, pink, .. } => {
                            let w = v.white();
                            let x = if pink { v.pink_next(w) } else { w };
                            let y = v.filt.process_sample(x);
                            // SVF band output peaks at Q; biquad bandpass peaks
                            // at 1 — normalize so `q` shapes width, not gain
                            if mode == SvfMode::Bandpass {
                                y / q
                            } else {
                                y
                            }
                        }
                    };
                    (m, m)
                };
                let e = v.env_next();
                mix_l += raw_l * e * gain;
                mix_r += raw_r * e * gain;
            }
            *ls += mix_l;
            *rs += mix_r;
        }
    }

    fn reset(&mut self) {
        for pad in &mut self.pads {
            pad.voice.reset();
            pad.tune_sm.snap(pad.tune_sm.target());
            pad.level_sm.snap(pad.level_sm.target());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 44_100.0;

    /// Render mono (L; L == R here) in ≤ MAX_BLOCK chunks.
    fn render(d: &mut DrumInst, secs: f32) -> Vec<f32> {
        let n = (secs * SR) as usize;
        let mut out = vec![0.0f32; n];
        let mut l = [0.0f32; 128];
        let mut r = [0.0f32; 128];
        let mut i = 0;
        while i < n {
            let len = (n - i).min(128);
            l[..len].fill(0.0);
            r[..len].fill(0.0);
            d.process(&mut l[..len], &mut r[..len]);
            out[i..i + len].copy_from_slice(&l[..len]);
            i += len;
        }
        out
    }

    fn peak(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |a, s| a.max(s.abs()))
    }

    /// Energy below/above `split_hz` via 2nd-order LP/HP.
    fn band_energy(buf: &[f32], split_hz: f32) -> (f32, f32) {
        let mut lp = Svf::new(SvfMode::Lowpass);
        let mut hp = Svf::new(SvfMode::Highpass);
        lp.prepare(SR);
        hp.prepare(SR);
        lp.set_cutoff_q(split_hz, core::f32::consts::FRAC_1_SQRT_2);
        hp.set_cutoff_q(split_hz, core::f32::consts::FRAC_1_SQRT_2);
        let mut lo = 0.0f32;
        let mut hi = 0.0f32;
        for &s in buf {
            let a = lp.process_sample(s);
            let b = hp.process_sample(s);
            lo += a * a;
            hi += b * b;
        }
        (lo, hi)
    }

    fn fresh() -> DrumInst {
        let mut d = DrumInst::new();
        d.prepare(SR);
        d
    }

    #[test]
    fn two_seconds_all_pads_no_nan() {
        let mut d = fresh();
        for p in 0..PAD_COUNT as u8 {
            d.note_on(p, 1.0);
        }
        let a = render(&mut d, 1.0);
        // retrigger mid-stream (mono per-pad steal path)
        for p in 0..PAD_COUNT as u8 {
            d.trigger(p, 0.1, 0.7);
        }
        let b = render(&mut d, 1.0);
        assert!(a.iter().chain(b.iter()).all(|s| s.is_finite()));
        assert!(peak(&a) > 1e-3 && peak(&b) > 1e-3);
    }

    #[test]
    fn param_sweep_stays_bounded() {
        // full-range corners plus out-of-range values (set_param must clamp)
        let tunes = [-12.0, 0.0, 12.0, -100.0, 100.0];
        let decays = [0.03, 0.5, 2.0, -1.0, 50.0];
        let levels = [-24.0, 0.0, 6.0, 60.0];
        for pad in 0..PAD_COUNT {
            for (&t, (&dc, &lv)) in tunes
                .iter()
                .zip(decays.iter().zip(levels.iter().cycle()))
            {
                let mut d = fresh();
                let key = |s: &str| format!("p{pad}_{s}");
                d.set_param(&key("tune"), t);
                d.set_param(&key("decay"), dc);
                d.set_param(&key("level"), lv);
                d.note_on(pad as u8, 1.0);
                let buf = render(&mut d, 0.5);
                assert!(buf.iter().all(|s| s.is_finite()), "pad {pad} t={t} d={dc}");
                assert!(peak(&buf) < 8.0, "pad {pad} blew up: {}", peak(&buf));
            }
        }
        // unknown keys are ignored, not panics
        let mut d = fresh();
        d.set_param("p9_tune", 1.0);
        d.set_param("cutoff", 1.0);
        d.set_param("", 1.0);
    }

    #[test]
    fn every_pad_sounds_then_decays_to_silence() {
        for pad in 0..PAD_COUNT {
            let mut d = fresh();
            d.note_on(pad as u8, 1.0);
            let buf = render(&mut d, 4.0); // crash (decay 1.3 s) is done < 2 s
            let head = &buf[..(0.5 * SR) as usize];
            let tail = &buf[buf.len() - (0.5 * SR) as usize..];
            assert!(peak(head) > 1e-3, "pad {pad} silent");
            assert!(peak(tail) < 1e-4, "pad {pad} did not decay: {}", peak(tail));
        }
    }

    #[test]
    fn kick_energy_concentrated_below_200hz() {
        let mut d = fresh();
        d.note_on(0, 1.0);
        let buf = render(&mut d, 2.0);
        // skip the 50 ms pitch sweep (starts at 6× base = ~392 Hz); the body
        // of the kick is the 65 Hz fundamental
        let body = &buf[(0.06 * SR) as usize..];
        let (lo, hi) = band_energy(body, 200.0);
        assert!(lo > 4.0 * hi, "kick lo={lo} hi={hi}");
    }

    #[test]
    fn closed_hat_energy_above_2khz() {
        let mut d = fresh();
        d.note_on(3, 1.0);
        let buf = render(&mut d, 1.0);
        let (lo, hi) = band_energy(&buf, 2_000.0);
        assert!(hi > 4.0 * lo, "hat lo={lo} hi={hi}");
    }

    #[test]
    fn process_is_additive_and_note_off_is_noop() {
        let mut d = fresh();
        let mut l = [0.25f32; 128];
        let mut r = [0.25f32; 128];
        d.process(&mut l, &mut r); // no voices: buffer untouched
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.25));
        d.note_on(4, 1.0);
        d.note_off(4); // no-op: pad still rings
        let buf = render(&mut d, 0.05);
        assert!(peak(&buf) > 1e-3);
        d.reset();
        let buf = render(&mut d, 0.05);
        assert!(peak(&buf) == 0.0, "reset must silence all voices");
    }

    #[test]
    fn level_param_scales_output() {
        let run = |db: f32| {
            let mut d = fresh();
            d.set_param("p1_level", db);
            d.reset(); // graph-build path: snap the level smoother
            d.note_on(1, 1.0);
            render(&mut d, 0.3)
        };
        let loud = run(6.0);
        let quiet = run(-24.0);
        let rms = |b: &[f32]| {
            libm::sqrtf(b.iter().map(|s| s * s).sum::<f32>() / b.len() as f32)
        };
        // same noise seed ⇒ identical signal shape, ~30 dB apart
        assert!(rms(&loud) > 10.0 * rms(&quiet));
    }

    /// 0.5 s 440 Hz sine at 22 050 Hz source rate (half the engine rate, so
    /// the interp reader must step at 0.5 to keep the pitch).
    fn sine_buf() -> std::sync::Arc<SampleBuf> {
        let src_sr = 22_050.0f32;
        let n = (0.5 * src_sr) as usize;
        let l: Vec<f32> = (0..n)
            .map(|i| {
                libm::sinf(2.0 * core::f32::consts::PI * 440.0 * i as f32 / src_sr)
            })
            .collect();
        std::sync::Arc::new(SampleBuf { sample_rate: src_sr, l, r: vec![] })
    }

    #[test]
    fn injected_sample_pad_plays_the_sample() {
        let mut d = fresh();
        d.set_sample(1, Some(sine_buf())); // snare pad: synth would be noise
        d.note_on(1, 1.0);
        let buf = render(&mut d, 1.5);
        // 22 050 Hz source at 44 100 engine → rate 0.5 → the 0.5 s sample
        // plays for 1.0 s at 440 Hz. Zero crossings over 0.4 s of the body:
        // 2 × 440 × 0.4 = 352 for a sine; snare noise would give thousands.
        let body = &buf[(0.01 * SR) as usize..(0.41 * SR) as usize];
        assert!(peak(body) > 0.5, "sample pad silent");
        let zc = body
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        assert!((300..=400).contains(&zc), "not a 440 Hz sine: {zc} crossings");
        let (lo, hi) = band_energy(body, 1_000.0);
        assert!(lo > 20.0 * hi, "energy not at 440 Hz: lo={lo} hi={hi}");
        // one-shot: silence after the sample ends (decay does not gate it —
        // pad 1's default decay is 0.18 s, far shorter than the 1 s playback)
        let tail = &buf[(1.05 * SR) as usize..];
        assert!(peak(tail) == 0.0, "sample did not stop: {}", peak(tail));
        // clearing the override restores the synthesized snare (noise)
        d.set_sample(1, None);
        d.note_on(1, 1.0);
        let buf = render(&mut d, 0.2);
        let zc = buf
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        assert!(zc > 1_000, "cleared pad should be noise again: {zc}");
    }

    #[test]
    fn pads_without_samples_keep_synthesizing() {
        let mut d = fresh();
        d.set_sample(1, Some(sine_buf())); // only the snare is overridden
        // kick still a low membrane
        d.note_on(0, 1.0);
        let buf = render(&mut d, 2.0);
        let (lo, hi) = band_energy(&buf[(0.06 * SR) as usize..], 200.0);
        assert!(lo > 4.0 * hi, "kick lo={lo} hi={hi}");
        // closed hat still high-band noise
        let mut d = fresh();
        d.set_sample(1, Some(sine_buf()));
        d.note_on(3, 1.0);
        let buf = render(&mut d, 1.0);
        let (lo, hi) = band_energy(&buf, 2_000.0);
        assert!(hi > 4.0 * lo, "hat lo={lo} hi={hi}");
    }

    #[test]
    fn tune_shifts_membrane_pitch() {
        let cycles = |tune: f32| {
            let mut d = fresh();
            d.set_param("p0_tune", tune);
            d.note_on(0, 1.0);
            let buf = render(&mut d, 0.4);
            // count zero crossings after the pitch sweep settles
            let body = &buf[(0.06 * SR) as usize..];
            body.windows(2)
                .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
                .count()
        };
        let base = cycles(0.0);
        let up = cycles(12.0);
        assert!(
            up as f32 > base as f32 * 1.6,
            "+12 st should ~double the rate: base={base} up={up}"
        );
    }
}
