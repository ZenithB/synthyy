//! `keys` instrument — "Dream Keys".
//!
//! Port of `makeKeys()` (src/audio/devices.ts): Tone.PolySynth(Tone.AMSynth,
//! maxPolyphony 12) with `modulation: sine`. Per AMSynth topology each voice is
//!
//!   out = carrier_sine(f) · ampEnv · vel · g₋₁₀ · a2g(mod_sine(harm·f) · modEnv · vel · g₋₁₀)
//!
//! where `a2g(x) = (x + 1) / 2` (Tone's AudioToGain) — the carrier is
//! amplitude-modulated by the modulator, producing sidebands at f·(1 ± harm) —
//! and `g₋₁₀ = 10^(-10/20)`: Tone's ModulationSynth constructs BOTH the
//! carrier and modulator inner Synths with `volume: -10` dB
//! (node_modules/tone/build/esm/instrument/ModulationSynth.js), and each
//! Synth's Volume node sits inside its output, so the carrier is trimmed
//! -10 dB and the modulation depth is 10^(-10/20) ≈ 0.316, not 1. Velocity
//! scales the modulator envelope too (ModulationSynth._triggerEnvelopeAttack
//! forwards `velocity` to both inner Synths). AMSynth defaults not exposed by
//! the schema: carrier type sine, modulationEnvelope (attack 0.5, decay 0,
//! sustain 1, release 0.5). Schema params (src/audio/schema.ts "keys"): harm
//! 0.5–4 (def 2) plus amp-envelope adsr(0.01, 0.4, 0.5, 0.8).

use crate::traits::InstrumentDsp;
use crate::util::{midi_hz, Adsr, PolyBlepOsc, Smoother, Wave, MAX_BLOCK};

const NUM_VOICES: usize = 12; // maxPolyphony in makeKeys()

// Tone.AMSynth default modulationEnvelope (not schema-addressable).
const MOD_ENV: (f32, f32, f32, f32) = (0.5, 0.0, 1.0, 0.5);

/// `10^(-10/20)`: Tone.ModulationSynth builds its carrier and modulator inner
/// Synths with `volume: -10` dB (tone/build/esm/instrument/ModulationSynth.js,
/// the two `new Synth({ ..., volume: -10 })` calls). Applied to the carrier as
/// an output trim and to the modulator as the AM depth into AudioToGain.
const SYNTH_VOLUME_GAIN: f32 = 0.316_227_77;

struct Voice {
    carrier: PolyBlepOsc,
    modulator: PolyBlepOsc,
    amp_env: Adsr,
    mod_env: Adsr,
    pitch: u8,
    freq: f32,
    vel: f32,
    /// Samples left until scheduled release (`trigger` path); 0 = held.
    remaining: u64,
    held: bool,
    active: bool,
    /// Allocation stamp for oldest-voice stealing.
    age: u64,
}

impl Voice {
    fn new() -> Self {
        Self {
            carrier: PolyBlepOsc::new(Wave::Sine),
            modulator: PolyBlepOsc::new(Wave::Sine),
            amp_env: Adsr::new(),
            mod_env: Adsr::new(),
            pitch: 0,
            freq: 440.0,
            vel: 0.0,
            remaining: 0,
            held: false,
            active: false,
            age: 0,
        }
    }

    fn prepare(&mut self, sample_rate: f32) {
        self.carrier.prepare(sample_rate);
        self.modulator.prepare(sample_rate);
        self.amp_env.prepare(sample_rate);
        self.mod_env.prepare(sample_rate);
        self.mod_env.set_adsr(MOD_ENV.0, MOD_ENV.1, MOD_ENV.2, MOD_ENV.3);
        self.kill();
    }

    fn kill(&mut self) {
        self.amp_env.reset();
        self.mod_env.reset();
        self.active = false;
        self.held = false;
        self.remaining = 0;
    }

