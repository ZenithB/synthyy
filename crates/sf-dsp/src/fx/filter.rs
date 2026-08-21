//! `filter` effect — port of the browser Tone.Filter device (devices.ts
//! `makeEffect("filter")`): a biquad-style LP/HP/BP filter whose `slope`
//! param picks a -12/-24/-48 dB/oct rolloff by cascading 1/2/4 identical
//! two-pole sections, all sharing the same frequency and Q. The schema
//! exposes no wet/dry mix — the device is fully wet, like the browser node.

use crate::traits::EffectDsp;
use crate::util::{Smoother, Svf, SvfMode};

/// Max cascaded two-pole sections (slope index 2 = 48 dB/oct).
const MAX_STAGES: usize = 4;

/// Schema ranges (src/audio/schema.ts, FROZEN).
const FREQ_MIN: f32 = 40.0;
const FREQ_MAX: f32 = 18_000.0;
const Q_MIN: f32 = 0.0;
const Q_MAX: f32 = 12.0;

/// slope index -> number of cascaded two-pole sections (12/24/48 dB/oct).
fn stages_for_slope(slope: i32) -> usize {
    match slope {
        s if s <= 0 => 1,
        1 => 2,
        _ => 4,
    }
}

fn mode_for_ftype(ftype: i32) -> SvfMode {
    match ftype {
        1 => SvfMode::Highpass,
        2 => SvfMode::Bandpass,
        _ => SvfMode::Lowpass,
    }
}

/// The browser device writes the raw schema `q` (0..12) into BiquadFilterNode.Q.
/// Web Audio interprets Q in dB for lowpass/highpass and as a dimensionless
/// bandwidth Q for bandpass — mirror that so the resonance character matches.
fn linear_q(q_param: f32, mode: SvfMode) -> f32 {
    match mode {
        SvfMode::Bandpass => q_param.max(0.025),
        _ => libm::powf(10.0, q_param * (1.0 / 20.0)),
    }
}

pub struct FilterFx {
    stages: [[Svf; MAX_STAGES]; 2], // [channel][stage], identical coefficients
    n_stages: usize,
    mode: SvfMode,
    freq_sm: Smoother,
    q_sm: Smoother,
    // last coefficient set actually applied to the SVFs, to skip redundant
    // per-sample tan() updates once the smoothers have settled
    applied_freq: f32,
    applied_q: f32,
    // Svf's bandpass tap is the constant-skirt form (center gain = Q); Web
    // Audio's bandpass biquad is unity-gain at center, so scale each BP
    // stage by 1/Q. Unity for LP/HP.
    stage_gain: f32,
}

impl FilterFx {
    pub fn new() -> Self {
        let mode = SvfMode::Lowpass;
        let mut fx = Self {
            stages: core::array::from_fn(|_| core::array::from_fn(|_| Svf::new(mode))),
            n_stages: 1,           // schema def slope = 0 (12 dB)
            mode,                  // schema def ftype = 0 (LP)
            freq_sm: Smoother::new(2_000.0), // schema def freq
            q_sm: Smoother::new(1.0),        // schema def q
            applied_freq: f32::NAN,
            applied_q: f32::NAN,
            stage_gain: 1.0,
        };
        fx.apply_coeffs(fx.freq_sm.current(), fx.q_sm.current());
        fx
    }

    fn apply_coeffs(&mut self, freq: f32, q_param: f32) {
        let q = linear_q(q_param, self.mode);
        self.stage_gain = match self.mode {
            SvfMode::Bandpass => 1.0 / q,
            _ => 1.0,
        };
        for ch in &mut self.stages {
            for svf in ch.iter_mut() {
                svf.set_mode(self.mode);
                svf.set_cutoff_q(freq, q);
            }
        }
        self.applied_freq = freq;
        self.applied_q = q_param;
    }
}

