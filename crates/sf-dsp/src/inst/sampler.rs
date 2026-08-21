//! `sampler` instrument — port of `makeSampler()` in src/audio/devices.ts:
//! ONE buffer (slot 0) keyed at C3 (MIDI 48) played back pitch-relative, so
//! incoming pitch `p` plays the sample at rate 2^((p + tune − 48)/12).
//! One-shot per note: the sample plays from its head until it runs out or the
//! release finishes. Envelope mirrors Tone.Sampler's attack/release fade
//! (attack ramp → full level → release on note_off / after `dur`), driven by
//! the frozen schema params `tune`/`attack`/`release`. Output gain 0.9 mirrors
//! the device's `new Tone.Gain(0.9)`. No sample loaded → silence, never panic.

use std::sync::Arc;

use crate::sample::SampleBuf;
use crate::traits::InstrumentDsp;
use crate::util::Adsr;

const MAX_VOICES: usize = 12;
const OUT_GAIN: f32 = 0.9;
/// The Tone.Sampler zone key: the sample sounds unpitched at C3 = MIDI 48.
const ROOT_MIDI: f32 = 48.0;

struct Voice {
    active: bool,
    pitch: u8,
    held: bool,
    pos: f64,
    step: f64,
    env: Adsr,
    vel: f32,
    /// Samples until the scheduler-path auto note_off (< 0 ⇒ none pending).
    remaining: f64,
    age: u64,
}

impl Voice {
    fn new() -> Self {
        Self {
            active: false,
            pitch: 0,
            held: false,
            pos: 0.0,
            step: 0.0,
            env: Adsr::new(),
            vel: 0.0,
            remaining: -1.0,
            age: 0,
        }
    }
}

pub struct SamplerInst {
    sample_rate: f32,
    buf: Option<Arc<SampleBuf>>,
    tune: f32,
    attack: f32,
    release: f32,
    voices: Vec<Voice>,
    age: u64,
}

impl SamplerInst {
    pub fn new() -> Self {
        Self {
            sample_rate: 44_100.0,
            buf: None,
            tune: 0.0,
            attack: 0.005,
            release: 0.4,
            voices: (0..MAX_VOICES).map(|_| Voice::new()).collect(),
            age: 0,
        }
    }

    /// Playback step in source frames per engine sample for `pitch`.
    fn step_for(&self, pitch: u8) -> f64 {
        let semis = pitch as f32 + self.tune - ROOT_MIDI;
        let rate = libm::powf(2.0, semis / 12.0) as f64;
        let src_sr = self.buf.as_ref().map(|b| b.sample_rate).unwrap_or(self.sample_rate);
        rate * src_sr as f64 / self.sample_rate.max(1.0) as f64
    }

    fn start_voice(&mut self, pitch: u8, vel: f32, dur_samples: f64) {
        if self.buf.is_none() {
            return;
        }
        let step = self.step_for(pitch);
        self.age += 1;
        let age = self.age;
        // free voice, else steal the oldest
        let vi = self
            .voices
            .iter()
            .position(|v| !v.active)
            .unwrap_or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, v)| v.age)
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            });
        let (attack, release) = (self.attack, self.release);
        let sr = self.sample_rate;
        let v = &mut self.voices[vi];
        v.active = true;
        v.pitch = pitch;
        v.held = dur_samples < 0.0;
        v.pos = 0.0;
        v.step = step;
        v.vel = vel.clamp(0.0, 1.0);
        v.remaining = dur_samples;
        v.age = age;
        v.env.prepare(sr);
        // Tone.Sampler fade: attack up, hold at 1, release down (no decay).
        v.env.set_adsr(attack, 0.0, 1.0, release);
        v.env.reset();
        v.env.note_on();
    }
}

