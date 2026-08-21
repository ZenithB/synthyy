//! `shift` effect — port of the Tone.FrequencyShifter built by
//! `makeEffect("shift")` in src/audio/devices.ts
//! (`new Tone.FrequencyShifter({ frequency: p.amt, wet: p.mix })`).
//!
//! Behavior spec (Tone/effect/FrequencyShifter.ts): single-sideband shift.
//! The input runs through Tone's PhaseShiftAllpass — the Niemitalo IIR
//! Hilbert approximation: two parallel 4-section allpass cascades (WebAudio
//! IIRFilters `[c², 0, −1] / [1, 0, −c²]`), the first followed by a
//! one-sample delay so the second leads it by ~90° across the band. The pair
//! is ring-modulated by a quadrature (sin/cos) oscillator at the shift
//! frequency and recombined so one sideband cancels: every partial moves by
//! the same ±Hz (inharmonic, unlike a pitch shifter). Positive `amt` shifts
//! up; negative shifts down (the oscillator simply runs backwards).
//! Wet/dry is Tone's CrossFade (equal-power cos/sin law).
//!
//! Params (schema.ts "shift"): `amt` −400..400 Hz (def 60), `mix` 0..1
//! (def 0.4) — both ~15 ms smoothed.

use crate::traits::EffectDsp;
use crate::util::Smoother;

/// Tone PhaseShiftAllpass bank 1 (Niemitalo coefficients). This cascade plus
/// a one-sample delay is the in-phase reference output.
const BANK_I: [f64; 4] = [0.6923878, 0.9360654322959, 0.988229522686, 0.9987488452737];

/// Tone PhaseShiftAllpass bank 2 — its output (`offset90`) leads the delayed
/// bank-1 path by ~90° from ~20 Hz to Nyquist.
const BANK_Q: [f64; 4] = [0.4021921162426, 0.856171088242, 0.9722909545651, 0.9952884791278];

/// One second-order allpass section `H(z) = (a − z⁻²)/(1 − a·z⁻²)`, `a = c²`
/// — exactly the WebAudio IIRFilter Tone builds per coefficient.
#[derive(Clone, Copy, Default)]
struct Allpass2 {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Allpass2 {
    #[inline]
    fn run(&mut self, a: f64, x: f64) -> f64 {
        let y = a * (x + self.y2) - self.x2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// Per-channel Hilbert pair: both cascades share the input; the I path takes
/// the extra one-sample delay (Tone's `_oneSampleDelay`).
#[derive(Clone, Copy, Default)]
struct Hilbert {
    i_bank: [Allpass2; 4],
    q_bank: [Allpass2; 4],
    i_z1: f64,
}

impl Hilbert {
    /// Returns `(i, q)` where `q` leads `i` by ~90°.
    #[inline]
    fn run(&mut self, x: f64) -> (f64, f64) {
        let mut i = x;
        for (s, c) in self.i_bank.iter_mut().zip(BANK_I) {
            i = s.run(c * c, i);
        }
        let mut q = x;
        for (s, c) in self.q_bank.iter_mut().zip(BANK_Q) {
            q = s.run(c * c, q);
        }
        let delayed = self.i_z1;
        self.i_z1 = i;
        (delayed, q)
    }

    fn clear(&mut self) {
        *self = Self::default();
    }
}

pub struct ShiftFx {
    sample_rate: f32,
    /// Shift amount in Hz (schema `amt`), ~15 ms smoothed.
    amt: Smoother,
    /// Wet/dry mix 0..1 (Tone `wet`), ~15 ms smoothed.
    mix: Smoother,
    /// Quadrature oscillator phase in cycles [0, 1) (shared by both channels,
    /// like Tone's single oscillator pair feeding a stereo ring mod).
    phase: f64,
    left: Hilbert,
    right: Hilbert,
}

impl ShiftFx {
    pub fn new() -> Self {
        // Schema defaults: amt=60 Hz, mix=0.4.
        Self {
            sample_rate: 44_100.0,
            amt: Smoother::new(60.0),
            mix: Smoother::new(0.4),
            phase: 0.0,
            left: Hilbert::default(),
            right: Hilbert::default(),
        }
    }
}

impl Default for ShiftFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for ShiftFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.amt.prepare(self.sample_rate);
        self.amt.snap(self.amt.target());
        self.mix.prepare(self.sample_rate);
        self.mix.snap(self.mix.target());
        self.left.clear();
        self.right.clear();
        self.phase = 0.0;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "amt" => self.amt.set_target(value.clamp(-400.0, 400.0)),
            "mix" => self.mix.set_target(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let inv_sr = 1.0 / self.sample_rate as f64;
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let f = self.amt.next();
            let m = self.mix.next();
            // Tone's wet control is a CrossFade: equal-power cos/sin.
            let dry_g = libm::cosf(m * core::f32::consts::FRAC_PI_2);
            let wet_g = libm::sinf(m * core::f32::consts::FRAC_PI_2);
            let th = self.phase * core::f64::consts::TAU;
            let (sin_th, cos_th) = (libm::sin(th), libm::cos(th));
            // Negative shift runs the oscillator backwards; wrap keeps [0,1).
            self.phase += f as f64 * inv_sr;
            self.phase -= libm::floor(self.phase);
            // SSB up-shift by f: Re[(i − j·q_lead)·e^{jθ}] = i·cosθ + q·sinθ
            // (q leads i by 90°, so −q is the Hilbert transform of i).
            let (il, ql) = self.left.run(*ls as f64);
            let (ir, qr) = self.right.run(*rs as f64);
            let wet_l = (il * cos_th + ql * sin_th) as f32;
            let wet_r = (ir * cos_th + qr * sin_th) as f32;
            *ls = *ls * dry_g + wet_l * wet_g;
            *rs = *rs * dry_g + wet_r * wet_g;
        }
    }

