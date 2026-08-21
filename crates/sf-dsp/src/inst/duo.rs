//! `duo` instrument — port of Tone.DuoSynth as wired by `makeDuo()` in
//! src/audio/devices.ts. Per voice: two sawtooth layers, the second at
//! `harm` × the base frequency; a shared vibrato LFO detuning both by
//! ±50 cents × `vibAmt` at `vibRate`; one lowpass (Q 1, 12 dB — the filter is
//! linear and both MonoSynth halves run identical settings, so one SVF on the
//! summed oscillators equals Tone's two); DuoSynth's default filter envelope
//! (0.01/0/1/0.5, 200 Hz base + 3 octaves linear-scaled through env², per
//! Tone.FrequencyEnvelope); the schema ADSR on the amp; both layers sum at
//! unity into the 0 dB instrument output (Tone 15 has no voice volume).
//! The browser device caps `maxPolyphony` at 6 ⇒ 6-voice pool, oldest-stolen.

use crate::traits::InstrumentDsp;
use crate::util::{lfo_shape_value, midi_hz, Adsr, PolyBlepOsc, Smoother, Svf, SvfMode, Wave};

const NUM_VOICES: usize = 6; // makeDuo: maxPolyphony 6
// Tone 15.1.22 DuoSynth has NO per-voice attenuation: `getDefaults()` sets no
// voice volume and the constructor wires `voice0.connect(this.output)` /
// `voice1.connect(this.output)` straight into the 0 dB Instrument volume node
// (node_modules/tone/build/esm/instrument/DuoSynth.js). The −10 dB voice
// volume was Tone v13; here both MonoSynth layers sum at unity.
const LAYER_GAIN: f32 = 1.0;
// Web Audio's native sawtooth is Σ (−1)^(n+1)·(2/(nπ))·sin(2πnft) — a ramp
// that starts at 0 mid-rise, i.e. our ramp `2t−1` at phase 0.5. Starting both
// oscillators there reproduces the browser's relative harmonic signs, which
// is load-bearing whenever `harm` is an integer ratio: the layers' overlapping
// partials then interfere exactly as they do in Tone (alternating sign) rather
// than fully constructively (~3 dB hot at harm = 2).
const SAW_START_PHASE: f32 = 0.5;
const FILT_BASE_HZ: f32 = 200.0; // MonoSynth filterEnvelope.baseFrequency
const FILT_TOP_HZ: f32 = 1_600.0; // base × 2^octaves, octaves = 3
const FILT_Q: f32 = 1.0; // MonoSynth filter default
// DuoSynth default filterEnvelope (makeDuo leaves it untouched)
const FENV: (f32, f32, f32, f32) = (0.01, 0.0, 1.0, 0.5);
const VIB_RANGE_CENTS: f32 = 50.0; // DuoSynth vibrato LFO span at vibAmt = 1

struct Voice {
    osc0: PolyBlepOsc,
    osc1: PolyBlepOsc,
    filt: Svf,
    amp: Adsr,
    fenv: Adsr,
    freq: f32,
    pitch: u8,
    vel: f32,
    age: u64,
    /// Remaining samples until the scheduler-path auto release; `None` = held.
    hold: Option<u32>,
    released: bool,
    last_cutoff: f32,
}

impl Voice {
    fn new() -> Self {
        let mut fenv = Adsr::new();
        fenv.set_adsr(FENV.0, FENV.1, FENV.2, FENV.3);
        Self {
            osc0: PolyBlepOsc::new(Wave::Saw),
            osc1: PolyBlepOsc::new(Wave::Saw),
            filt: Svf::new(SvfMode::Lowpass),
            amp: Adsr::new(),
            fenv,
            freq: 440.0,
            pitch: 0,
            vel: 0.0,
            age: 0,
            hold: None,
            released: false,
            last_cutoff: 0.0,
        }
    }

    fn prepare(&mut self, sample_rate: f32) {
        self.osc0.prepare(sample_rate);
        self.osc1.prepare(sample_rate);
        self.filt.prepare(sample_rate);
        self.amp.prepare(sample_rate);
        self.fenv.prepare(sample_rate);
        self.fenv.set_adsr(FENV.0, FENV.1, FENV.2, FENV.3);
        self.last_cutoff = 0.0;
    }

