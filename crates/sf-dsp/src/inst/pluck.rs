//! `pluck` instrument — Karplus-Strong string (port of `makePluck()` in
//! src/audio/devices.ts, which wraps Tone.PluckSynth).
//!
//! Browser topology per voice: pink-noise burst of one period (attackNoise=1)
//! → one-pole lowpass at `dampen` → feedback comb tuned to the note
//! (y[n] = x[n] + res·y[n−D], output y[n−D]) → 0.9 output gain. The lowpass
//! sits *before* the comb (Tone's LowpassCombFilter), so `dampen` shapes the
//! excitation brightness while `res` sets the sustain. `noteOff` is a no-op in
//! the browser device (notes ring out by feedback decay alone) — kept here.
//! Pool of 4 round-robin voices, exactly like the browser (`pool[i++ % 4]`).

use crate::traits::InstrumentDsp;
use crate::util::{midi_hz, OnePole, OnePoleMode, Smoother};

/// Browser pool size (`Array.from({ length: 4 }, …)`).
const NUM_VOICES: usize = 4;
/// `new Tone.Gain(0.9)` output stage.
const OUT_GAIN: f32 = 0.9;
/// A finished voice whose block peak stays below this is retired.
const SILENCE_FLOOR: f32 = 1.0e-6;

/// Karplus-Strong voice: excitation burst → input lowpass → tuned comb.
struct Voice {
    buf: Vec<f32>,   // comb delay line (power-of-two length)
    mask: usize,     // buf.len() - 1
    write_pos: usize,
    delay: f32,      // loop length in samples = sr / f0
    burst_left: u32, // remaining pink-noise excitation samples
    quiet_run: u32,  // consecutive sub-floor output samples (retirement)
    vel: f32,
    active: bool,
    lp: OnePole,
    // pink noise state (Paul Kellet economy filter over xorshift32 white)
    rng: u32,
    b0: f32,
    b1: f32,
    b2: f32,
}

impl Voice {
    fn new(max_delay: usize, seed: u32) -> Self {
        let len = max_delay.max(8).next_power_of_two();
        Self {
            buf: vec![0.0; len],
            mask: len - 1,
            write_pos: 0,
            delay: 1.0,
            burst_left: 0,
            quiet_run: 0,
            vel: 0.0,
            active: false,
            lp: OnePole::new(OnePoleMode::Lowpass),
            rng: seed,
            b0: 0.0,
            b1: 0.0,
            b2: 0.0,
        }
    }

    fn prepare(&mut self, sample_rate: f32, max_delay: usize) {
        let len = max_delay.max(8).next_power_of_two();
        if len != self.buf.len() {
            self.buf = vec![0.0; len]; // prepare() may allocate; process() never does
            self.mask = len - 1;
        }
        self.lp.prepare(sample_rate);
        self.clear();
    }

    fn clear(&mut self) {
        self.buf.fill(0.0);
        self.write_pos = 0;
        self.burst_left = 0;
        self.quiet_run = 0;
        self.active = false;
        self.lp.reset();
        self.b0 = 0.0;
        self.b1 = 0.0;
        self.b2 = 0.0;
    }

    fn start(&mut self, delay_samples: f32, vel: f32) {
        // Steal in place: silence the old string so the new note starts clean.
        self.buf.fill(0.0);
        self.lp.reset();
        self.delay = delay_samples.clamp(1.0, (self.buf.len() - 1) as f32);
        // noise.start(t) … noise.stop(t + 1/f0 · attackNoise), attackNoise = 1
        self.burst_left = self.delay as u32 + 1;
        self.quiet_run = 0;
        self.vel = vel.clamp(0.0, 1.0);
        self.active = true;
    }

    #[inline]
    fn pink(&mut self) -> f32 {
        // xorshift32 white noise in [-1, 1)
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        let white = (x as f32) * (2.0 / u32::MAX as f32) - 1.0;
        // Kellet "economy" pink approximation, scaled to stay within ±1
        self.b0 = 0.997_65 * self.b0 + white * 0.099_046;
        self.b1 = 0.963 * self.b1 + white * 0.296_516_4;
        self.b2 = 0.57 * self.b2 + white * 1.052_691_3;
        (self.b0 + self.b1 + self.b2 + white * 0.1848) * 0.28
    }

