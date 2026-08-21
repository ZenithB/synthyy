//! `comp` effect — port of the Tone.Compressor insert built by
//! `makeEffect("comp")` in src/audio/devices.ts.
//!
//! Tone.Compressor is a thin wrapper around the NATIVE DynamicsCompressorNode
//! (node_modules/tone/build/esm/component/dynamics/Compressor.js —
//! `this.context.createDynamicsCompressor()`), so the behavior spec is the
//! Blink/WebKit `DynamicsCompressorKernel` (Chromium
//! third_party/blink/renderer/platform/audio/dynamics_compressor_kernel.cc),
//! ported here operation-for-operation:
//!
//! * Static curve: linear below threshold, then an exponential soft knee
//!   spanning [threshold, threshold + knee] dB (NOT centered on threshold),
//!   then a constant-ratio dB slope matched to the knee end. The knee shape
//!   constant `k` is solved by 15-step bisection so the knee's dB/dB slope at
//!   the knee end equals 1/ratio.
//! * Makeup: the node self-compensates —
//!   `full_range_gain = Saturate(1, k); makeup = (1/full_range_gain)^0.6`
//!   (the "empirical/perceptual tuning" line in the kernel). Signals below
//!   threshold are therefore AMPLIFIED by the makeup, exactly like the
//!   browser node.
//! * Detector: stereo-linked instant-attack / ~2.5 ms-release follower of the
//!   static-curve attenuation, asin/sin gain warping, adaptive release via
//!   the kernel's 4th-order polynomial, and a 6 ms lookahead pre-delay.
//! * `gr_db()` mirrors the node's `reduction` attribute: the metering gain
//!   (dB, ≤ 0), which tracks the post-warp compressor gain and excludes
//!   makeup.
//!
//! Schema (schema.ts "comp"): thresh −60..0 dB (def −20), ratio 1..20 (def 4),
//! attack 0.001..0.3 s (def 0.01), release 0.02..1 s (def 0.2). Knee is never
//! set by devices.ts, so it stays at the DynamicsCompressorNode default 30 dB.
//! No mix param — the insert is fully wet.

use crate::traits::EffectDsp;
use crate::util::{db_to_lin, lin_to_db, Smoother};

/// DynamicsCompressorNode default knee width in dB (devices.ts never sets it).
const KNEE_DB: f32 = 30.0;
/// dynamics_compressor.cc: SetParameterValue(kParamPreDelay, 0.006f).
const PRE_DELAY_SEC: f32 = 0.006;
/// dynamics_compressor_kernel.h: kMaxPreDelayFrames = 1024 (power of two).
const MAX_PRE_DELAY: usize = 1024;
const PRE_DELAY_MASK: usize = MAX_PRE_DELAY - 1;
/// Kernel processes the envelope in sub-divisions of 32 frames.
const DIVISION_FRAMES: u32 = 32;
/// Kernel: detector ("sat") release time, seconds.
const SAT_RELEASE_SEC: f32 = 0.0025;
/// Kernel: kSpacingDb for the adaptive release rate.
const SPACING_DB: f32 = 5.0;
/// Kernel: metering hits peaks instantly, releases with this time constant.
const METERING_RELEASE_SEC: f32 = 0.325;
/// dynamics_compressor.cc: release zone params (fractions of release_frames).
const RELEASE_ZONES: [f32; 4] = [0.09, 0.16, 0.42, 0.98];

const FRAC_PI_2: f32 = core::f32::consts::FRAC_PI_2;

/// Blink KneeCurve(x, k): linear up to the threshold, then an exponential
/// approach with sharpness `k`.
#[inline]
fn knee_curve(lin_thresh: f32, x: f32, k: f32) -> f32 {
    if x < lin_thresh {
        x
    } else {
        lin_thresh + (1.0 - libm::expf(-k * (x - lin_thresh))) / k
    }
}