    fn reset(&mut self) {
        self.left.clear();
        self.right.clear();
        self.phase = 0.0;
        self.amt.snap(self.amt.target());
        self.mix.snap(self.mix.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn run_blocks(fx: &mut ShiftFx, l: &mut [f32], r: &mut [f32]) {
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
        let mut s = 0x12345678u32;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1 << 24) as f32 - 0.5
            })
            .collect()
    }

    fn sine(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * libm::sinf(core::f32::consts::TAU * freq * i as f32 / SR))
            .collect()
    }

    /// Goertzel magnitude at `freq`, normalized so a full-scale sine reads ~1.
    fn goertzel(x: &[f32], freq: f32) -> f32 {
        let w = core::f64::consts::TAU * freq as f64 / SR as f64;
        let c = 2.0 * libm::cos(w);
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v as f64 + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let p = s1 * s1 + s2 * s2 - c * s1 * s2;
        (2.0 * libm::sqrt(p.max(0.0)) / x.len() as f64) as f32
    }

    #[test]
    fn two_seconds_no_nan_inf_and_wet_non_silent() {
        let mut fx = ShiftFx::new(); // schema defaults: amt 60 Hz, mix 0.4
        fx.set_param("mix", 1.0);
        fx.prepare(SR);

        let n = (2.0 * SR) as usize;
        let src = noise(n);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        // Wet path present and non-silent: mix 1 replaces dry with the
        // shifted signal, which must differ from the input.
        let energy: f32 = l.iter().map(|v| v * v).sum();
        assert!(energy > 1.0, "wet output is silent (energy {energy})");
        let diff: f32 = l.iter().zip(&src).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 1.0, "output identical to dry (diff {diff})");
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = ShiftFx::new();
        fx.prepare(SR);
        let src = noise(BLOCK);
        for step in 0..=8 {
            let t = step as f32 / 8.0;
            fx.set_param("amt", -400.0 + t * 800.0); // full schema range, incl. 0
            fx.set_param("mix", t);
            for _ in 0..40 {
                let mut l = src.clone();
                let mut r = src.clone();
                fx.process(&mut l, &mut r);
                assert!(
                    l.iter().chain(r.iter()).all(|v| v.is_finite() && v.abs() < 100.0),
                    "blew up at t={t}"
                );
            }
        }
        // Unknown keys are ignored; mix=0 is exactly dry after reset.
        fx.set_param("nope", 3.0);
        fx.set_param("mix", 0.0);
        fx.reset();
        let mut l = [0.25f32; BLOCK];
        let mut r = [-0.25f32; BLOCK];
        fx.process(&mut l, &mut r);
        assert!(l.iter().all(|v| (v - 0.25).abs() < 1e-6));
        assert!(r.iter().all(|v| (v + 0.25).abs() < 1e-6));
    }

    #[test]
    fn sine_440_shift_100_peaks_at_540() {
        let mut fx = ShiftFx::new();
        fx.set_param("amt", 100.0);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);

        let n = (2.0 * SR) as usize;
        let src = sine(n, 440.0, 0.5);
        let mut l = src.clone();
        let mut r = src;
        run_blocks(&mut fx, &mut l, &mut r);
        assert!(l.iter().all(|v| v.is_finite()));

        // Analyze the last integer second (integer-Hz bins are leakage-free).
        let tail = &l[n - SR as usize..];
        let up = goertzel(tail, 540.0);
        assert!(up > 0.3, "shifted line too small ({up})");
        // Dominant line across the band is exactly 540 Hz (coarse 20 Hz scan).
        let (mut peak_f, mut peak) = (0.0f32, 0.0f32);
        let mut f = 20.0f32;
        while f <= 2_000.0 {
            let g = goertzel(tail, f);
            if g > peak {
                peak = g;
                peak_f = f;
            }
            f += 20.0;
        }
        assert!(
            (peak_f - 540.0).abs() < 1.0,
            "dominant output at {peak_f} Hz, expected 540"
        );
        // Carrier leak and the rejected lower sideband sit well below.
        let carrier = goertzel(tail, 440.0);
        let image = goertzel(tail, 340.0);
        assert!(carrier < 0.1 * up, "carrier leak {carrier} vs {up}");
        assert!(image < 0.1 * up, "image sideband {image} vs {up}");
    }

    #[test]
    fn negative_amt_shifts_down() {
        let mut fx = ShiftFx::new();
        fx.set_param("amt", -100.0);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);

        let n = (2.0 * SR) as usize;
        let mut l = sine(n, 440.0, 0.5);
        let mut r = l.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        let tail = &l[n - SR as usize..];
        let down = goertzel(tail, 340.0);
        let image = goertzel(tail, 540.0);
        assert!(down > 0.3, "down-shifted line too small ({down})");
        assert!(image < 0.1 * down, "image sideband {image} vs {down}");
    }
}
