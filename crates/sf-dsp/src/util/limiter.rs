//! Master-bus limiter matching the browser reference `Tone.Limiter(-1)`
//! (RUST_ARCH.md: master fx → limiter (−1 dBFS) → out).
//!
//! Tone.Limiter is NOT a brickwall. It is a thin wrapper around a native
//! DynamicsCompressorNode constructed with ratio 20, attack 0.003 s,
//! release 0.01 s and the node's default 30 dB knee
//! (node_modules/tone/build/esm/component/dynamics/Limiter.js →
//! Compressor.js → `context.createDynamicsCompressor()`). With the knee
//! spanning [threshold, threshold + 30] dB it is nearly transparent at
//! moderate overs, so the previous hard −1 dBFS brickwall here lost ~2.5–3.5
//! dB on hot mixes relative to the browser (docs/PARITY.md: the fx-opto /
//! fx-comp ΔRMS residuals).
//!
//! The DSP is the Blink `DynamicsCompressorKernel` port in `crate::fx::comp`
//! (`CompFx::with_params`), reused as-is; this type only pins Tone.Limiter's
//! fixed parameter set and keeps the old master-chain API surface
//! (`new` / `prepare` / `reset` / `gr_db` / `process`).

use crate::fx::comp::CompFx;
use crate::traits::EffectDsp;

/// Tone.Limiter's fixed compressor settings (Limiter.js constructor).
const RATIO: f32 = 20.0;
const ATTACK_SEC: f32 = 0.003;
const RELEASE_SEC: f32 = 0.01;

pub struct Limiter {
    comp: CompFx,
}

impl Limiter {
    /// `threshold_db` ≤ 0 (−1 dBFS for the master bus). This is
    /// Tone.Limiter's `threshold` — the knee onset, not a hard ceiling.
    pub fn new(threshold_db: f32) -> Self {
        Self {
            comp: CompFx::with_params(threshold_db, RATIO, ATTACK_SEC, RELEASE_SEC),
        }
    }

    pub fn prepare(&mut self, sample_rate: f32) {
        self.comp.prepare(sample_rate);
    }

    pub fn reset(&mut self) {
        self.comp.reset();
    }