impl Default for FilterFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for FilterFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.freq_sm.prepare(sample_rate);
        self.q_sm.prepare(sample_rate);
        for ch in &mut self.stages {
            for svf in ch.iter_mut() {
                svf.prepare(sample_rate);
            }
        }
        self.apply_coeffs(self.freq_sm.current(), self.q_sm.current());
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "ftype" => {
                let mode = mode_for_ftype(value as i32);
                if mode != self.mode {
                    // Tone switches node.type in place, filter state carries over.
                    self.mode = mode;
                    self.apply_coeffs(self.applied_freq, self.q_sm.current());
                }
            }
            "freq" => self.freq_sm.set_target(value.clamp(FREQ_MIN, FREQ_MAX)),
            "q" => self.q_sm.set_target(value.clamp(Q_MIN, Q_MAX)),
            "slope" => {
                let n = stages_for_slope(value as i32);
                if n > self.n_stages {
                    // clear stale state in sections that are being switched in
                    for ch in &mut self.stages {
                        for svf in &mut ch[self.n_stages..n] {
                            svf.reset();
                        }
                    }
                }
                self.n_stages = n;
            }
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        for i in 0..n {
            let freq = self.freq_sm.next();
            let q = self.q_sm.next();
            // update coefficients only while the smoothers are still moving
            if (freq - self.applied_freq).abs() > 1e-3 || (q - self.applied_q).abs() > 1e-5 {
                self.apply_coeffs(freq, q);
            }
            let mut sl = l[i];
            let mut sr = r[i];
            for s in 0..self.n_stages {
                sl = self.stages[0][s].process_sample(sl) * self.stage_gain;
                sr = self.stages[1][s].process_sample(sr) * self.stage_gain;
            }
            l[i] = sl;
            r[i] = sr;
        }
    }

    fn reset(&mut self) {
        for ch in &mut self.stages {
            for svf in ch.iter_mut() {
                svf.reset();
            }
        }
        self.freq_sm.snap(self.freq_sm.target());
        self.q_sm.snap(self.q_sm.target());
        self.apply_coeffs(self.freq_sm.current(), self.q_sm.current());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn sine(freq: f32, i: usize) -> f32 {
        libm::sinf(core::f32::consts::TAU * freq * i as f32 / SR)
    }

    /// Run `secs` of a sine through the device in MAX_BLOCK-ish chunks and
    /// return the output RMS over the second half (post-settle).
    fn run_sine_rms(fx: &mut FilterFx, freq: f32, secs: f32) -> f32 {
        let total = (SR * secs) as usize;
        let mut acc = 0.0f64;
        let mut counted = 0usize;
        let mut i = 0usize;
        let mut l = [0.0f32; 128];
        let mut r = [0.0f32; 128];
        while i < total {
            let n = 128.min(total - i);
            for (j, (sl, sr)) in l[..n].iter_mut().zip(r[..n].iter_mut()).enumerate() {
                let x = sine(freq, i + j);
                *sl = x;
                *sr = x;
            }
            fx.process(&mut l[..n], &mut r[..n]);
            for &y in &l[..n] {
                assert!(y.is_finite(), "non-finite output at sample {i}");
            }
            for (j, &y) in l[..n].iter().enumerate() {
                if i + j >= total / 2 {
                    acc += f64::from(y) * f64::from(y);
                    counted += 1;
                }
            }
            i += n;
        }
        libm::sqrt(acc / counted as f64) as f32
    }

    #[test]
    fn two_seconds_no_nan_and_non_silent() {
        let mut fx = FilterFx::new();
        fx.prepare(SR);
        // defaults: LP 2 kHz — a 220 Hz sine passes nearly untouched
        let rms = run_sine_rms(&mut fx, 220.0, 2.0);
        assert!(rms > 0.5, "wet output near-silent: rms {rms}");
    }

    #[test]
    fn param_sweep_stays_bounded() {
        for ftype in 0..3 {
            for slope in 0..3 {
                for &freq in &[40.0f32, 500.0, 2_000.0, 18_000.0] {
                    for &q in &[0.0f32, 1.0, 6.0, 12.0] {
                        let mut fx = FilterFx::new();
                        fx.prepare(SR);
                        fx.set_param("ftype", ftype as f32);
                        fx.set_param("slope", slope as f32);
                        fx.set_param("freq", freq);
                        fx.set_param("q", q);
                        // drive at the cutoff so stacked resonance is exercised
                        let rms = run_sine_rms(&mut fx, freq.min(SR * 0.45), 0.5);
                        assert!(
                            rms.is_finite() && rms < 1_000.0,
                            "unbounded: ftype {ftype} slope {slope} f {freq} q {q} rms {rms}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn mid_stream_param_changes_stay_finite() {
        let mut fx = FilterFx::new();
        fx.prepare(SR);
        let mut l = [0.0f32; 128];
        let mut r = [0.0f32; 128];
        for blk in 0..400 {
            for (j, s) in l.iter_mut().enumerate() {
                *s = sine(330.0, blk * 128 + j);
            }
            r.copy_from_slice(&l);
            // hop every param around at block boundaries
            fx.set_param("ftype", (blk % 3) as f32);
            fx.set_param("slope", ((blk / 3) % 3) as f32);
            fx.set_param("freq", if blk % 2 == 0 { 40.0 } else { 18_000.0 });
            fx.set_param("q", (blk % 13) as f32);
            fx.process(&mut l, &mut r);
            for &y in l.iter().chain(r.iter()) {
                assert!(y.is_finite());
            }
        }
    }

    #[test]
    fn lp_500hz_attenuates_4khz_by_20db() {
        let mut fx = FilterFx::new();
        fx.prepare(SR);
        fx.set_param("ftype", 0.0);
        fx.set_param("freq", 500.0);
        fx.set_param("q", 1.0);
        fx.set_param("slope", 0.0); // gentlest slope (12 dB/oct) must still pass
        let rms = run_sine_rms(&mut fx, 4_000.0, 2.0);
        let sine_rms = core::f32::consts::FRAC_1_SQRT_2;
        let db = 20.0 * libm::log10f((rms / sine_rms).max(1e-12));
        assert!(db < -20.0, "LP 500 Hz only attenuated 4 kHz by {db:.1} dB");
    }

    #[test]
    fn hp_and_bp_shape() {
        // HP at 2 kHz kills a 100 Hz sine
        let mut hp = FilterFx::new();
        hp.prepare(SR);
        hp.set_param("ftype", 1.0);
        hp.set_param("freq", 2_000.0);
        let low = run_sine_rms(&mut hp, 100.0, 1.0);
        assert!(low < 0.05, "HP passed lows: rms {low}");

        // BP at 1 kHz passes its center and rejects a far-away band
        let mut bp = FilterFx::new();
        bp.prepare(SR);
        bp.set_param("ftype", 2.0);
        bp.set_param("freq", 1_000.0);
        bp.set_param("q", 2.0);
        let center = run_sine_rms(&mut bp, 1_000.0, 1.0);
        let far = run_sine_rms(&mut bp, 12_000.0, 1.0);
        assert!(center > 10.0 * far, "BP center {center} vs far {far}");
    }

    #[test]
    fn steeper_slope_attenuates_more() {
        let stop_rms = |slope: f32| {
            let mut fx = FilterFx::new();
            fx.prepare(SR);
            fx.set_param("freq", 500.0);
            fx.set_param("slope", slope);
            run_sine_rms(&mut fx, 4_000.0, 1.0)
        };
        let s12 = stop_rms(0.0);
        let s24 = stop_rms(1.0);
        let s48 = stop_rms(2.0);
        assert!(s12 > s24 && s24 > s48, "slopes not ordered: {s12} {s24} {s48}");
    }
}
