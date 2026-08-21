//! `opto` effect — LA-2A-style optical compressor (devices.ts `OptoComp`):
//! soft-knee, gentle, program-dependent levelling, then makeup gain.
//!
//! The browser device is a native DynamicsCompressor with fixed curves per
//! mode — Comp: ratio 3, knee 30 dB, attack 10 ms, release 450 ms; Limit:
//! ratio 10, knee 12 dB, attack 5 ms, release 300 ms — and Peak Reduction
//! (0..1) lowering the threshold linearly to −40 dB. Schema
//! (src/audio/schema.ts "opto") has no mix param — the device is a serial
//! insert, 100% wet.
//!
//! The core is a faithful port of Blink's `DynamicsCompressor::Process`
//! (chromium `third_party/blink/renderer/platform/audio/dynamics_compressor.cc`,
//! which rendered the goldens):
//! - static curve: identity below threshold, Blink's exponential knee over
//!   [threshold, threshold + knee] (`KneeCurve` with `k` solved by
//!   `KAtSlope` binary search so the dB slope at the knee end is 1/ratio),
//!   then the straight 1/ratio segment (`Saturate`);
//! - detector (dc.cc `Process` inner loop): per-sample linear attenuation
//!   `Saturate(|x|)/|x|` on the stereo max, instant attack, 2.5 ms
//!   sat-release (`kSatReleaseTime`) — it recovers between the peaks of
//!   low-frequency material, which is part of the node's loudness;
//! - envelope (per 32-frame division): asin-prewarped desired gain,
//!   **adaptive release** — a constant dB-per-frame rate of 5 dB
//!   (`kDbSpacing`) per `y(x)` frames, `y` the 4th-order polynomial through
//!   the release zones 0.09/0.16/0.42/0.98 × release (dc.cc kReleaseZone*),
//!   so deep over-compression releases in tens of ms, not at the nominal
//!   release one-pole — and the attack law
//!   `rate = 1 − (0.25/diff_db)^(1/attack_frames)`;
//! - sin(π/2·g) post-warp of the gain (dc.cc line "post_warp"), a 6 ms
//!   lookahead pre-delay (`kPreDelay`: gain from the undelayed signal,
//!   applied to the delayed one), and the built-in makeup gain
//!   `(1/Saturate(1,k))^0.6` on top of the device's user makeup `gain`.
//!
//! On top of the kernel this port adds the opto-cell character the device
//! models: the T4 cell's memory makes release *two-stage and
//! program-dependent*. A slow envelope (~0.5 s charge, 6× the mode release)
//! tracks the kernel's applied gain reduction and is blended in at 50%: in
//! steady state it converges to the kernel GR (loudness parity with the
//! native node), while after sustained loud program its slow discharge drags
//! the second half of the release out over seconds — a brief peak barely
//! charges it, so recovery from transients stays quick.

use crate::traits::EffectDsp;
use crate::util::{db_to_lin, lin_to_db, lin_to_db_floor, Smoother};

/// Fixed gain-computer curves per mode (Comp = 0, Limit = 1), from OptoComp.
const RATIO: [f32; 2] = [3.0, 10.0];
const KNEE_DB: [f32; 2] = [30.0, 12.0];
const ATTACK_S: [f32; 2] = [0.010, 0.005];
const RELEASE_S: [f32; 2] = [0.45, 0.30];

/// Opto-cell memory: slow envelope charge time and release multiple.
const SLOW_ATTACK_S: f32 = 0.5;
const SLOW_RELEASE_MUL: f32 = 6.0;
/// Fast/slow stage blend (equal split: ~half the release recovers fast).
const STAGE_MIX: f32 = 0.5;

/// Detector floor — below this the gain computer sees silence.
const DET_FLOOR_DB: f32 = -96.0;

// ---- Blink DynamicsCompressor kernel constants (dynamics_compressor.cc). --

