//! `eq7` effect — 7-band parametric EQ (low shelf, 5 peaking bands, high
//! shelf) ported from the `Eq7` Tone composite in `src/audio/devices.ts`:
//! a series chain of Web Audio biquads with flat `b{i}_(freq|gain|q)` param
//! keys. 100% wet by design (the schema defines no mix param). Coefficients
//! follow the Web Audio spec (RBJ cookbook): shelf bands ignore Q (fixed
//! S = 1 slope), peaking uses Q directly; the browser clamps Q at 0.05.

use crate::traits::EffectDsp;
use crate::util::Smoother;

const BANDS: usize = 7;
/// Default band centers from `EQ7_FREQS` in `src/audio/schema.ts`.
const FREQS: [f32; BANDS] = [60.0, 150.0, 400.0, 1_000.0, 2_500.0, 6_000.0, 12_000.0];

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    LowShelf,
    Peaking,
    HighShelf,
}

fn kind_of(i: usize) -> Kind {
    match i {
        0 => Kind::LowShelf,
        x if x == BANDS - 1 => Kind::HighShelf,
        _ => Kind::Peaking,
    }
}

/// One stereo biquad band (transposed direct form II per channel).
struct Band {
    kind: Kind,
    freq: Smoother,
    gain: Smoother,
    q: Smoother,
    // Normalized coefficients (a0 divided out), recomputed per block.
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    zl1: f32,
    zl2: f32,
    zr1: f32,
    zr2: f32,
}

impl Band {
    fn new(i: usize) -> Self {
        Self {
            kind: kind_of(i),
            freq: Smoother::new(FREQS[i]),
            gain: Smoother::new(0.0),
            q: Smoother::new(1.0),
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            zl1: 0.0,
            zl2: 0.0,
            zr1: 0.0,
            zr2: 0.0,
        }
    }

    fn prepare(&mut self, sample_rate: f32) {
        self.freq.prepare(sample_rate);
        self.gain.prepare(sample_rate);
        self.q.prepare(sample_rate);
        // Graph build: land on the set params immediately, no 15 ms fade-in.
        self.freq.snap(self.freq.target());
        self.gain.snap(self.gain.target());
        self.q.snap(self.q.target());
        self.reset_state();
        self.update_coeffs(sample_rate);
    }

    fn reset_state(&mut self) {
        self.zl1 = 0.0;
        self.zl2 = 0.0;
        self.zr1 = 0.0;
        self.zr2 = 0.0;
    }

    /// Advance the param smoothers by `n` samples (coefficients are updated
    /// once per block; ≤ MAX_BLOCK at ~15 ms smoothing stays click-free).
    fn advance(&mut self, n: usize) {
        for _ in 0..n {
            self.freq.next();
            self.gain.next();
            self.q.next();
        }
    }

    fn update_coeffs(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        // Keep w0 strictly inside (0, π) so the trig terms stay well behaved.
        let f = self.freq.current().clamp(10.0, 0.49 * sr);
        let g = self.gain.current().clamp(-24.0, 24.0);
        // Browser side clamps Q at 0.05 (Eq7.setParam in devices.ts).
        let q = self.q.current().max(0.05);

        let w0 = core::f32::consts::TAU * f / sr;
        let cw = libm::cosf(w0);
        let sw = libm::sinf(w0);
        let a = libm::powf(10.0, g / 40.0);

        let (b0, b1, b2, a0, a1, a2) = match self.kind {
            Kind::Peaking => {
                let alpha = sw / (2.0 * q);
                (
                    1.0 + alpha * a,
                    -2.0 * cw,
                    1.0 - alpha * a,
                    1.0 + alpha / a,
                    -2.0 * cw,
                    1.0 - alpha / a,
                )
            }
            Kind::LowShelf => {
                // Web Audio shelves use fixed slope S = 1 and ignore Q.
                let aa = sw * libm::sqrtf(2.0 * a); // = 2·√A·α with α = sin/2·√2
                (
                    a * ((a + 1.0) - (a - 1.0) * cw + aa),
                    2.0 * a * ((a - 1.0) - (a + 1.0) * cw),
                    a * ((a + 1.0) - (a - 1.0) * cw - aa),
                    (a + 1.0) + (a - 1.0) * cw + aa,
                    -2.0 * ((a - 1.0) + (a + 1.0) * cw),
                    (a + 1.0) + (a - 1.0) * cw - aa,
                )
            }
            Kind::HighShelf => {
                let aa = sw * libm::sqrtf(2.0 * a);
                (
                    a * ((a + 1.0) + (a - 1.0) * cw + aa),
                    -2.0 * a * ((a - 1.0) + (a + 1.0) * cw),
                    a * ((a + 1.0) + (a - 1.0) * cw - aa),
                    (a + 1.0) - (a - 1.0) * cw + aa,
                    2.0 * ((a - 1.0) - (a + 1.0) * cw),
                    (a + 1.0) - (a - 1.0) * cw - aa,
                )
            }
        };

        let inv = 1.0 / a0;
        self.b0 = b0 * inv;
        self.b1 = b1 * inv;
        self.b2 = b2 * inv;
        self.a1 = a1 * inv;
        self.a2 = a2 * inv;
    }