    fn start(&mut self, pitch: u8, vel: f32, age: u64) {
        self.pitch = pitch;
        self.freq = midi_hz(f32::from(pitch));
        self.vel = vel.clamp(0.0, 1.0);
        self.carrier.set_freq(self.freq);
        // Envelopes re-attack from their current level (Tone semantics), so a
        // stolen/retriggered voice ramps without a click; oscillators stay
        // phase-continuous.
        self.amp_env.note_on();
        self.mod_env.note_on();
        self.held = true;
        self.active = true;
        self.remaining = 0;
        self.age = age;
    }

    fn release(&mut self) {
        self.held = false;
        self.remaining = 0;
        self.amp_env.note_off();
        self.mod_env.note_off();
    }

    /// Render ADDITIVELY into `l`/`r`. `harm` holds the per-sample smoothed
    /// harmonicity for this chunk (same length as `l`).
    fn render(&mut self, l: &mut [f32], r: &mut [f32], harm: &[f32]) {
        for ((ls, rs), &h) in l.iter_mut().zip(r.iter_mut()).zip(harm.iter()) {
            if self.remaining > 0 {
                self.remaining -= 1;
                if self.remaining == 0 {
                    self.release();
                }
            }
            if !self.amp_env.is_active() {
                self.active = false;
                return;
            }
            self.modulator.set_freq(h * self.freq);
            // Modulator Synth output = sine · modEnv · vel · (-10 dB volume);
            // AudioToGain then maps it to a gain (see module docs).
            let m = self.modulator.next() * self.mod_env.next() * self.vel * SYNTH_VOLUME_GAIN;
            let a2g = (m + 1.0) * 0.5;
            // Carrier Synth output = sine · ampEnv · vel · (-10 dB volume),
            // multiplied by the modulation gain.
            let s = self.carrier.next() * self.amp_env.next() * self.vel * SYNTH_VOLUME_GAIN * a2g;
            *ls += s;
            *rs += s;
        }
    }
}

pub struct KeysInst {
    voices: [Voice; NUM_VOICES],
    harm: Smoother,
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,
    sample_rate: f32,
    age_counter: u64,
    harm_buf: [f32; MAX_BLOCK],
}

impl KeysInst {
    pub fn new() -> Self {
        Self {
            voices: core::array::from_fn(|_| Voice::new()),
            harm: Smoother::new(2.0),
            attack: 0.01,
            decay: 0.4,
            sustain: 0.5,
            release: 0.8,
            sample_rate: 44_100.0,
            age_counter: 0,
            harm_buf: [2.0; MAX_BLOCK],
        }
    }

    fn apply_env(&mut self) {
        for v in &mut self.voices {
            v.amp_env
                .set_adsr(self.attack, self.decay, self.sustain, self.release);
        }
    }

    /// Same-pitch reuse first (PolySynth reuses the voice already playing the
    /// note), then a free voice, then steal the oldest.
    fn alloc_voice(&mut self, pitch: u8) -> &mut Voice {
        let mut free: Option<usize> = None;
        let mut oldest = 0usize;
        let mut oldest_age = u64::MAX;
        for (i, v) in self.voices.iter().enumerate() {
            if v.active {
                if v.pitch == pitch {
                    return &mut self.voices[i];
                }
                if v.age < oldest_age {
                    oldest_age = v.age;
                    oldest = i;
                }
            } else if free.is_none() {
                free = Some(i);
            }
        }
        &mut self.voices[free.unwrap_or(oldest)]
    }

    fn start_voice(&mut self, pitch: u8, vel: f32, dur_sec: Option<f32>) {
        self.age_counter += 1;
        let age = self.age_counter;
        let remaining = dur_sec.map(|d| {
            let n = (d.max(0.0) * self.sample_rate) as u64;
            n.max(1)
        });
        let v = self.alloc_voice(pitch);
        v.start(pitch, vel, age);
        if let Some(n) = remaining {
            v.remaining = n;
        }
    }
}