/// Detector release time (`kSatReleaseTime`).
const SAT_RELEASE_S: f32 = 0.0025;
/// Lookahead pre-delay (`kPreDelay`).
const PRE_DELAY_S: f32 = 0.006;
/// Envelope rates are recomputed every this many frames
/// (`kNumberOfDivisionFrames`).
const DIVISION_FRAMES: u32 = 32;
/// Adaptive release dB spacing (`kDbSpacing`).
const SPACING_DB: f32 = 5.0;
/// Release zone values 0 → 1 (`kReleaseZone1..4`).
const RELEASE_ZONES: [f32; 4] = [0.09, 0.16, 0.42, 0.98];

/// Blink's 4th-order polynomial fit through the four release-zone points
/// ((0,y1) (1,y2) (2,y3) (3,y4)) — the `kABase..kEBase` coefficient rows from
/// dynamics_compressor.cc (as their f32 values, which is what Blink's `float`
/// constants hold), applied to `release_frames × zones`.
fn release_poly(release_frames: f32) -> [f32; 5] {
    let [y1, y2, y3, y4] = RELEASE_ZONES.map(|z| release_frames * z);
    [
        1.0 * y1 + 1.843_221_9e-16 * y2 - 1.937_339_4e-16 * y3 + 8.824_516e-18 * y4,
        -1.578_832 * y1 + 2.330_583_8 * y2 - 0.914_119_4 * y3 + 0.162_367_75 * y4,
        0.533_414_3 * y1 - 1.272_736_8 * y2 + 0.925_885_6 * y3 - 0.186_563_1 * y4,
        0.087_834_634 * y1 - 0.169_416_3 * y2 + 0.085_880_58 * y3 - 0.004_298_914 * y4,
        -0.042_416_88 * y1 + 0.111_569_38 * y2 - 0.097_646_765 * y3 + 0.028_494_263 * y4,
    ]
}

/// One-pole coefficient for time constant `tau` seconds at `sr`.
#[inline]
fn tau_coeff(tau: f32, sr: f32) -> f32 {
    1.0 - libm::expf(-1.0 / (tau.max(1e-4) * sr))
}

// ---- Native DynamicsCompressor static curve, threshold-normalized: the
// curve in dB is a function of (level − threshold) only, so we solve it once
// per mode with the threshold at 0 dB (linear 1.0) and shift by the smoothed
// threshold at use.

/// Blink `KneeCurve` with `linear_threshold_` = 1: identity below the
/// threshold, exponential approach above it.
#[inline]
fn knee_curve(x: f32, k: f32) -> f32 {
    if x < 1.0 {
        x
    } else {
        1.0 + (1.0 - libm::expf(-k * (x - 1.0))) / k
    }
}

/// Blink `KAtSlope` slope probe: dB-domain slope of the knee curve at linear
/// level `x`.
fn knee_slope_at(x: f32, k: f32) -> f32 {
    if x < 1.0 {
        return 1.0;
    }
    let x2 = x * 1.001;
    let x_db = lin_to_db(x);
    let x2_db = lin_to_db(x2);
    let y_db = lin_to_db(knee_curve(x, k));
    let y2_db = lin_to_db(knee_curve(x2, k));
    (y2_db - y_db) / (x2_db - x_db)
}

/// Blink `KAtSlope`: binary-search `k` so the knee curve's slope at the knee
/// end (`knee_db` above threshold) is `desired_slope` (= 1/ratio).
fn k_at_slope(desired_slope: f32, knee_db: f32) -> f32 {
    let x = db_to_lin(knee_db);
    let mut min_k = 0.1f32;
    let mut max_k = 10_000.0f32;
    let mut k = 5.0f32;
    for _ in 0..15 {
        // A higher k gives a more compressed knee (smaller slope at the end).
        if knee_slope_at(x, k) < desired_slope {
            max_k = k;
        } else {
            min_k = k;
        }
        k = libm::sqrtf(min_k * max_k);
    }
    k
}

