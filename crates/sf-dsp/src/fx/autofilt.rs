//! `autofilt` effect — port of the Tone.AutoFilter composite built by
//! `makeEffect("autofilt")` in src/audio/devices.ts.
//!
//! Behavior spec (Tone/effect/AutoFilter.ts + LFOEffect.ts + LFO.ts):
//! a sine LFO drives the cutoff of a lowpass filter (Tone.Filter defaults:
//! −12 dB/oct, Q = 1). The LFO chain is oscillator → amplitude(depth) →
//! AudioToGain ((x+1)/2) → linear Scale(min, max) with min = baseFrequency and
//! max = baseFrequency · 2^octaves, so
//!   cutoff = base + (base·2^oct − base) · (1 + depth·sin(2πft)) / 2.
//! makeEffect passes `octaves: 3.5` (fixed; not a schema param). Wet/dry is
//! Tone's CrossFade (equal-power cos/sin law). devices.ts gates the LFO off
//! when depth or mix ≈ 0 — a stopped Tone.LFO rests at its mid value, which is
//! exactly the depth = 0 output, so the gate is a CPU optimization with no
//! audible behavior to replicate.
//! The rate knob is switchable (makeRate/rateHzOf in devices.ts/schema.ts):
//! rateMode 0 = Sync (tempo division index in rateSync, retuned from the
//! transport bpm via `tick()`), 1 = Low (`rate` Hz), 2 = High (`rateHi` Hz).

use crate::traits::EffectDsp;
use crate::util::{Smoother, Svf, SvfMode};

/// makeEffect("autofilt"): `octaves: 3.5` → max cutoff = base · 2^3.5.
const OCTAVES_POW: f32 = 11.313_708; // 2^3.5

/// schema.ts LFO_DIV_TICKS — cycle length in transport ticks per rateSync
/// index (PPQ = 96 ⇒ quarter = 96, bar = 384).
const DIV_TICKS: [f64; 9] = [3072.0, 1536.0, 768.0, 384.0, 192.0, 96.0, 48.0, 32.0, 24.0];

pub struct AutofiltFx {
    sample_rate: f32,
    bpm: f64,
    // Switchable rate state (makeRate defaults: mode 1/Low, rateHi 440, sync 5).
    rate_mode: i32,
    rate_lo: f32,
    rate_hi: f32,
    rate_sync: usize,
    /// LFO phase in cycles [0, 1); step = hz / sample_rate.
    phase: f64,
    step: f64,
    /// LFO amplitude (Tone `depth`), ~15 ms smoothed.
    depth: Smoother,
    /// Filter base (minimum) cutoff in Hz (Tone `baseFrequency`), smoothed.
    base: Smoother,
    /// Wet/dry mix, 0..1 (Tone `wet`).
    mix: Smoother,
    filt_l: Svf,
    filt_r: Svf,
}

impl AutofiltFx {
    pub fn new() -> Self {
        // Schema defaults: rate=1 Hz, depth=0.7, base=350 Hz, mix=1.
        let mut fx = Self {
            sample_rate: 44_100.0,
            bpm: 120.0,
            rate_mode: 1,
            rate_lo: 1.0,
            rate_hi: 440.0,
            rate_sync: 5,
            phase: 0.0,
            step: 0.0,
            depth: Smoother::new(0.7),
            base: Smoother::new(350.0),
            mix: Smoother::new(1.0),
            filt_l: Svf::new(SvfMode::Lowpass),
            filt_r: Svf::new(SvfMode::Lowpass),
        };
        fx.update_step();
        fx
    }

    /// schema.ts `rateHzOf()`: effective LFO frequency for the stored quad.
    fn rate_hz(&self) -> f32 {
        match self.rate_mode {
            2 => self.rate_hi.clamp(301.0, 1000.0),
            0 => {
                let ticks = DIV_TICKS[self.rate_sync.min(DIV_TICKS.len() - 1)];
                ((self.bpm.max(1.0) * 96.0) / (60.0 * ticks)) as f32
            }
            _ => self.rate_lo.clamp(0.01, 300.0),
        }
    }

    /// Frequency changes are phase-continuous (no discontinuity in the LFO
    /// output), so the step needs no smoother.
    fn update_step(&mut self) {
        self.step = self.rate_hz() as f64 / self.sample_rate.max(1.0) as f64;
    }
}

