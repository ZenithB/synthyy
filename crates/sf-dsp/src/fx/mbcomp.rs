//! `mbcomp` effect — line-for-line port of the `sf-mbdyn` AudioWorklet
//! (public/sf-mbdyn.js), the canonical DSP for this device.
//!
//! Signal: 3-band Linkwitz-Riley (LR4) crossover → per-band compressor OR
//! downward-expander → makeup → sum. LR4 = two cascaded Butterworth (Q=1/√2)
//! RBJ biquads per slope; adjacent LR4 bands sum flat. Per-band gain reduction
//! is exposed through `gr_bands()` (dB ≤ 0, windowed max like the worklet's
//! ~30 ms meter posts).
//!
//! PARITY NOTE (fx-mbcomp golden, 2026-08): the checked-in golden WAV was
//! rendered with the worklet degraded to a PASSTHROUGH — render.ts caps the
//! offline `audioWorklet.addModule('/sf-mbdyn.js')` at 1.5 s and on timeout
//! "mbcomp → passthrough" (src/audio/render.ts, registerOfflineWorklets
//! comment). Evidence: the golden's per-band RMS and 100 ms band envelopes
//! track a dry render within the same ~0.6 dB offset inst-mono shows, with
//! no trace of the fixture's 8:1/−45 dB low-band GR (~28 dB at peaks) or its
//! +6 dB high-band makeup. An exact float64 port of public/sf-mbdyn.js fed
//! the dry render reproduces THIS implementation within ~0.5 dB. The ≈−8 dB
//! ΔRMS in docs/PARITY.md is therefore a golden pathology (regenerate with
//! the worklet actually registered), not a gain-staging bug here.
//!
//! Schema (schema.ts "mbcomp"): mode 0/1 (Comp/Expand, stepped), xlo
//! 60..1000 Hz (def 250), xhi 1000..12000 Hz (def 2500), attack 0.001..0.3 s
//! (def 0.02), release 0.02..1 s (def 0.18), per band bN_thresh −60..0 dB
//! (def −24), bN_ratio 1..20 (def 2), bN_gain −24..24 dB (def 0). No mix
//! param — the insert is fully wet — and no tempo-synced rate, so `tick` is
//! the trait default.

use crate::traits::EffectDsp;
use crate::util::{db_to_lin, Smoother};

/// ln(10)/20 — the worklet's `Math.exp(-grDb * 0.11512925)` = 10^(−grDb/20).
const DB_TO_NAT: f32 = 0.115_129_25;

/// Direct-Form-I biquad with its own state (one per channel per filter slot).
#[derive(Clone, Copy)]
struct Biquad {
    x1: f32,
    x2: f32,
    y1: f32,
    y2: f32,
    c: [f32; 5],
}

impl Biquad {
    const fn new() -> Self {
        Self {
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
            c: [1.0, 0.0, 0.0, 0.0, 0.0],
        }
    }

    #[inline]
    fn set(&mut self, c: [f32; 5]) {
        self.c = c;
    }

    fn reset_state(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }

    #[inline]
    fn proc(&mut self, x: f32) -> f32 {
        let c = &self.c;
        let y = c[0] * x + c[1] * self.x1 + c[2] * self.x2 - c[3] * self.y1 - c[4] * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// RBJ cookbook lowpass, normalized by a0, Butterworth Q (LR4 building block).
fn lp_coeffs(f: f32, sr: f32) -> [f32; 5] {
    let w0 = 2.0 * core::f32::consts::PI * f.min(sr * 0.49) / sr;
    let cw = libm::cosf(w0);
    let sw = libm::sinf(w0);
    let alpha = sw / (2.0 * core::f32::consts::FRAC_1_SQRT_2);
    let a0 = 1.0 + alpha;
    [
        ((1.0 - cw) * 0.5) / a0,
        (1.0 - cw) / a0,
        ((1.0 - cw) * 0.5) / a0,
        (-2.0 * cw) / a0,
        (1.0 - alpha) / a0,
    ]
}

/// RBJ cookbook highpass, normalized by a0, Butterworth Q.
fn hp_coeffs(f: f32, sr: f32) -> [f32; 5] {
    let w0 = 2.0 * core::f32::consts::PI * f.min(sr * 0.49) / sr;
    let cw = libm::cosf(w0);
    let sw = libm::sinf(w0);
    let alpha = sw / (2.0 * core::f32::consts::FRAC_1_SQRT_2);
    let a0 = 1.0 + alpha;
    [
        ((1.0 + cw) * 0.5) / a0,
        (-(1.0 + cw)) / a0,
        ((1.0 + cw) * 0.5) / a0,
        (-2.0 * cw) / a0,
        (1.0 - alpha) / a0,
    ]
}

pub struct MbcompFx {
    sample_rate: f32,
    /// 0 = compress, 1 = downward expand (stepped switch, no smoothing).
    mode: i32,
    /// Crossover frequencies in Hz (audible, smoothed ~15 ms; biquad
    /// coefficients are re-derived from the smoothed value at block rate,
    /// matching the worklet's block-boundary `recompute`).
    xlo: Smoother,
    xhi: Smoother,
    /// Attack/release retune the envelope coefficients (a smoothing process
    /// themselves), so no extra Smoother — same as the comp device.
    attack_sec: f32,
    release_sec: f32,
    att_c: f32,
    rel_c: f32,
    /// Per-band threshold in dB and ratio (audible, smoothed ~15 ms).
    thresh: [Smoother; 3],
    ratio: [Smoother; 3],
    /// Per-band makeup as linear gain (audible, smoothed ~15 ms).
    makeup: [Smoother; 3],
    /// bq[ch] = [LPa,LPb (xlo low), HPa,HPb (xlo high), LPc,LPd (xhi mid),
    /// HPc,HPd (xhi high)] — worklet slot layout.
    bq: [[Biquad; 8]; 2],
    /// Per-band level detector state (linear, stereo-linked peak).
    env: [f32; 3],
    /// Crossover values the current coefficients were derived from.
    applied_xlo: f32,
    applied_xhi: f32,
    /// Windowed GR meter: max positive dB since the last report rollover
    /// (mirrors the worklet's ~30 ms `postMessage` cadence).
    gr_max: [f32; 3],
    gr_report: [f32; 3],
    rep: usize,
    rep_every: usize,
}

impl MbcompFx {
    pub fn new() -> Self {
        let mut fx = Self {
            sample_rate: 44_100.0,
            mode: 0,
            xlo: Smoother::new(250.0),
            xhi: Smoother::new(2500.0),
            attack_sec: 0.02,
            release_sec: 0.18,
            att_c: 0.0,
            rel_c: 0.0,
            thresh: [
                Smoother::new(-24.0),
                Smoother::new(-24.0),
                Smoother::new(-24.0),
            ],
            ratio: [Smoother::new(2.0), Smoother::new(2.0), Smoother::new(2.0)],
            makeup: [Smoother::new(1.0), Smoother::new(1.0), Smoother::new(1.0)],
            bq: [[Biquad::new(); 8]; 2],
            env: [0.0; 3],
            applied_xlo: 0.0,
            applied_xhi: 0.0,
            gr_max: [0.0; 3],
            gr_report: [0.0; 3],
            rep: 0,
            rep_every: 1323, // max(256, 44100 * 0.03)
        };
        fx.update_env_coeffs();
        fx.recompute_filters(250.0, 2500.0);
        fx
    }

    fn update_env_coeffs(&mut self) {
        self.att_c = libm::expf(-1.0 / (self.attack_sec.max(0.0005) * self.sample_rate));
        self.rel_c = libm::expf(-1.0 / (self.release_sec.max(0.005) * self.sample_rate));
    }

    fn recompute_filters(&mut self, xlo: f32, xhi: f32) {
        let lo = xlo.min(xhi - 20.0); // keep crossovers ordered
        let hi = xhi.max(xlo + 20.0);
        let lp = lp_coeffs(lo, self.sample_rate);
        let hp = hp_coeffs(lo, self.sample_rate);
        let lp2 = lp_coeffs(hi, self.sample_rate);
        let hp2 = hp_coeffs(hi, self.sample_rate);
        for ch in &mut self.bq {
            ch[0].set(lp);
            ch[1].set(lp);
            ch[2].set(hp);
            ch[3].set(hp);
            ch[4].set(lp2);
            ch[5].set(lp2);
            ch[6].set(hp2);
            ch[7].set(hp2);
        }
        self.applied_xlo = xlo;
        self.applied_xhi = xhi;
    }

    /// Two cascaded LR4 splits: input → (low, mid, high) for one channel.
    #[inline]
    fn split(bank: &mut [Biquad; 8], x: f32) -> (f32, f32, f32) {
        let lo = {
            let a = bank[0].proc(x);
            bank[1].proc(a)
        };
        let h = {
            let a = bank[2].proc(x);
            bank[3].proc(a)
        };
        let mid = {
            let a = bank[4].proc(h);
            bank[5].proc(a)
        };
        let hi = {
            let a = bank[6].proc(h);
            bank[7].proc(a)
        };
        (lo, mid, hi)
    }
}

impl Default for MbcompFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for MbcompFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.rep_every = (self.sample_rate * 0.03).max(256.0) as usize;
        self.xlo.prepare(self.sample_rate);
        self.xhi.prepare(self.sample_rate);
        self.xlo.snap(self.xlo.target());
        self.xhi.snap(self.xhi.target());
        for b in 0..3 {
            self.thresh[b].prepare(self.sample_rate);
            self.ratio[b].prepare(self.sample_rate);
            self.makeup[b].prepare(self.sample_rate);
            self.thresh[b].snap(self.thresh[b].target());
            self.ratio[b].snap(self.ratio[b].target());
            self.makeup[b].snap(self.makeup[b].target());
        }
        self.update_env_coeffs();
        self.recompute_filters(self.xlo.current(), self.xhi.current());
        self.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "mode" => self.mode = value as i32,
            "xlo" => self.xlo.set_target(value.clamp(60.0, 1000.0)),
            "xhi" => self.xhi.set_target(value.clamp(1000.0, 12_000.0)),
            "attack" => {
                self.attack_sec = value.clamp(0.001, 0.3);
                self.update_env_coeffs();
            }
            "release" => {
                self.release_sec = value.clamp(0.02, 1.0);
                self.update_env_coeffs();
            }
            "b0_thresh" => self.thresh[0].set_target(value.clamp(-60.0, 0.0)),
            "b1_thresh" => self.thresh[1].set_target(value.clamp(-60.0, 0.0)),
            "b2_thresh" => self.thresh[2].set_target(value.clamp(-60.0, 0.0)),
            "b0_ratio" => self.ratio[0].set_target(value.clamp(1.0, 20.0)),
            "b1_ratio" => self.ratio[1].set_target(value.clamp(1.0, 20.0)),
            "b2_ratio" => self.ratio[2].set_target(value.clamp(1.0, 20.0)),
            "b0_gain" => self.makeup[0].set_target(db_to_lin(value.clamp(-24.0, 24.0))),
            "b1_gain" => self.makeup[1].set_target(db_to_lin(value.clamp(-24.0, 24.0))),
            "b2_gain" => self.makeup[2].set_target(db_to_lin(value.clamp(-24.0, 24.0))),
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        // Crossover coefficients follow the smoothed values at block rate —
        // the worklet likewise re-derives them at block boundaries only.
        let cur_lo = self.xlo.current();
        let cur_hi = self.xhi.current();
        if (cur_lo - self.applied_xlo).abs() > 0.01 || (cur_hi - self.applied_xhi).abs() > 0.01 {
            self.recompute_filters(cur_lo, cur_hi);
        }
        let comp = self.mode == 0;
        for (ls, rs) in l[..n].iter_mut().zip(r[..n].iter_mut()) {
            self.xlo.next();
            self.xhi.next();
            let th = [
                self.thresh[0].next(),
                self.thresh[1].next(),
                self.thresh[2].next(),
            ];
            let ra = [
                self.ratio[0].next(),
                self.ratio[1].next(),
                self.ratio[2].next(),
            ];
            let mk = [
                self.makeup[0].next(),
                self.makeup[1].next(),
                self.makeup[2].next(),
            ];
            let (l0, l1, l2) = Self::split(&mut self.bq[0], *ls);
            let (r0, r1, r2) = Self::split(&mut self.bq[1], *rs);
            let bl = [l0, l1, l2];
            let br = [r0, r1, r2];
            let mut sl = 0.0f32;
            let mut sr = 0.0f32;
            // band 0 = low, 1 = mid, 2 = high
            for b in 0..3 {
                let rect = bl[b].abs().max(br[b].abs());
                let c = if rect > self.env[b] {
                    self.att_c
                } else {
                    self.rel_c
                };
                self.env[b] = rect + (self.env[b] - rect) * c;
                let lvl_db = 20.0 * libm::log10f(self.env[b] + 1e-9);
                let ratio = ra[b].max(1.0);
                let mut gr_db = 0.0f32;
                if comp {
                    let over = lvl_db - th[b];
                    if over > 0.0 {
                        gr_db = over * (1.0 - 1.0 / ratio);
                    }
                } else {
                    let under = th[b] - lvl_db;
                    if under > 0.0 {
                        gr_db = (under * (ratio - 1.0)).min(48.0);
                    }
                }
                if gr_db > self.gr_max[b] {
                    self.gr_max[b] = gr_db;
                }
                let gain = libm::expf(-gr_db * DB_TO_NAT) * mk[b]; // 10^(−grDb/20)·makeup
                sl += bl[b] * gain;
                sr += br[b] * gain;
            }
            *ls = sl;
            *rs = sr;
        }
        self.rep += n;
        if self.rep >= self.rep_every {
            self.rep = 0;
            self.gr_report = self.gr_max;
            self.gr_max = [0.0; 3];
        }
    }