pub struct OptoFx {
    sample_rate: f32,
    mode: usize, // 0 = Comp, 1 = Limit (stepped, no smoothing)
    thresh_sm: Smoother, // threshold dB = −40 · reduction
    makeup_sm: Smoother, // linear makeup gain
    // Gain-computer constants for the current mode.
    slope: f32,     // 1/ratio − 1 (≤ 0)
    knee: f32,      // knee width in dB, spanning [threshold, threshold+knee]
    k_knee: f32,    // Blink exponential-knee shape parameter (KAtSlope)
    y_knee_db: f32, // curve output at the knee end, dB above threshold
    // Kernel coefficients (recomputed on prepare / mode change).
    inv_attack_frames: f32, // 1 / (attack seconds × sr)
    rel_poly: [f32; 5],     // adaptive-release polynomial a..e (in frames)
    sat_release_frames: f32,
    att_slow: f32,
    rel_slow: f32,
    // Kernel state (Blink member variables).
    det_avg: f32,             // `detector_average_`: linear attenuation ≤ 1
    comp_gain: f32,           // `compressor_gain_`: linear pre-warp gain ≤ 1
    max_attack_diff_db: f32,  // `db_max_attack_compression_diff_` (−1 uninit)
    env_rate: f32,            // per-division `envelope_rate`
    scaled_desired: f32,      // per-division asin-prewarped desired gain
    div_pos: u32,             // frames into the current division
    // Lookahead pre-delay ring (gain from undelayed, applied to delayed).
    delay_l: Vec<f32>,
    delay_r: Vec<f32>,
    delay_idx: usize,
    // Opto slow cell: GR envelope in dB (≤ 0) tracking the kernel GR.
    env_slow: f32,
    gr: f32, // blended GR for the meter
}

impl OptoFx {
    pub fn new() -> Self {
        let mut fx = Self {
            sample_rate: 44_100.0,
            mode: 0,
            thresh_sm: Smoother::new(-40.0 * 0.4), // schema defaults
            makeup_sm: Smoother::new(1.0),
            slope: 0.0,
            knee: 0.0,
            k_knee: 1.0,
            y_knee_db: 0.0,
            inv_attack_frames: 0.0,
            rel_poly: [0.0; 5],
            sat_release_frames: 1.0,
            att_slow: 0.0,
            rel_slow: 0.0,
            det_avg: 0.0,
            comp_gain: 1.0,
            max_attack_diff_db: -1.0,
            env_rate: 1.0,
            scaled_desired: 1.0,
            div_pos: 0,
            delay_l: Vec::new(),
            delay_r: Vec::new(),
            delay_idx: 0,
            env_slow: 0.0,
            gr: 0.0,
        };
        fx.update_mode();
        fx.resize_delay();
        fx
    }

    fn update_mode(&mut self) {
        let m = self.mode;
        self.slope = 1.0 / RATIO[m] - 1.0;
        self.knee = KNEE_DB[m];
        self.k_knee = k_at_slope(1.0 / RATIO[m], self.knee);
        self.y_knee_db = lin_to_db(knee_curve(db_to_lin(self.knee), self.k_knee));
        // Blink: attack_frames = max(0.001, attack) × sr.
        self.inv_attack_frames = 1.0 / (ATTACK_S[m].max(0.001) * self.sample_rate);
        self.rel_poly = release_poly(RELEASE_S[m] * self.sample_rate);
        self.sat_release_frames = SAT_RELEASE_S * self.sample_rate;
        self.att_slow = tau_coeff(SLOW_ATTACK_S, self.sample_rate);
        self.rel_slow = tau_coeff(RELEASE_S[m] * SLOW_RELEASE_MUL, self.sample_rate);
    }

    fn resize_delay(&mut self) {
        // Blink `SetPreDelayTime`: pre_delay_frames = kPreDelay × sr.
        let frames = ((PRE_DELAY_S * self.sample_rate) as usize).max(1);
        self.delay_l = vec![0.0; frames];
        self.delay_r = vec![0.0; frames];
        self.delay_idx = 0;
    }

    /// Native-curve gain computer: desired GR in dB (≤ 0) for a level `over`
    /// dB above threshold. Identity below the threshold, Blink's exponential
    /// knee over [0, knee], then the straight 1/ratio segment continuing from
    /// the knee end value (Blink `Saturate`).
    #[inline]
    fn gr_target(&self, over: f32) -> f32 {
        if over <= 0.0 {
            0.0
        } else if over < self.knee {
            let x = db_to_lin(over);
            lin_to_db(knee_curve(x, self.k_knee)) - over
        } else {
            self.y_knee_db - self.knee + self.slope * (over - self.knee)
        }
    }

