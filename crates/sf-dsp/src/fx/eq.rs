//! `eq` effect — port of the Tone.EQ3 composite in `makeEffect("eq")`
//! (src/audio/devices.ts): a 3-band split (MultibandSplit at 400 Hz / 2500 Hz,
//! 12 dB/oct filters) into per-band gains, summed. Schema (src/audio/schema.ts
//! "EQ Three"): `low`/`mid`/`high` band gains in dB, −12..+12, default 0. The
//! crossover frequencies are Tone.EQ3 defaults and not exposed as params;
//! there is no wet/dry mix param and no tempo sync.

use crate::traits::EffectDsp;
use crate::util::{db_to_lin, Smoother, Svf, SvfMode};

/// Tone.EQ3 default crossovers (MultibandSplit lowFrequency/highFrequency).
const LOW_XOVER_HZ: f32 = 400.0;
const HIGH_XOVER_HZ: f32 = 2_500.0;
/// MultibandSplit default Q is 1 — Web Audio LP/HP Q is a resonance in dB,
/// so the traditional filter Q is 10^(1/20).
const XOVER_Q: f32 = 1.122_018_5;

/// One channel of the band split: low = LP(400); mid = HP(400)→LP(2500);
/// high = HP(2500). Mirrors the MultibandSplit filter topology exactly.
struct SplitChannel {
    low_lp: Svf,
    mid_hp: Svf,
    mid_lp: Svf,
    high_hp: Svf,
}

impl SplitChannel {
    fn new() -> Self {
        Self {
            low_lp: Svf::new(SvfMode::Lowpass),
            mid_hp: Svf::new(SvfMode::Highpass),
            mid_lp: Svf::new(SvfMode::Lowpass),
            high_hp: Svf::new(SvfMode::Highpass),
        }
    }

    fn prepare(&mut self, sample_rate: f32) {
        for f in [
            &mut self.low_lp,
            &mut self.mid_hp,
            &mut self.mid_lp,
            &mut self.high_hp,
        ] {
            f.prepare(sample_rate);
        }
        self.low_lp.set_cutoff_q(LOW_XOVER_HZ, XOVER_Q);
        self.mid_hp.set_cutoff_q(LOW_XOVER_HZ, XOVER_Q);
        self.mid_lp.set_cutoff_q(HIGH_XOVER_HZ, XOVER_Q);
        self.high_hp.set_cutoff_q(HIGH_XOVER_HZ, XOVER_Q);
    }

    fn reset(&mut self) {
        self.low_lp.reset();
        self.mid_hp.reset();
        self.mid_lp.reset();
        self.high_hp.reset();
    }

    #[inline]
    fn process_sample(&mut self, x: f32, g_low: f32, g_mid: f32, g_high: f32) -> f32 {
        let low = self.low_lp.process_sample(x);
        let mid = self.mid_lp.process_sample(self.mid_hp.process_sample(x));
        let high = self.high_hp.process_sample(x);
        low * g_low + mid * g_mid + high * g_high
    }
}

pub struct EqFx {
    left: SplitChannel,
    right: SplitChannel,
    // Smoothed *linear* band gains — dB→lin conversion happens at the block
    // boundary in set_param, never per sample.
    low: Smoother,
    mid: Smoother,
    high: Smoother,
}

impl EqFx {
    pub fn new() -> Self {
        Self {
            left: SplitChannel::new(),
            right: SplitChannel::new(),
            low: Smoother::new(1.0),
            mid: Smoother::new(1.0),
            high: Smoother::new(1.0),
        }
    }
}