    /// One Karplus-Strong step: returns the (pre-gain) voice output. Once the
    /// burst is spent and a full loop period of consecutive output samples sits
    /// below the silence floor, the whole string is quiet (reads walk every
    /// loop slot) and can only shrink further (|res| < 1) — retire the voice.
    #[inline]
    fn tick(&mut self, res: f32, dampen_hz: f32) -> f32 {
        let x = if self.burst_left > 0 {
            self.burst_left -= 1;
            self.pink() * self.vel
        } else {
            0.0
        };
        self.lp.set_cutoff(dampen_hz);
        let filtered = self.lp.process_sample(x);
        // fractional read `delay` samples behind the next write slot
        let d = self.delay;
        let di = d as usize;
        let frac = d - di as f32;
        let i0 = (self.write_pos + self.buf.len() - di) & self.mask;
        let i1 = (i0 + self.buf.len() - 1) & self.mask;
        let s0 = self.buf[i0];
        let s1 = self.buf[i1];
        let delayed = s0 + (s1 - s0) * frac;
        self.buf[self.write_pos] = filtered + delayed * res;
        self.write_pos = (self.write_pos + 1) & self.mask;
        if self.burst_left == 0 {
            if delayed.abs() < SILENCE_FLOOR {
                self.quiet_run += 1;
                if self.quiet_run as f32 > self.delay + 2.0 {
                    self.active = false;
                }
            } else {
                self.quiet_run = 0;
            }
        }
        delayed
    }
}

pub struct PluckInst {
    voices: Vec<Voice>, // fixed pool, allocated in new()/prepare()
    next: usize,        // round-robin cursor (browser: pool[i++ % 4])
    sample_rate: f32,
    dampen: Smoother, // 'dampen' — excitation lowpass cutoff (Hz)
    res: Smoother,    // 'res' — comb feedback (sustain)
}

impl PluckInst {
    pub fn new() -> Self {
        let sr = 44_100.0;
        let mut inst = Self {
            voices: (0..NUM_VOICES)
                .map(|i| Voice::new(Self::max_delay(sr), 0x9e37_79b9_u32.wrapping_mul(i as u32 + 1)))
                .collect(),
            next: 0,
            sample_rate: sr,
            dampen: Smoother::new(4_000.0), // schema defaults
            res: Smoother::new(0.93),
        };
        for v in &mut inst.voices {
            v.prepare(sr, Self::max_delay(sr));
        }
        inst
    }

    /// Longest loop we must hold: MIDI 0 = 8.176 Hz → sr / 8.176 < sr / 8.
    fn max_delay(sample_rate: f32) -> usize {
        (sample_rate * 0.125) as usize + 4
    }

    fn start_note(&mut self, pitch: u8, vel: f32) {
        let freq = midi_hz(pitch as f32);
        let delay = self.sample_rate / freq;
        let idx = self.next % NUM_VOICES;
        self.next = (self.next + 1) % NUM_VOICES;
        self.voices[idx].start(delay, vel);
    }
}