/// Blink SlopeAt(x, k): finite-difference dB/dB slope of the knee curve.
#[inline]
fn slope_at(lin_thresh: f32, x: f32, k: f32) -> f32 {
    if x < lin_thresh {
        return 1.0;
    }
    let x2 = x * 1.001;
    let x_db = lin_to_db(x);
    let x2_db = lin_to_db(x2);
    let y_db = lin_to_db(knee_curve(lin_thresh, x, k));
    let y2_db = lin_to_db(knee_curve(lin_thresh, x2, k));
    (y2_db - y_db) / (x2_db - x_db)
}

/// Blink KAtSlope: bisect `k` so the knee's slope at the knee end equals
/// `desired_slope` (= 1/ratio).
fn k_at_slope(lin_thresh: f32, thresh_db: f32, desired_slope: f32) -> f32 {
    let x = db_to_lin(thresh_db + KNEE_DB);
    let mut min_k = 0.1f32;
    let mut max_k = 10000.0f32;
    let mut k = 5.0f32;
    for _ in 0..15 {
        // A higher k asymptotes faster toward slope 0.
        if slope_at(lin_thresh, x, k) < desired_slope {
            max_k = k;
        } else {
            min_k = k;
        }
        k = libm::sqrtf(min_k * max_k);
    }
    k
}

pub struct CompFx {
    sample_rate: f32,
    /// Threshold in dB (audible, smoothed ~15 ms).
    thresh: Smoother,
    /// Ratio ≥ 1 (audible, smoothed ~15 ms).
    ratio: Smoother,
    attack_sec: f32,
    release_sec: f32,

    // --- static curve cache (recomputed when thresh/ratio move) ---
    curve_thresh_db: f32,
    curve_ratio: f32,
    lin_thresh: f32,
    knee_thresh: f32,
    knee_thresh_db: f32,
    yknee_db: f32,
    slope: f32,
    k: f32,
    /// Blink master_linear_gain: dbToLinear(postGain=0) * (1/Saturate(1,k))^0.6.
    master_linear_gain: f32,

    // --- time-derived coefficients ---
    attack_frames: f32,
    sat_release_frames: f32,
    metering_release_k: f32,
    /// Adaptive-release polynomial coefficients (functions of release_frames).
    rel_a: f32,
    rel_b: f32,
    rel_c: f32,
    rel_d: f32,
    rel_e: f32,

    // --- kernel state ---
    detector_average: f32,
    compressor_gain: f32,
    /// dB (≤ 0 once running); Blink initializes the field to 1.
    metering_gain: f32,
    /// −1 is the kernel's "uninitialized" sentinel.
    max_attack_compression_diff_db: f32,
    scaled_desired_gain: f32,
    envelope_rate: f32,
    /// Samples left until the next 32-frame envelope division boundary.
    div_countdown: u32,

    // --- lookahead pre-delay ---
    delay_l: [f32; MAX_PRE_DELAY],
    delay_r: [f32; MAX_PRE_DELAY],
    read_idx: usize,
    write_idx: usize,
}

impl CompFx {
    pub fn new() -> Self {
        let mut fx = Self {
            sample_rate: 44_100.0,
            // Schema defaults: thresh −20 dB, ratio 4, attack 10 ms, release 200 ms.
            thresh: Smoother::new(-20.0),
            ratio: Smoother::new(4.0),
            attack_sec: 0.01,
            release_sec: 0.2,
            curve_thresh_db: f32::NAN,
            curve_ratio: f32::NAN,
            lin_thresh: 0.0,
            knee_thresh: 0.0,
            knee_thresh_db: 0.0,
            yknee_db: 0.0,
            slope: 1.0,
            k: 5.0,
            master_linear_gain: 1.0,
            attack_frames: 1.0,
            sat_release_frames: 1.0,
            metering_release_k: 0.0,
            rel_a: 0.0,
            rel_b: 0.0,
            rel_c: 0.0,
            rel_d: 0.0,
            rel_e: 0.0,
            detector_average: 0.0,
            compressor_gain: 1.0,
            metering_gain: 1.0,
            max_attack_compression_diff_db: -1.0,
            scaled_desired_gain: 0.0,
            envelope_rate: 1.0,
            div_countdown: 0,
            delay_l: [0.0; MAX_PRE_DELAY],
            delay_r: [0.0; MAX_PRE_DELAY],
            read_idx: 0,
            write_idx: 0,
        };
        fx.update_curve(fx.thresh.target(), fx.ratio.target());
        fx.update_time_coeffs();
        fx.reset_kernel();
        fx
    }