impl Default for EqFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for EqFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.left.prepare(sample_rate);
        self.right.prepare(sample_rate);
        for s in [&mut self.low, &mut self.mid, &mut self.high] {
            s.prepare(sample_rate);
            s.snap(s.target());
        }
    }

    fn set_param(&mut self, key: &str, value: f32) {
        // Tone.EQ3 applies the raw dB value with no clamp; the schema range
        // (−12..+12) is enforced upstream by the UI.
        let lin = db_to_lin(value);
        match key {
            "low" => self.low.set_target(lin),
            "mid" => self.mid.set_target(lin),
            "high" => self.high.set_target(lin),
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        for i in 0..n {
            let g_low = self.low.next();
            let g_mid = self.mid.next();
            let g_high = self.high.next();
            l[i] = self.left.process_sample(l[i], g_low, g_mid, g_high);
            r[i] = self.right.process_sample(r[i], g_low, g_mid, g_high);
        }
    }

    fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
        self.low.snap(self.low.target());
        self.mid.snap(self.mid.target());
        self.high.snap(self.high.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 44_100.0;

    /// Deterministic white-ish noise in [−1, 1] (LCG — no rand dep).
    struct Lcg(u32);
    impl Lcg {
        fn next(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (self.0 >> 8) as f32 / (1u32 << 23) as f32 - 1.0
        }
    }

    fn run_blocks(fx: &mut EqFx, secs: f32, mut src: impl FnMut(usize) -> f32) -> Vec<f32> {
        let total = (SR * secs) as usize;
        let mut out = Vec::with_capacity(total);
        let mut i = 0usize;
        while i < total {
            let n = 128.min(total - i);
            let mut l: Vec<f32> = (0..n).map(|j| src(i + j)).collect();
            let mut r = l.clone();
            fx.process(&mut l[..], &mut r[..]);
            out.extend_from_slice(&l);
            i += n;
        }
        out
    }

    fn sine_rms_at(fx: &mut EqFx, freq: f32) -> f32 {
        let out = run_blocks(fx, 1.0, |i| {
            libm::sinf(core::f32::consts::TAU * freq * i as f32 / SR)
        });
        // skip the first half: filter + smoother settling
        let tail = &out[out.len() / 2..];
        let acc: f32 = tail.iter().map(|s| s * s).sum();
        libm::sqrtf(acc / tail.len() as f32)
    }

    #[test]
    fn no_nan_inf_on_two_seconds_of_noise() {
        let mut fx = EqFx::new();
        fx.prepare(SR);
        fx.set_param("low", 12.0);
        fx.set_param("mid", -12.0);
        fx.set_param("high", 6.0);
        let mut noise = Lcg(0xC0FF_EE01);
        let out = run_blocks(&mut fx, 2.0, |_| noise.next());
        assert!(out.iter().all(|s| s.is_finite()), "NaN/inf in output");
    }

    #[test]
    fn param_sweep_stays_bounded() {
        let mut fx = EqFx::new();
        fx.prepare(SR);
        let mut noise = Lcg(0xBADC_0DE1);
        for step in 0..=24 {
            let db = -12.0 + step as f32; // full schema range on every band
            fx.set_param("low", db);
            fx.set_param("mid", -db);
            fx.set_param("high", db);
            let out = run_blocks(&mut fx, 0.05, |_| noise.next());
            for s in &out {
                assert!(s.is_finite());
                // +12 dB on noise ≤ 1.0 with crossover overlap: generous cap
                assert!(s.abs() < 16.0, "blew up at {db} dB: {s}");
            }
        }
    }

    #[test]
    fn wet_output_is_non_silent() {
        let mut fx = EqFx::new();
        fx.prepare(SR);
        fx.set_param("low", 3.0);
        let rms = sine_rms_at(&mut fx, 440.0);
        assert!(rms > 0.1, "output silent: rms {rms}");
    }

    #[test]
    fn flat_settings_are_near_unity() {
        let mut fx = EqFx::new();
        fx.prepare(SR);
        for f in [100.0, 1_000.0, 8_000.0] {
            let rms = sine_rms_at(&mut fx, f);
            let db = 20.0 * libm::log10f(rms / core::f32::consts::FRAC_1_SQRT_2);
            // 12 dB/oct crossovers color the sum slightly (matches Tone.EQ3)
            assert!(db.abs() < 3.0, "flat response at {f} Hz off by {db:.2} dB");
        }
    }

    #[test]
    fn low_minus_20_db_attenuates_100hz_vs_flat() {
        let mut flat = EqFx::new();
        flat.prepare(SR);
        let flat_rms = sine_rms_at(&mut flat, 100.0);

        let mut cut = EqFx::new();
        cut.prepare(SR);
        cut.set_param("low", -20.0);
        let cut_rms = sine_rms_at(&mut cut, 100.0);

        let delta_db = 20.0 * libm::log10f(cut_rms / flat_rms);
        assert!(
            delta_db < -15.0,
            "low −20 dB only attenuated 100 Hz by {delta_db:.1} dB"
        );
    }

    #[test]
    fn high_band_untouched_by_low_cut() {
        let mut flat = EqFx::new();
        flat.prepare(SR);
        let flat_rms = sine_rms_at(&mut flat, 8_000.0);

        let mut cut = EqFx::new();
        cut.prepare(SR);
        cut.set_param("low", -12.0);
        let cut_rms = sine_rms_at(&mut cut, 8_000.0);

        let delta_db = 20.0 * libm::log10f(cut_rms / flat_rms);
        assert!(
            delta_db.abs() < 1.0,
            "low cut leaked into 8 kHz by {delta_db:.2} dB"
        );
    }
}
