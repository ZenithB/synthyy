//! `fm` instrument — port of `makeFm()` (src/audio/devices.ts):
//! Tone.PolySynth(Tone.FMSynth, maxPolyphony: 12). Each voice is a sine
//! carrier frequency-modulated by a sine modulator at `freq · harmonicity`;
//! peak deviation is `freq · modulationIndex`, scaled by a fixed modulation
//! envelope (0.005 / 0.4 / 0.6 / 0.4). The amp envelope is the schema ADSR,
//! velocity scales both the amp and modulation envelopes (ModulationSynth
//! passes it to both internal Synths), and both internal Synths carry Tone's
//! fixed -10 dB voice volume. Params: harm [0.25, 8], modIdx [0.5, 40],
//! attack/decay/sustain/release (src/audio/schema.ts "fm" entry).

use crate::traits::InstrumentDsp;
use crate::util::{midi_hz, Adsr, PolyBlepOsc, Smoother, Wave};

const NUM_VOICES: usize = 12; // Tone maxPolyphony: 12

// Tone.ModulationSynth builds its two internal Tone.Synth voices with
// `volume: -10` dB (node_modules/tone/build/esm/instrument/ModulationSynth.js,
// carrier line 25 and modulator line 31). The carrier volume attenuates the
// audible output by -10 dB; the modulator volume sits on the signal that
// drives `_modulationNode.gain` (FMSynth.js line 30), so it scales the
// effective FM depth by the same factor.
const SYNTH_VOLUME_GAIN: f32 = 0.316_227_77; // 10^(-10/20)

// Fixed modulationEnvelope from makeFm() — not user-addressable.
const MOD_ATTACK: f32 = 0.005;
const MOD_DECAY: f32 = 0.4;
const MOD_SUSTAIN: f32 = 0.6;
const MOD_RELEASE: f32 = 0.4;

struct Voice {
    // Modulator freq is always ≥ 0 so the shared osc works; the carrier keeps
    // a raw phase accumulator because through-zero FM needs a *signed*
    // instantaneous frequency, which PolyBlepOsc clamps away (step ∈ [0, 0.5]).
    modulator: PolyBlepOsc,
    carrier_phase: f32,
    amp_env: Adsr,
    mod_env: Adsr,
    freq: f32,
    vel: f32,
    pitch: u8,
    held: bool,
    serial: u64,
    /// Samples until the scheduler-path auto release; 0 = no pending release.
    gate_remaining: u64,
}

impl Voice {
    fn new() -> Self {
        let mut v = Self {
            modulator: PolyBlepOsc::new(Wave::Sine),
            carrier_phase: 0.0,
            amp_env: Adsr::new(),
            mod_env: Adsr::new(),
            freq: 440.0,
            vel: 1.0,
            pitch: 69,
            held: false,
            serial: 0,
            gate_remaining: 0,
        };
        v.mod_env.set_adsr(MOD_ATTACK, MOD_DECAY, MOD_SUSTAIN, MOD_RELEASE);
        v
    }

    fn prepare(&mut self, sample_rate: f32) {
        self.modulator.prepare(sample_rate);
        self.amp_env.prepare(sample_rate);
        self.mod_env.prepare(sample_rate);
        self.carrier_phase = 0.0;
        self.held = false;
        self.gate_remaining = 0;
    }

    #[inline]
    fn active(&self) -> bool {
        self.amp_env.is_active()
    }

    fn release(&mut self) {
        self.held = false;
        self.gate_remaining = 0;
        self.amp_env.note_off();
        self.mod_env.note_off();
    }

    /// One sample of the FM pair. `harm`/`mod_idx` are the block-smoothed
    /// param values; `inv_sr` = 1 / sample_rate.
    #[inline]
    fn render(&mut self, harm: f32, mod_idx: f32, inv_sr: f32) -> f32 {
        self.modulator.set_freq(self.freq * harm);
        // Modulator is a full Tone.Synth: osc · env · velocity · volume(-10 dB).
        // ModulationSynth._triggerEnvelopeAttack passes velocity to BOTH the
        // carrier and modulator envelopes, and Envelope.triggerAttack ramps to
        // `velocity`, so velocity scales the FM depth too.
        let m = self.modulator.next() * self.mod_env.next() * self.vel * SYNTH_VOLUME_GAIN;
        // Instantaneous carrier freq: f + f·modIdx·env·sin(...). Signed step,
        // clamped to ±Nyquist; the phase wrap handles backward motion.
        let step = (self.freq * (1.0 + mod_idx * m) * inv_sr).clamp(-0.5, 0.5);
        let s = libm::sinf(self.carrier_phase * core::f32::consts::TAU);
        self.carrier_phase += step;
        self.carrier_phase -= libm::floorf(self.carrier_phase);
        // Carrier Tone.Synth volume: -10 dB (ModulationSynth.js line 25).
        s * self.amp_env.next() * self.vel * SYNTH_VOLUME_GAIN
    }
}