    /// Per-band gain reduction, dB ≤ 0 (the worklet posts positive dB of
    /// reduction; devices.ts negates it for the meters — done here).
    fn gr_bands(&self) -> [f32; 3] {
        [
            -self.gr_report[0],
            -self.gr_report[1],
            -self.gr_report[2],
        ]
    }

    fn reset(&mut self) {
        self.xlo.snap(self.xlo.target());
        self.xhi.snap(self.xhi.target());
        for b in 0..3 {
            self.thresh[b].snap(self.thresh[b].target());
            self.ratio[b].snap(self.ratio[b].target());
            self.makeup[b].snap(self.makeup[b].target());
        }
        for ch in &mut self.bq {
            for q in ch.iter_mut() {
                q.reset_state();
            }
        }
        self.recompute_filters(self.xlo.current(), self.xhi.current());
        self.env = [0.0; 3];
        self.gr_max = [0.0; 3];
        self.gr_report = [0.0; 3];
        self.rep = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn run_blocks(fx: &mut MbcompFx, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        let mut i = 0;
        while i < n {
            let end = (i + BLOCK).min(n);
            fx.process(&mut l[i..end], &mut r[i..end]);
            i = end;
        }
    }

    fn sine(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * libm::sinf(2.0 * core::f32::consts::PI * freq * i as f32 / SR))
            .collect()
    }

