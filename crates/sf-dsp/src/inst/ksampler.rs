//! `ksampler` instrument — port of `makeKSampler()` in src/audio/devices.ts:
//! ONE buffer (slot 0) played chromatically across the keyboard. The browser
//! keys the Tone.Sampler at C3 (MIDI 48) and feeds it `note − root`, so the
//! playback rate for pitch `p` is 2^((p − root + tune − 48)/12) — playing the
//! chosen root pitch class in octave 3 sounds the sample unpitched. `root`
//! (0–11 = C…B) and `tune` fold into the pitch math live; new notes pick them
//! up (Tone computes hz at trigger time). One-shot playback with Tone.Sampler's
//! attack/release fade; fixed 12-voice pool with oldest stealing. Output gain
//! 0.9 mirrors `new Tone.Gain(0.9)`. No sample → silence, never panic.

use std::sync::Arc;

use crate::sample::SampleBuf;
use crate::traits::InstrumentDsp;
use crate::util::Adsr;

const MAX_VOICES: usize = 12;
const OUT_GAIN: f32 = 0.9;
/// Tone.Sampler zone key: the sample sounds unpitched at C3 = MIDI 48.
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

pub struct KsamplerInst {
    sample_rate: f32,
    buf: Option<Arc<SampleBuf>>,
    root: f32,
    tune: f32,
    attack: f32,
    release: f32,
    voices: Vec<Voice>,
    age: u64,
}

impl KsamplerInst {
    pub fn new() -> Self {
        Self {
            sample_rate: 44_100.0,
            buf: None,
            root: 0.0,
            tune: 0.0,
            attack: 0.004,
            release: 0.4,
            voices: (0..MAX_VOICES).map(|_| Voice::new()).collect(),
            age: 0,
        }
    }

    /// Playback step in source frames per engine sample for `pitch`.
    fn step_for(&self, pitch: u8) -> f64 {
        let semis = pitch as f32 - self.root + self.tune - ROOT_MIDI;
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

impl Default for KsamplerInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for KsamplerInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for v in &mut self.voices {
            v.active = false;
            v.env.prepare(self.sample_rate);
        }
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "root" => self.root = (value as i32) as f32, // stepped: 0–11 = C…B
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

    fn render(d: &mut KsamplerInst, samples: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; samples];
        let mut r = vec![0.0f32; samples];
        for (cl, cr) in out.chunks_mut(128).zip(r.chunks_mut(128)) {
            d.process(cl, cr);
        }
        out
    }

    fn zero_crossings(buf: &[f32]) -> usize {
        buf.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count()
    }

    #[test]
    fn silent_without_sample_and_additive() {
        let mut d = KsamplerInst::new();
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
    fn octave_up_doubles_zero_crossing_rate() {
        let sr = 44_100.0;
        let mut d = KsamplerInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(sr, 110.0, 3.0)));
        d.set_param("attack", 0.001);
        d.trigger(48, 0.5, 1.0); // C3 = natural pitch
        let a = render(&mut d, 22_050);
        d.reset();
        d.trigger(60, 0.5, 1.0); // +12 semitones
        let b = render(&mut d, 22_050);
        let (za, zb) = (zero_crossings(&a) as f32, zero_crossings(&b) as f32);
        assert!(a.iter().chain(b.iter()).all(|s| s.is_finite()));
        assert!(za > 20.0, "za {za}");
        assert!((zb / za - 2.0).abs() < 0.1, "za {za} zb {zb}");
    }

    #[test]
    fn root_shifts_the_mapping() {
        let sr = 44_100.0;
        let mut d = KsamplerInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(sr, 110.0, 3.0)));
        d.set_param("root", 7.0); // G: playing G3 (55) sounds unpitched
        d.trigger(55, 0.5, 1.0);
        let a = render(&mut d, 22_050);
        let za = zero_crossings(&a) as f32;
        // natural pitch: ~110 Hz over 0.5 s ⇒ ~55 rising crossings
        assert!((za - 55.0).abs() <= 3.0, "za {za}");
    }

    #[test]
    fn note_off_releases_and_param_sweep_is_finite() {
        let sr = 44_100.0;
        let mut d = KsamplerInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(sr, 220.0, 3.0)));
        d.set_param("release", 0.05);
        d.note_on(48, 0.9);
        let _ = render(&mut d, 4_410);
        d.note_off(48);
        let tail = render(&mut d, 22_050);
        assert!(tail.iter().all(|s| s.is_finite()));
        // released: the last quarter second must be silent
        let late = &tail[11_025..];
        assert!(late.iter().all(|s| s.abs() < 1e-3), "release did not end the voice");
        for v in [-24.0, 0.0, 24.0] {
            d.set_param("tune", v);
            d.set_param("root", 11.0);
            d.trigger(60, 0.05, 1.0);
        }
        let sweep = render(&mut d, 8_192);
        assert!(sweep.iter().all(|s| s.is_finite()));
    }
}