    /// The native node's built-in makeup gain in dB (Blink
    /// `linear_post_gain`): linear `(1/Saturate(1,k))^0.6`, i.e. −0.6 × the
    /// static-curve GR at 0 dBFS.
    #[inline]
    fn auto_makeup_db(&self, thresh_db: f32) -> f32 {
        -0.6 * self.gr_target(-thresh_db)
    }

    /// Blink "Calculate desired gain / Deal with envelopes", run once per
    /// 32-frame division: pick the envelope slew rate for attack (approach
    /// the asin-prewarped desired gain) or release (constant dB/frame from
    /// the adaptive-release polynomial).
    fn compute_division_rates(&mut self) {
        let desired = self.det_avg.clamp(0.0, 1.0);
        // Pre-warp so we get desired_gain after the sin() warp.
        self.scaled_desired = libm::asinf(desired) / core::f32::consts::FRAC_PI_2;

        let is_releasing = self.scaled_desired > self.comp_gain;
        let mut diff_db = if self.scaled_desired == 0.0 {
            if is_releasing {
                -1.0
            } else {
                1.0
            }
        } else {
            lin_to_db(self.comp_gain / self.scaled_desired)
        };
        if !diff_db.is_finite() {
            diff_db = if is_releasing { -1.0 } else { 1.0 };
        }

        if is_releasing {
            // Adaptive release — deeper over-compression releases faster.
            self.max_attack_diff_db = -1.0;
            // Contain within −12..0, then scale to 0..3.
            let x = 0.25 * (diff_db.clamp(-12.0, 0.0) + 12.0);
            let [a, b, c, d, e] = self.rel_poly;
            let x2 = x * x;
            let release_frames = a + b * x + c * x2 + d * x2 * x + e * x2 * x2;
            self.env_rate = db_to_lin(SPACING_DB / release_frames);
        } else {
            // Attack — rate from the largest diff seen this attack.
            if self.max_attack_diff_db == -1.0 || self.max_attack_diff_db < diff_db {
                self.max_attack_diff_db = diff_db;
            }
            let eff_diff = self.max_attack_diff_db.max(0.5);
            self.env_rate = 1.0 - libm::powf(0.25 / eff_diff, self.inv_attack_frames);
        }
    }
}

