//! `dist` effect — port of the browser Tone.Distortion device (devices.ts
//! `makeEffect("dist")`): a 4×-oversampled waveshaper plus wet/dry mix.
//!
//! Tone's shaping curve, with `k = amt * 100` and `deg = π/180`:
//!   f(x) = ((3 + k) · x · 20 · deg) / (π + k · |x|)
//! WaveShaperNode clamps its input to the curve domain [-1, 1], so inputs
//! beyond ±1 saturate at f(±1). The curve is odd-symmetric (odd harmonics
//! only) and bounded (|f| ≤ (3 + k)·(π/9)/(π + k) < 0.35 for k ≥ 0).
//! Tone's `wet` is a Tone.CrossFade — an equal-power sin/cos blend — so the
//! mix param is honored the same way here. Wet and dry are mixed *inside*
//! the oversampled block so both paths share the resampler latency and
//! partial-mix settings don't comb-filter.

use crate::traits::EffectDsp;
use crate::util::{Oversampler4, Smoother, MAX_BLOCK};

const OS_FACTOR: f32 = 4.0;
const FRAC_PI_2: f32 = core::f32::consts::FRAC_PI_2;

/// Tone.Distortion transfer curve (see module docs). `k = amt * 100`.
#[inline]
fn shape(x: f32, k: f32) -> f32 {
    // 20 · π/180 = π/9
    const GAIN: f32 = core::f32::consts::PI / 9.0;
    let xc = x.clamp(-1.0, 1.0);
    ((3.0 + k) * xc * GAIN) / (core::f32::consts::PI + k * xc.abs())
}

pub struct DistFx {
    os: Oversampler4,
    // Both smoothers are stepped in the 4× domain (prepared at 4·sr) so the
    // ~15 ms time constant holds at the rate they are consumed.
    drive_sm: Smoother, // schema `amt` 0..1, def 0.4
    mix_sm: Smoother,   // schema `mix` 0..1, def 1
}

impl DistFx {
    pub fn new() -> Self {
        Self {
            os: Oversampler4::new(),
            drive_sm: Smoother::new(0.4),
            mix_sm: Smoother::new(1.0),
        }
    }
}

