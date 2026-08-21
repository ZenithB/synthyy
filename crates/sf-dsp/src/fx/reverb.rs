//! `reverb` effect — port of the Tone composite in `src/audio/devices.ts`
//! (`Tone.Reverb { decay: p.size, preDelay: 0.02, wet: p.mix }`).
//!
//! Implemented as an 8-line FDN with a Hadamard feedback matrix
//! (RUST_REWRITE_PLAN §5: match character — decay time tracks the `size`
//! param — not samples). Per-line feedback gains are set so the loop loses
//! 60 dB in `size` seconds (`g = 10^(-3·d/T60)`); the Hadamard mix is
//! orthogonal, so |g| < 1 keeps the tank unconditionally stable. There is NO
//! in-loop damping filter: the browser IR is undamped white noise (render.ts
//! and Tone.Reverb alike), so its T60 is uniform across the band — a damping
//! filter would shorten the HF decay and break the flat convolver energy law
//! the tap gain is calibrated to.
//! A fixed 20 ms pre-delay sits before the tank, matching the Tone preset.
//!
//! LEVEL STAGING follows the browser's OFFLINE reverb — the path that renders
//! the golden WAVs. Tone.Reverb cannot run inside `Tone.Offline` (its IR is a
//! nested offline render, which aborts), so `src/audio/render.ts`
//! `makeOffDevice('reverb')` substitutes a raw Convolver fed a synthesized
//! decaying-noise IR plus a LINEAR dry/wet mix:
//! `new Tone.Gain(mix)` on the wet branch and `new Tone.Gain(1 - mix)` on the
//! dry branch — NOT Tone CrossFade's equal-power law. The mix here mirrors
//! that: `dry·(1-mix) + wet·mix`; at mix 0 the dry gain is exactly 1 and the
//! wet gain exactly 0.
//! The wet tap gain is derived from the Web Audio ConvolverNode IR
//! normalization (render.ts sets `conv.normalize = true`), so the wet level
//! tracks the browser's `0.00125 / rms(IR)` calibration — see
//! `update_fb_targets`.
//!
//! Schema (`src/audio/schema.ts` "reverb"): size 0.2..10 s (def 2.2),
//! mix 0..1 (def 0.3).

use crate::traits::EffectDsp;
use crate::util::{DelayLine, Smoother};

const LINES: usize = 8;

/// Mutually-incommensurate line lengths (ms) — dense, colorless tail.
const LINE_MS: [f32; LINES] = [29.7, 37.1, 41.1, 43.7, 53.3, 59.5, 61.7, 68.3];

/// Pre-delay before the tank (Tone.Reverb `preDelay: 0.02`).
const PRE_DELAY_SEC: f32 = 0.02;

/// Web Audio ConvolverNode normalization calibration (spec `GainCalibration`).
/// The golden render path (src/audio/render.ts `makeOffDevice('reverb')`)
/// builds a 2-channel IR of exactly `decay * sampleRate` samples of enveloped
/// white noise and feeds it into a ConvolverNode with `conv.normalize = true`,
/// so the browser scales the IR to `0.00125 / rms(IR)` times
/// `44100 / sampleRate`. The wet path's per-channel impulse energy is
/// therefore exactly `(0.00125 * 44100 / sr)^2 * N` with `N = decay * sr`
/// samples — independent of the IR's decay envelope shape (the render.ts IR
/// has no pre-delay and its `(1 - i/len)^2.5` envelope drops out of the
/// normalized energy). The FDN wet tap gain below is derived to reproduce
/// that energy law.
const GAIN_CALIBRATION: f32 = 0.001_25;
const GAIN_CALIBRATION_SR: f32 = 44_100.0;

/// 6·ln(10): converts a T60 into the energy integral `∫10^(-6t/T60) dt = T60/(6·ln10)`.
const SIX_LN10: f32 = 13.815_511;

const SIZE_MIN: f32 = 0.2;
const SIZE_MAX: f32 = 10.0;

pub struct ReverbFx {
    sample_rate: f32,
    lines: [DelayLine; LINES],
    delay_samps: [usize; LINES],
    fb_gain: [Smoother; LINES],
    tap_gain: Smoother,
    pre_l: DelayLine,
    pre_r: DelayLine,
    pre_samps: usize,
    mix: Smoother,
    size: f32,
}