    #[inline]
    fn tick_stereo(&mut self, l: f32, r: f32) -> (f32, f32) {
        let yl = self.b0 * l + self.zl1;
        self.zl1 = self.b1 * l - self.a1 * yl + self.zl2;
        self.zl2 = self.b2 * l - self.a2 * yl;
        let yr = self.b0 * r + self.zr1;
        self.zr1 = self.b1 * r - self.a1 * yr + self.zr2;
        self.zr2 = self.b2 * r - self.a2 * yr;
        (yl, yr)
    }
}

pub struct Eq7Fx {
    sample_rate: f32,
    bands: [Band; BANDS],
}

impl Eq7Fx {
    pub fn new() -> Self {
        Self {
            sample_rate: 44_100.0,
            bands: core::array::from_fn(Band::new),
        }
    }
}

impl Default for Eq7Fx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for Eq7Fx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        for band in &mut self.bands {
            band.prepare(self.sample_rate);
        }
    }

    fn set_param(&mut self, key: &str, value: f32) {
        // Flat keys: b{i}_freq | b{i}_gain | b{i}_q (alloc-free parse).
        let bytes = key.as_bytes();
        if bytes.len() < 3 || bytes[0] != b'b' || bytes[2] != b'_' {
            return;
        }
        let idx = (bytes[1].wrapping_sub(b'0')) as usize;
        if idx >= BANDS {
            return;
        }
        let band = &mut self.bands[idx];
        match &key[3..] {
            "freq" => band.freq.set_target(value.clamp(10.0, 20_000.0)),
            "gain" => band.gain.set_target(value.clamp(-24.0, 24.0)),
            "q" => band.q.set_target(value.max(0.05)),
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        for band in &mut self.bands {
            band.advance(n);
            band.update_coeffs(self.sample_rate);
        }
        for (ls, rs) in l[..n].iter_mut().zip(r[..n].iter_mut()) {
            let mut sl = *ls;
            let mut sr = *rs;
            for band in &mut self.bands {
                let (yl, yr) = band.tick_stereo(sl, sr);
                sl = yl;
                sr = yr;
            }
            *ls = sl;
            *rs = sr;
        }
    }

    fn reset(&mut self) {
        for band in &mut self.bands {
            band.reset_state();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(params: &[(&str, f32)]) -> Eq7Fx {
        let mut d = Eq7Fx::new();
        for &(k, v) in params {
            d.set_param(k, v);
        }
        d.prepare(48_000.0);
        d
    }

    fn run_blocks(d: &mut Eq7Fx, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        let mut i = 0;
        while i < n {
            let end = (i + 128).min(n);
            let (lc, rc) = (&mut l[i..end], &mut r[i..end]);
            d.process(lc, rc);
            i = end;
        }
    }

    /// Steady-state gain (linear) of the device at `freq` Hz: run a sine for
    /// 1.5 s, compare RMS over the last second against the input RMS.
    fn sine_gain(d: &mut Eq7Fx, freq: f32) -> f32 {
        let sr = 48_000.0f32;
        let n = (1.5 * sr) as usize;
        let mut l: Vec<f32> = (0..n)
            .map(|i| (core::f32::consts::TAU * freq * i as f32 / sr).sin() * 0.5)
            .collect();
        let mut r = l.clone();
        run_blocks(d, &mut l, &mut r);
        let tail = &l[n - sr as usize..];
        let rms = (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt();
        rms / (0.5 / core::f32::consts::SQRT_2)
    }

    fn db(lin: f32) -> f32 {
        20.0 * lin.max(1e-9).log10()
    }

    #[test]
    fn defaults_are_transparent_and_finite() {
        // 2 s of pseudo-random noise through the flat EQ: finite, non-silent,
        // and near-identity (all gains 0 dB).
        let mut d = make(&[]);
        let n = 96_000;
        let mut state = 0x1234_5678u32;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let mut l: Vec<f32> = (0..n).map(|_| next()).collect();
        let mut r = l.clone();
        let l_in = l.clone();
        run_blocks(&mut d, &mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
        let energy: f32 = l.iter().map(|s| s * s).sum();
        assert!(energy > 1.0, "wet output is silent");
        let diff: f32 = l
            .iter()
            .zip(&l_in)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f32::max);
        assert!(diff < 1e-3, "flat EQ should be ~identity, max diff {diff}");
    }

    #[test]
    fn param_sweep_stays_bounded() {
        // Sweep every band across the full schema ranges while processing.
        let mut d = make(&[]);
        let mut phase = 0.0f32;
        for step in 0..80 {
            let t = step as f32 / 79.0;
            for i in 0..BANDS {
                let (fk, gk, qk) = match i {
                    0 => ("b0_freq", "b0_gain", "b0_q"),
                    1 => ("b1_freq", "b1_gain", "b1_q"),
                    2 => ("b2_freq", "b2_gain", "b2_q"),
                    3 => ("b3_freq", "b3_gain", "b3_q"),
                    4 => ("b4_freq", "b4_gain", "b4_q"),
                    5 => ("b5_freq", "b5_gain", "b5_q"),
                    _ => ("b6_freq", "b6_gain", "b6_q"),
                };
                d.set_param(fk, 20.0 + t * (20_000.0 - 20.0));
                d.set_param(gk, -15.0 + t * 30.0);
                d.set_param(qk, 0.3 + t * (12.0 - 0.3));
            }
            let mut l = [0.0f32; 128];
            for s in &mut l {
                phase = (phase + 220.0 / 48_000.0).fract();
                *s = (core::f32::consts::TAU * phase).sin() * 0.5;
            }
            let mut r = l;
            d.process(&mut l, &mut r);
            assert!(
                l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() < 100.0),
                "unbounded output at sweep step {step}"
            );
        }
    }

    #[test]
    fn boosting_one_band_raises_only_that_band() {
        // +12 dB on band 3 (peaking, 1 kHz): ~+12 dB at 1 kHz, the far band
        // centers (60 Hz low shelf, 12 kHz high shelf) stay essentially flat.
        let boost = [("b3_gain", 12.0f32)];
        let gain_at = |f: f32, params: &[(&str, f32)]| {
            let mut d = make(params);
            db(sine_gain(&mut d, f))
        };
        let at_center = gain_at(1_000.0, &boost) - gain_at(1_000.0, &[]);
        let at_low = gain_at(60.0, &boost) - gain_at(60.0, &[]);
        let at_high = gain_at(12_000.0, &boost) - gain_at(12_000.0, &[]);
        assert!(at_center > 10.0, "center boost too small: {at_center} dB");
        assert!(at_low.abs() < 1.0, "60 Hz moved {at_low} dB");
        assert!(at_high.abs() < 1.0, "12 kHz moved {at_high} dB");
    }

    #[test]
    fn shelves_shape_the_spectrum_ends() {
        // Low shelf cut pulls down 60 Hz, leaves 12 kHz alone; high shelf
        // boost raises 12 kHz, leaves 60 Hz alone.
        let mut d = make(&[("b0_gain", -12.0)]);
        assert!(db(sine_gain(&mut d, 40.0)) < -9.0);
        let mut d = make(&[("b0_gain", -12.0)]);
        assert!(db(sine_gain(&mut d, 12_000.0)).abs() < 1.0);
        let mut d = make(&[("b6_gain", 9.0)]);
        assert!(db(sine_gain(&mut d, 16_000.0)) > 6.0);
        let mut d = make(&[("b6_gain", 9.0)]);
        assert!(db(sine_gain(&mut d, 60.0)).abs() < 1.0);
    }

    #[test]
    fn param_smoothing_avoids_instant_jumps() {
        // A gain jump mid-stream must not land within one block (~15 ms one-
        // pole): the first post-change block's coefficients are still moving.
        let mut d = make(&[]);
        let sr = 48_000.0f32;
        let freq = 1_000.0f32;
        let sine = |i: usize| (core::f32::consts::TAU * freq * i as f32 / sr).sin() * 0.5;
        let mut l: Vec<f32> = (0..4_800).map(sine).collect();
        let mut r = l.clone();
        run_blocks(&mut d, &mut l, &mut r);
        d.set_param("b3_gain", 12.0);
        let mut l2 = [0.0f32; 128];
        for (i, s) in l2.iter_mut().enumerate() {
            *s = sine(4_800 + i);
        }
        let mut r2 = l2;
        d.process(&mut l2, &mut r2);
        let peak = l2.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        // Full +12 dB on a 0.5 peak sine would approach 2.0.
        assert!(peak < 1.0, "gain jump was not smoothed: peak {peak}");
    }

    #[test]
    fn reset_clears_state_and_bogus_keys_are_ignored() {
        let mut d = make(&[("b3_gain", 12.0)]);
        d.set_param("nope", 1.0);
        d.set_param("b9_gain", 12.0);
        d.set_param("b3_wat", 1.0);
        let mut l = [1.0f32; 64];
        let mut r = [1.0f32; 64];
        d.process(&mut l, &mut r);
        d.reset();
        let mut l2 = [0.0f32; 64];
        let mut r2 = [0.0f32; 64];
        d.process(&mut l2, &mut r2);
        assert!(l2.iter().all(|s| *s == 0.0), "state not cleared by reset");
        assert!(r2.iter().all(|s| *s == 0.0));
    }
}