impl Default for DistFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for DistFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.drive_sm.prepare(sample_rate * OS_FACTOR);
        self.mix_sm.prepare(sample_rate * OS_FACTOR);
        self.os.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "amt" => self.drive_sm.set_target(value.clamp(0.0, 1.0)),
            "mix" => self.mix_sm.set_target(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let Self {
            os,
            drive_sm,
            mix_sm,
        } = self;
        let n = l.len().min(r.len());
        for (cl, cr) in l[..n].chunks_mut(MAX_BLOCK).zip(r[..n].chunks_mut(MAX_BLOCK)) {
            os.process(cl, cr, |hl, hr| {
                for (sl, sr) in hl.iter_mut().zip(hr.iter_mut()) {
                    let k = drive_sm.next() * 100.0;
                    let mix = mix_sm.next();
                    // Tone.CrossFade equal-power law.
                    let wet_g = libm::sinf(mix * FRAC_PI_2);
                    let dry_g = libm::cosf(mix * FRAC_PI_2);
                    *sl = wet_g * shape(*sl, k) + dry_g * *sl;
                    *sr = wet_g * shape(*sr, k) + dry_g * *sr;
                }
            });
        }
    }

    fn reset(&mut self) {
        self.os.reset();
        let d = self.drive_sm.target();
        self.drive_sm.snap(d);
        let m = self.mix_sm.target();
        self.mix_sm.snap(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run_sine(fx: &mut DistFx, freq: f32, amp: f32, n: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0.0f32; n];
        for (i, s) in l.iter_mut().enumerate() {
            *s = amp * libm::sinf(core::f32::consts::TAU * freq * i as f32 / SR);
        }
        let mut r = l.clone();
        for (cl, cr) in l.chunks_mut(MAX_BLOCK).zip(r.chunks_mut(MAX_BLOCK)) {
            fx.process(cl, cr);
        }
        (l, r)
    }

    /// |DFT| of `x` at exactly `cycles` cycles over the window, normalized.
    fn dft_mag(x: &[f32], cycles: usize) -> f32 {
        let n = x.len() as f64;
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (i, s) in x.iter().enumerate() {
            let ph = core::f64::consts::TAU * cycles as f64 * i as f64 / n;
            re += *s as f64 * ph.cos();
            im -= *s as f64 * ph.sin();
        }
        (2.0 * (re * re + im * im).sqrt() / n) as f32
    }

    #[test]
    fn no_nan_inf_on_two_seconds() {
        let mut fx = DistFx::new();
        fx.prepare(SR);
        fx.set_param("amt", 0.7);
        fx.set_param("mix", 0.5);
        let (l, r) = run_sine(&mut fx, 220.0, 0.9, 2 * SR as usize);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
    }

    #[test]
    fn param_sweep_stays_bounded() {
        let mut fx = DistFx::new();
        fx.prepare(SR);
        let mut phase = 0.0f32;
        for step in 0..200 {
            let t = step as f32 / 199.0;
            fx.set_param("amt", t); // full 0..1 range
            fx.set_param("mix", 1.0 - t); // full 1..0 range
            let mut l = [0.0f32; MAX_BLOCK];
            for s in l.iter_mut() {
                phase += core::f32::consts::TAU * 440.0 / SR;
                *s = 1.5 * libm::sinf(phase); // hotter than 0 dBFS on purpose
            }
            let mut r = l;
            fx.process(&mut l, &mut r);
            for s in l.iter().chain(r.iter()) {
                assert!(s.is_finite() && s.abs() < 4.0, "unbounded: {s}");
            }
        }
    }

    #[test]
    fn wet_output_is_not_silent() {
        let mut fx = DistFx::new();
        fx.prepare(SR);
        fx.set_param("amt", 0.4);
        fx.set_param("mix", 1.0);
        let (l, _) = run_sine(&mut fx, 220.0, 0.5, SR as usize / 2);
        let rms = (l.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / l.len() as f64).sqrt();
        assert!(rms > 0.01, "wet output silent: rms = {rms}");
    }

    #[test]
    fn sine_in_odd_harmonics_out_and_bounded() {
        let mut fx = DistFx::new();
        fx.prepare(SR);
        fx.set_param("amt", 1.0);
        fx.set_param("mix", 1.0);
        // Warm up so smoothers/FIR history settle.
        run_sine(&mut fx, 375.0, 0.8, 4096);
        // 375 Hz = 32 exact cycles in a 4096-sample window at 48 kHz.
        let n = 4096;
        let (l, _) = run_sine(&mut fx, 375.0, 0.8, n);
        let h1 = dft_mag(&l, 32);
        let h2 = dft_mag(&l, 64);
        let h3 = dft_mag(&l, 96);
        assert!(h1 > 0.05, "fundamental missing: {h1}");
        assert!(h3 > 0.005, "odd (3rd) harmonic missing: {h3}");
        assert!(
            h2 < h3 * 0.1,
            "even harmonic should be absent: h2 = {h2}, h3 = {h3}"
        );
        // Curve maxes below π/9·(3+k)/(π+k) < 0.35; allow FIR overshoot.
        assert!(l.iter().all(|s| s.abs() < 0.5), "output not bounded");
    }

    #[test]
    fn mix_zero_is_dry_passthrough_level() {
        let mut fx = DistFx::new();
        fx.prepare(SR);
        fx.set_param("amt", 1.0);
        fx.set_param("mix", 0.0);
        fx.reset(); // snap smoothers to targets
        run_sine(&mut fx, 375.0, 0.5, 4096); // settle FIR history
        let (l, _) = run_sine(&mut fx, 375.0, 0.5, 4096);
        let h1 = dft_mag(&l, 32);
        let h3 = dft_mag(&l, 96);
        assert!((h1 - 0.5).abs() < 0.02, "dry level off: {h1}");
        assert!(h3 < 1e-3, "dry path should carry no distortion: {h3}");
    }
}