impl Default for ReverbFx {
    fn default() -> Self {
        Self::new()
    }
}

impl ReverbFx {
    pub fn new() -> Self {
        let mut fx = Self {
            sample_rate: 44_100.0,
            lines: core::array::from_fn(|_| DelayLine::new(2)),
            delay_samps: [1; LINES],
            fb_gain: core::array::from_fn(|_| Smoother::new(0.0)),
            tap_gain: Smoother::new(0.0),
            pre_l: DelayLine::new(2),
            pre_r: DelayLine::new(2),
            pre_samps: 1,
            mix: Smoother::new(0.3),
            size: 2.2,
        };
        fx.build(44_100.0);
        fx
    }

    /// Allocate buffers and derive coefficients for `sample_rate`
    /// (graph build only — never called from the audio loop).
    fn build(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for (k, line) in self.lines.iter_mut().enumerate() {
            let d = (LINE_MS[k] * 1e-3 * self.sample_rate) as usize;
            self.delay_samps[k] = d.max(1);
            *line = DelayLine::new(self.delay_samps[k] + 1);
        }
        self.pre_samps = ((PRE_DELAY_SEC * self.sample_rate) as usize).max(1);
        self.pre_l = DelayLine::new(self.pre_samps + 1);
        self.pre_r = DelayLine::new(self.pre_samps + 1);
        for g in &mut self.fb_gain {
            g.prepare(self.sample_rate);
        }
        self.tap_gain.prepare(self.sample_rate);
        self.mix.prepare(self.sample_rate);
        self.update_fb_targets();
        for g in &mut self.fb_gain {
            g.snap(g.target());
        }
        self.tap_gain.snap(self.tap_gain.target());
        self.mix.snap(self.mix.target());
    }

    /// Per-line loop gain for a 60 dB decay over `size` seconds, plus the wet
    /// tap gain that matches the browser's convolver-normalized wet level.
    ///
    /// Browser law (see `GAIN_CALIBRATION`): per-output-channel wet impulse
    /// energy is `cal² · T60 · sr` with `cal = 0.00125·44100/sr` (the golden
    /// IR is `decay·sr` samples long — render.ts `len = decay * sampleRate` —
    /// so pre-delay adds no energy).
    ///
    /// FDN model: every line decays `10^(-3/(T60·sr))` in amplitude per sample
    /// (g_k = 10^(-3·d_k/T60) over d_k samples — the per-sample rate is line-
    /// independent), so for stereo input at power σ² per channel the tank
    /// holds `8σ²·T60·sr/(6·ln10)` in steady state, and a tap summing all 8
    /// lines emits `tap² · (E_tank/8) · Σ(1/d_k)` per output channel. Equating
    /// the two gives `tap² = 6·ln10 · cal² / Σ(1/d_k)`.
    fn update_fb_targets(&mut self) {
        let t60 = self.size.clamp(SIZE_MIN, SIZE_MAX);
        for (k, g) in self.fb_gain.iter_mut().enumerate() {
            let d_sec = self.delay_samps[k] as f32 / self.sample_rate;
            g.set_target(libm::powf(10.0, -3.0 * d_sec / t60));
        }
        let inv_d_sum: f32 = self.delay_samps.iter().map(|&d| 1.0 / d as f32).sum();
        let cal = GAIN_CALIBRATION * GAIN_CALIBRATION_SR / self.sample_rate;
        let tap_sq = SIX_LN10 * cal * cal / inv_d_sum;
        self.tap_gain.set_target(libm::sqrtf(tap_sq.max(0.0)));
    }

    /// In-place 8-point Hadamard butterfly, normalized to be orthogonal.
    #[inline]
    fn hadamard8(v: &mut [f32; LINES]) {
        for stride in [1usize, 2, 4] {
            let mut i = 0;
            while i < LINES {
                for j in i..i + stride {
                    let a = v[j];
                    let b = v[j + stride];
                    v[j] = a + b;
                    v[j + stride] = a - b;
                }
                i += stride * 2;
            }
        }
        const NORM: f32 = 0.353_553_39; // 1/sqrt(8)
        for x in v {
            *x *= NORM;
        }
    }
}

