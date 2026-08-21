//! `cheby` effect — port of the browser Tone.Chebyshev device (devices.ts
//! `makeEffect("cheby")`): a 4×-oversampled polynomial waveshaper plus
//! wet/dry mix. Schema: `order` 2..14 (int, def 3), `mix` 0..1 (def 0.35).
//!
//! Tone builds its curve from the Chebyshev recurrence with the base case
//! T0 := 0 (not the textbook 1): C1 = x, Cn = 2x·C(n−1) − C(n−2), which is
//! Cn(x) = x·U(n−1)(x). Zeroing T0 makes curve(0) = 0, so silence in is
//! silence out (no DC thump for even orders). For a full-scale sine the
//! polynomial emits harmonics N, N−2, N−4… (Nth at amplitude 1, lower ones
//! at 2) and nothing above N. WaveShaperNode clamps its input to the curve
//! domain [-1, 1]; |Cn| ≤ n there, so output is bounded by the order.
//! `order` maps through devices.ts `Math.max(1, v | 0)` — truncate, floor 1 —
//! and is inherently stepped (Tone swaps the curve instantly), so it takes no
//! smoother. Tone's `wet` is a Tone.CrossFade (equal-power sin/cos blend);
//! the mix param is honored the same way, blended *inside* the oversampled
//! block so wet and dry share the resampler latency and partial-mix settings
//! don't comb-filter.

use crate::traits::EffectDsp;
use crate::util::{Oversampler4, Smoother, MAX_BLOCK};

const OS_FACTOR: f32 = 4.0;
const FRAC_PI_2: f32 = core::f32::consts::FRAC_PI_2;
/// Schema max for `order`; also caps the per-sample recurrence loop.
const MAX_ORDER: i32 = 14;

/// Tone.Chebyshev transfer curve (see module docs).
#[inline]
fn shape(x: f32, order: i32) -> f32 {
    let xc = x.clamp(-1.0, 1.0); // WaveShaperNode curve domain
    if order <= 1 {
        return xc;
    }
    // C0 = 0, C1 = x, Cn = 2x·C(n−1) − C(n−2)  (Tone's memoized recurrence)
    let mut prev2 = 0.0f32;
    let mut prev1 = xc;
    for _ in 2..=order {
        let cur = 2.0 * xc * prev1 - prev2;
        prev2 = prev1;
        prev1 = cur;
    }
    prev1
}

pub struct ChebyFx {
    os: Oversampler4,
    order: i32, // stepped — devices.ts `Math.max(1, v | 0)`
    // Smoothed in the 4× domain (prepared at 4·sr) so the ~15 ms time
    // constant holds at the rate it is consumed.
    mix_sm: Smoother, // schema `mix` 0..1, def 0.35
}

impl ChebyFx {
    pub fn new() -> Self {
        Self {
            os: Oversampler4::new(),
            order: 3,
            mix_sm: Smoother::new(0.35),
        }
    }
}

