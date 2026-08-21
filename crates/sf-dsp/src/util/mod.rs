//! Allocation-free DSP building blocks shared by every device
//! (RUST_ARCH.md "Rules for every device"). Everything here is RT-safe after
//! construction: no alloc, no locks, no panics in the sample loops.

mod adsr;
mod delay;
mod ladder;
mod lfo;
mod limiter;
mod onepole;
mod osc;
mod oversample;
mod smoother;
mod svf;

pub use adsr::Adsr;
pub use delay::DelayLine;
pub use ladder::Ladder;
pub use lfo::{lfo_shape_value, Lfo};
pub use limiter::Limiter;
pub use onepole::{OnePole, OnePoleMode};
pub use osc::{PolyBlepOsc, Wave};
pub use oversample::Oversampler4;
pub use smoother::Smoother;
pub use svf::{Svf, SvfMode};

/// Maximum frames per processed block (RUST_ARCH.md "Block model").
pub const MAX_BLOCK: usize = 128;

/// Decibels → linear gain.
#[inline]
pub fn db_to_lin(db: f32) -> f32 {
    libm::powf(10.0, db * 0.05)
}

/// Linear gain → decibels (floored at −120 dB so silence never yields −inf).
#[inline]
pub fn lin_to_db(lin: f32) -> f32 {
    lin_to_db_floor(lin, -120.0)
}

#[inline]
pub fn lin_to_db_floor(lin: f32, floor_db: f32) -> f32 {
    if lin <= 0.0 {
        return floor_db;
    }
    let db = 20.0 * libm::log10f(lin);
    if db < floor_db {
        floor_db
    } else {
        db
    }
}

/// MIDI note number → frequency in Hz (A4 = 69 = 440 Hz).
#[inline]
pub fn midi_hz(pitch: f32) -> f32 {
    440.0 * libm::powf(2.0, (pitch - 69.0) / 12.0)
}

/// Equal-power pan: `pan` in [-1, 1] → (left gain, right gain).
/// Center (0) gives cos(π/4) ≈ 0.707 per side; full L/R gives (1, 0)/(0, 1).
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    let a = (p + 1.0) * core::f32::consts::FRAC_PI_4;
    (libm::cosf(a), libm::sinf(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_lin_roundtrip() {
        assert!((db_to_lin(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_lin(-6.0) - 0.501_187).abs() < 1e-4);
        assert!((lin_to_db(db_to_lin(-13.5)) + 13.5).abs() < 1e-3);
        assert_eq!(lin_to_db(0.0), -120.0);
    }

    #[test]
    fn midi_hz_reference_points() {
        assert!((midi_hz(69.0) - 440.0).abs() < 1e-3);
        assert!((midi_hz(57.0) - 220.0).abs() < 1e-3);
        assert!((midi_hz(60.0) - 261.6256).abs() < 1e-2);
    }

    #[test]
    fn pan_is_equal_power() {
        let (l, r) = pan_gains(0.0);
        assert!((l - r).abs() < 1e-6);
        assert!((l * l + r * r - 1.0).abs() < 1e-5);
        let (l, r) = pan_gains(-1.0);
        assert!((l - 1.0).abs() < 1e-6 && r.abs() < 1e-6);
        let (l, r) = pan_gains(1.0);
        assert!(l.abs() < 1e-6 && (r - 1.0).abs() < 1e-6);
    }
}