impl EffectDsp for ReverbFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.build(sample_rate);
        self.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "size" => {
                self.size = value.clamp(SIZE_MIN, SIZE_MAX);
                self.update_fb_targets();
            }
            "mix" => self.mix.set_target(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        for i in 0..n {
            let dry_l = l[i];
            let dry_r = r[i];

            self.pre_l.write(dry_l);
            self.pre_r.write(dry_r);
            let in_l = self.pre_l.read(self.pre_samps);
            let in_r = self.pre_r.read(self.pre_samps);

            let mut v = [0.0f32; LINES];
            for (k, line) in self.lines.iter().enumerate() {
                v[k] = line.read(self.delay_samps[k]);
            }

            // Decorrelated stereo taps: orthogonal sign patterns over the lines.
            let tap = self.tap_gain.next();
            let wet_l = tap * (v[0] - v[1] + v[2] - v[3] + v[4] - v[5] + v[6] - v[7]);
            let wet_r = tap * (v[0] + v[1] - v[2] - v[3] + v[4] + v[5] - v[6] - v[7]);

            Self::hadamard8(&mut v);
            for (k, line) in self.lines.iter_mut().enumerate() {
                let g = self.fb_gain[k].next();
                let inj = if k & 1 == 0 { in_l } else { in_r };
                line.write(v[k] * g + inj);
            }

            // LINEAR dry/wet mix — the golden render path is `Gain(mix)` wet +
            // `Gain(1 - mix)` dry (src/audio/render.ts makeOffDevice 'reverb'),
            // not Tone CrossFade's equal-power law.
            let m = self.mix.next();
            l[i] = dry_l * (1.0 - m) + wet_l * m;
            r[i] = dry_r * (1.0 - m) + wet_r * m;
        }
    }

    fn reset(&mut self) {
        for line in &mut self.lines {
            line.clear();
        }
        self.pre_l.clear();
        self.pre_r.clear();
        for g in &mut self.fb_gain {
            g.snap(g.target());
        }
        self.tap_gain.snap(self.tap_gain.target());
        self.mix.snap(self.mix.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 44_100.0;

    fn make(size: f32, mix: f32) -> ReverbFx {
        let mut fx = ReverbFx::new();
        fx.set_param("size", size);
        fx.set_param("mix", mix);
        fx.prepare(SR); // prepare snaps smoothers to targets
        fx
    }

    fn process_secs(fx: &mut ReverbFx, l: &mut [f32], r: &mut [f32], secs: f32) {
        let total = (secs * SR) as usize;
        let mut done = 0;
        while done < total {
            let n = 128.min(total - done);
            fx.process(&mut l[done..done + n], &mut r[done..done + n]);
            done += n;
        }
    }

    #[test]
    fn no_nan_inf_over_two_seconds() {
        let mut fx = make(2.2, 0.5);
        let n = (2.0 * SR) as usize;
        let mut l: Vec<f32> = (0..n)
            .map(|i| libm::sinf(core::f32::consts::TAU * 220.0 * i as f32 / SR) * 0.5)
            .collect();
        let mut r = l.clone();
        process_secs(&mut fx, &mut l, &mut r, 2.0);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
    }

    #[test]
    fn param_sweep_stays_bounded() {
        for &size in &[0.2f32, 2.2, 10.0] {
            for &mix in &[0.0f32, 0.5, 1.0] {
                let mut fx = make(size, mix);
                let n = SR as usize;
                // deterministic pseudo-noise input at ±0.5
                let mut seed = 0x1234_5678u32;
                let mut l: Vec<f32> = (0..n)
                    .map(|_| {
                        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        (seed >> 16) as f32 / 65_536.0 - 0.5
                    })
                    .collect();
                let mut r = l.clone();
                // sweep params mid-stream too
                fx.process(&mut l[..4096], &mut r[..4096]);
                fx.set_param("size", SIZE_MAX - size);
                fx.set_param("mix", 1.0 - mix);
                fx.process(&mut l[4096..], &mut r[4096..]);
                assert!(
                    l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() < 10.0),
                    "blew up at size={size} mix={mix}"
                );
            }
        }
    }

    #[test]
    fn wet_output_is_non_silent() {
        let mut fx = make(2.2, 1.0);
        let n = SR as usize / 2;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        l[0] = 1.0;
        r[0] = 1.0;
        process_secs(&mut fx, &mut l, &mut r, 0.5);
        // energy after the pre-delay + first line arrivals
        let start = (0.1 * SR) as usize;
        let e: f32 = l[start..].iter().chain(r[start..].iter()).map(|s| s * s).sum();
        assert!(e > 1e-6, "wet tail is silent (energy {e})");
    }

    #[test]
    fn impulse_tail_decays_60db_in_about_size_seconds() {
        let size = 1.0f32;
        let mut fx = make(size, 1.0);
        let secs = 2.5f32;
        let n = (secs * SR) as usize;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        l[0] = 1.0;
        r[0] = 1.0;
        process_secs(&mut fx, &mut l, &mut r, secs);

        let win = 2_048usize;
        let rms = |t: f32| -> f32 {
            let s = (t * SR) as usize;
            let e: f32 = l[s..s + win].iter().map(|x| x * x).sum();
            libm::sqrtf(e / win as f32)
        };
        // early tail reference once all lines have rung in
        let t0 = 0.1f32;
        let r0 = rms(t0);
        assert!(r0 > 1e-5, "no early tail energy");
        let floor = r0 * 1e-3; // −60 dB
        let mut cross = secs;
        let mut t = t0;
        while t < secs - win as f32 / SR {
            if rms(t) <= floor {
                cross = t;
                break;
            }
            t += win as f32 / SR;
        }
        let decay = cross - t0;
        assert!(
            decay > 0.35 * size && decay < 1.6 * size,
            "60 dB decay took {decay:.2}s for size={size}s"
        );
    }

    #[test]
    fn dry_path_intact_at_mix_zero() {
        let mut fx = make(2.2, 0.0);
        let mut l: Vec<f32> = (0..512).map(|i| (i as f32 * 0.013).sin() * 0.7).collect();
        let mut r: Vec<f32> = (0..512).map(|i| (i as f32 * 0.031).cos() * 0.7).collect();
        let (l0, r0) = (l.clone(), r.clone());
        fx.process(&mut l, &mut r);
        assert_eq!(l, l0);
        assert_eq!(r, r0);
    }

    #[test]
    fn wet_impulse_energy_matches_convolver_norm() {
        // Golden-path law (render.ts makeOffDevice 'reverb': normalized IR of
        // decay*sr samples): per-channel wet impulse energy
        // = (0.00125*44100/sr)^2 * size*sr.
        for &size in &[1.0f32, 4.0] {
            let mut fx = make(size, 1.0);
            let secs = size * 2.0 + 0.5;
            let n = (secs * SR) as usize;
            let mut l = vec![0.0f32; n];
            let mut r = vec![0.0f32; n];
            l[0] = 1.0;
            r[0] = 1.0;
            process_secs(&mut fx, &mut l, &mut r, secs);
            let e_l: f32 = l.iter().map(|x| x * x).sum();
            let cal = GAIN_CALIBRATION * GAIN_CALIBRATION_SR / SR;
            let target = cal * cal * size * SR;
            let db = 10.0 * libm::log10f(e_l / target);
            assert!(
                db.abs() < 2.0,
                "size={size}: wet energy {e_l:.4} vs target {target:.4} ({db:+.2} dB)"
            );
        }
    }

    #[test]
    fn larger_size_rings_longer() {
        let tail_energy = |size: f32| -> f32 {
            let mut fx = make(size, 1.0);
            let n = (2.0 * SR) as usize;
            let mut l = vec![0.0f32; n];
            let mut r = vec![0.0f32; n];
            l[0] = 1.0;
            r[0] = 1.0;
            process_secs(&mut fx, &mut l, &mut r, 2.0);
            let s = (1.5 * SR) as usize;
            l[s..].iter().chain(r[s..].iter()).map(|x| x * x).sum()
        };
        assert!(tail_energy(8.0) > tail_energy(0.5) * 10.0);
    }
}
