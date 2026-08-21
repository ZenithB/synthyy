//! 4× oversampling wrapper for nonlinear devices (dist, crush, cheby —
//! RUST_ARCH.md). Two cascaded 2× polyphase halfband FIR stages each way:
//! sharp 23-tap at the low rate, relaxed 11-tap at the high rate (~34 taps
//! total). All buffers sized for `MAX_BLOCK` at construction; `process` is
//! allocation-free.

use super::MAX_BLOCK;

/// Windowed-sinc halfband taps (Blackman). Odd length, center tap 0.5,
/// every other side tap identically zero. Passband gain ≈ 1.
fn halfband_taps(n: usize) -> Vec<f32> {
    debug_assert!(n % 2 == 1);
    let m = (n - 1) / 2;
    let mut h = vec![0.0f32; n];
    for (j, tap) in h.iter_mut().enumerate() {
        let k = j as isize - m as isize;
        if k == 0 {
            *tap = 0.5;
            continue;
        }
        if k % 2 == 0 {
            continue; // halfband zeros
        }
        let x = core::f64::consts::PI * k as f64 * 0.5;
        let ideal = libm::sin(x) / (2.0 * x); // sin(πk/2)/(πk)
        let w = 0.42 - 0.5 * libm::cos(core::f64::consts::TAU * j as f64 / (n - 1) as f64)
            + 0.08 * libm::cos(2.0 * core::f64::consts::TAU * j as f64 / (n - 1) as f64);
        *tap = (ideal * w) as f32;
    }
    // Normalize the side taps so each polyphase branch has exactly unity DC
    // gain (side sum 0.5, center 0.5 ⇒ H(0) = 1). Keeps cascade gain honest.
    let side: f32 = h
        .iter()
        .enumerate()
        .filter(|(j, _)| *j != m)
        .map(|(_, t)| *t)
        .sum();
    if side > 0.0 {
        let scale = 0.5 / side;
        for (j, tap) in h.iter_mut().enumerate() {
            if j != m {
                *tap *= scale;
            }
        }
    }
    h
}

/// Polyphase 2× upsampler: even output phase = FIR over the even taps (×2),
/// odd output phase = pure delayed input (center tap 0.5 × 2).
struct Up2 {
    even_taps: Vec<f32>, // h[0], h[2], … scaled ×2
    hist: Vec<f32>,
    pos: usize,
    center_delay: usize, // (M − 1) / 2 at the input rate
}

impl Up2 {
    fn new(n: usize) -> Self {
        let h = halfband_taps(n);
        let even_taps: Vec<f32> = h.iter().step_by(2).map(|t| t * 2.0).collect();
        let m = (n - 1) / 2;
        let len = even_taps.len().max(m / 2 + 1).next_power_of_two();
        Self {
            even_taps,
            hist: vec![0.0; len],
            pos: 0,
            center_delay: (m - 1) / 2,
        }
    }

    fn reset(&mut self) {
        self.hist.fill(0.0);
        self.pos = 0;
    }

    /// One input sample → two output samples.
    #[inline]
    fn push(&mut self, x: f32) -> (f32, f32) {
        let mask = self.hist.len() - 1;
        self.hist[self.pos] = x;
        let mut acc = 0.0f32;
        for (i, t) in self.even_taps.iter().enumerate() {
            acc += t * self.hist[(self.pos + self.hist.len() - i) & mask];
        }
        let odd = self.hist[(self.pos + self.hist.len() - self.center_delay) & mask];
        self.pos = (self.pos + 1) & mask;
        (acc, odd)
    }
}

/// Polyphase 2× downsampler: even-phase FIR + center tap on the odd phase.
struct Down2 {
    even_taps: Vec<f32>, // h[0], h[2], … unscaled
    hist_even: Vec<f32>,
    hist_odd: Vec<f32>,
    pos_e: usize,
    pos_o: usize,
    center_delay: usize, // (M + 1) / 2 in odd-phase steps
}

