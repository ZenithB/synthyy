//! Fixed-maximum interpolated delay line. Allocates its buffer once in
//! `new()`; `write`/`read_frac` are RT-safe.

pub struct DelayLine {
    buf: Vec<f32>,
    mask: usize, // power-of-two length - 1
    write_pos: usize,
}

impl DelayLine {
    /// `max_samples` is rounded up to the next power of two.
    pub fn new(max_samples: usize) -> Self {
        let len = max_samples.max(2).next_power_of_two();
        Self {
            buf: vec![0.0; len],
            mask: len - 1,
            write_pos: 0,
        }
    }

    pub fn max_delay(&self) -> usize {
        self.buf.len() - 1
    }

    pub fn clear(&mut self) {
        self.buf.fill(0.0);
        self.write_pos = 0;
    }

    #[inline]
    pub fn write(&mut self, x: f32) {
        self.buf[self.write_pos] = x;
        self.write_pos = (self.write_pos + 1) & self.mask;
    }

    /// Read `delay` samples behind the most recent `write` (1.0 = the sample
    /// written one call ago), linearly interpolated. Clamped to the buffer.
    #[inline]
    pub fn read_frac(&self, delay: f32) -> f32 {
        let d = delay.clamp(1.0, self.max_delay() as f32);
        let di = d as usize;
        let frac = d - di as f32;
        let i0 = (self.write_pos + self.buf.len() - di) & self.mask;
        let i1 = (i0 + self.buf.len() - 1) & self.mask;
        let s0 = self.buf[i0];
        let s1 = self.buf[i1];
        s0 + (s1 - s0) * frac
    }

    /// Integer-delay read (no interpolation).
    #[inline]
    pub fn read(&self, delay: usize) -> f32 {
        let d = delay.clamp(1, self.max_delay());
        self.buf[(self.write_pos + self.buf.len() - d) & self.mask]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_at_integer_delay() {
        let mut d = DelayLine::new(64);
        for i in 0..200 {
            d.write(i as f32);
        }
        // last write was 199 at delay 1
        assert_eq!(d.read(1), 199.0);
        assert_eq!(d.read(10), 190.0);
        assert_eq!(d.read_frac(10.0), 190.0);
    }

    #[test]
    fn fractional_read_interpolates() {
        let mut d = DelayLine::new(32);
        for i in 0..40 {
            d.write(i as f32);
        }
        // halfway between delay 5 (value 35) and delay 6 (value 34)
        let v = d.read_frac(5.5);
        assert!((v - 34.5).abs() < 1e-5, "got {v}");
    }

    #[test]
    fn no_alloc_after_new_and_clamps() {
        let mut d = DelayLine::new(100); // rounds to 128
        assert_eq!(d.max_delay(), 127);
        for _ in 0..1_000 {
            d.write(1.0);
        }
        // out-of-range delays clamp instead of panicking
        assert_eq!(d.read_frac(1e9), 1.0);
        assert_eq!(d.read_frac(0.0), 1.0);
        d.clear();
        assert_eq!(d.read(1), 0.0);
    }
}