    fn start(&mut self, pitch: u8, vel: f32, age: u64, hold: Option<u32>, adsr: [f32; 4]) {
        self.pitch = pitch;
        self.freq = midi_hz(f32::from(pitch));
        self.vel = vel.clamp(0.0, 1.0);
        self.age = age;
        self.hold = hold;
        self.released = false;
        self.amp.set_adsr(adsr[0], adsr[1], adsr[2], adsr[3]);
        // phase-coherent start; envelopes attack from their current level so
        // stolen voices don't click
        self.osc0.reset_phase(SAW_START_PHASE);
        self.osc1.reset_phase(SAW_START_PHASE);
        self.amp.note_on();
        self.fenv.note_on();
    }

    fn release(&mut self) {
        self.released = true;
        self.hold = None;
        self.amp.note_off();
        self.fenv.note_off();
    }

    /// One mono sample (pre layer-gain). `vib` is the frequency multiplier
    /// from the shared vibrato LFO, `harm` the smoothed interval ratio.
    #[inline]
    fn sample(&mut self, harm: f32, vib: f32) -> f32 {
        if !self.amp.is_active() {
            return 0.0;
        }
        if let Some(rem) = self.hold {
            if rem == 0 {
                self.release();
            } else {
                self.hold = Some(rem - 1);
            }
        }
        // Tone.FrequencyEnvelope: linear Scale(base, base·2^oct) after env^2
        let fe = self.fenv.next();
        let cutoff = FILT_BASE_HZ + (FILT_TOP_HZ - FILT_BASE_HZ) * fe * fe;
        if (cutoff - self.last_cutoff).abs() > self.last_cutoff * 0.005 {
            self.filt.set_cutoff_q(cutoff, FILT_Q);
            self.last_cutoff = cutoff;
        }
        self.osc0.set_freq(self.freq * vib);
        self.osc1.set_freq(self.freq * harm * vib);
        let x = self.osc0.next() + self.osc1.next();
        self.filt.process_sample(x) * self.amp.next() * self.vel
    }
}

pub struct DuoInst {
    voices: [Voice; NUM_VOICES],
    harm: Smoother,
    vib_amt: Smoother,
    vib_rate: Smoother,
    adsr: [f32; 4],
    vib_phase: f64,
    sample_rate: f32,
    counter: u64,
}

impl DuoInst {
    pub fn new() -> Self {
        Self {
            voices: core::array::from_fn(|_| Voice::new()),
            // schema defaults: harm 1.5, vibAmt 0.12, vibRate 4.5
            harm: Smoother::new(1.5),
            vib_amt: Smoother::new(0.12),
            vib_rate: Smoother::new(4.5),
            adsr: [0.02, 0.3, 0.6, 0.6],
            vib_phase: 0.0,
            sample_rate: 44_100.0,
            counter: 0,
        }
    }

    fn start_voice(&mut self, pitch: u8, vel: f32, hold: Option<u32>) {
        self.counter += 1;
        // reuse the held voice on this pitch, else a free voice, else steal
        // the oldest (smallest start counter)
        let idx = self
            .voices
            .iter()
            .position(|v| v.amp.is_active() && !v.released && v.pitch == pitch)
            .or_else(|| self.voices.iter().position(|v| !v.amp.is_active()))
            .unwrap_or_else(|| {
                let mut best = 0;
                let mut best_age = u64::MAX;
                for (i, v) in self.voices.iter().enumerate() {
                    if v.age < best_age {
                        best_age = v.age;
                        best = i;
                    }
                }
                best
            });
        self.voices[idx].start(pitch, vel, self.counter, hold, self.adsr);
    }

    fn apply_adsr(&mut self) {
        for v in &mut self.voices {
            v.amp
                .set_adsr(self.adsr[0], self.adsr[1], self.adsr[2], self.adsr[3]);
        }
    }

    #[inline]
    fn advance_vibrato(&mut self) -> f64 {
        let rate = self.vib_rate.next();
        self.vib_phase += f64::from(rate / self.sample_rate);
        if self.vib_phase >= 1.0 {
            self.vib_phase -= 1.0;
        }
        self.vib_phase
    }
}

