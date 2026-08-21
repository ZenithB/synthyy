//! `crush` effect — bit-depth reduction.
//!
//! Port of the browser device: `makeEffect("crush")` wraps `Tone.BitCrusher`,
//! whose worklet quantizes every sample to `step = 0.5^(bits-1)` with
//! round-half-up (`step * floor(x/step + 0.5)`). The frozen schema carries
//! only `bits` (1–16, int) and `mix` — there is no sample-rate reduction.
//! Wet/dry follows Tone's CrossFade equal-power law. The quantizer is a hard
//! nonlinearity, so it runs 4× oversampled (RUST_ARCH.md); the dry path is
//! delayed by the oversampler's measured latency so intermediate mix values
//! stay phase-aligned instead of comb-filtering.

use crate::traits::EffectDsp;
use crate::util::{pan_gains, DelayLine, Oversampler4, Smoother, MAX_BLOCK};

fn quant_step(bits: i32) -> f32 {
    // Tone worklet: Math.pow(0.5, bits - 1)
    libm::powf(0.5, (bits - 1) as f32)
}

pub struct CrushFx {
    step: f32,
    mix: Smoother,
    os: Oversampler4,
    dry_delay_l: DelayLine,
    dry_delay_r: DelayLine,
    latency: usize, // oversampler round-trip delay, base-rate samples
    dry_l: [f32; MAX_BLOCK],
    dry_r: [f32; MAX_BLOCK],
}

impl CrushFx {
    pub fn new() -> Self {
        let mut fx = Self {
            step: quant_step(8), // schema defaults: bits 8, mix 1
            mix: Smoother::new(1.0),
            os: Oversampler4::new(),
            dry_delay_l: DelayLine::new(64),
            dry_delay_r: DelayLine::new(64),
            latency: 1,
            dry_l: [0.0; MAX_BLOCK],
            dry_r: [0.0; MAX_BLOCK],
        };
        fx.prepare(44_100.0);
        fx
    }

    fn process_block(&mut self, l: &mut [f32], r: &mut [f32]) {
        debug_assert!(l.len() == r.len() && l.len() <= MAX_BLOCK);
        for (i, (&xl, &xr)) in l.iter().zip(r.iter()).enumerate() {
            self.dry_delay_l.write(xl);
            self.dry_delay_r.write(xr);
            // read(1) is the just-written sample, so a `latency`-sample
            // delay is read(latency + 1).
            self.dry_l[i] = self.dry_delay_l.read(self.latency + 1);
            self.dry_r[i] = self.dry_delay_r.read(self.latency + 1);
        }

        let step = self.step;
        let inv_step = 1.0 / step;
        self.os.process(l, r, |hl, hr| {
            for s in hl.iter_mut().chain(hr.iter_mut()) {
                *s = step * libm::floorf(*s * inv_step + 0.5);
            }
        });

        let dry = self.dry_l.iter().zip(self.dry_r.iter());
        for ((wl, wr), (&dl, &dr)) in l.iter_mut().zip(r.iter_mut()).zip(dry) {
            let mix = self.mix.next();
            // Tone CrossFade: fade → equal-power panner curve.
            let (dry_g, wet_g) = pan_gains(mix * 2.0 - 1.0);
            *wl = dl * dry_g + *wl * wet_g;
            *wr = dr * dry_g + *wr * wet_g;
        }
    }
}