impl Default for KeysInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for KeysInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.harm.prepare(self.sample_rate);
        self.harm.snap(self.harm.target());
        for v in &mut self.voices {
            v.prepare(self.sample_rate);
        }
        self.apply_env();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "harm" => self.harm.set_target(value.clamp(0.5, 4.0)),
            "attack" => {
                self.attack = value.clamp(0.001, 2.0);
                self.apply_env();
            }
            "decay" => {
                self.decay = value.clamp(0.01, 2.0);
                self.apply_env();
            }
            "sustain" => {
                self.sustain = value.clamp(0.0, 1.0);
                self.apply_env();
            }
            "release" => {
                self.release = value.clamp(0.01, 4.0);
                self.apply_env();
            }
            _ => {}
        }
    }

    fn note_on(&mut self, pitch: u8, vel: f32) {
        self.start_voice(pitch, vel, None);
    }

    fn note_off(&mut self, pitch: u8) {
        for v in &mut self.voices {
            if v.active && v.held && v.pitch == pitch {
                v.release();
            }
        }
    }

    fn trigger(&mut self, pitch: u8, dur_sec: f32, vel: f32) {
        self.start_voice(pitch, vel, Some(dur_sec));
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        debug_assert_eq!(l.len(), r.len());
        for (lc, rc) in l.chunks_mut(MAX_BLOCK).zip(r.chunks_mut(MAX_BLOCK)) {
            let n = lc.len().min(rc.len());
            for h in self.harm_buf[..n].iter_mut() {
                *h = self.harm.next();
            }
            for v in &mut self.voices {
                if v.active {
                    v.render(&mut lc[..n], &mut rc[..n], &self.harm_buf[..n]);
                }
            }
        }
    }

    fn reset(&mut self) {
        for v in &mut self.voices {
            v.kill();
        }
        self.harm.snap(self.harm.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 44_100.0;

    fn make() -> KeysInst {
        let mut d = KeysInst::new();
        d.prepare(SR);
        d
    }

    fn run(d: &mut KeysInst, seconds: f32) -> (Vec<f32>, Vec<f32>) {
        let n = (seconds * SR) as usize;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        for (lc, rc) in l.chunks_mut(MAX_BLOCK).zip(r.chunks_mut(MAX_BLOCK)) {
            d.process(lc, rc);
        }
        (l, r)
    }

    fn rms(buf: &[f32]) -> f32 {
        libm::sqrtf(buf.iter().map(|s| s * s).sum::<f32>() / buf.len().max(1) as f32)
    }

    /// Goertzel magnitude at `freq` over `buf`.
    fn goertzel(buf: &[f32], freq: f32) -> f32 {
        let w = core::f32::consts::TAU * freq / SR;
        let coeff = 2.0 * libm::cosf(w);
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in buf {
            let s0 = x + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        libm::sqrtf(power.max(0.0)) / buf.len() as f32
    }

    #[test]
    fn additive_and_silent_when_idle() {
        let mut d = make();
        let mut l = [0.25f32; 64];
        let mut r = [0.25f32; 64];
        d.process(&mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.25));
    }

    #[test]
    fn no_nan_inf_over_two_seconds() {
        let mut d = make();
        d.note_on(48, 1.0);
        d.note_on(60, 0.8);
        d.trigger(72, 0.3, 1.0);
        let (l, r) = run(&mut d, 0.5);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
        d.note_off(48);
        d.set_param("harm", 4.0);
        d.set_param("release", 0.05);
        let (l, r) = run(&mut d, 1.5);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
    }

    #[test]
    fn param_sweep_stays_bounded() {
        let sweeps: [(&str, f32, f32); 5] = [
            ("harm", 0.5, 4.0),
            ("attack", 0.001, 2.0),
            ("decay", 0.01, 2.0),
            ("sustain", 0.0, 1.0),
            ("release", 0.01, 4.0),
        ];
        let mut d = make();
        for pitch in [36u8, 60, 84, 108] {
            d.note_on(pitch, 1.0);
        }
        for step in 0..=10 {
            let t = step as f32 / 10.0;
            for (key, lo, hi) in sweeps {
                d.set_param(key, lo + (hi - lo) * t);
            }
            let (l, r) = run(&mut d, 0.05);
            // 12 voices at unit velocity can stack, but must stay well-bounded
            assert!(
                l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() < 16.0),
                "unbounded at sweep step {step}"
            );
        }
    }

    #[test]
    fn note_sounds_then_release_reaches_silence() {
        let mut d = make();
        d.set_param("release", 0.2);
        d.note_on(60, 1.0);
        let (l, _) = run(&mut d, 0.3);
        assert!(rms(&l) > 1e-3, "note should be audible: rms {}", rms(&l));
        d.note_off(60);
        let _ = run(&mut d, 1.0); // >> release + env floor
        let (l, r) = run(&mut d, 0.2);
        assert!(rms(&l) == 0.0 && rms(&r) == 0.0, "release must reach exact silence");
    }

    #[test]
    fn trigger_auto_releases() {
        let mut d = make();
        d.set_param("release", 0.1);
        d.trigger(64, 0.1, 1.0);
        let (l, _) = run(&mut d, 0.1);
        assert!(rms(&l) > 1e-3);
        let _ = run(&mut d, 1.0);
        let (l, _) = run(&mut d, 0.1);
        assert_eq!(rms(&l), 0.0, "triggered note must decay to silence");
    }

    #[test]
    fn harm_shifts_am_sidebands() {
        // AM sidebands sit at f0·(1 ± harm). At f0 = 440 (MIDI 69):
        //   harm 0.5 → 220 / 660 Hz;  harm 3 → 1760 Hz (and a folded lower band).
        // 660 Hz must be strong only for harm 0.5, 1760 Hz only for harm 3.
        let analyze = |harm: f32| -> (f32, f32, f32) {
            let mut d = make();
            d.set_param("harm", harm);
            d.prepare(SR); // re-snap smoother state after param change
            d.set_param("harm", harm);
            d.set_param("sustain", 1.0);
            d.note_on(69, 1.0);
            let (l, _) = run(&mut d, 1.0); // past the 0.5 s mod-env attack
            let tail = &l[(0.8 * SR) as usize..];
            (goertzel(tail, 440.0), goertzel(tail, 660.0), goertzel(tail, 1760.0))
        };
        let (c_a, s660_a, s1760_a) = analyze(0.5);
        let (c_b, s660_b, s1760_b) = analyze(3.0);
        // carrier present in both
        assert!(c_a > 0.05 && c_b > 0.05, "carrier missing: {c_a} {c_b}");
        assert!(
            s660_a > 10.0 * s660_b.max(1e-6),
            "660 Hz sideband should follow harm=0.5: {s660_a} vs {s660_b}"
        );
        assert!(
            s1760_b > 10.0 * s1760_a.max(1e-6),
            "1760 Hz sideband should follow harm=3: {s1760_b} vs {s1760_a}"
        );
    }

    #[test]
    fn voice_stealing_is_bounded_and_oldest_first() {
        let mut d = make();
        // 20 notes into a 12-voice pool: must not blow up, oldest are stolen.
        for p in 40..60u8 {
            d.note_on(p, 1.0);
            let mut l = [0.0f32; 32];
            let mut r = [0.0f32; 32];
            d.process(&mut l, &mut r);
            assert!(l.iter().all(|s| s.is_finite()));
        }
        let active = d.voices.iter().filter(|v| v.active).count();
        assert!(active <= NUM_VOICES);
        // the 12 newest pitches survive
        let min_pitch = d
            .voices
            .iter()
            .filter(|v| v.active)
            .map(|v| v.pitch)
            .min()
            .unwrap();
        assert_eq!(min_pitch, 48, "oldest voices should have been stolen");
    }
}