    /// Kernel-reuse constructor: the same Blink `DynamicsCompressorKernel`
    /// with explicit node parameters, bypassing the "comp" schema clamps in
    /// `set_param` (`util::Limiter` needs Tone.Limiter's 0.01 s release,
    /// below the schema's 0.02 s floor). The schema-driven `new()`/`set_param`
    /// path is untouched. Clamps here are the native node's ranges
    /// (threshold −100..0 dB, ratio 1..20) plus a 1 ms floor on the time
    /// constants so the frame-rate math stays finite.
    pub fn with_params(thresh_db: f32, ratio: f32, attack_sec: f32, release_sec: f32) -> Self {
        let mut fx = Self::new();
        fx.thresh = Smoother::new(thresh_db.clamp(-100.0, 0.0));
        fx.ratio = Smoother::new(ratio.clamp(1.0, 20.0));
        fx.attack_sec = attack_sec.clamp(0.001, 1.0);
        fx.release_sec = release_sec.clamp(0.001, 1.0);
        fx.update_curve(fx.thresh.target(), fx.ratio.target());
        fx.update_time_coeffs();
        fx.reset_kernel();
        fx
    }

    /// Blink UpdateStaticCurveParameters + the makeup-gain lines of Process.
    fn update_curve(&mut self, thresh_db: f32, ratio: f32) {
        self.curve_thresh_db = thresh_db;
        self.curve_ratio = ratio;
        self.lin_thresh = db_to_lin(thresh_db);
        self.slope = 1.0 / ratio;
        self.k = k_at_slope(self.lin_thresh, thresh_db, self.slope);
        self.knee_thresh_db = thresh_db + KNEE_DB;
        self.knee_thresh = db_to_lin(self.knee_thresh_db);
        self.yknee_db = lin_to_db(knee_curve(self.lin_thresh, self.knee_thresh, self.k));
        // Makeup gain — the node self-compensates (dynamics_compressor_kernel.cc:
        // fullRangeGain = Saturate(1, k); makeupGain = (1/fullRangeGain)^0.6;
        // masterLinearGain = dbToLinear(postGain=0) * makeupGain).
        let full_range_gain = self.saturate(1.0);
        self.master_linear_gain = libm::powf(1.0 / full_range_gain, 0.6);
    }

    /// Blink Saturate(x, k): the full static curve (linear / knee / ratio).
    #[inline]
    fn saturate(&self, x: f32) -> f32 {
        if x < self.knee_thresh {
            knee_curve(self.lin_thresh, x, self.k)
        } else {
            // Constant ratio after the knee, 1st-derivative matched at its end.
            db_to_lin(self.yknee_db + self.slope * (lin_to_db(x) - self.knee_thresh_db))
        }
    }

