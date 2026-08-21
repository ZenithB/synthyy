//! `poly` instrument — port of `makePoly()` in src/audio/devices.ts.
//!
//! Browser topology: Tone.PolySynth(Tone.Synth, maxPolyphony 16) → single
//! shared Tone.Filter (lowpass, rolloff −12/−24/−48 dB by `slope`), with the
//! built-in inst LFO (`makeInstLfo`) summed into the cutoff (±cutoff·lfoAmt).
//! Each voice is one oscillator + amp ADSR; "fat" waves (indices 4–6) layer
//! 3 unison oscillators detuned [−spread/2, 0, +spread/2] cents at
//! −6 − count·1.1 dB each with phases i/count, matching Tone.FatOscillator.

use crate::traits::InstrumentDsp;
use crate::util::{
    db_to_lin, lfo_shape_value, midi_hz, Adsr, PolyBlepOsc, Smoother, Svf, SvfMode, Wave,
};

/// Browser device uses `maxPolyphony: 16` (devices.ts makePoly).
const VOICES: usize = 16;
/// Fat waves use `count: 3` unison oscillators.
const UNISON: usize = 3;
/// Control-rate interval (samples) for filter-coefficient / LFO updates.
const CTRL: usize = 16;
/// Tone.FatOscillator per-osc volume: −6 − count·1.1 dB = −9.3 dB.
const FAT_GAIN: f32 = 0.342_8;
/// Slope index 12/24/48 dB/oct → number of cascaded 2-pole SVF stages,
/// mirroring Tone.Filter's biquad cascade for rolloff −12/−24/−48.
const STAGE_COUNT: [usize; 3] = [1, 2, 4];
/// instLfo shape index (Sin/Tri/Saw/Sqr, schema INST_LFO_SHAPES) → the shared
/// `lfo_shape_value` shape ids (0 sine, 1 tri, 2 saw up, 4 square).
const LFO_SHAPE_MAP: [u32; 4] = [0, 1, 2, 4];

struct Voice {
    oscs: [PolyBlepOsc; UNISON],
    env: Adsr,
    pitch: u8,
    vel: f32,
    serial: u64,    // note-on order for oldest-voice stealing
    gate_left: u32, // samples until the scheduled note_off (0 = none pending)
    gated: bool,
    in_use: bool,
}

pub struct PolyInst {
    sample_rate: f32,
    voices: [Voice; VOICES],
    serial: u64,

    // stepped params
    wave: usize,      // frozen WAVES index 0..6 (≥4 = fat)
    slope: usize,     // 0..2
    lfo_shape: usize, // 0..3 (INST_LFO_SHAPES index)

    // smoothed continuous params
    spread_s: Smoother,
    cutoff_s: Smoother,
    res_s: Smoother,
    lfo_rate_s: Smoother,
    lfo_amt_s: Smoother,

    // envelope params (applied to voices; stepped like Tone's synth.set)
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,

    lfo_phase: f64,
    stages: [Svf; 4],
    scratch: [f32; CTRL],
}

impl PolyInst {
    pub fn new() -> Self {
        // Defaults from the frozen "poly" schema entry (src/audio/schema.ts).
        let mut cutoff_s = Smoother::new(7_000.0);
        let mut lfo_amt_s = Smoother::new(0.0);
        // JS ramps cutoff and LFO depth with rampTo(…, 0.03); keep that 30 ms.
        cutoff_s.set_time_ms(30.0);
        lfo_amt_s.set_time_ms(30.0);
        Self {
            sample_rate: 44_100.0,
            voices: core::array::from_fn(|_| Voice {
                oscs: core::array::from_fn(|_| PolyBlepOsc::new(Wave::Saw)),
                env: Adsr::new(),
                pitch: 0,
                vel: 0.0,
                serial: 0,
                gate_left: 0,
                gated: false,
                in_use: false,
            }),
            serial: 0,
            wave: 0,
            slope: 0,
            lfo_shape: 0,
            spread_s: Smoother::new(18.0),
            cutoff_s,
            res_s: Smoother::new(0.7),
            lfo_rate_s: Smoother::new(2.0),
            lfo_amt_s,
            attack: 0.01,
            decay: 0.15,
            sustain: 0.6,
            release: 0.4,
            lfo_phase: 0.0,
            stages: core::array::from_fn(|_| Svf::new(SvfMode::Lowpass)),
            scratch: [0.0; CTRL],
        }
    }

