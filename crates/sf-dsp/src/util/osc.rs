//! PolyBLEP-antialiased oscillator. Wave indices mirror the frozen `WAVES`
//! order in src/audio/schema.ts:
//!   0 sawtooth, 1 square, 2 triangle, 3 sine, 4 fatsawtooth, 5 fatsquare,
//!   6 fattriangle
//! The "fat" variants are the same waveform — Tone fattens by layering
//! detuned unison voices, which is a device concern (spread param), so
//! `Wave::from_index` maps them onto their base shape.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Wave {
    #[default]
    Saw,
    Square,
    Triangle,
    Sine,
}

impl Wave {
    /// Map a frozen `WAVES` index (schema `wave` param) to a shape.
    pub fn from_index(i: usize) -> Self {
        match i {
            1 | 5 => Wave::Square,
            2 | 6 => Wave::Triangle,
            3 => Wave::Sine,
            _ => Wave::Saw, // 0, 4, and out-of-range default to saw (index 0)
        }
    }
}

pub struct PolyBlepOsc {
    wave: Wave,
    phase: f32, // [0, 1)
    step: f32,  // freq / sample_rate
    sample_rate: f32,
    tri_state: f32, // leaky integrator for the BLEP triangle
}

impl PolyBlepOsc {
    pub fn new(wave: Wave) -> Self {
        Self {
            wave,
            phase: 0.0,
            step: 0.0,
            sample_rate: 44_100.0,
            tri_state: 0.0,
        }
    }

    pub fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
    }

    pub fn set_wave(&mut self, wave: Wave) {
        if wave != self.wave {
            self.wave = wave;
            self.tri_state = 0.0;
        }
    }

    /// Phase-continuous frequency change (only the step size moves).
    #[inline]
    pub fn set_freq(&mut self, freq_hz: f32) {
        self.step = (freq_hz / self.sample_rate).clamp(0.0, 0.5);
    }

    pub fn reset_phase(&mut self, phase: f32) {
        self.phase = phase - libm::floorf(phase);
        self.tri_state = 0.0;
    }

    /// Two-sample polynomial band-limited step correction around phase 0.
    #[inline]
    fn poly_blep(t: f32, dt: f32) -> f32 {
        if t < dt {
            let x = t / dt;
            x + x - x * x - 1.0
        } else if t > 1.0 - dt {
            let x = (t - 1.0) / dt;
            x * x + x + x + 1.0
        } else {
            0.0
        }
    }

    // `next` is the contract name (RUST_ARCH.md: set_target/next/snap), not an Iterator
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        let t = self.phase;
        let dt = self.step.max(1e-9);
        let out = match self.wave {
            Wave::Sine => libm::sinf(t * core::f32::consts::TAU),
            Wave::Saw => 2.0 * t - 1.0 - Self::poly_blep(t, dt),
            Wave::Square => {
                let mut s = if t < 0.5 { 1.0 } else { -1.0 };
                s += Self::poly_blep(t, dt);
                let t2 = t + 0.5;
                s - Self::poly_blep(t2 - libm::floorf(t2), dt)
            }
            Wave::Triangle => {
                // Leaky-integrated BLEP square: gain 4·dt keeps unit amplitude.
                let mut s = if t < 0.5 { 1.0 } else { -1.0 };
                s += Self::poly_blep(t, dt);
                let t2 = t + 0.5;
                s -= Self::poly_blep(t2 - libm::floorf(t2), dt);
                self.tri_state = 4.0 * dt * s + (1.0 - 4.0 * dt) * self.tri_state;
                self.tri_state
            }
        };
        self.phase += self.step;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero_crossings(buf: &[f32]) -> usize {
        buf.windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count()
    }

    fn rms(buf: &[f32]) -> f32 {
        libm::sqrtf(buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32)
    }

    fn render(wave: Wave, freq: f32, sr: f32, n: usize) -> Vec<f32> {
        let mut o = PolyBlepOsc::new(wave);
        o.prepare(sr);
        o.set_freq(freq);
        (0..n).map(|_| o.next()).collect()
    }

    #[test]
    fn pitch_via_zero_crossings() {
        let sr = 44_100.0;
        for wave in [Wave::Sine, Wave::Saw, Wave::Square, Wave::Triangle] {
            let buf = render(wave, 440.0, sr, sr as usize);
            let zc = zero_crossings(&buf);
            // 2 crossings per cycle; triangle integrator settles over a few cycles
            assert!(
                (zc as f32 - 880.0).abs() < 20.0,
                "{wave:?}: {zc} crossings for 440 Hz"
            );
        }
    }

    #[test]
    fn rms_sanity_saw_square() {
        let buf = render(Wave::Saw, 220.0, 44_100.0, 44_100);
        let r = rms(&buf);
        assert!((r - 0.577).abs() < 0.05, "saw rms {r}");
        let buf = render(Wave::Square, 220.0, 44_100.0, 44_100);
        let r = rms(&buf);
        assert!((r - 1.0).abs() < 0.05, "square rms {r}");
    }

    #[test]
    fn output_bounded_and_finite_at_high_freq() {
        for wave in [Wave::Sine, Wave::Saw, Wave::Square, Wave::Triangle] {
            let buf = render(wave, 8_000.0, 44_100.0, 8_192);
            assert!(buf.iter().all(|s| s.is_finite() && s.abs() < 1.5), "{wave:?}");
        }
    }

    #[test]
    fn freq_change_is_phase_continuous() {
        let mut o = PolyBlepOsc::new(Wave::Sine);
        o.prepare(48_000.0);
        o.set_freq(440.0);
        let mut prev = 0.0f32;
        for i in 0..4_096 {
            if i == 2_048 {
                o.set_freq(880.0);
            }
            let v = o.next();
            // sine at these rates never jumps more than ~2π·f/sr per sample
            assert!((v - prev).abs() < 0.13, "discontinuity at {i}");
            prev = v;
        }
    }

    #[test]
    fn wave_index_mapping_matches_frozen_waves_order() {
        assert_eq!(Wave::from_index(0), Wave::Saw);
        assert_eq!(Wave::from_index(1), Wave::Square);
        assert_eq!(Wave::from_index(2), Wave::Triangle);
        assert_eq!(Wave::from_index(3), Wave::Sine);
        assert_eq!(Wave::from_index(4), Wave::Saw);
        assert_eq!(Wave::from_index(5), Wave::Square);
        assert_eq!(Wave::from_index(6), Wave::Triangle);
    }
}