pub struct FmInst {
    voices: [Voice; NUM_VOICES],
    harm: Smoother,
    mod_idx: Smoother,
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
    sample_rate: f32,
    next_serial: u64,
}

impl FmInst {
    pub fn new() -> Self {
        // Schema defaults: harm 3, modIdx 10, ADSR 0.005/0.3/0.4/0.6.
        let mut inst = Self {
            voices: core::array::from_fn(|_| Voice::new()),
            harm: Smoother::new(3.0),
            mod_idx: Smoother::new(10.0),
            attack: 0.005,
            decay: 0.3,
            sustain: 0.4,
            release: 0.6,
            sample_rate: 44_100.0,
            next_serial: 1,
        };
        inst.apply_adsr();
        inst
    }

    fn apply_adsr(&mut self) {
        for v in &mut self.voices {
            v.amp_env
                .set_adsr(self.attack, self.decay, self.sustain, self.release);
        }
    }

    /// Voice to (re)use: same held pitch first (Tone.PolySynth reuses the
    /// per-note voice), then a free voice, else steal the oldest.
    fn alloc_voice(&mut self, pitch: u8) -> &mut Voice {
        let mut same: Option<usize> = None;
        let mut free: Option<usize> = None;
        let mut oldest = 0usize;
        let mut oldest_serial = u64::MAX;
        for (i, v) in self.voices.iter().enumerate() {
            if v.held && v.pitch == pitch {
                same = Some(i);
                break;
            }
            if !v.active() {
                free.get_or_insert(i);
            }
            if v.serial < oldest_serial {
                oldest_serial = v.serial;
                oldest = i;
            }
        }
        &mut self.voices[same.or(free).unwrap_or(oldest)]
    }

    fn start(&mut self, pitch: u8, vel: f32, gate_samples: u64) {
        let serial = self.next_serial;
        self.next_serial += 1;
        let v = self.alloc_voice(pitch);
        v.pitch = pitch;
        v.freq = midi_hz(pitch as f32);
        v.vel = vel.clamp(0.0, 1.0);
        v.serial = serial;
        v.held = gate_samples == 0;
        v.gate_remaining = gate_samples;
        v.amp_env.note_on();
        v.mod_env.note_on();
    }
}