    // The adaptive-release coefficients below are kept digit-for-digit as the
    // (double-precision) literals in dynamics_compressor_kernel.cc so they can
    // be audited against the kernel source; f32 rounds them identically.
    #[allow(clippy::excessive_precision)]
    fn update_time_coeffs(&mut self) {
        let sr = self.sample_rate;
        self.attack_frames = self.attack_sec.max(0.001) * sr;
        self.sat_release_frames = SAT_RELEASE_SEC * sr;
        self.metering_release_k = 1.0 - libm::expf(-1.0 / (sr * METERING_RELEASE_SEC));
        // Adaptive release: 4th-order polynomial through
        // (0, y1) (1, y2) (2, y3) (3, y4) — coefficients verbatim from
        // dynamics_compressor_kernel.cc.
        let release_frames = sr * self.release_sec;
        let y1 = release_frames * RELEASE_ZONES[0];
        let y2 = release_frames * RELEASE_ZONES[1];
        let y3 = release_frames * RELEASE_ZONES[2];
        let y4 = release_frames * RELEASE_ZONES[3];
        self.rel_a = 0.999_999_999_999_999_8 * y1 + 1.843_222e-16 * y2 - 1.937_339_4e-16 * y3
            + 8.824_516e-18 * y4;
        self.rel_b = -1.578_832_035_284_588_8 * y1 + 2.330_583_703_207_428_6 * y2
            - 0.914_119_420_484_042_9 * y3
            + 0.162_367_752_561_203_2 * y4;
        self.rel_c = 0.533_414_286_910_642_4 * y1 - 1.272_736_789_213_631 * y2
            + 0.925_885_604_220_751_2 * y3
            - 0.186_563_101_917_762_26 * y4;
        self.rel_d = 0.087_834_631_382_072_34 * y1 - 0.169_416_296_792_562_2 * y2
            + 0.085_880_579_515_952_72 * y3
            - 0.004_298_914_105_462_83 * y4;
        self.rel_e = -0.042_416_883_008_123_074 * y1 + 0.111_569_382_798_760_2 * y2
            - 0.097_646_763_252_658_72 * y3
            + 0.028_494_263_462_021_576 * y4;
    }

    /// Blink Reset() + SetPreDelayTime(): clear detector/gain state and the
    /// lookahead line (read = 0, write = pre-delay frames).
    fn reset_kernel(&mut self) {
        self.detector_average = 0.0;
        self.compressor_gain = 1.0;
        self.metering_gain = 1.0;
        self.max_attack_compression_diff_db = -1.0;
        self.scaled_desired_gain = 0.0;
        self.envelope_rate = 1.0;
        self.div_countdown = 0;
        self.delay_l = [0.0; MAX_PRE_DELAY];
        self.delay_r = [0.0; MAX_PRE_DELAY];
        let pd = ((PRE_DELAY_SEC * self.sample_rate) as usize).min(MAX_PRE_DELAY - 1);
        self.read_idx = 0;
        self.write_idx = pd;
    }

    /// Envelope work done once per 32-frame division in the kernel: pick the
    /// slew rate toward the detector's desired gain (adaptive release /
    /// peak-tracked attack).
    fn update_division(&mut self) {
        let desired_gain = self.detector_average;
        // Pre-warp so we land on desired_gain after the sin() warp below.
        let scaled = libm::asinf(desired_gain) / FRAC_PI_2;
        self.scaled_desired_gain = scaled;
        let is_releasing = scaled > self.compressor_gain;
        // Distance between the current compression level and the desired one.
        let mut diff_db = lin_to_db(self.compressor_gain / scaled);
        if is_releasing {
            self.max_attack_compression_diff_db = -1.0;
            // Fix gremlins.
            if !diff_db.is_finite() {
                diff_db = -1.0;
            }
            // Adaptive release: heavier compression releases faster.
            // Map −12..0 dB → x in 0..3 and evaluate the polynomial.
            let x = 0.25 * (diff_db.clamp(-12.0, 0.0) + 12.0);
            let x2 = x * x;
            let release_frames =
                self.rel_a + self.rel_b * x + self.rel_c * x2 + self.rel_d * x2 * x
                    + self.rel_e * x2 * x2;
            let db_per_frame = SPACING_DB / release_frames;
            self.envelope_rate = db_to_lin(db_per_frame); // > 1
        } else {
            // Fix gremlins.
            if !diff_db.is_finite() {
                diff_db = 1.0;
            }
            // Attack rate from the largest diff seen during this attack.
            if self.max_attack_compression_diff_db == -1.0
                || self.max_attack_compression_diff_db < diff_db
            {
                self.max_attack_compression_diff_db = diff_db;
            }
            let eff_atten_diff_db = self.max_attack_compression_diff_db.max(0.5);
            let x = 0.25 / eff_atten_diff_db;
            self.envelope_rate = 1.0 - libm::powf(x, 1.0 / self.attack_frames); // < 1
        }
    }
}