impl Default for DuoInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for DuoInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for v in &mut self.voices {
            v.prepare(self.sample_rate);
        }
        self.harm.prepare(self.sample_rate);
        self.vib_amt.prepare(self.sample_rate);
        self.vib_rate.prepare(self.sample_rate);
        self.harm.snap(self.harm.target());
        self.vib_amt.snap(self.vib_amt.target());
        self.vib_rate.snap(self.vib_rate.target());
        self.apply_adsr();
        self.vib_phase = 0.0;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "harm" => self.harm.set_target(value.clamp(0.5, 3.0)),
            "vibAmt" => self.vib_amt.set_target(value.clamp(0.0, 0.6)),
            "vibRate" => self.vib_rate.set_target(value.clamp(0.5, 10.0)),
            "attack" => {
                self.adsr[0] = value.max(0.0);
                self.apply_adsr();
            }
            "decay" => {
                self.adsr[1] = value.max(0.0);
                self.apply_adsr();
            }
            "sustain" => {
                self.adsr[2] = value.clamp(0.0, 1.0);
                self.apply_adsr();
            }
            "release" => {
                self.adsr[3] = value.max(0.0);
                self.apply_adsr();
            }
            _ => {}
        }
    }

    fn note_on(&mut self, pitch: u8, vel: f32) {
        self.start_voice(pitch, vel, None);
    }

    fn note_off(&mut self, pitch: u8) {
        for v in &mut self.voices {
            if v.amp.is_active() && !v.released && v.pitch == pitch {
                v.release();
            }
        }
    }

    fn trigger(&mut self, pitch: u8, dur_sec: f32, vel: f32) {
        let samples = (dur_sec.max(0.0) * self.sample_rate) as u32;
        self.start_voice(pitch, vel, Some(samples.max(1)));
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        if !self.voices.iter().any(|v| v.amp.is_active()) {
            // keep smoothers and the vibrato phase moving while silent
            for _ in 0..n {
                self.harm.next();
                self.vib_amt.next();
                self.advance_vibrato();
            }
            return;
        }
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()).take(n) {
            let harm = self.harm.next();
            let amt = self.vib_amt.next();
            let phase = self.advance_vibrato();
            let cents = lfo_shape_value(0, phase) * VIB_RANGE_CENTS * amt;
            let vib = libm::powf(2.0, cents / 1_200.0);
            let mut sum = 0.0;
            for v in &mut self.voices {
                sum += v.sample(harm, vib);
            }
            let s = sum * LAYER_GAIN;
            *ls += s;
            *rs += s;
        }
    }

    fn reset(&mut self) {
        for v in &mut self.voices {
            v.amp.reset();
            v.fenv.reset();
            v.filt.reset();
            v.hold = None;
            v.released = false;
            v.last_cutoff = 0.0;
        }
        self.vib_phase = 0.0;
        self.harm.snap(self.harm.target());
        self.vib_amt.snap(self.vib_amt.target());
        self.vib_rate.snap(self.vib_rate.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 44_100.0;
    const BLOCK: usize = 128;

    fn rms(buf: &[f32]) -> f32 {
        libm::sqrtf(buf.iter().map(|s| s * s).sum::<f32>() / buf.len().max(1) as f32)
    }

    /// Render `secs` of ADDITIVE output into fresh zeroed buffers; returns L.
    fn render(d: &mut DuoInst, secs: f32) -> Vec<f32> {
        let n = (secs * SR) as usize;
        let mut out = vec![0.0f32; n];
        let mut r = [0.0f32; BLOCK];
        for chunk in out.chunks_mut(BLOCK) {
            let m = chunk.len();
            r[..m].fill(0.0);
            d.process(chunk, &mut r[..m]);
        }
        out
    }

    #[test]
    fn additive_and_silent_when_idle() {
        let mut d = DuoInst::new();
        d.prepare(SR);
        let mut l = [0.5f32; BLOCK];
        let mut r = [0.5f32; BLOCK];
        d.process(&mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.5));
    }

    #[test]
    fn no_nan_inf_over_two_seconds() {
        let mut d = DuoInst::new();
        d.prepare(SR);
        d.note_on(48, 0.9);
        d.note_on(60, 1.0);
        d.trigger(67, 0.5, 0.8);
        let a = render(&mut d, 1.0);
        d.note_off(48);
        d.note_off(60);
        let b = render(&mut d, 1.0);
        assert!(a.iter().chain(b.iter()).all(|s| s.is_finite()));
    }

    #[test]
    fn param_sweep_stays_bounded() {
        // schema ranges (fixtures/schema-dump.json "duo")
        let ranges: [(&str, f32, f32); 7] = [
            ("harm", 0.5, 3.0),
            ("vibAmt", 0.0, 0.6),
            ("vibRate", 0.5, 10.0),
            ("attack", 0.001, 2.0),
            ("decay", 0.01, 2.0),
            ("sustain", 0.0, 1.0),
            ("release", 0.01, 4.0),
        ];
        let mut d = DuoInst::new();
        d.prepare(SR);
        d.note_on(36, 1.0);
        d.note_on(96, 1.0);
        for (key, lo, hi) in ranges {
            for val in [lo, 0.5 * (lo + hi), hi] {
                d.set_param(key, val);
                let buf = render(&mut d, 0.1);
                // 2 voices × 2 unity saw layers (+ filter resonance) can
                // legitimately peak near 4–5; bound guards blow-ups only
                assert!(
                    buf.iter().all(|s| s.is_finite() && s.abs() < 8.0),
                    "{key}={val} blew up"
                );
            }
        }
    }

    #[test]
    fn note_sounds_then_decays_to_silence() {
        let mut d = DuoInst::new();
        d.prepare(SR);
        assert!(rms(&render(&mut d, 0.1)) < 1e-6, "silent before note");
        d.note_on(60, 1.0);
        assert!(rms(&render(&mut d, 0.5)) > 0.01, "note is audible");
        d.note_off(60);
        render(&mut d, 3.0); // default release 0.6 s — let the tail die
        assert!(rms(&render(&mut d, 0.2)) < 1e-4, "tail did not decay");

        // scheduler path auto-releases without note_off
        d.trigger(60, 0.2, 1.0);
        assert!(rms(&render(&mut d, 0.2)) > 0.01);
        render(&mut d, 3.0);
        assert!(rms(&render(&mut d, 0.2)) < 1e-4, "trigger tail did not decay");
    }

    /// Peak/trough ratio of short-window RMS during sustain: near 1 for a
    /// static periodic signal, well above 1 when the two layers beat.
    fn beat_ratio(harm: f32) -> f32 {
        let mut d = DuoInst::new();
        d.prepare(SR);
        d.set_param("vibAmt", 0.0); // isolate the harm-induced beating
        d.set_param("harm", harm);
        d.set_param("attack", 0.01);
        d.set_param("decay", 0.01);
        d.set_param("sustain", 1.0);
        d.prepare(SR); // snap smoothers to the new targets
        d.note_on(57, 1.0); // 220 Hz
        render(&mut d, 0.4); // settle attack + filter sweep
        let buf = render(&mut d, 1.0);
        let mut lo = f32::MAX;
        let mut hi = 0.0f32;
        for w in buf.chunks(2_048) {
            let r = rms(w);
            lo = lo.min(r);
            hi = hi.max(r);
        }
        hi / lo.max(1e-9)
    }

    #[test]
    fn detune_interval_produces_beating() {
        let unison = beat_ratio(1.0);
        // 3% interval at 220 Hz → ~6.6 Hz beat, deep amplitude modulation
        let detuned = beat_ratio(1.03);
        assert!(unison < 1.2, "unison should not beat: ratio {unison}");
        assert!(detuned > 1.5, "detuned layers should beat: ratio {detuned}");
        assert!(
            detuned - unison > 0.4,
            "spectrum not widened by interval: unison {unison} detuned {detuned}"
        );
    }

    #[test]
    fn voice_pool_steals_without_artifacts() {
        let mut d = DuoInst::new();
        d.prepare(SR);
        for p in 40..60u8 {
            // 20 notes into a 6-voice pool
            d.note_on(p, 1.0);
            let buf = render(&mut d, 0.02);
            // 6 voices × 2 unity saw layers can peak ~12–14 (Tone 15 applies
            // no per-voice attenuation); bound guards blow-ups only
            assert!(buf.iter().all(|s| s.is_finite() && s.abs() < 16.0));
        }
        d.reset();
        assert!(rms(&render(&mut d, 0.1)) < 1e-6, "reset must silence the pool");
    }
}