impl Default for CrushFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for CrushFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.mix.prepare(sample_rate);
        self.mix.snap(self.mix.target());
        // Measure the oversampler's identity latency once (graph build,
        // off the audio thread) so the dry path can be phase-aligned.
        self.os.reset();
        let mut best = 0.0f32;
        let mut lat = 1usize;
        for blk in 0..2usize {
            let mut l = [0.0f32; MAX_BLOCK];
            let mut r = [0.0f32; MAX_BLOCK];
            if blk == 0 {
                l[0] = 1.0;
                r[0] = 1.0;
            }
            self.os.process(&mut l, &mut r, |_, _| {});
            for (i, &v) in l.iter().enumerate() {
                if v.abs() > best {
                    best = v.abs();
                    lat = blk * MAX_BLOCK + i;
                }
            }
        }
        self.latency = lat.clamp(1, self.dry_delay_l.max_delay() - 1);
        self.os.reset();
        self.dry_delay_l.clear();
        self.dry_delay_r.clear();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            // devices.ts: Math.max(1, v|0); the worklet param clamps at 16.
            // Inherently stepped (schema int) — no smoothing.
            "bits" => self.step = quant_step((value as i32).clamp(1, 16)),
            "mix" => self.mix.set_target(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let (l, r) = (&mut l[..n], &mut r[..n]);
        for (cl, cr) in l.chunks_mut(MAX_BLOCK).zip(r.chunks_mut(MAX_BLOCK)) {
            self.process_block(cl, cr);
        }
    }

    fn reset(&mut self) {
        self.os.reset();
        self.dry_delay_l.clear();
        self.dry_delay_r.clear();
        self.mix.snap(self.mix.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    const SR: f32 = 48_000.0;

    fn rms(buf: &[f32]) -> f32 {
        libm::sqrtf(buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32)
    }

    /// Feed `blocks` blocks of a sine into `fx`, return the left output.
    fn run_sine(fx: &mut CrushFx, freq: f32, amp: f32, blocks: usize) -> Vec<f32> {
        let mut out = Vec::new();
        let mut phase = 0.0f32;
        for _ in 0..blocks {
            let mut l = [0.0f32; MAX_BLOCK];
            let mut r = [0.0f32; MAX_BLOCK];
            for s in l.iter_mut() {
                *s = amp * libm::sinf(phase * core::f32::consts::TAU);
                phase += freq / SR;
                phase -= libm::floorf(phase);
            }
            r.copy_from_slice(&l);
            fx.process(&mut l, &mut r);
            assert!(r.iter().all(|s| s.is_finite()));
            out.extend_from_slice(&l);
        }
        out
    }

    #[test]
    fn two_seconds_no_nan_inf() {
        let mut fx = CrushFx::new();
        fx.prepare(SR);
        fx.set_param("bits", 8.0);
        fx.set_param("mix", 1.0);
        let blocks = (2.0 * SR / MAX_BLOCK as f32) as usize + 1;
        let out = run_sine(&mut fx, 440.0, 0.9, blocks);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn param_sweep_stays_bounded() {
        let mut fx = CrushFx::new();
        fx.prepare(SR);
        for bits in 1..=16 {
            for m in 0..=4 {
                fx.set_param("bits", bits as f32);
                fx.set_param("mix", m as f32 / 4.0);
                let out = run_sine(&mut fx, 330.0, 1.0, 8);
                assert!(
                    out.iter().all(|s| s.is_finite() && s.abs() < 4.0),
                    "blew up at bits={bits} mix={}",
                    m as f32 / 4.0
                );
            }
        }
        fx.set_param("bogus", 123.0); // unknown keys are ignored
        let out = run_sine(&mut fx, 330.0, 1.0, 4);
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn wet_output_is_not_silent() {
        let mut fx = CrushFx::new();
        fx.prepare(SR);
        fx.set_param("bits", 4.0);
        fx.set_param("mix", 1.0);
        fx.reset();
        let out = run_sine(&mut fx, 440.0, 0.8, 40);
        assert!(rms(&out[MAX_BLOCK * 4..]) > 0.1);
    }

    /// Analytic: bits = 4 ⇒ step 0.125. Steady-state DC outputs sit on the
    /// 2^4 + 1 level grid across ±1 and match the worklet's round-half-up.
    #[test]
    fn bits4_quantizes_to_grid() {
        let mut fx = CrushFx::new();
        fx.prepare(SR);
        fx.set_param("bits", 4.0);
        fx.set_param("mix", 1.0);
        fx.reset();
        let step = 0.125f32;
        let mut grid = BTreeSet::new();
        for k in 0..=40 {
            // offset dodges exact half-step boundaries where FIR rounding
            // could legitimately land on either neighbor
            let x = -0.987 + k as f32 * 0.05;
            let mut last = 0.0f32;
            for _ in 0..20 {
                let mut l = [x; MAX_BLOCK];
                let mut r = [x; MAX_BLOCK];
                fx.process(&mut l, &mut r);
                last = l[MAX_BLOCK - 1];
            }
            let expect = step * libm::floorf(x / step + 0.5);
            assert!(
                (last - expect).abs() < 1e-3,
                "x={x}: got {last}, want {expect}"
            );
            grid.insert(libm::roundf(last / step) as i32);
        }
        assert!(grid.len() <= 17, "expected <= 17 levels, got {}", grid.len());
    }

    /// mix = 0 must be pure dry: the input delayed by the measured
    /// oversampler latency, bit depth irrelevant.
    #[test]
    fn mix_zero_is_pure_delayed_dry() {
        let mut fx = CrushFx::new();
        fx.prepare(SR);
        fx.set_param("bits", 1.0);
        fx.set_param("mix", 0.0);
        fx.reset(); // snap the mix smoother to 0
        let lat = fx.latency;
        assert!((1..64).contains(&lat), "implausible latency {lat}");

        let mut input = Vec::new();
        let mut output = Vec::new();
        let mut phase = 0.0f32;
        for _ in 0..10 {
            let mut l = [0.0f32; MAX_BLOCK];
            let mut r = [0.0f32; MAX_BLOCK];
            for s in l.iter_mut() {
                *s = 0.7 * libm::sinf(phase * core::f32::consts::TAU);
                phase += 523.25 / SR;
                phase -= libm::floorf(phase);
            }
            r.copy_from_slice(&l);
            input.extend_from_slice(&l);
            fx.process(&mut l, &mut r);
            output.extend_from_slice(&l);
        }
        for (i, &o) in output.iter().enumerate().skip(lat) {
            let want = input[i - lat];
            assert!((o - want).abs() < 1e-6, "i={i}: got {o}, want {want}");
        }
    }
}