impl Down2 {
    fn new(n: usize) -> Self {
        let h = halfband_taps(n);
        let even_taps: Vec<f32> = h.iter().step_by(2).copied().collect();
        let m = (n - 1) / 2;
        let center_delay = m.div_ceil(2);
        let le = even_taps.len().next_power_of_two();
        let lo = (center_delay + 1).next_power_of_two();
        Self {
            even_taps,
            hist_even: vec![0.0; le],
            hist_odd: vec![0.0; lo],
            pos_e: 0,
            pos_o: 0,
            center_delay,
        }
    }

    fn reset(&mut self) {
        self.hist_even.fill(0.0);
        self.hist_odd.fill(0.0);
        self.pos_e = 0;
        self.pos_o = 0;
    }

    /// Two high-rate samples → one output sample.
    #[inline]
    fn push_pair(&mut self, v0: f32, v1: f32) -> f32 {
        let me = self.hist_even.len() - 1;
        let mo = self.hist_odd.len() - 1;
        self.hist_even[self.pos_e] = v0;
        self.hist_odd[self.pos_o] = v1;
        let mut acc = 0.0f32;
        for (i, t) in self.even_taps.iter().enumerate() {
            acc += t * self.hist_even[(self.pos_e + self.hist_even.len() - i) & me];
        }
        acc += 0.5 * self.hist_odd[(self.pos_o + self.hist_odd.len() - self.center_delay) & mo];
        self.pos_e = (self.pos_e + 1) & me;
        self.pos_o = (self.pos_o + 1) & mo;
        acc
    }
}

const SHARP_TAPS: usize = 23; // stage at the base rate
const RELAX_TAPS: usize = 11; // stage at 2× (wider transition band available)

pub struct Oversampler4 {
    up1_l: Up2,
    up1_r: Up2,
    up2_l: Up2,
    up2_r: Up2,
    dn2_l: Down2,
    dn2_r: Down2,
    dn1_l: Down2,
    dn1_r: Down2,
    buf2_l: Vec<f32>,
    buf2_r: Vec<f32>,
    buf4_l: Vec<f32>,
    buf4_r: Vec<f32>,
}

impl Oversampler4 {
    pub fn new() -> Self {
        Self {
            up1_l: Up2::new(SHARP_TAPS),
            up1_r: Up2::new(SHARP_TAPS),
            up2_l: Up2::new(RELAX_TAPS),
            up2_r: Up2::new(RELAX_TAPS),
            dn2_l: Down2::new(RELAX_TAPS),
            dn2_r: Down2::new(RELAX_TAPS),
            dn1_l: Down2::new(SHARP_TAPS),
            dn1_r: Down2::new(SHARP_TAPS),
            buf2_l: vec![0.0; 2 * MAX_BLOCK],
            buf2_r: vec![0.0; 2 * MAX_BLOCK],
            buf4_l: vec![0.0; 4 * MAX_BLOCK],
            buf4_r: vec![0.0; 4 * MAX_BLOCK],
        }
    }

    pub fn reset(&mut self) {
        self.up1_l.reset();
        self.up1_r.reset();
        self.up2_l.reset();
        self.up2_r.reset();
        self.dn2_l.reset();
        self.dn2_r.reset();
        self.dn1_l.reset();
        self.dn1_r.reset();
        self.buf2_l.fill(0.0);
        self.buf2_r.fill(0.0);
        self.buf4_l.fill(0.0);
        self.buf4_r.fill(0.0);
    }

