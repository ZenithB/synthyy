//! LFO shapes — exact mirror of `lfoShapeValue` in src/audio/schema.ts
//! (indices are the frozen `LFO_SHAPES` order):
//!   0 Sine, 1 Triangle, 2 Saw ↑, 3 Saw ↓, 4 Square, 5 S&H, 6 Random
//! Math is f64 to match the JS engine bit-for-bit-ish: the S&H hash must
//! agree across collaborators (deterministic sin-based hash).

/// Deterministic [0,1) hash — port of `lfoHash` in schema.ts.
#[inline]
fn lfo_hash(n: f64) -> f64 {
    let x = libm::sin(n * 127.1 + 311.7) * 43758.5453;
    x - libm::floor(x)
}

/// LFO waveform value in [-1, 1] for a given shape index and (unwrapped)
/// phase. Out-of-range shapes fall back to sine, as in the JS source.
pub fn lfo_shape_value(shape: u32, phase: f64) -> f32 {
    let f = phase - libm::floor(phase); // [0, 1)
    let v = match shape {
        1 => {
            if f < 0.5 {
                4.0 * f - 1.0
            } else {
                3.0 - 4.0 * f
            }
        }
        2 => 2.0 * f - 1.0,
        3 => 1.0 - 2.0 * f,
        4 => {
            if f < 0.5 {
                1.0
            } else {
                -1.0
            }
        }
        5 => lfo_hash(libm::floor(phase)) * 2.0 - 1.0,
        6 => {
            let a = lfo_hash(libm::floor(phase));
            let b = lfo_hash(libm::floor(phase) + 1.0);
            let t = f * f * (3.0 - 2.0 * f); // smoothstep
            (a + (b - a) * t) * 2.0 - 1.0
        }
        _ => libm::sin(f * core::f64::consts::TAU),
    };
    v as f32
}

/// Free-running LFO: advances an unwrapped phase so S&H/Random keep their
/// per-cycle hash identity. Phase offset is applied at evaluation time.
pub struct Lfo {
    shape: u32,
    phase: f64, // unwrapped cycles
    step: f64,  // cycles per sample
    offset: f64,
    sample_rate: f64,
}

impl Lfo {
    pub fn new() -> Self {
        Self {
            shape: 0,
            phase: 0.0,
            step: 0.0,
            offset: 0.0,
            sample_rate: 44_100.0,
        }
    }

    pub fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0) as f64;
    }

    pub fn set_shape(&mut self, shape: u32) {
        self.shape = shape;
    }

    pub fn set_freq(&mut self, freq_hz: f32) {
        self.step = (freq_hz.max(0.0) as f64) / self.sample_rate;
    }

    /// Phase offset in cycles [0, 1] (schema `phase` param).
    pub fn set_phase_offset(&mut self, offset: f32) {
        self.offset = offset as f64;
    }

    /// Jump the unwrapped phase (transport relocate / retrigger).
    pub fn set_phase(&mut self, cycles: f64) {
        self.phase = cycles;
    }

    pub fn phase(&self) -> f64 {
        self.phase
    }

    // `next` is the contract name (RUST_ARCH.md: set_target/next/snap), not an Iterator
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        let v = lfo_shape_value(self.shape, self.phase + self.offset);
        self.phase += self.step;
        v
    }

    /// Evaluate at an explicit unwrapped phase (tempo-synced use).
    #[inline]
    pub fn value_at(&self, cycles: f64) -> f32 {
        lfo_shape_value(self.shape, cycles + self.offset)
    }
}

impl Default for Lfo {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_match_schema_reference_points() {
        // sine
        assert!(lfo_shape_value(0, 0.25) - 1.0 < 1e-6);
        assert!(lfo_shape_value(0, 0.0).abs() < 1e-6);
        // triangle: -1 at 0, +1 at 0.5, back to -1
        assert!((lfo_shape_value(1, 0.0) + 1.0).abs() < 1e-6);
        assert!((lfo_shape_value(1, 0.5) - 1.0).abs() < 1e-6);
        assert!((lfo_shape_value(1, 0.25)).abs() < 1e-6);
        // saw up / saw down
        assert!((lfo_shape_value(2, 0.75) - 0.5).abs() < 1e-6);
        assert!((lfo_shape_value(3, 0.75) + 0.5).abs() < 1e-6);
        // square
        assert_eq!(lfo_shape_value(4, 0.25), 1.0);
        assert_eq!(lfo_shape_value(4, 0.75), -1.0);
    }

    #[test]
    fn sample_and_hold_is_deterministic_per_cycle() {
        let a = lfo_shape_value(5, 3.1);
        let b = lfo_shape_value(5, 3.9);
        let c = lfo_shape_value(5, 4.1);
        assert_eq!(a, b, "S&H must hold within a cycle");
        assert_ne!(a, c, "S&H must change across cycles");
        assert!((-1.0..=1.0).contains(&a));
    }

    #[test]
    fn random_is_smooth_and_bounded() {
        let mut prev = lfo_shape_value(6, 0.0);
        for i in 1..1_000 {
            let v = lfo_shape_value(6, i as f64 * 0.01);
            assert!((-1.0..=1.0).contains(&v));
            assert!((v - prev).abs() < 0.1, "random shape jumped at step {i}");
            prev = v;
        }
    }

    #[test]
    fn struct_advances_at_rate() {
        let mut l = Lfo::new();
        l.prepare(1_000.0);
        l.set_shape(2); // saw up: value == 2·frac(phase) − 1
        l.set_freq(10.0); // 100 samples per cycle
        for _ in 0..50 {
            l.next();
        }
        assert!((l.phase() - 0.5).abs() < 1e-9);
        assert!(l.value_at(l.phase()).abs() < 1e-6); // mid-cycle saw = 0
    }
}