impl Default for OptoFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for OptoFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.thresh_sm.prepare(self.sample_rate);
        self.makeup_sm.prepare(self.sample_rate);
        self.update_mode();
        self.resize_delay();
        self.det_avg = 0.0;
        self.comp_gain = 1.0;
        self.max_attack_diff_db = -1.0;
        self.env_rate = 1.0;
        self.scaled_desired = 1.0;
        self.div_pos = 0;
        self.env_slow = 0.0;
        self.gr = 0.0;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            // Peak Reduction lowers the threshold (0 → −40 dB), like the hardware.
            "reduction" => self.thresh_sm.set_target(-40.0 * value.clamp(0.0, 1.0)),
            "gain" => self.makeup_sm.set_target(db_to_lin(value.clamp(-12.0, 24.0))),
            "mode" => {
                let m = usize::from(value >= 0.5);
                if m != self.mode {
                    self.mode = m;
                    self.update_mode();
                }
            }
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        for (sl, sr) in l[..n].iter_mut().zip(r[..n].iter_mut()) {
            let thresh = self.thresh_sm.next();
            let makeup = self.makeup_sm.next();
            if self.div_pos == 0 {
                self.compute_division_rates();
            }
            self.div_pos = (self.div_pos + 1) % DIVISION_FRAMES;

            // Detector (Blink inner loop): linear attenuation of the stereo
            // peak through the static curve, instant attack, sat-release.
            let level = sl.abs().max(sr.abs());
            let (attenuation, attenuation_db) = if level <= 1e-4 {
                (1.0, 2.0)
            } else {
                let over = lin_to_db_floor(level, DET_FLOOR_DB) - thresh;
                let gr = self.gr_target(over);
                (db_to_lin(gr), (-gr).max(2.0))
            };
            if attenuation > self.det_avg {
                let sat_rate = db_to_lin(attenuation_db / self.sat_release_frames) - 1.0;
                self.det_avg += (attenuation - self.det_avg) * sat_rate;
                self.det_avg = self.det_avg.min(1.0);
            } else {
                self.det_avg = attenuation;
            }

            // Envelope: exponential approach on attack, constant-dB-rate
            // climb toward 1 on release.
            if self.env_rate < 1.0 {
                self.comp_gain += (self.scaled_desired - self.comp_gain) * self.env_rate;
            } else {
                self.comp_gain = (self.comp_gain * self.env_rate).min(1.0);
            }

            // Warp pre-compression gain to smooth the exponential
            // transitions; this is the kernel's applied gain reduction.
            let warped = libm::sinf(core::f32::consts::FRAC_PI_2 * self.comp_gain);
            let fast_gr = lin_to_db_floor(warped, DET_FLOOR_DB);

            // Opto slow cell: charges toward the kernel GR over ~0.5 s and
            // discharges at 6× the mode release — the program-dependent tail.
            if fast_gr < self.env_slow {
                self.env_slow += (fast_gr - self.env_slow) * self.att_slow;
            } else {
                self.env_slow += (fast_gr - self.env_slow) * self.rel_slow;
            }
            let gr = STAGE_MIX * fast_gr + (1.0 - STAGE_MIX) * self.env_slow;
            self.gr = gr;

            // Native built-in makeup on top of the device's user makeup gain,
            // applied to the pre-delayed (lookahead) signal.
            let g = db_to_lin(gr + self.auto_makeup_db(thresh)) * makeup;
            let (dl, dr) = (self.delay_l[self.delay_idx], self.delay_r[self.delay_idx]);
            self.delay_l[self.delay_idx] = *sl;
            self.delay_r[self.delay_idx] = *sr;
            self.delay_idx = (self.delay_idx + 1) % self.delay_l.len();
            *sl = dl * g;
            *sr = dr * g;
        }
    }

    fn gr_db(&self) -> f32 {
        self.gr
    }

    fn reset(&mut self) {
        self.det_avg = 0.0;
        self.comp_gain = 1.0;
        self.max_attack_diff_db = -1.0;
        self.env_rate = 1.0;
        self.scaled_desired = 1.0;
        self.div_pos = 0;
        self.env_slow = 0.0;
        self.gr = 0.0;
        self.delay_l.fill(0.0);
        self.delay_r.fill(0.0);
        self.delay_idx = 0;
        let t = self.thresh_sm.target();
        self.thresh_sm.snap(t);
        let m = self.makeup_sm.target();
        self.makeup_sm.snap(m);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::MAX_BLOCK;

    const SR: f32 = 48_000.0;

    /// Run a sine of `amp` for `n` samples in MAX_BLOCK chunks, recording
    /// gr_db() after each block. Returns (out_l, gr per block).
    fn run_sine(fx: &mut OptoFx, freq: f32, amp: f32, n: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0.0f32; n];
        for (i, s) in l.iter_mut().enumerate() {
            *s = amp * libm::sinf(core::f32::consts::TAU * freq * i as f32 / SR);
        }
        let mut r = l.clone();
        let mut grs = Vec::new();
        for (cl, cr) in l.chunks_mut(MAX_BLOCK).zip(r.chunks_mut(MAX_BLOCK)) {
            fx.process(cl, cr);
            grs.push(fx.gr_db());
        }
        (l, grs)
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn no_nan_inf_on_two_seconds() {
        let mut fx = OptoFx::new();
        fx.prepare(SR);
        fx.set_param("reduction", 0.8);
        fx.set_param("gain", 6.0);
        let (l, _) = run_sine(&mut fx, 220.0, 0.9, 2 * SR as usize);
        assert!(l.iter().all(|s| s.is_finite()));
        assert!(fx.gr_db().is_finite() && fx.gr_db() <= 0.0);
    }

    #[test]
    fn param_sweep_stays_bounded() {
        let mut fx = OptoFx::new();
        fx.prepare(SR);
        let mut phase = 0.0f32;
        for step in 0..200 {
            let t = step as f32 / 199.0;
            fx.set_param("reduction", t); // full 0..1
            fx.set_param("gain", -12.0 + 36.0 * t); // full −12..24
            fx.set_param("mode", f32::from(step % 2 == 0)); // toggle Comp/Limit
            let mut l = [0.0f32; MAX_BLOCK];
            for s in l.iter_mut() {
                phase += core::f32::consts::TAU * 440.0 / SR;
                *s = 1.5 * libm::sinf(phase); // hotter than 0 dBFS on purpose
            }
            let mut r = l;
            fx.process(&mut l, &mut r);
            for s in l.iter().chain(r.iter()) {
                // +24 dB makeup on a 1.5 peak is ≈ 24× worst case pre-GR.
                assert!(s.is_finite() && s.abs() < 40.0, "unbounded: {s}");
            }
            assert!(fx.gr_db().is_finite() && fx.gr_db() <= 0.0);
        }
    }

    #[test]
    fn output_is_not_silent_at_defaults() {
        let mut fx = OptoFx::new();
        fx.prepare(SR);
        let (l, _) = run_sine(&mut fx, 220.0, 0.5, SR as usize / 2);
        assert!(rms(&l) > 0.01, "output silent: rms = {}", rms(&l));
    }

    #[test]
    fn compresses_hot_input_and_reports_gr() {
        let mut fx = OptoFx::new();
        fx.prepare(SR);
        fx.set_param("reduction", 1.0); // threshold −40 dB
        fx.set_param("gain", 0.0);
        fx.reset();
        let n = 2 * SR as usize;
        let (l, _) = run_sine(&mut fx, 220.0, 1.0, n);
        // Comp mode, peak 0 dBFS → 40 dB over: GR past the knee end. The
        // native curve targets ≈ −16 dB here; the kernel detector recovers a
        // little between sine peaks, so the settled GR is somewhat shallower.
        assert!(fx.gr_db() < -9.0, "expected deep GR, got {}", fx.gr_db());
        // Output settles below input level, net of the native node's built-in
        // makeup gain (−0.6 × GR at 0 dBFS ≈ +9.7 dB).
        let tail = &l[n / 2..];
        let in_rms = 1.0 / core::f32::consts::SQRT_2;
        assert!(rms(tail) < in_rms as f64 * 0.8, "not compressing: {}", rms(tail));
        // …and the GR is genuinely deeper than the makeup restores at higher
        // drive: the same signal through Limit mode nets a real level drop.
        let mut lim = OptoFx::new();
        lim.prepare(SR);
        lim.set_param("reduction", 1.0);
        lim.set_param("mode", 1.0);
        lim.set_param("gain", 0.0);
        lim.reset();
        let (ll, _) = run_sine(&mut lim, 220.0, 0.5, n);
        assert!(
            rms(&ll[n / 2..]) < in_rms as f64 * 0.5,
            "Limit not compressing: {}",
            rms(&ll[n / 2..])
        );
    }

    /// Analytic check: GR onset (compression) is fast, recovery on a level
    /// drop is much slower — and the release is two-stage (a slow opto tail
    /// remains after the fast stage would have finished).
    #[test]
    fn release_is_slower_than_compression_and_two_stage() {
        let mut fx = OptoFx::new();
        fx.prepare(SR);
        fx.set_param("reduction", 1.0);
        fx.reset();
        let block_s = MAX_BLOCK as f32 / SR;

        // 3 s hot: fully charges both cells → steady-state GR.
        let (_, gr_on) = run_sine(&mut fx, 220.0, 0.5, 3 * SR as usize);
        let steady = *gr_on.last().unwrap();
        assert!(steady < -6.0, "no steady GR: {steady}");
        // Onset: blocks until 63% of steady depth.
        let onset_blocks = gr_on.iter().position(|g| *g <= steady * 0.63).unwrap();
        let onset_s = onset_blocks as f32 * block_s;
        assert!(onset_s < 0.5, "onset too slow: {onset_s} s");

        // Drop to −60 dBFS (below threshold): watch the recovery.
        let (_, gr_off) = run_sine(&mut fx, 220.0, 0.001, 5 * SR as usize);
        let depth_at = |t_s: f32| gr_off[(t_s / block_s) as usize] / steady;
        // 63% recovery takes several times longer than the 63% onset.
        let rec63_blocks = gr_off
            .iter()
            .position(|g| *g >= steady * 0.37)
            .expect("never recovered");
        let rec63_s = rec63_blocks as f32 * block_s;
        assert!(
            rec63_s > 3.0 * onset_s.max(block_s),
            "release not slower than compression: onset {onset_s} s, recovery {rec63_s} s"
        );
        // Two-stage: at t = τ_fast (0.45 s) the fast stage has clearly acted
        // (a slow-only release would still hold ~85% depth) …
        assert!(depth_at(0.45) < 0.75, "fast stage missing: {}", depth_at(0.45));
        // … yet at 3·τ_fast a slow tail remains (a single fast exponential
        // would be down to ~5% depth).
        assert!(depth_at(1.35) > 0.15, "slow tail missing: {}", depth_at(1.35));
    }

    /// Program dependence: recovery after a brief peak is faster than after
    /// sustained loud program (the slow cell barely charges on a transient).
    #[test]
    fn release_is_program_dependent() {
        let recovered_depth_after = |hot_s: f32| -> f32 {
            let mut fx = OptoFx::new();
            fx.prepare(SR);
            fx.set_param("reduction", 1.0);
            fx.reset();
            let (_, gr_on) = run_sine(&mut fx, 220.0, 0.5, (hot_s * SR) as usize);
            let depth = *gr_on.last().unwrap();
            // 0.6 s after the drop, how much of that depth is left?
            let (_, gr_off) = run_sine(&mut fx, 220.0, 0.001, (0.6 * SR) as usize);
            gr_off.last().unwrap() / depth
        };
        let brief = recovered_depth_after(0.05);
        let sustained = recovered_depth_after(3.0);
        assert!(
            sustained > brief + 0.05,
            "release not program-dependent: brief {brief}, sustained {sustained}"
        );
    }

    #[test]
    fn makeup_gain_and_mode_switch_apply() {
        // Quiet signal far below threshold: device ≈ pure makeup gain.
        let mut fx = OptoFx::new();
        fx.prepare(SR);
        fx.set_param("reduction", 0.0); // threshold 0 dB — nothing compresses
        fx.set_param("gain", 12.0);
        fx.reset();
        // Settle: the native kernel starts with detector_average = 0 (Blink
        // Reset()), so even a quiet signal dips at t=0 and recovers at the
        // shallow-compression adaptive-release rate (~11 dB/s).
        run_sine(&mut fx, 220.0, 0.1, SR as usize); // settle smoothers + kernel
        let (l, _) = run_sine(&mut fx, 220.0, 0.1, SR as usize / 4);
        let expect = 0.1 / core::f32::consts::SQRT_2 * db_to_lin(12.0);
        assert!(
            (rms(&l) - expect as f64).abs() < expect as f64 * 0.05,
            "makeup off: rms {} vs {expect}",
            rms(&l)
        );
        // Limit mode bites harder than Comp on the same hot signal.
        let gr_for_mode = |mode: f32| -> f32 {
            let mut fx = OptoFx::new();
            fx.prepare(SR);
            fx.set_param("reduction", 1.0);
            fx.set_param("mode", mode);
            fx.reset();
            run_sine(&mut fx, 220.0, 0.5, 2 * SR as usize);
            fx.gr_db()
        };
        let comp = gr_for_mode(0.0);
        let limit = gr_for_mode(1.0);
        assert!(limit < comp - 3.0, "Limit not harder than Comp: {limit} vs {comp}");
    }
}