    /// Upsample 4×, run `f` on the high-rate block, downsample back in place.
    /// `l`/`r` must be the same length, ≤ `MAX_BLOCK`.
    pub fn process(
        &mut self,
        l: &mut [f32],
        r: &mut [f32],
        mut f: impl FnMut(&mut [f32], &mut [f32]),
    ) {
        let n = l.len().min(r.len()).min(MAX_BLOCK);
        debug_assert!(l.len() == r.len() && l.len() <= MAX_BLOCK);

        for i in 0..n {
            let (a, b) = self.up1_l.push(l[i]);
            self.buf2_l[2 * i] = a;
            self.buf2_l[2 * i + 1] = b;
            let (a, b) = self.up1_r.push(r[i]);
            self.buf2_r[2 * i] = a;
            self.buf2_r[2 * i + 1] = b;
        }
        for i in 0..2 * n {
            let (a, b) = self.up2_l.push(self.buf2_l[i]);
            self.buf4_l[2 * i] = a;
            self.buf4_l[2 * i + 1] = b;
            let (a, b) = self.up2_r.push(self.buf2_r[i]);
            self.buf4_r[2 * i] = a;
            self.buf4_r[2 * i + 1] = b;
        }

        f(&mut self.buf4_l[..4 * n], &mut self.buf4_r[..4 * n]);

        for i in 0..2 * n {
            self.buf2_l[i] = self.dn2_l.push_pair(self.buf4_l[2 * i], self.buf4_l[2 * i + 1]);
            self.buf2_r[i] = self.dn2_r.push_pair(self.buf4_r[2 * i], self.buf4_r[2 * i + 1]);
        }
        for i in 0..n {
            l[i] = self.dn1_l.push_pair(self.buf2_l[2 * i], self.buf2_l[2 * i + 1]);
            r[i] = self.dn1_r.push_pair(self.buf2_r[2 * i], self.buf2_r[2 * i + 1]);
        }
    }
}

impl Default for Oversampler4 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rms(buf: &[f32]) -> f32 {
        libm::sqrtf(buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32)
    }

    #[test]
    fn identity_closure_preserves_sine() {
        let sr = 44_100.0;
        let mut os = Oversampler4::new();
        let mut out = Vec::new();
        let mut phase = 0.0f32;
        for _ in 0..40 {
            let mut l = [0.0f32; MAX_BLOCK];
            let mut r = [0.0f32; MAX_BLOCK];
            for s in l.iter_mut() {
                *s = libm::sinf(phase * core::f32::consts::TAU);
                phase += 1_000.0 / sr;
                phase -= libm::floorf(phase);
            }
            r.copy_from_slice(&l);
            os.process(&mut l, &mut r, |_, _| {});
            out.extend_from_slice(&l);
        }
        // skip filter warmup, then RMS must match a unit sine (±0.5 dB)
        let steady = &out[MAX_BLOCK * 4..];
        let got = rms(steady);
        let want = core::f32::consts::FRAC_1_SQRT_2;
        let db = 20.0 * libm::log10f(got / want);
        assert!(db.abs() < 0.5, "identity path off by {db:.2} dB");
        assert!(steady.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn upsampled_block_is_4x_and_unity_gain() {
        let sr = 44_100.0;
        let mut os = Oversampler4::new();
        let mut hi_rms = 0.0f32;
        let mut hi_len = 0usize;
        let mut phase = 0.0f32;
        for pass in 0..20 {
            let mut l = [0.0f32; 64];
            let mut r = [0.0f32; 64];
            for s in l.iter_mut() {
                *s = libm::sinf(phase * core::f32::consts::TAU);
                phase += 440.0 / sr;
                phase -= libm::floorf(phase);
            }
            r.copy_from_slice(&l);
            os.process(&mut l, &mut r, |hl, hr| {
                assert_eq!(hl.len(), 256);
                assert_eq!(hr.len(), 256);
                if pass > 4 {
                    hi_rms += hl.iter().map(|s| s * s).sum::<f32>();
                    hi_len += hl.len();
                }
            });
        }
        let got = libm::sqrtf(hi_rms / hi_len as f32);
        let db = 20.0 * libm::log10f(got / core::f32::consts::FRAC_1_SQRT_2);
        assert!(db.abs() < 0.5, "high-rate gain off by {db:.2} dB");
    }

    #[test]
    fn hard_clip_at_4x_stays_bounded() {
        let mut os = Oversampler4::new();
        for _ in 0..10 {
            let mut l = [0.9f32; MAX_BLOCK];
            let mut r = [-0.9f32; MAX_BLOCK];
            os.process(&mut l, &mut r, |hl, hr| {
                for s in hl.iter_mut().chain(hr.iter_mut()) {
                    *s = s.clamp(-0.5, 0.5);
                }
            });
            assert!(l.iter().chain(r.iter()).all(|s| s.is_finite() && s.abs() < 1.0));
        }
    }
}
