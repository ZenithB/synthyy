//! All device DSP: pure `process(&mut [f32], ...)` sample loops, no I/O, no
//! allocation after `prepare()`. Ports land here phase by phase
//! (RUST_REWRITE_PLAN.md §5): worklets from `public/sf-fx.js` / `sf-mbdyn.js`
//! first (canonical), then synths, then the Tone-composite effects by spec.
//!
//! Every device follows the same shape:
//!   `prepare(sample_rate)` — allocate/derive everything up front
//!   `process(block)`       — RT-safe: no alloc, no locks, no syscalls
//!   `set_param(key, value)` — param keys are the frozen schema's keys
//!
//! Layout (docs/RUST_ARCH.md):
//!   `traits` — the frozen `EffectDsp` / `InstrumentDsp` contract
//!   `util`   — shared allocation-free building blocks (Smoother, PolyBLEP
//!              osc, SVF, ladder, ADSR, delay line, LFO, one-pole, 4×
//!              oversampler, dB/pan/MIDI helpers, `MAX_BLOCK`)
//!   `inst` / `fx` — one file per device + the pre-wired registries
//!              (`make_instrument` / `make_effect` cover every frozen type
//!              string; device files start as stubs and are replaced in the
//!              device-port wave without touching the shared mod.rs files)

pub mod fx;
pub mod inst;
pub mod sample;
pub mod traits;
pub mod util;

pub use fx::make_effect;
pub use inst::make_instrument;
pub use traits::{EffectDsp, InstrumentDsp};
pub use util::MAX_BLOCK;

/// Non-bandlimited sine test oscillator — Phase 0 hardware-access proof only
/// (real oscillators land in Phase 3 as PolyBLEP/wavetable).
pub struct TestTone {
    phase: f32,
    step: f32,
    gain: f32,
}

impl TestTone {
    pub fn new(freq_hz: f32, sample_rate: f32, gain: f32) -> Self {
        Self { phase: 0.0, step: freq_hz / sample_rate, gain }
    }

    /// Fill an interleaved stereo buffer. RT-safe.
    pub fn process_interleaved_stereo(&mut self, out: &mut [f32]) {
        for frame in out.as_chunks_mut::<2>().0 {
            let s = libm::sinf(self.phase * core::f32::consts::TAU) * self.gain;
            self.phase += self.step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            frame[0] = s;
            frame[1] = s;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_is_bounded_and_nonzero() {
        let mut t = TestTone::new(440.0, 48_000.0, 0.2);
        let mut buf = [0.0f32; 256];
        t.process_interleaved_stereo(&mut buf);
        assert!(buf.iter().any(|s| s.abs() > 1e-4));
        assert!(buf.iter().all(|s| s.abs() <= 0.2 + 1e-6));
        // stereo: L == R
        assert!(buf.as_chunks::<2>().0.iter().all(|f| f[0] == f[1]));
    }
}