impl Default for AutofiltFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for AutofiltFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.depth.prepare(self.sample_rate);
        self.depth.snap(self.depth.target());
        self.base.prepare(self.sample_rate);
        self.base.snap(self.base.target());
        self.mix.prepare(self.sample_rate);
        self.mix.snap(self.mix.target());
        self.filt_l.prepare(self.sample_rate);
        self.filt_r.prepare(self.sample_rate);
        self.update_step();
        self.phase = 0.0;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "rate" => {
                self.rate_lo = value;
                self.update_step();
            }
            "rateMode" => {
                self.rate_mode = (value as i32).clamp(0, 2);
                self.update_step();
            }
            "rateHi" => {
                self.rate_hi = value;
                self.update_step();
            }
            "rateSync" => {
                self.rate_sync = (value as i32).clamp(0, DIV_TICKS.len() as i32 - 1) as usize;
                self.update_step();
            }
            "depth" => self.depth.set_target(value.clamp(0.0, 1.0)),
            "base" => self.base.set_target(value.max(1.0)),
            "mix" => self.mix.set_target(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn tick(&mut self, _pos_ticks: f64, _playing: bool, bpm: f64) {
        if bpm > 0.0 && bpm != self.bpm {
            self.bpm = bpm;
            if self.rate_mode == 0 {
                self.update_step();
            }
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let depth = self.depth.next();
            let base = self.base.next();
            let mix = self.mix.next();
            // Tone's wet control is a CrossFade: equal-power cos/sin.
            let dry_g = libm::cosf(mix * core::f32::consts::FRAC_PI_2);
            let wet_g = libm::sinf(mix * core::f32::consts::FRAC_PI_2);
            let s = libm::sinf(core::f32::consts::TAU * self.phase as f32);
            self.phase += self.step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            // LFO → cutoff: linear Scale between base and base·2^3.5.
            let span = base * (OCTAVES_POW - 1.0);
            let cutoff = base + span * 0.5 * (1.0 + depth * s);
            // Tone.Filter defaults: lowpass, −12 dB/oct, Q = 1 (Svf clamps
            // the prewarp below Nyquist internally).
            self.filt_l.set_cutoff_q(cutoff, 1.0);
            self.filt_r.set_cutoff_q(cutoff, 1.0);
            let wl = self.filt_l.process_sample(*ls);
            let wr = self.filt_r.process_sample(*rs);
            *ls = *ls * dry_g + wl * wet_g;
            *rs = *rs * dry_g + wr * wet_g;
        }
    }

    fn reset(&mut self) {
        self.filt_l.reset();
        self.filt_r.reset();
        self.depth.snap(self.depth.target());
        self.base.snap(self.base.target());
        self.mix.snap(self.mix.target());
        self.phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn run_blocks(fx: &mut AutofiltFx, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        let mut i = 0;
        while i < n {
            let end = (i + BLOCK).min(n);
            fx.process(&mut l[i..end], &mut r[i..end]);
            i = end;
        }
    }

    /// Deterministic pseudo-noise in [-0.5, 0.5].
    fn noise(n: usize) -> Vec<f32> {
        let mut s = 0x12345678u32;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1 << 24) as f32 - 0.5
            })
            .collect()
    }

    fn sine(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * libm::sinf(core::f32::consts::TAU * freq * i as f32 / SR))
            .collect()
    }

    /// Goertzel magnitude at `freq` given the sequence's own sample rate.
    fn goertzel(x: &[f32], freq: f32, fs: f32) -> f32 {
        let w = core::f64::consts::TAU * freq as f64 / fs as f64;
        let c = 2.0 * libm::cos(w);
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v as f64 + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let p = s1 * s1 + s2 * s2 - c * s1 * s2;
        (2.0 * libm::sqrt(p.max(0.0)) / x.len() as f64) as f32
    }

    fn rms(x: &[f32]) -> f32 {
        libm::sqrtf(x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32)
    }

    #[test]
    fn two_seconds_no_nan_inf_and_wet_non_silent() {
        let mut fx = AutofiltFx::new(); // schema defaults: rate 1, depth 0.7, base 350, mix 1
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

        let n = (2.0 * SR) as usize;
        let src = noise(n);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        assert!(rms(&l) > 1e-3, "wet output is silent");
        // Fully wet lowpass at defaults must actually differ from the dry input.
        let diff: f32 = l.iter().zip(&src).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 1.0, "output identical to dry input (diff {diff})");
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = AutofiltFx::new();
        fx.prepare(SR);
        let src = noise(BLOCK);
        let mut bpm = 120.0;
        for mode in 0..=2 {
            for step in 0..=4 {
                let t = step as f32 / 4.0;
                fx.set_param("rateMode", mode as f32);
                fx.set_param("rate", 0.01 + t * (300.0 - 0.01));
                fx.set_param("rateHi", 301.0 + t * (1000.0 - 301.0));
                fx.set_param("rateSync", (step * 2) as f32);
                fx.set_param("depth", t);
                fx.set_param("base", 80.0 + t * (4000.0 - 80.0));
                fx.set_param("mix", 1.0 - t);
                bpm = if bpm > 150.0 { 60.0 } else { bpm + 37.0 };
                fx.tick(0.0, true, bpm);
                for _ in 0..40 {
                    let mut l = src.clone();
                    let mut r = src.clone();
                    fx.process(&mut l, &mut r);
                    assert!(
                        l.iter().chain(r.iter()).all(|v| v.is_finite() && v.abs() < 100.0),
                        "blew up at mode={mode} t={t} bpm={bpm}"
                    );
                }
            }
        }
    }

    /// depth = 0 parks the cutoff at the sweep midpoint (stopped Tone.LFO):
    /// base 80 ⇒ cutoff ≈ 493 Hz. A 100 Hz sine must pass and an 8 kHz sine
    /// must be strongly attenuated — the filter is a real lowpass.
    #[test]
    fn static_filter_attenuates_highs_passes_lows() {
        let n = SR as usize;
        let atten = |freq: f32| -> f32 {
            let mut fx = AutofiltFx::new();
            fx.set_param("depth", 0.0);
            fx.set_param("base", 80.0);
            fx.set_param("mix", 1.0);
            fx.prepare(SR);
            let mut l = sine(n, freq, 0.5);
            let mut r = l.clone();
            run_blocks(&mut fx, &mut l, &mut r);
            // skip the first half: filter/smoother settling
            20.0 * libm::log10f((rms(&l[n / 2..]) / rms(&sine(n, freq, 0.5)[n / 2..])).max(1e-9))
        };
        assert!(atten(100.0).abs() < 2.0, "passband off: {} dB", atten(100.0));
        assert!(atten(8_000.0) < -20.0, "stopband weak: {} dB", atten(8_000.0));
    }

    /// Analytic check: with noise input, the output's HF energy (first
    /// difference, a crude highpass) must oscillate at the LFO rate — the
    /// windowed HF envelope has a dominant spectral line at the set rate.
    #[test]
    fn hf_energy_oscillates_at_lfo_rate() {
        const LFO_HZ: f32 = 4.0;
        let mut fx = AutofiltFx::new();
        fx.set_param("rateMode", 1.0);
        fx.set_param("rate", LFO_HZ);
        fx.set_param("depth", 1.0);
        fx.set_param("base", 200.0);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

        let n = (2.0 * SR) as usize;
        let mut l = noise(n);
        let mut r = l.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        // Per-window RMS of the differenced output = HF energy envelope,
        // sampled at SR / WIN.
        const WIN: usize = 128;
        let mut env: Vec<f32> = l
            .as_chunks::<WIN>()
            .0
            .iter()
            .map(|w| {
                let e: f32 = w.windows(2).map(|p| (p[1] - p[0]) * (p[1] - p[0])).sum();
                libm::sqrtf(e / (WIN - 1) as f32)
            })
            .collect();
        let mean = env.iter().sum::<f32>() / env.len() as f32;
        for v in &mut env {
            *v -= mean;
        }
        let env_fs = SR / WIN as f32;
        let at_rate = goertzel(&env, LFO_HZ, env_fs);
        let control = goertzel(&env, 11.0, env_fs); // non-harmonic of 4 Hz
        assert!(
            at_rate > 2.0 * control,
            "HF envelope not dominated by LFO rate: {at_rate} vs control {control}"
        );
        // The sweep must actually open and close the filter.
        let max = env.iter().fold(f32::MIN, |a, &b| a.max(b));
        let min = env.iter().fold(f32::MAX, |a, &b| a.min(b));
        assert!(max - min > mean * 0.5, "HF envelope barely moves");
    }

    /// Sync mode: rate follows the transport bpm (rateSync 5 = 1/4 note ⇒
    /// LFO period = one beat). Doubling the bpm doubles the modulation rate.
    #[test]
    fn sync_mode_tracks_bpm() {
        let env_peak_rate = |bpm: f64| -> f32 {
            let mut fx = AutofiltFx::new();
            fx.set_param("rateMode", 0.0);
            fx.set_param("rateSync", 5.0); // 1/4 = 96 ticks ⇒ hz = bpm/60
            fx.set_param("depth", 1.0);
            fx.set_param("base", 200.0);
            fx.set_param("mix", 1.0);
            fx.prepare(SR);
            fx.tick(0.0, true, bpm);
            let n = (2.0 * SR) as usize;
            let mut l = noise(n);
            let mut r = l.clone();
            run_blocks(&mut fx, &mut l, &mut r);
            const WIN: usize = 128;
            let mut env: Vec<f32> = l
                .as_chunks::<WIN>()
                .0
                .iter()
                .map(|w| {
                    let e: f32 = w.windows(2).map(|p| (p[1] - p[0]) * (p[1] - p[0])).sum();
                    libm::sqrtf(e / (WIN - 1) as f32)
                })
                .collect();
            let mean = env.iter().sum::<f32>() / env.len() as f32;
            for v in &mut env {
                *v -= mean;
            }
            let env_fs = SR / WIN as f32;
            // pick the stronger of the two candidate rates
            let a = goertzel(&env, 2.0, env_fs); // 120 bpm ⇒ 2 Hz
            let b = goertzel(&env, 4.0, env_fs); // 240 bpm ⇒ 4 Hz
            if a > b {
                2.0
            } else {
                4.0
            }
        };
        assert_eq!(env_peak_rate(120.0), 2.0);
        assert_eq!(env_peak_rate(240.0), 4.0);
    }
}