    /// Current gain reduction in dB (≤ 0) for a GR meter — mirrors
    /// Tone.Limiter's `reduction` attribute.
    pub fn gr_db(&self) -> f32 {
        self.comp.gr_db()
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        self.comp.process(l, r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::db_to_lin;

    // The previous tests here (`never_exceeds_ceiling`, `unity_below_threshold`
    // with the 5 ms lookahead delay, `reports_gain_reduction_when_limiting`
    // expecting > 3 dB of GR at +3.5 dBFS) encoded the old brickwall design.
    // Tone.Limiter(-1) — a DynamicsCompressorNode with threshold −1, ratio 20,
    // knee 30 (Limiter.js/Compressor.js) — guarantees none of that: its knee
    // spans [−1, +29] dB, so moderate overs pass nearly untouched and the
    // ceiling is soft. They are replaced by tests pinning the compressor
    // semantics and the kernel reuse.

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn run_blocks(process: &mut dyn FnMut(&mut [f32], &mut [f32]), l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        let mut i = 0;
        while i < n {
            let end = (i + BLOCK).min(n);
            let (la, ra) = (&mut l[i..end], &mut r[i..end]);
            process(la, ra);
            i = end;
        }
    }

    fn sine(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * libm::sinf(2.0 * core::f32::consts::PI * freq * i as f32 / SR))
            .collect()
    }

    /// The limiter IS the comp kernel with Tone.Limiter's parameters:
    /// sample-exact against `CompFx::with_params(-1, 20, 0.003, 0.01)`.
    #[test]
    fn is_comp_kernel_with_tone_limiter_params() {
        let mut lim = Limiter::new(-1.0);
        lim.prepare(SR);
        let mut comp = CompFx::with_params(-1.0, RATIO, ATTACK_SEC, RELEASE_SEC);
        comp.prepare(SR);

        let n = 8192;
        let src = sine(n, 220.0, 1.4);
        let (mut ll, mut lr) = (src.clone(), src.clone());
        let (mut cl, mut cr) = (src.clone(), src.clone());
        run_blocks(&mut |l, r| lim.process(l, r), &mut ll, &mut lr);
        run_blocks(&mut |l, r| comp.process(l, r), &mut cl, &mut cr);
        assert_eq!(ll, cl, "limiter must be bit-exact with the comp kernel");
        assert_eq!(lr, cr);
        assert_eq!(lim.gr_db(), comp.gr_db());
    }

    /// Moderate overs pass nearly transparent — the defining difference from
    /// the old brickwall: a +1 dBFS sine comes out ABOVE the old −1 dBFS
    /// ceiling, within ~1 dB of the input level.
    #[test]
    fn nearly_transparent_at_moderate_overs() {
        let mut lim = Limiter::new(-1.0);
        lim.prepare(SR);
        let n = SR as usize;
        let amp = db_to_lin(1.0); // +1 dBFS
        let mut l = sine(n, 220.0, amp);
        let mut r = l.clone();
        run_blocks(&mut |l, r| lim.process(l, r), &mut l, &mut r);
        let peak = l[n / 2..].iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(
            peak > db_to_lin(-1.0),
            "peak {peak} clamped at the old brickwall ceiling"
        );
        assert!(
            peak > amp * db_to_lin(-1.0) && peak < amp * db_to_lin(1.0),
            "peak {peak} not within ±1 dB of input {amp}"
        );
        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
    }

    /// Below threshold the node is a (slightly-above-)unity gain stage with
    /// the kernel's 6 ms pre-delay — not the old 5 ms-lookahead unity path.
    #[test]
    fn below_threshold_near_unity_with_6ms_pre_delay() {
        let mut lim = Limiter::new(-1.0);
        lim.prepare(SR);
        // impulse: emerges after round(0.006·sr) samples
        let pd = (0.006 * SR) as usize;
        let n = 4096;
        let mut l = vec![0.0f32; n];
        l[0] = 0.25;
        let mut r = l.clone();
        run_blocks(&mut |l, r| lim.process(l, r), &mut l, &mut r);
        let peak_idx = l
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap()
            .0;
        assert_eq!(peak_idx, pd, "impulse should emerge after {pd} samples");

        // steady quiet DC: gain = the node's fixed makeup, ≈ unity at
        // threshold −1 (fullRangeGain barely below 1) — never attenuating.
        // 2 s: the kernel's startup transient (detector_average resets to 0
        // and ramps up, briefly dipping the gain) must drain through the
        // 0.325 s metering release before the meter reads ~0.
        lim.reset();
        let m = 2 * SR as usize;
        let mut ql = vec![0.05f32; m];
        let mut qr = vec![-0.05f32; m];
        run_blocks(&mut |l, r| lim.process(l, r), &mut ql, &mut qr);
        let g = ql[m - 1] / 0.05;
        assert!(
            (1.0..1.05).contains(&g),
            "below-threshold gain {g} should be makeup ≈ unity"
        );
        assert!(lim.gr_db() > -0.1, "gr {} should be ~0", lim.gr_db());
    }

    /// Hot input still meters gain reduction, and reset clears it.
    #[test]
    fn reports_gain_reduction_when_limiting() {
        let mut lim = Limiter::new(-1.0);
        lim.prepare(SR);
        let n = SR as usize;
        let mut l = vec![8.0f32; n]; // +18 dBFS — deep into the knee
        let mut r = vec![8.0f32; n];
        run_blocks(&mut |l, r| lim.process(l, r), &mut l, &mut r);
        let gr = lim.gr_db();
        assert!(gr < -0.2, "gr {gr} should show reduction at +18 dBFS");
        assert!(gr > -20.0 && gr.is_finite());
        lim.reset();
        assert_eq!(lim.gr_db(), 0.0);
    }
}
