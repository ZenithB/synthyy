//! `widen` effect — stereo widener.
//!
//! Port of the `Tone.StereoWidener` composite in `src/audio/devices.ts`
//! `makeEffect("widen")`: mid/side width control where `mid *= 2*(1-width)`
//! and `side *= 2*width` on a √½-normalized M/S split/merge. Width 0.5 is
//! unity, 0 collapses to mono (mid only), 1 is all side (max wide). Schema
//! (src/audio/schema.ts): single param `width` in [0, 1], default 0.8 — the
//! device has no mix or tempo-synced params.

use crate::traits::EffectDsp;
use crate::util::Smoother;

const DEF_WIDTH: f32 = 0.8;

pub struct WidenFx {
    width: Smoother,
}

impl WidenFx {
    pub fn new() -> Self {
        Self {
            width: Smoother::new(DEF_WIDTH),
        }
    }
}

impl Default for WidenFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for WidenFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.width.prepare(sample_rate);
        self.width.snap(self.width.target());
    }

    fn set_param(&mut self, key: &str, value: f32) {
        if key == "width" {
            self.width.set_target(value.clamp(0.0, 1.0));
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let w = self.width.next();
            // √½ split and √½ merge fold into a single ×0.5 on each branch.
            let mid = (*ls + *rs) * (1.0 - w); // 0.5 * 2*(1-w)
            let side = (*ls - *rs) * w; // 0.5 * 2*w
            *ls = mid + side;
            *rs = mid - side;
        }
    }

    fn reset(&mut self) {
        self.width.snap(self.width.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn make(width: f32) -> WidenFx {
        let mut d = WidenFx::new();
        d.prepare(SR);
        d.set_param("width", width);
        d.reset(); // snap past the smoother for analytic checks
        d
    }

    /// Deterministic stereo test signal: decorrelated sine pair.
    fn stereo_input(n: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        for i in 0..n {
            let t = i as f32 / SR;
            l[i] = 0.5 * libm::sinf(core::f32::consts::TAU * 220.0 * t);
            r[i] = 0.5 * libm::sinf(core::f32::consts::TAU * 331.0 * t + 0.7);
        }
        (l, r)
    }

    fn side_energy(l: &[f32], r: &[f32]) -> f64 {
        l.iter()
            .zip(r)
            .map(|(a, b)| {
                let s = 0.5 * (a - b);
                (s * s) as f64
            })
            .sum()
    }

    #[test]
    fn no_nan_inf_over_two_seconds() {
        let mut d = make(0.8);
        let n = (2.0 * SR) as usize;
        let (mut l, mut r) = stereo_input(n);
        for (cl, cr) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            d.process(cl, cr);
        }
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
    }

    #[test]
    fn param_sweep_stays_bounded() {
        let mut d = WidenFx::new();
        d.prepare(SR);
        let (mut l, mut r) = stereo_input(4096);
        for (i, (cl, cr)) in l.chunks_mut(128).zip(r.chunks_mut(128)).enumerate() {
            // Sweep the full schema range (plus out-of-range values, which
            // set_param must clamp) while audio runs.
            d.set_param("width", -0.5 + i as f32 * 0.1);
            d.set_param("nope", 42.0); // unknown keys are ignored
            d.process(cl, cr);
        }
        assert!(l
            .iter()
            .chain(r.iter())
            .all(|s| s.is_finite() && s.abs() <= 4.0));
    }

    #[test]
    fn wet_output_is_non_silent() {
        let mut d = make(0.8);
        let (mut l, mut r) = stereo_input(1024);
        d.process(&mut l, &mut r);
        let energy: f64 = l.iter().chain(r.iter()).map(|s| (s * s) as f64).sum();
        assert!(energy > 1e-3);
    }

    #[test]
    fn width_half_is_unity() {
        let mut d = make(0.5);
        let (l0, r0) = stereo_input(512);
        let (mut l, mut r) = (l0.clone(), r0.clone());
        d.process(&mut l, &mut r);
        for i in 0..l.len() {
            assert!((l[i] - l0[i]).abs() < 1e-5 && (r[i] - r0[i]).abs() < 1e-5);
        }
    }

    #[test]
    fn width_zero_collapses_to_mono() {
        let mut d = make(0.0);
        let (mut l, mut r) = stereo_input(512);
        d.process(&mut l, &mut r);
        for i in 0..l.len() {
            assert!((l[i] - r[i]).abs() < 1e-6, "L != R at {i}");
        }
    }

    #[test]
    fn width_one_boosts_side_energy() {
        let mut d = make(1.0);
        let (l0, r0) = stereo_input(2048);
        let (mut l, mut r) = (l0.clone(), r0.clone());
        d.process(&mut l, &mut r);
        let e_in = side_energy(&l0, &r0);
        let e_out = side_energy(&l, &r);
        assert!(e_in > 1e-6, "test signal must carry side content");
        assert!(e_out > e_in, "side energy out {e_out} <= in {e_in}");
    }

    #[test]
    fn width_changes_are_smoothed() {
        let mut d = make(0.5);
        d.set_param("width", 0.0); // step the target; output must ramp
        let (mut l, mut r) = stereo_input(64);
        let (l0, r0) = (l.clone(), r.clone());
        d.process(&mut l, &mut r);
        // First sample is still ~unity (smoother barely moved)…
        assert!((l[0] - l0[0]).abs() < 1e-3 && (r[0] - r0[0]).abs() < 1e-3);
        // …and not yet mono a block later.
        let last = l.len() - 1;
        assert!((l[last] - r[last]).abs() > 1e-4);
    }
}