impl Default for CompFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for CompFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.thresh.prepare(self.sample_rate);
        self.ratio.prepare(self.sample_rate);
        self.thresh.snap(self.thresh.target());
        self.ratio.snap(self.ratio.target());
        self.update_curve(self.thresh.target(), self.ratio.target());
        self.update_time_coeffs();
        self.reset_kernel();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "thresh" => self.thresh.set_target(value.clamp(-60.0, 0.0)),
            "ratio" => self.ratio.set_target(value.clamp(1.0, 20.0)),
            "attack" => {
                self.attack_sec = value.clamp(0.001, 0.3);
                self.update_time_coeffs();
            }
            "release" => {
                self.release_sec = value.clamp(0.02, 1.0);
                self.update_time_coeffs();
            }
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let t = self.thresh.next();
            let ratio = self.ratio.next();
            if self.div_countdown == 0 {
                // Params are k-rate in the kernel; re-solve the curve only
                // when they actually moved.
                if (t - self.curve_thresh_db).abs() > 1e-3
                    || (ratio - self.curve_ratio).abs() > 1e-3
                {
                    self.update_curve(t, ratio);
                }
                self.update_division();
                self.div_countdown = DIVISION_FRAMES;
            }
            self.div_countdown -= 1;

            // Write into the lookahead line; detect on the UNDELAYED input
            // (stereo-linked channel max), exactly like the kernel.
            self.delay_l[self.write_idx] = *ls;
            self.delay_r[self.write_idx] = *rs;
            let abs_input = ls.abs().max(rs.abs());

            // Shaped power through the static curve → instantaneous attenuation.
            let shaped_input = self.saturate(abs_input);
            let attenuation = if abs_input <= 1e-4 {
                1.0
            } else {
                shaped_input / abs_input
            };
            let attenuation_db = (-lin_to_db(attenuation)).max(2.0);
            let db_per_frame = attenuation_db / self.sat_release_frames;
            let sat_release_rate = db_to_lin(db_per_frame) - 1.0;
            let rate = if attenuation > self.detector_average {
                sat_release_rate
            } else {
                1.0 // instant attack
            };
            self.detector_average += (attenuation - self.detector_average) * rate;
            self.detector_average = self.detector_average.min(1.0);
            if !self.detector_average.is_finite() {
                self.detector_average = 1.0;
            }

            // Exponential approach to the desired gain.
            if self.envelope_rate < 1.0 {
                self.compressor_gain +=
                    (self.scaled_desired_gain - self.compressor_gain) * self.envelope_rate;
            } else {
                self.compressor_gain = (self.compressor_gain * self.envelope_rate).min(1.0);
            }

            // Warp with sin() to smooth sharp exponential transition points.
            let post_warp = libm::sinf(FRAC_PI_2 * self.compressor_gain);
            let total_gain = self.master_linear_gain * post_warp;

            // Metering (this is the node's `reduction`): instant on peaks,
            // slow release; excludes makeup.
            let db_real_gain = lin_to_db(post_warp);
            if db_real_gain < self.metering_gain {
                self.metering_gain = db_real_gain;
            } else {
                self.metering_gain += (db_real_gain - self.metering_gain) * self.metering_release_k;
            }

            *ls = self.delay_l[self.read_idx] * total_gain;
            *rs = self.delay_r[self.read_idx] * total_gain;
            self.read_idx = (self.read_idx + 1) & PRE_DELAY_MASK;
            self.write_idx = (self.write_idx + 1) & PRE_DELAY_MASK;
        }
    }

    fn gr_db(&self) -> f32 {
        self.metering_gain.min(0.0)
    }

    fn reset(&mut self) {
        self.thresh.snap(self.thresh.target());
        self.ratio.snap(self.ratio.target());
        self.update_curve(self.thresh.target(), self.ratio.target());
        self.reset_kernel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn run_blocks(fx: &mut CompFx, l: &mut [f32], r: &mut [f32]) {
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
        let mut s = 0x2468aceu32;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1 << 24) as f32 - 0.5
            })
            .collect()
    }

    fn sine(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * libm::sinf(2.0 * core::f32::consts::PI * freq * i as f32 / SR))
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        libm::sqrtf(x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32)
    }

    #[test]
    fn two_seconds_no_nan_inf_and_non_silent() {
        let mut fx = CompFx::new(); // schema defaults
        fx.prepare(SR);
        let n = (2.0 * SR) as usize;
        let mut l = sine(n, 220.0, 0.9);
        let mut r = l.clone();
        run_blocks(&mut fx, &mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        assert!(rms(&l) > 0.05, "output silent (rms {})", rms(&l));
        assert!(fx.gr_db() <= 0.0 && fx.gr_db().is_finite());
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = CompFx::new();
        fx.prepare(SR);
        let src = noise(BLOCK);
        for thresh in [-60.0f32, -30.0, 0.0] {
            for ratio in [1.0f32, 4.0, 20.0] {
                for attack in [0.001f32, 0.05, 0.3] {
                    for release in [0.02f32, 0.3, 1.0] {
                        fx.set_param("thresh", thresh);
                        fx.set_param("ratio", ratio);
                        fx.set_param("attack", attack);
                        fx.set_param("release", release);
                        for _ in 0..20 {
                            let mut l = src.clone();
                            let mut r = src.clone();
                            fx.process(&mut l, &mut r);
                            let ok = l
                                .iter()
                                .chain(r.iter())
                                .all(|v| v.is_finite() && v.abs() < 20.0);
                            assert!(
                                ok,
                                "blew up at thresh={thresh} ratio={ratio} \
                                 attack={attack} release={release}"
                            );
                            assert!(fx.gr_db() <= 0.0 && fx.gr_db() > -120.0);
                        }
                    }
                }
            }
        }
        // Unknown keys are ignored.
        fx.set_param("nope", 42.0);
        let mut l = src.clone();
        let mut r = src.clone();
        fx.process(&mut l, &mut r);
        assert!(l.iter().all(|v| v.is_finite()));
    }

    /// Steady-state staging invariant of the browser kernel on a DC input at
    /// 0 dBFS: the detector settles at the static-curve attenuation `a`
    /// (= Saturate(1,k), the full-range gain), the metered reduction reads
    /// 20·log10(a), and the output gain is makeup·a = a^0.4 — i.e.
    /// out_db = 0.4 × gr_db. This pins the (1/fullRangeGain)^0.6 makeup law.
    #[test]
    fn dc_steady_state_obeys_makeup_power_law() {
        for ratio in [4.0f32, 20.0] {
            let mut fx = CompFx::new();
            fx.set_param("thresh", -20.0);
            fx.set_param("ratio", ratio);
            fx.set_param("attack", 0.005);
            fx.set_param("release", 0.1);
            fx.prepare(SR);

            let n = 2 * SR as usize; // 2 s — many attack time constants
            let mut l = vec![1.0f32; n];
            let mut r = vec![1.0f32; n];
            run_blocks(&mut fx, &mut l, &mut r);

            let out_db = lin_to_db(l[n - 1].abs());
            let gr = fx.gr_db();
            assert!(
                (out_db - 0.4 * gr).abs() < 0.15,
                "ratio {ratio}: out {out_db} dB vs 0.4*gr {}",
                0.4 * gr
            );
            // 0 dBFS sits inside the [−20, +10] dB knee, so the reduction is
            // real but MUCH softer than the hard-knee value −20·(1 − 1/ratio):
            // Blink's exponential knee at these params gives ≈ −2.4 dB
            // (ratio 4, k ≈ 0.74) and ≈ −4.4 dB (ratio 20, k ≈ 1.4).
            let hard_knee = -20.0 * (1.0 - 1.0 / ratio);
            assert!(
                gr < -2.0 && gr > hard_knee - 0.5,
                "ratio {ratio}: gr {gr} outside ({hard_knee}, -2)"
            );
        }
    }

    /// Higher ratio ⇒ deeper metered reduction on a real (sine) signal, and
    /// the net output still drops (makeup only refunds 60% of the reduction
    /// in dB terms).
    #[test]
    fn sine_reduction_increases_with_ratio() {
        let mut out_rms = [0.0f32; 2];
        let mut grs = [0.0f32; 2];
        for (i, ratio) in [2.0f32, 12.0].iter().enumerate() {
            let mut fx = CompFx::new();
            fx.set_param("thresh", -20.0);
            fx.set_param("ratio", *ratio);
            fx.prepare(SR);
            let n = (1.0 * SR) as usize;
            let mut l = sine(n, 220.0, 1.0);
            let mut r = l.clone();
            run_blocks(&mut fx, &mut l, &mut r);
            out_rms[i] = rms(&l[n / 2..]); // steady-state half
            grs[i] = fx.gr_db();
            assert!(fx.gr_db() < -1.0, "ratio {ratio}: no reduction reported");
        }
        assert!(
            grs[1] < grs[0] - 1.0,
            "ratio 12 gr {} not deeper than ratio 2 gr {}",
            grs[1],
            grs[0]
        );
        assert!(
            out_rms[1] < out_rms[0],
            "ratio 12 output {} not below ratio 2 {}",
            out_rms[1],
            out_rms[0]
        );
    }

    /// Below threshold the curve is linear (attenuation 1, gr ≈ 0) but the
    /// browser node still applies its fixed makeup gain, so quiet signals
    /// come out AMPLIFIED by exactly master_linear_gain — not at unity.
    #[test]
    fn below_threshold_gain_is_makeup_not_unity() {
        let mut fx = CompFx::new(); // thresh −20, ratio 4
        fx.prepare(SR);
        let makeup = fx.master_linear_gain;
        assert!(makeup > 1.0, "makeup {makeup} should exceed unity");
        let n = SR as usize;
        let mut l = vec![0.005f32; n]; // −46 dBFS, well below threshold
        let mut r = vec![-0.005f32; n];
        run_blocks(&mut fx, &mut l, &mut r);
        // Steady state: output = input · makeup (detector fully released).
        let got = l[n - 1] / 0.005;
        assert!(
            (got - makeup).abs() < 0.02 * makeup,
            "gain {got} vs makeup {makeup}"
        );
        assert!((r[n - 1] / -0.005 - makeup).abs() < 0.02 * makeup);
        assert!(fx.gr_db() > -0.5, "gr_db {} should be ~0", fx.gr_db());
    }

    /// The kernel's 6 ms lookahead: output is the input delayed by
    /// round(0.006·sr) samples (times the gain).
    #[test]
    fn output_is_pre_delayed_by_6ms() {
        let mut fx = CompFx::new();
        fx.prepare(SR);
        let pd = (PRE_DELAY_SEC * SR) as usize;
        let n = 4096;
        let mut l = vec![0.0f32; n];
        l[0] = 0.25; // single impulse
        let mut r = l.clone();
        run_blocks(&mut fx, &mut l, &mut r);
        let peak = l
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap()
            .0;
        assert_eq!(peak, pd, "impulse should emerge after {pd} samples");
        assert!(l[..pd].iter().all(|v| v.abs() < 1e-9));
    }

    /// Reset clears the envelope so the meter returns to 0.
    #[test]
    fn reset_clears_gain_reduction() {
        let mut fx = CompFx::new();
        fx.prepare(SR);
        let mut l = vec![1.0f32; 8192];
        let mut r = vec![1.0f32; 8192];
        run_blocks(&mut fx, &mut l, &mut r);
        assert!(fx.gr_db() < -1.0);
        fx.reset();
        assert_eq!(fx.gr_db(), 0.0);
    }
}
