//! One-pole parameter smoother (~15 ms default) — every audible param goes
//! through one of these unless inherently stepped (RUST_ARCH.md).

pub struct Smoother {
    value: f32,
    target: f32,
    coeff: f32,
    time_ms: f32,
    sample_rate: f32,
}

impl Smoother {
    pub const DEFAULT_MS: f32 = 15.0;

    pub fn new(initial: f32) -> Self {
        let mut s = Self {
            value: initial,
            target: initial,
            coeff: 0.0,
            time_ms: Self::DEFAULT_MS,
            sample_rate: 44_100.0,
        };
        s.update_coeff();
        s
    }

    pub fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.update_coeff();
    }

    /// Smoothing time constant in milliseconds (default 15 ms).
    pub fn set_time_ms(&mut self, ms: f32) {
        self.time_ms = ms.max(0.0);
        self.update_coeff();
    }

    fn update_coeff(&mut self) {
        let samples = self.time_ms * 1e-3 * self.sample_rate;
        self.coeff = if samples < 1.0 {
            0.0
        } else {
            libm::expf(-1.0 / samples)
        };
    }

    #[inline]
    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Jump straight to `value` (graph build, reset — never mid-block).
    #[inline]
    pub fn snap(&mut self, value: f32) {
        self.value = value;
        self.target = value;
    }

    #[inline]
    pub fn target(&self) -> f32 {
        self.target
    }

    #[inline]
    pub fn current(&self) -> f32 {
        self.value
    }

    // `next` is the contract name (RUST_ARCH.md: set_target/next/snap), not an Iterator
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        self.value = self.target + (self.value - self.target) * self.coeff;
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converges_to_target() {
        let mut s = Smoother::new(0.0);
        s.prepare(48_000.0);
        s.set_target(1.0);
        let mut v = 0.0;
        for _ in 0..48_000 {
            v = s.next();
        }
        assert!((v - 1.0).abs() < 1e-4, "did not converge: {v}");
    }

    #[test]
    fn reaches_63_percent_at_time_constant() {
        let sr = 48_000.0;
        let mut s = Smoother::new(0.0);
        s.prepare(sr);
        s.set_target(1.0);
        let n = (Smoother::DEFAULT_MS * 1e-3 * sr) as usize;
        let mut v = 0.0;
        for _ in 0..n {
            v = s.next();
        }
        assert!((v - 0.632).abs() < 0.02, "one tau should be ~63%: {v}");
    }

    #[test]
    fn snap_is_instant_and_monotonic_rise() {
        let mut s = Smoother::new(0.5);
        s.prepare(44_100.0);
        s.snap(0.2);
        assert_eq!(s.next(), 0.2);
        s.set_target(0.9);
        let mut prev = 0.2;
        for _ in 0..1000 {
            let v = s.next();
            assert!(v >= prev && v <= 0.9 + 1e-6);
            prev = v;
        }
    }
}