    /// Allocate a voice (free first, else steal the oldest) and gate it on.
    /// Returns the voice index so `trigger` can attach a scheduled note-off.
    fn start_note(&mut self, pitch: u8, vel: f32) -> usize {
        let idx = self
            .voices
            .iter()
            .position(|v| !v.in_use)
            .unwrap_or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, v)| v.serial)
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });
        self.serial += 1;
        let fat = self.wave >= 4;
        let v = &mut self.voices[idx];
        v.pitch = pitch;
        v.vel = vel.clamp(0.0, 1.0);
        v.serial = self.serial;
        v.gate_left = 0;
        v.gated = true;
        v.in_use = true;
        // Tone voices start their source per note: fat phases are i/count.
        for (i, o) in v.oscs.iter_mut().enumerate() {
            o.reset_phase(if fat { i as f32 / UNISON as f32 } else { 0.0 });
        }
        // Attack ramps from the current level (Adsr), so steals don't click.
        v.env.set_adsr(self.attack, self.decay, self.sustain, self.release);
        v.env.note_on();
        idx
    }
}

impl Default for PolyInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for PolyInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for v in &mut self.voices {
            for o in &mut v.oscs {
                o.prepare(self.sample_rate);
            }
            v.env.prepare(self.sample_rate);
            v.env.reset();
            v.in_use = false;
            v.gated = false;
            v.gate_left = 0;
        }
        for st in &mut self.stages {
            st.prepare(self.sample_rate);
        }
        for s in [
            &mut self.spread_s,
            &mut self.cutoff_s,
            &mut self.res_s,
            &mut self.lfo_rate_s,
            &mut self.lfo_amt_s,
        ] {
            s.prepare(self.sample_rate);
            s.snap(s.target());
        }
        self.lfo_phase = 0.0;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "wave" => self.wave = (value as i32).clamp(0, 6) as usize,
            "spread" => self.spread_s.set_target(value.clamp(0.0, 60.0)),
            "cutoff" => self.cutoff_s.set_target(value.max(1.0)),
            "res" => self.res_s.set_target(value.clamp(0.0, 10.0)),
            "slope" => self.slope = (value as i32).clamp(0, 2) as usize,
            "lfoShape" => self.lfo_shape = (value as i32).clamp(0, 3) as usize,
            "lfoRate" => self.lfo_rate_s.set_target(value.max(0.0)),
            "lfoAmt" => self.lfo_amt_s.set_target(value.clamp(0.0, 1.0)),
            // envelope keys apply to live voices too (Tone synth.set does)
            "attack" | "decay" | "sustain" | "release" => {
                match key {
                    "attack" => self.attack = value.max(0.0),
                    "decay" => self.decay = value.max(0.0),
                    "sustain" => self.sustain = value.clamp(0.0, 1.0),
                    _ => self.release = value.max(0.0),
                }
                let (a, d, s, r) = (self.attack, self.decay, self.sustain, self.release);
                for v in self.voices.iter_mut().filter(|v| v.in_use) {
                    v.env.set_adsr(a, d, s, r);
                }
            }
            _ => {}
        }
    }

    fn note_on(&mut self, pitch: u8, vel: f32) {
        self.start_note(pitch, vel);
    }

    fn note_off(&mut self, pitch: u8) {
        for v in &mut self.voices {
            if v.in_use && v.gated && v.pitch == pitch {
                v.env.note_off();
                v.gated = false;
                v.gate_left = 0;
            }
        }
    }

    fn trigger(&mut self, pitch: u8, dur_sec: f32, vel: f32) {
        let idx = self.start_note(pitch, vel);
        let samples = (dur_sec.max(0.0) * self.sample_rate) as u32;
        self.voices[idx].gate_left = samples.max(1);
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let mut done = 0;
        while done < n {
            let m = (n - done).min(CTRL);

            // Advance smoothers per-sample; apply at control rate (CTRL).
            let mut spread = self.spread_s.current();
            let mut cutoff = self.cutoff_s.current();
            let mut res = self.res_s.current();
            let mut rate = self.lfo_rate_s.current();
            let mut amt = self.lfo_amt_s.current();
            for _ in 0..m {
                spread = self.spread_s.next();
                cutoff = self.cutoff_s.next();
                res = self.res_s.next();
                rate = self.lfo_rate_s.next();
                amt = self.lfo_amt_s.next();
            }

            // LFO is gated exactly like makeInstLfo: stopped (and silent)
            // while lfoAmt ≈ 0, so existing projects sound unchanged.
            let lfo_on = amt > 0.001;
            let lfo_v = if lfo_on {
                lfo_shape_value(LFO_SHAPE_MAP[self.lfo_shape], self.lfo_phase)
            } else {
                0.0
            };
            if lfo_on {
                self.lfo_phase += f64::from(rate) * m as f64 / f64::from(self.sample_rate);
            }

            // Effective cutoff = base + cutoff·amt·lfo (depth gain in JS).
            // WebAudio lowpass biquad Q is in dB → linear q = 10^(res/20),
            // same q on every cascaded stage (Tone.Filter shares one Q).
            let eff = cutoff + cutoff * amt * lfo_v;
            let q = db_to_lin(res);
            let n_st = STAGE_COUNT[self.slope];
            for st in self.stages[..n_st].iter_mut() {
                st.set_cutoff_q(eff, q);
            }

            let scratch = &mut self.scratch[..m];
            scratch.fill(0.0);

            let wave = Wave::from_index(self.wave);
            let fat = self.wave >= 4;
            // ±spread/2 cents at the outer unison voices
            let det = libm::powf(2.0, spread * 0.5 / 1200.0);
            for v in self.voices.iter_mut().filter(|v| v.in_use) {
                let base = midi_hz(f32::from(v.pitch));
                let (count, gain) = if fat { (UNISON, FAT_GAIN) } else { (1, 1.0) };
                for (i, o) in v.oscs[..count].iter_mut().enumerate() {
                    o.set_wave(wave);
                    let f = if fat {
                        match i {
                            0 => base / det,
                            1 => base,
                            _ => base * det,
                        }
                    } else {
                        base
                    };
                    o.set_freq(f);
                }
                for s in scratch.iter_mut() {
                    if v.gate_left > 0 {
                        v.gate_left -= 1;
                        if v.gate_left == 0 && v.gated {
                            v.env.note_off();
                            v.gated = false;
                        }
                    }
                    let e = v.env.next();
                    if e != 0.0 {
                        let mut x = 0.0;
                        for o in v.oscs[..count].iter_mut() {
                            x += o.next();
                        }
                        *s += x * gain * e * v.vel;
                    }
                }
                if !v.env.is_active() {
                    v.in_use = false;
                }
            }

            for st in self.stages[..n_st].iter_mut() {
                st.process(scratch);
            }
            for (k, s) in scratch.iter().enumerate() {
                l[done + k] += *s;
                r[done + k] += *s;
            }
            done += m;
        }
    }

    fn reset(&mut self) {
        for v in &mut self.voices {
            v.env.reset();
            v.in_use = false;
            v.gated = false;
            v.gate_left = 0;
        }
        for st in &mut self.stages {
            st.reset();
        }
        for s in [
            &mut self.spread_s,
            &mut self.cutoff_s,
            &mut self.res_s,
            &mut self.lfo_rate_s,
            &mut self.lfo_amt_s,
        ] {
            s.snap(s.target());
        }
        self.lfo_phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 44_100.0;

    fn render(inst: &mut PolyInst, secs: f32) -> Vec<f32> {
        let n = (secs * SR) as usize;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        for (lc, rc) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            inst.process(lc, rc);
        }
        l
    }

    fn rms(buf: &[f32]) -> f32 {
        libm::sqrtf(buf.iter().map(|s| s * s).sum::<f32>() / buf.len().max(1) as f32)
    }

    /// Single-bin DFT magnitude (Goertzel), normalized by window length.
    fn goertzel(buf: &[f32], freq: f32) -> f32 {
        let w = core::f32::consts::TAU * freq / SR;
        let c = 2.0 * libm::cosf(w);
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in buf {
            let s0 = x + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        libm::sqrtf((s1 * s1 + s2 * s2 - c * s1 * s2).max(0.0)) / (buf.len() as f32 / 2.0)
    }

    #[test]
    fn no_nan_inf_over_two_seconds() {
        let mut d = PolyInst::new();
        d.prepare(SR);
        d.note_on(48, 1.0);
        d.note_on(60, 0.8);
        d.trigger(64, 0.5, 0.9);
        d.set_param("wave", 4.0); // fat saw mid-flight
        d.set_param("lfoAmt", 0.8);
        d.set_param("lfoRate", 6.0);
        let out = render(&mut d, 2.0);
        assert!(out.iter().all(|s| s.is_finite()), "NaN/inf in output");
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let ranges: [(&str, f32, f32); 12] = [
            ("wave", 0.0, 6.0),
            ("spread", 0.0, 60.0),
            ("cutoff", 80.0, 14_000.0),
            ("res", 0.0, 10.0),
            ("slope", 0.0, 2.0),
            ("attack", 0.001, 2.0),
            ("decay", 0.01, 2.0),
            ("sustain", 0.0, 1.0),
            ("release", 0.01, 4.0),
            ("lfoShape", 0.0, 3.0),
            ("lfoRate", 0.05, 16.0),
            ("lfoAmt", 0.0, 1.0),
        ];
        let mut d = PolyInst::new();
        d.prepare(SR);
        d.note_on(45, 1.0);
        d.note_on(57, 1.0);
        d.note_on(64, 1.0);
        let mut l = [0.0f32; 128];
        let mut r = [0.0f32; 128];
        for (key, lo, hi) in ranges {
            for step in 0..=4 {
                d.set_param(key, lo + (hi - lo) * step as f32 / 4.0);
                for _ in 0..20 {
                    l.fill(0.0);
                    r.fill(0.0);
                    d.process(&mut l, &mut r);
                    assert!(
                        l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() < 32.0),
                        "blew up sweeping {key}"
                    );
                }
            }
        }
    }

    #[test]
    fn note_sounds_then_decays_to_silence() {
        let mut d = PolyInst::new();
        d.prepare(SR);
        d.note_on(60, 0.9);
        let held = render(&mut d, 0.5);
        assert!(rms(&held) > 0.01, "held note should be audible");
        d.note_off(60);
        let tail = render(&mut d, 3.0);
        let end = &tail[tail.len() - (0.2 * SR) as usize..];
        assert!(rms(end) < 1e-5, "did not decay to silence: rms {}", rms(end));
    }

    #[test]
    fn trigger_gates_off_after_duration() {
        let mut d = PolyInst::new();
        d.prepare(SR);
        d.trigger(60, 0.25, 1.0);
        let out = render(&mut d, 3.0);
        assert!(rms(&out[..(0.2 * SR) as usize]) > 0.01, "trigger silent");
        let end = &out[out.len() - (0.2 * SR) as usize..];
        assert!(rms(end) < 1e-5, "trigger did not release: rms {}", rms(end));
    }

    #[test]
    fn chord_of_three_notes_has_three_pitches() {
        // sine wave + open filter for a clean spectrum
        let mut d = PolyInst::new();
        d.prepare(SR);
        d.set_param("wave", 3.0);
        d.set_param("cutoff", 14_000.0);
        d.set_param("sustain", 1.0);
        d.note_on(60, 1.0); // C4 261.63 Hz
        d.note_on(64, 1.0); // E4 329.63 Hz
        d.note_on(67, 1.0); // G4 392.00 Hz
        let out = render(&mut d, 1.0);
        let window = &out[(0.2 * SR) as usize..]; // past the attack
        let off = goertzel(window, 470.0).max(1e-6); // no chord tone near 470 Hz
        for hz in [261.63, 329.63, 392.0] {
            let peak = goertzel(window, hz);
            assert!(
                peak > 5.0 * off,
                "no spectral peak at {hz} Hz: {peak} vs off-bin {off}"
            );
        }
        // isolated-note pitch via zero crossings: A4 sine = 880 crossings/s
        let mut d = PolyInst::new();
        d.prepare(SR);
        d.set_param("wave", 3.0);
        d.set_param("cutoff", 14_000.0);
        d.set_param("sustain", 1.0);
        d.note_on(69, 1.0);
        let out = render(&mut d, 2.0);
        let tail = &out[out.len() / 2..]; // settled second half (1 s)
        let zc = tail
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        assert!(
            (zc as f32 - 880.0).abs() < 25.0,
            "A4 zero crossings: {zc} (want ~880)"
        );
    }

    #[test]
    fn process_is_additive_and_stealing_is_safe() {
        let mut d = PolyInst::new();
        d.prepare(SR);
        let mut l = [0.5f32; 64];
        let mut r = [0.5f32; 64];
        d.process(&mut l, &mut r); // no notes: buffer must survive untouched
        assert!(l.iter().chain(r.iter()).all(|s| (*s - 0.5).abs() < 1e-9));
        // more notes than the pool: oldest-stealing, no panic, finite audio
        for p in 0..24u8 {
            d.note_on(40 + p, 1.0);
        }
        let out = render(&mut d, 0.25);
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(rms(&out) > 0.01);
        d.reset();
        let quiet = render(&mut d, 0.1);
        assert!(quiet.iter().all(|s| *s == 0.0), "reset must silence voices");
    }
}