impl Default for FmInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for FmInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.harm.prepare(self.sample_rate);
        self.mod_idx.prepare(self.sample_rate);
        for v in &mut self.voices {
            v.prepare(self.sample_rate);
        }
        self.apply_adsr();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "harm" => self.harm.set_target(value.clamp(0.25, 8.0)),
            "modIdx" => self.mod_idx.set_target(value.clamp(0.5, 40.0)),
            // Envelope edits apply to all voices, live ones included, matching
            // synth.set({ envelope: { … } }) in makeFm().
            "attack" => {
                self.attack = value.clamp(0.001, 2.0);
                self.apply_adsr();
            }
            "decay" => {
                self.decay = value.clamp(0.01, 2.0);
                self.apply_adsr();
            }
            "sustain" => {
                self.sustain = value.clamp(0.0, 1.0);
                self.apply_adsr();
            }
            "release" => {
                self.release = value.clamp(0.01, 4.0);
                self.apply_adsr();
            }
            _ => {}
        }
    }

    fn note_on(&mut self, pitch: u8, vel: f32) {
        self.start(pitch, vel, 0);
    }

    fn note_off(&mut self, pitch: u8) {
        for v in &mut self.voices {
            if v.held && v.pitch == pitch {
                v.release();
            }
        }
    }

    fn trigger(&mut self, pitch: u8, dur_sec: f32, vel: f32) {
        let gate = (dur_sec.max(0.0) * self.sample_rate) as u64;
        self.start(pitch, vel, gate.max(1));
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let inv_sr = 1.0 / self.sample_rate;
        let n = l.len().min(r.len());
        for i in 0..n {
            let harm = self.harm.next();
            let mod_idx = self.mod_idx.next();
            let mut s = 0.0f32;
            for v in &mut self.voices {
                if !v.active() {
                    continue;
                }
                if v.gate_remaining > 0 {
                    v.gate_remaining -= 1;
                    if v.gate_remaining == 0 {
                        v.release();
                    }
                }
                s += v.render(harm, mod_idx, inv_sr);
            }
            l[i] += s;
            r[i] += s;
        }
    }

    fn reset(&mut self) {
        for v in &mut self.voices {
            v.amp_env.reset();
            v.mod_env.reset();
            v.held = false;
            v.gate_remaining = 0;
            v.carrier_phase = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    /// Render in ≤128-frame chunks (block-model contract) into a mono capture.
    fn render(inst: &mut FmInst, secs: f32) -> Vec<f32> {
        let n = (secs * SR) as usize;
        let mut out = Vec::with_capacity(n);
        let mut l = [0.0f32; 128];
        let mut r = [0.0f32; 128];
        let mut done = 0;
        while done < n {
            let len = (n - done).min(128);
            l[..len].fill(0.0);
            r[..len].fill(0.0);
            inst.process(&mut l[..len], &mut r[..len]);
            out.extend_from_slice(&l[..len]);
            done += len;
        }
        out
    }

    fn peak(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |a, s| a.max(s.abs()))
    }

    fn rms(buf: &[f32]) -> f32 {
        libm::sqrtf(buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32)
    }

    #[test]
    fn additive_and_silent_when_idle() {
        let mut d = FmInst::new();
        d.prepare(SR);
        let mut l = [0.5f32; 64];
        let mut r = [0.5f32; 64];
        d.process(&mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.5));
    }

    #[test]
    fn two_seconds_no_nan_inf() {
        let mut d = FmInst::new();
        d.prepare(SR);
        d.note_on(60, 0.9);
        d.trigger(67, 0.5, 0.8);
        let buf = render(&mut d, 1.0);
        d.note_off(60);
        let tail = render(&mut d, 1.0);
        assert!(buf.iter().chain(tail.iter()).all(|s| s.is_finite()));
        assert!(peak(&buf) > 1e-3, "notes should be audible");
    }

    #[test]
    fn param_sweep_stays_bounded() {
        for harm in [0.25, 3.0, 8.0] {
            for mod_idx in [0.5, 10.0, 40.0] {
                for (a, dec, sus, rel) in [(0.001, 0.01, 0.0, 0.01), (2.0, 2.0, 1.0, 4.0)] {
                    let mut d = FmInst::new();
                    d.prepare(SR);
                    d.set_param("harm", harm);
                    d.set_param("modIdx", mod_idx);
                    d.set_param("attack", a);
                    d.set_param("decay", dec);
                    d.set_param("sustain", sus);
                    d.set_param("release", rel);
                    for p in [24u8, 60, 108] {
                        d.note_on(p, 1.0);
                    }
                    let buf = render(&mut d, 0.5);
                    for p in [24u8, 60, 108] {
                        d.note_off(p);
                    }
                    let tail = render(&mut d, 0.25);
                    assert!(
                        buf.iter().chain(tail.iter()).all(|s| s.is_finite()),
                        "harm {harm} modIdx {mod_idx}"
                    );
                    // 3 voices at vel 1 can sum to at most ~3
                    assert!(peak(&buf) <= 3.5, "harm {harm} modIdx {mod_idx}: {}", peak(&buf));
                }
            }
        }
    }

    #[test]
    fn note_sounds_then_decays_to_silence_after_release() {
        let mut d = FmInst::new();
        d.prepare(SR);
        d.set_param("release", 0.2);
        d.note_on(64, 1.0);
        let held = render(&mut d, 0.4);
        assert!(rms(&held) > 1e-3, "held note is silent");
        d.note_off(64);
        let _tail = render(&mut d, 1.0);
        let after = render(&mut d, 0.2);
        assert!(peak(&after) < 1e-3, "did not decay: {}", peak(&after));
    }

    #[test]
    fn trigger_auto_releases() {
        let mut d = FmInst::new();
        d.prepare(SR);
        d.set_param("release", 0.1);
        d.trigger(60, 0.2, 1.0);
        let body = render(&mut d, 0.2);
        assert!(rms(&body) > 1e-3);
        let _tail = render(&mut d, 1.0);
        let after = render(&mut d, 0.2);
        assert!(peak(&after) < 1e-3, "trigger never released: {}", peak(&after));
    }

    #[test]
    fn voice_pool_steals_instead_of_growing() {
        let mut d = FmInst::new();
        d.prepare(SR);
        for p in 40..70 {
            d.note_on(p, 1.0); // 30 notes into a 12-voice pool
        }
        let buf = render(&mut d, 0.25);
        assert!(buf.iter().all(|s| s.is_finite()));
        assert!(peak(&buf) <= NUM_VOICES as f32 + 0.5);
    }

    /// FM analytic check: raising modIdx widens the sideband spread, so the
    /// share of energy above the carrier region must grow. First-difference
    /// energy ratio is a high-frequency-emphasis proxy that needs no FFT.
    #[test]
    fn higher_mod_index_adds_hf_sidebands() {
        let hf_ratio = |mod_idx: f32| {
            let mut d = FmInst::new();
            d.prepare(SR);
            d.set_param("modIdx", mod_idx);
            d.set_param("harm", 2.0);
            d.note_on(57, 1.0); // A3
            let buf = render(&mut d, 1.0);
            let settled = &buf[(0.2 * SR) as usize..]; // skip smoother/attack
            let diff_energy: f32 = settled.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum();
            let energy: f32 = settled.iter().map(|s| s * s).sum();
            diff_energy / energy.max(1e-12)
        };
        let low = hf_ratio(1.0);
        let high = hf_ratio(25.0);
        assert!(
            high > low * 2.0,
            "modIdx 25 should have far more HF energy than 1: {high} vs {low}"
        );
    }
}