impl Default for ChebyFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for ChebyFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.mix_sm.prepare(sample_rate * OS_FACTOR);
        self.os.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            // `v | 0` truncates toward zero; floor 1 per devices.ts, cap at
            // the schema max so the recurrence loop stays bounded.
            "order" => self.order = (value as i32).clamp(1, MAX_ORDER),
            "mix" => self.mix_sm.set_target(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let Self { os, order, mix_sm } = self;
        let order = *order;
        let n = l.len().min(r.len());
        for (cl, cr) in l[..n].chunks_mut(MAX_BLOCK).zip(r[..n].chunks_mut(MAX_BLOCK)) {
            os.process(cl, cr, |hl, hr| {
                for (sl, sr) in hl.iter_mut().zip(hr.iter_mut()) {
                    let mix = mix_sm.next();
                    // Tone.CrossFade equal-power law.
                    let wet_g = libm::sinf(mix * FRAC_PI_2);
                    let dry_g = libm::cosf(mix * FRAC_PI_2);
                    *sl = wet_g * shape(*sl, order) + dry_g * *sl;
                    *sr = wet_g * shape(*sr, order) + dry_g * *sr;
                }
            });
        }
    }

    fn reset(&mut self) {
        self.os.reset();
        let m = self.mix_sm.target();
        self.mix_sm.snap(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run_sine(fx: &mut ChebyFx, freq: f32, amp: f32, n: usize) -> (Vec<f32>, Vec<f32>) {
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
    fn curve_matches_tone_recurrence() {
        // Odd orders are odd-symmetric, even orders even-symmetric, and
        // curve(0) = 0 for every order (Tone's zeroed T0).
        for order in 1..=MAX_ORDER {
            assert_eq!(shape(0.0, order), 0.0, "curve(0) != 0 at order {order}");
            let s = if order % 2 == 1 { -1.0 } else { 1.0 };
            for x in [-0.9f32, -0.5, -0.1] {
                let (a, b) = (shape(x, order), shape(-x, order));
                assert!((a - s * b).abs() < 1e-5, "symmetry broken at order {order}");
            }
        }
        // Spot values: C2 = 2x², C3 = 4x³ − x; Cn(1) = n.
        assert!((shape(0.5, 2) - 0.5).abs() < 1e-6);
        assert!((shape(0.5, 3) - 0.0).abs() < 1e-6);
        for order in 1..=MAX_ORDER {
            assert!((shape(1.0, order) - order as f32).abs() < 1e-3);
        }
    }

    #[test]
    fn no_nan_inf_on_two_seconds() {
        let mut fx = ChebyFx::new();
        fx.prepare(SR);
        fx.set_param("order", 8.0);
        fx.set_param("mix", 0.35);
        let (l, r) = run_sine(&mut fx, 220.0, 0.9, 2 * SR as usize);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
    }

    #[test]
    fn param_sweep_stays_bounded() {
        let mut fx = ChebyFx::new();
        fx.prepare(SR);
        let mut phase = 0.0f32;
        for step in 0..200 {
            let t = step as f32 / 199.0;
            fx.set_param("order", 2.0 + t * 12.0); // full 2..14 range
            fx.set_param("mix", 1.0 - t); // full 1..0 range
            let mut l = [0.0f32; MAX_BLOCK];
            for s in l.iter_mut() {
                phase += core::f32::consts::TAU * 440.0 / SR;
                *s = 1.5 * libm::sinf(phase); // hotter than 0 dBFS on purpose
            }
            let mut r = l;
            fx.process(&mut l, &mut r);
            for s in l.iter().chain(r.iter()) {
                // |Cn| ≤ 14 on the clamped domain; allow FIR overshoot.
                assert!(s.is_finite() && s.abs() < 20.0, "unbounded: {s}");
            }
        }
    }

    #[test]
    fn wet_output_is_not_silent() {
        let mut fx = ChebyFx::new();
        fx.prepare(SR);
        fx.set_param("order", 3.0);
        fx.set_param("mix", 1.0);
        let (l, _) = run_sine(&mut fx, 220.0, 0.5, SR as usize / 2);
        let rms = (l.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / l.len() as f64).sqrt();
        assert!(rms > 0.01, "wet output silent: rms = {rms}");
    }

    /// Order-N full-scale sine: the polynomial's top harmonic is exactly N
    /// (amplitude ≈ 1), nothing above N, and output stays bounded. For the
    /// Tone curve C3(sinθ) = 2·sinθ − sin3θ, so H3 ≈ 1 and H1 ≈ 2.
    #[test]
    fn order_n_sine_yields_nth_harmonic() {
        for (order, cycles) in [(3usize, 32usize), (6, 32)] {
            let mut fx = ChebyFx::new();
            fx.prepare(SR);
            fx.set_param("order", order as f32);
            fx.set_param("mix", 1.0);
            fx.reset(); // snap the mix smoother
            // 375 Hz = 32 exact cycles in a 4096-sample window at 48 kHz.
            run_sine(&mut fx, 375.0, 1.0, 4096); // settle FIR history
            let (l, _) = run_sine(&mut fx, 375.0, 1.0, 4096);
            let hn = dft_mag(&l, cycles * order);
            let above = dft_mag(&l, cycles * (order + 1)).max(dft_mag(&l, cycles * (order + 2)));
            assert!(
                (0.7..1.3).contains(&hn),
                "order {order}: Nth harmonic should be ~1: {hn}"
            );
            assert!(
                above < 0.05 * hn,
                "order {order}: harmonics above N should vanish: {above} vs {hn}"
            );
            // Nth harmonic is dominant-ish: within 2× of every other bin
            // (the N−2 harmonic legitimately sits at 2× in Tone's curve).
            for k in 1..order {
                let hk = dft_mag(&l, cycles * k);
                assert!(
                    hn > 0.4 * hk,
                    "order {order}: H{order} = {hn} buried under H{k} = {hk}"
                );
            }
            assert!(
                l.iter().all(|s| s.is_finite() && s.abs() < order as f32 + 2.0),
                "order {order}: output not bounded"
            );
        }
    }

    #[test]
    fn mix_zero_is_dry_passthrough_level() {
        let mut fx = ChebyFx::new();
        fx.prepare(SR);
        fx.set_param("order", 14.0);
        fx.set_param("mix", 0.0);
        fx.reset(); // snap smoothers to targets
        run_sine(&mut fx, 375.0, 0.5, 4096); // settle FIR history
        let (l, _) = run_sine(&mut fx, 375.0, 0.5, 4096);
        let h1 = dft_mag(&l, 32);
        let h2 = dft_mag(&l, 64);
        assert!((h1 - 0.5).abs() < 0.02, "dry level off: {h1}");
        assert!(h2 < 1e-3, "dry path should carry no distortion: {h2}");
    }
}