impl Default for PluckInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for PluckInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        let max_delay = Self::max_delay(self.sample_rate);
        for v in &mut self.voices {
            v.prepare(self.sample_rate, max_delay);
        }
        self.dampen.prepare(self.sample_rate);
        self.res.prepare(self.sample_rate);
        self.dampen.snap(self.dampen.target());
        self.res.snap(self.res.target());
        self.next = 0;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            // schema: 500–12000 Hz; keep below Nyquist for odd rates
            "dampen" => self
                .dampen
                .set_target(value.clamp(10.0, self.sample_rate * 0.45)),
            // schema: 0.3–0.98; worklet caps feedback at 0.9999
            "res" => self.res.set_target(value.clamp(0.0, 0.9999)),
            _ => {}
        }
    }

    fn note_on(&mut self, pitch: u8, vel: f32) {
        self.start_note(pitch, vel);
    }

    // Browser device: `noteOff: () => {}` — plucks ring out on their own.
    fn note_off(&mut self, _pitch: u8) {}

    // Browser device ignores duration (and velocity) on the scheduler path;
    // velocity is honored like note_on for consistency.
    fn trigger(&mut self, pitch: u8, _dur_sec: f32, vel: f32) {
        self.start_note(pitch, vel);
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        if !self.voices.iter().any(|v| v.active) {
            // nothing sounding: keep param ramps moving, skip the voice loop
            for _ in 0..n {
                self.dampen.next();
                self.res.next();
            }
            return;
        }
        for i in 0..n {
            let dampen = self.dampen.next();
            let res = self.res.next();
            let mut sum = 0.0f32;
            for v in &mut self.voices {
                if v.active {
                    sum += v.tick(res, dampen);
                }
            }
            let out = sum * OUT_GAIN;
            l[i] += out;
            r[i] += out; // mono voice up-mixed equally, as in the browser graph
        }
    }

    fn reset(&mut self) {
        for v in &mut self.voices {
            v.clear();
        }
        self.next = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn render(inst: &mut PluckInst, seconds: f32) -> Vec<f32> {
        let n = (SR * seconds) as usize;
        let mut out = Vec::with_capacity(n);
        let mut l = [0.0f32; 128];
        let mut r = [0.0f32; 128];
        let mut done = 0;
        while done < n {
            let len = 128.min(n - done);
            l[..len].fill(0.0);
            r[..len].fill(0.0);
            inst.process(&mut l[..len], &mut r[..len]);
            out.extend_from_slice(&l[..len]);
            done += len;
        }
        out
    }

    fn rms(x: &[f32]) -> f32 {
        if x.is_empty() {
            return 0.0;
        }
        libm::sqrtf(x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32)
    }

    #[test]
    fn no_nan_inf_over_two_seconds() {
        let mut d = PluckInst::new();
        d.prepare(SR);
        d.note_on(48, 1.0);
        d.note_on(60, 0.8);
        d.note_on(67, 0.5);
        d.trigger(72, 0.25, 1.0);
        d.note_on(55, 1.0); // 5th note steals a voice (pool of 4)
        let out = render(&mut d, 2.0);
        assert!(out.iter().all(|s| s.is_finite()), "NaN/inf in output");
    }

    #[test]
    fn additive_into_existing_content() {
        let mut d = PluckInst::new();
        d.prepare(SR);
        let mut l = [0.5f32; 128];
        let mut r = [0.5f32; 128];
        d.process(&mut l, &mut r); // silent: must leave content untouched
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.5));
        d.note_on(60, 1.0);
        // comb output is the delayed tap: sound appears after one loop period
        let mut mixed = false;
        for _ in 0..8 {
            l.fill(0.5);
            r.fill(0.5);
            d.process(&mut l, &mut r);
            mixed |= l.iter().any(|s| *s != 0.5);
        }
        assert!(mixed, "note did not mix in");
    }

    #[test]
    fn param_sweep_stays_bounded() {
        let mut d = PluckInst::new();
        d.prepare(SR);
        let steps = 40;
        for i in 0..steps {
            let t = i as f32 / (steps - 1) as f32;
            d.set_param("dampen", 500.0 + t * (12_000.0 - 500.0));
            d.set_param("res", 0.3 + t * (0.98 - 0.3));
            d.note_on(36 + (i * 2) as u8, 1.0);
            let out = render(&mut d, 0.05);
            assert!(
                out.iter().all(|s| s.is_finite() && s.abs() < 10.0),
                "blew up at sweep step {i}"
            );
        }
    }

    #[test]
    fn note_sounds_then_decays_to_silence() {
        let mut d = PluckInst::new();
        d.prepare(SR);
        d.set_param("res", 0.93); // default sustain
        d.note_on(69, 1.0);
        d.note_off(69); // no-op in the browser device; string rings out
        let out = render(&mut d, 3.0);
        let early = rms(&out[..(SR * 0.1) as usize]);
        let late = rms(&out[(SR * 2.5) as usize..]);
        assert!(early > 1.0e-3, "note produced no sound: rms {early}");
        assert!(late < 1.0e-4, "did not decay to silence: rms {late}");
        assert!(late < early * 0.01, "no decay: {early} -> {late}");
    }

    #[test]
    fn output_decays_and_fundamental_matches_pitch() {
        let mut d = PluckInst::new();
        d.prepare(SR);
        d.set_param("res", 0.98); // long sustain for a clean tone estimate
        d.note_on(69, 1.0); // A4 = 440 Hz
        let out = render(&mut d, 1.0);

        // decays: energy in a late window well below an early window
        let a = rms(&out[(SR * 0.05) as usize..(SR * 0.15) as usize]);
        let b = rms(&out[(SR * 0.85) as usize..(SR * 0.95) as usize]);
        assert!(a > 0.0 && b < a, "no decay: {a} -> {b}");

        // fundamental via autocorrelation over the sustained part
        let seg = &out[(SR * 0.2) as usize..(SR * 0.8) as usize];
        let min_lag = (SR / 1_000.0) as usize;
        let max_lag = (SR / 60.0) as usize;
        let mut best_lag = min_lag;
        let mut best = f32::MIN;
        let n = seg.len() - max_lag;
        for lag in min_lag..=max_lag {
            let mut acc = 0.0f32;
            for i in 0..n {
                acc += seg[i] * seg[i + lag];
            }
            if acc > best {
                best = acc;
                best_lag = lag;
            }
        }
        let expected = SR / 440.0; // ~109.09 samples
        let err = (best_lag as f32 - expected).abs() / expected;
        assert!(
            err < 0.02,
            "fundamental off: lag {best_lag} vs expected {expected:.1}"
        );
    }

    #[test]
    fn dampen_darkens_the_attack() {
        // higher `dampen` (Tone knob) lets more excitation energy through
        let energy = |cutoff: f32| {
            let mut d = PluckInst::new();
            d.prepare(SR);
            d.set_param("dampen", cutoff);
            // let the smoother settle before plucking
            render(&mut d, 0.05);
            d.note_on(57, 1.0);
            rms(&render(&mut d, 0.25))
        };
        let dark = energy(500.0);
        let bright = energy(12_000.0);
        assert!(
            bright > dark * 1.2,
            "dampen had no effect: dark {dark}, bright {bright}"
        );
    }

    #[test]
    fn reset_silences_everything() {
        let mut d = PluckInst::new();
        d.prepare(SR);
        d.note_on(60, 1.0);
        render(&mut d, 0.1);
        d.reset();
        let out = render(&mut d, 0.1);
        assert!(out.iter().all(|s| *s == 0.0), "reset left sound behind");
    }
}