    /// Deterministic pseudo-noise in [-0.5, 0.5].
    fn noise(n: usize) -> Vec<f32> {
        let mut s = 0x13579bdu32;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1 << 24) as f32 - 0.5
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        libm::sqrtf(x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32)
    }

    #[test]
    fn two_seconds_no_nan_inf_and_non_silent() {
        let mut fx = MbcompFx::new(); // schema defaults
        fx.prepare(SR);
        let n = (2.0 * SR) as usize;
        let mut l = sine(n, 220.0, 0.9);
        let mut r = l.clone();
        run_blocks(&mut fx, &mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        assert!(rms(&l) > 0.05, "wet output silent (rms {})", rms(&l));
        let gr = fx.gr_bands();
        assert!(gr.iter().all(|g| g.is_finite() && *g <= 0.0));
        // 220 Hz at −0.9 dBFS is 23 dB over the −24 dB low threshold.
        assert!(gr[0] < -1.0, "low band should report reduction: {gr:?}");
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = MbcompFx::new();
        fx.prepare(SR);
        let src = noise(BLOCK);
        for mode in [0.0f32, 1.0] {
            for (xlo, xhi) in [(60.0f32, 1000.0f32), (250.0, 2500.0), (1000.0, 12_000.0)] {
                for (attack, release) in [(0.001f32, 0.02f32), (0.3, 1.0)] {
                    for (th, ra, mk) in [(-60.0f32, 20.0f32, 24.0f32), (0.0, 1.0, -24.0)] {
                        fx.set_param("mode", mode);
                        fx.set_param("xlo", xlo);
                        fx.set_param("xhi", xhi);
                        fx.set_param("attack", attack);
                        fx.set_param("release", release);
                        for b in 0..3 {
                            fx.set_param(&format!("b{b}_thresh"), th);
                            fx.set_param(&format!("b{b}_ratio"), ra);
                            fx.set_param(&format!("b{b}_gain"), mk);
                        }
                        for _ in 0..20 {
                            let mut l = src.clone();
                            let mut r = src.clone();
                            fx.process(&mut l, &mut r);
                            let ok = l
                                .iter()
                                .chain(r.iter())
                                .all(|v| v.is_finite() && v.abs() < 100.0);
                            assert!(
                                ok,
                                "blew up at mode={mode} xlo={xlo} xhi={xhi} \
                                 attack={attack} release={release} th={th} ra={ra} mk={mk}"
                            );
                            // Expander GR caps at 48 dB; comp GR is over·(1−1/ratio),
                            // bounded here by level ≥ −60 dBFS vs thresh ≥ −60 dB.
                            let gr = fx.gr_bands();
                            assert!(gr.iter().all(|g| g.is_finite() && *g <= 0.0 && *g >= -80.0));
                        }
                    }
                }
            }
        }
        // Unknown keys are ignored.
        fx.set_param("nope", 42.0);
        let mut l = src.clone();
        let mut r = src;
        fx.process(&mut l, &mut r);
        assert!(l.iter().all(|v| v.is_finite()));
    }

    /// Analytic check: a band-limited loud signal only reduces its own band,
    /// and gr_bands report independently. 100 Hz sits in the low band
    /// (crossovers 250/2500); LR4 leakage into mid is ~30 dB down, below the
    /// −24 dB threshold, so mid/high must show no reduction — and vice versa
    /// for a 6 kHz tone in the high band.
    #[test]
    fn band_limited_signal_only_reduces_its_own_band() {
        for (freq, hot) in [(100.0f32, 0usize), (6000.0, 2)] {
            let mut fx = MbcompFx::new(); // thresholds −24 dB, xlo 250, xhi 2500
            for b in 0..3 {
                fx.set_param(&format!("b{b}_ratio"), 8.0);
            }
            fx.set_param("attack", 0.005);
            fx.set_param("release", 0.1);
            fx.prepare(SR);

            let n = SR as usize; // 1 s
            let dry = sine(n, freq, 0.9);
            let mut l = dry.clone();
            let mut r = dry.clone();
            run_blocks(&mut fx, &mut l, &mut r);

            let gr = fx.gr_bands();
            assert!(gr[hot] < -6.0, "{freq} Hz: band {hot} not reduced: {gr:?}");
            for (b, g) in gr.iter().enumerate() {
                if b != hot {
                    assert!(*g > -1.0, "{freq} Hz: band {b} should be untouched: {gr:?}");
                }
            }
            // The signal's band was compressed, so the wet output is quieter.
            let steady = n / 2..;
            assert!(
                rms(&l[steady.clone()]) < rms(&dry[steady]) * 0.9,
                "{freq} Hz: output not attenuated"
            );
        }
    }

    /// With ratio 1 everywhere nothing compresses: the LR4 bands sum back to
    /// (allpassed) unity, so a mid-band sine passes at its input level.
    #[test]
    fn ratio_one_bands_sum_to_unity() {
        let mut fx = MbcompFx::new();
        for b in 0..3 {
            fx.set_param(&format!("b{b}_ratio"), 1.0);
        }
        fx.prepare(SR);
        let n = SR as usize;
        let dry = sine(n, 1000.0, 0.5);
        let mut l = dry.clone();
        let mut r = dry.clone();
        run_blocks(&mut fx, &mut l, &mut r);
        let steady = n / 2..;
        let out = rms(&l[steady.clone()]);
        let inp = rms(&dry[steady]);
        assert!(
            (out - inp).abs() < inp * 0.12,
            "LR4 sum not ~flat: out {out} vs in {inp}"
        );
        assert!(fx.gr_bands().iter().all(|g| *g > -1e-3));
    }

    /// Expand mode: a quiet signal below threshold is pushed further down.
    #[test]
    fn expander_attenuates_below_threshold() {
        let mut fx = MbcompFx::new(); // thresholds −24 dB
        fx.set_param("mode", 1.0);
        fx.set_param("b0_ratio", 2.0);
        fx.prepare(SR);
        let n = SR as usize;
        let dry = sine(n, 100.0, 0.01); // −40 dBFS, 16 dB under threshold
        let mut l = dry.clone();
        let mut r = dry.clone();
        run_blocks(&mut fx, &mut l, &mut r);
        let steady = n / 2..;
        assert!(
            rms(&l[steady.clone()]) < rms(&dry[steady]) * 0.5,
            "expander did not attenuate"
        );
        let gr = fx.gr_bands();
        assert!(gr[0] < -6.0, "low band GR missing: {gr:?}");
    }

    /// Reset clears the detector and meter state.
    #[test]
    fn reset_clears_gain_reduction() {
        let mut fx = MbcompFx::new();
        fx.prepare(SR);
        let mut l = sine(8192, 220.0, 0.9);
        let mut r = l.clone();
        run_blocks(&mut fx, &mut l, &mut r);
        assert!(fx.gr_bands()[0] < -1.0);
        fx.reset();
        assert_eq!(fx.gr_bands(), [0.0; 3]);
    }
}