impl Default for SamplerInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for SamplerInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for v in &mut self.voices {
            v.active = false;
            v.env.prepare(self.sample_rate);
        }
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "tune" => self.tune = value,
            "attack" => self.attack = value.max(0.0),
            "release" => self.release = value.max(0.0),
            _ => {}
        }
    }

    fn note_on(&mut self, pitch: u8, vel: f32) {
        self.start_voice(pitch, vel, -1.0);
    }

    fn note_off(&mut self, pitch: u8) {
        for v in &mut self.voices {
            if v.active && v.held && v.pitch == pitch {
                v.held = false;
                v.env.note_off();
            }
        }
    }

    fn trigger(&mut self, pitch: u8, dur_sec: f32, vel: f32) {
        self.start_voice(pitch, vel, (dur_sec.max(0.0) * self.sample_rate) as f64);
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let Some(buf) = self.buf.as_ref() else { return };
        let frames = buf.frames() as f64;
        if frames <= 0.0 {
            return;
        }
        let n = l.len().min(r.len());
        for v in &mut self.voices {
            if !v.active {
                continue;
            }
            for k in 0..n {
                if v.remaining >= 0.0 {
                    v.remaining -= 1.0;
                    if v.remaining < 0.0 {
                        v.env.note_off();
                    }
                }
                let e = v.env.next();
                if !v.env.is_active() || v.pos >= frames {
                    v.active = false;
                    break;
                }
                let (sl, sr) = buf.read_lin(v.pos);
                let g = e * v.vel * OUT_GAIN;
                l[k] += sl * g;
                r[k] += sr * g;
                v.pos += v.step;
            }
        }
    }

    fn set_sample(&mut self, slot: u32, buf: Option<Arc<SampleBuf>>) {
        if slot != 0 {
            return;
        }
        // control-path call: silence voices so no one reads a swapped buffer
        for v in &mut self.voices {
            v.active = false;
        }
        self.buf = buf;
    }

    fn reset(&mut self) {
        for v in &mut self.voices {
            v.active = false;
            v.env.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_buf(sr: f32, freq: f32, secs: f32) -> Arc<SampleBuf> {
        let n = (sr * secs) as usize;
        let l: Vec<f32> = (0..n)
            .map(|i| libm::sinf(core::f32::consts::TAU * freq * i as f32 / sr))
            .collect();
        Arc::new(SampleBuf { sample_rate: sr, l, r: vec![] })
    }

    fn render(d: &mut SamplerInst, samples: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; samples];
        let mut r = vec![0.0f32; samples];
        for (cl, cr) in out.chunks_mut(128).zip(r.chunks_mut(128)) {
            d.process(cl, cr);
        }
        out
    }

    fn rms(b: &[f32]) -> f32 {
        (b.iter().map(|s| s * s).sum::<f32>() / b.len().max(1) as f32).sqrt()
    }

    #[test]
    fn silent_without_sample_and_additive() {
        let mut d = SamplerInst::new();
        d.prepare(48_000.0);
        d.note_on(60, 1.0);
        d.trigger(64, 0.25, 0.8);
        let mut l = [0.5f32; 8];
        let mut r = [0.5f32; 8];
        d.process(&mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.5));
        d.note_off(60);
        d.reset();
    }

    #[test]
    fn one_shot_plays_and_ends() {
        let sr = 44_100.0;
        let mut d = SamplerInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(sr, 220.0, 0.2))); // 0.2 s sample
        d.trigger(48, 10.0, 1.0); // root pitch: natural rate, dur longer than sample
        let out = render(&mut d, sr as usize); // 1 s
        assert!(out.iter().all(|s| s.is_finite()));
        assert!(rms(&out[..4_000]) > 0.05, "one-shot silent at the head");
        // the 0.2 s sample must have ended well before 0.5 s
        assert!(rms(&out[(sr * 0.5) as usize..]) < 1e-5, "one-shot did not end");
    }

    #[test]
    fn pitch_selects_playback_rate() {
        let sr = 44_100.0;
        let mut d = SamplerInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(sr, 100.0, 2.0)));
        let zc = |buf: &[f32]| {
            buf.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count()
        };
        d.set_param("attack", 0.001);
        d.trigger(48, 0.5, 1.0); // natural pitch
        let a = render(&mut d, 22_050);
        d.reset();
        d.trigger(60, 0.5, 1.0); // +12 semitones = double rate
        let b = render(&mut d, 22_050);
        let (za, zb) = (zc(&a) as f32, zc(&b) as f32);
        assert!(za > 10.0);
        assert!((zb / za - 2.0).abs() < 0.1, "za {za} zb {zb}");
    }

    #[test]
    fn sample_rate_mismatch_is_resampled() {
        // a 22.05 kHz source in a 44.1 kHz engine must play at half step,
        // preserving the source's real-time frequency
        let sr = 44_100.0;
        let mut d = SamplerInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(22_050.0, 100.0, 2.0)));
        d.trigger(48, 0.4, 1.0);
        let out = render(&mut d, 17_640); // 0.4 s
        let zc = out.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count() as f32;
        assert!((zc - 40.0).abs() <= 3.0, "zero crossings {zc}"); // 100 Hz × 0.4 s
    }
}
