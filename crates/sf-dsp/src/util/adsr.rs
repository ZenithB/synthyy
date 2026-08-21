//! ADSR envelope matching Tone.js Envelope semantics: linear attack to 1
//! (from the current level, so retriggers don't click), exponential decay to
//! sustain, exponential release to 0 from wherever the level currently is.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

pub struct Adsr {
    stage: Stage,
    value: f32,
    attack_step: f32,  // linear per-sample rise
    decay_coeff: f32,  // one-pole toward sustain
    release_coeff: f32, // one-pole toward 0
    sustain: f32,
    sample_rate: f32,
    attack_s: f32,
    decay_s: f32,
    release_s: f32,
}

/// Exp stages approach their target asymptotically. τ = time / EXP_TIME_MULT
/// ⇒ the level is ~99.8% of the way there when the nominal stage time elapses.
const EXP_TIME_MULT: f32 = 6.2;
const IDLE_FLOOR: f32 = 1e-4; // ≈ −80 dB: release is over

impl Adsr {
    pub fn new() -> Self {
        let mut e = Self {
            stage: Stage::Idle,
            value: 0.0,
            attack_step: 0.0,
            decay_coeff: 0.0,
            release_coeff: 0.0,
            sustain: 0.5,
            sample_rate: 44_100.0,
            attack_s: 0.01,
            decay_s: 0.1,
            release_s: 0.3,
        };
        e.update();
        e
    }

    pub fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.update();
    }

    pub fn set_adsr(&mut self, attack_s: f32, decay_s: f32, sustain: f32, release_s: f32) {
        self.attack_s = attack_s.max(0.0);
        self.decay_s = decay_s.max(0.0);
        self.sustain = sustain.clamp(0.0, 1.0);
        self.release_s = release_s.max(0.0);
        self.update();
    }

    fn update(&mut self) {
        let a = self.attack_s * self.sample_rate;
        self.attack_step = if a < 1.0 { 1.0 } else { 1.0 / a };
        self.decay_coeff = Self::exp_coeff(self.decay_s, self.sample_rate);
        self.release_coeff = Self::exp_coeff(self.release_s, self.sample_rate);
    }

    fn exp_coeff(time_s: f32, sr: f32) -> f32 {
        let tau_samples = time_s * sr / EXP_TIME_MULT;
        if tau_samples < 1.0 {
            0.0
        } else {
            libm::expf(-1.0 / tau_samples)
        }
    }

    /// Gate on: linear attack toward 1 from the current level.
    pub fn note_on(&mut self) {
        self.stage = Stage::Attack;
    }

    /// Gate off: exponential release from the current level.
    pub fn note_off(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }

    pub fn reset(&mut self) {
        self.stage = Stage::Idle;
        self.value = 0.0;
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.stage != Stage::Idle
    }

    #[inline]
    pub fn value(&self) -> f32 {
        self.value
    }

    // `next` is the contract name (RUST_ARCH.md: set_target/next/snap), not an Iterator
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        match self.stage {
            Stage::Idle => {}
            Stage::Attack => {
                self.value += self.attack_step;
                if self.value >= 1.0 {
                    self.value = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.value = self.sustain + (self.value - self.sustain) * self.decay_coeff;
                if self.value - self.sustain < IDLE_FLOOR {
                    self.value = self.sustain;
                    self.stage = Stage::Sustain;
                    if self.sustain <= 0.0 {
                        self.stage = Stage::Idle;
                        self.value = 0.0;
                    }
                }
            }
            Stage::Sustain => self.value = self.sustain,
            Stage::Release => {
                self.value *= self.release_coeff;
                if self.value < IDLE_FLOOR {
                    self.value = 0.0;
                    self.stage = Stage::Idle;
                }
            }
        }
        self.value
    }
}

impl Default for Adsr {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_cycle_shape() {
        let sr = 44_100.0;
        let mut e = Adsr::new();
        e.prepare(sr);
        e.set_adsr(0.01, 0.05, 0.6, 0.1);
        e.note_on();

        // attack: monotonic rise to 1
        let a_n = (0.01 * sr) as usize;
        let mut prev = 0.0;
        for _ in 0..a_n {
            let v = e.next();
            assert!(v >= prev, "attack not monotonic");
            prev = v;
        }
        // absorb float drift around the attack→decay handover, keep the peak
        for _ in 0..4 {
            prev = prev.max(e.next());
        }
        assert!((prev - 1.0).abs() < 2e-3, "attack peak {prev}");

        // decay: monotonic fall toward sustain
        for _ in 0..(0.2 * sr) as usize {
            let v = e.next();
            assert!(v <= prev + 1e-6, "decay not monotonic");
            assert!(v >= 0.6 - 1e-3);
            prev = v;
        }
        assert!((prev - 0.6).abs() < 5e-3, "sustain level {prev}");

        // release: monotonic fall, reaches ~0
        e.note_off();
        for _ in 0..(0.3 * sr) as usize {
            let v = e.next();
            assert!(v <= prev + 1e-6, "release not monotonic");
            prev = v;
        }
        assert!(prev < 1e-3, "release did not reach zero: {prev}");
        assert!(!e.is_active());
    }

    #[test]
    fn release_from_mid_attack() {
        let mut e = Adsr::new();
        e.prepare(48_000.0);
        e.set_adsr(0.5, 0.1, 0.8, 0.05);
        e.note_on();
        for _ in 0..1_000 {
            e.next();
        }
        let level = e.value();
        assert!(level > 0.0 && level < 1.0);
        e.note_off();
        let v = e.next();
        assert!(v <= level, "release should fall from the current level");
        for _ in 0..48_000 {
            e.next();
        }
        assert_eq!(e.value(), 0.0);
    }

    #[test]
    fn zero_times_are_instant() {
        let mut e = Adsr::new();
        e.prepare(44_100.0);
        e.set_adsr(0.0, 0.0, 0.25, 0.0);
        e.note_on();
        assert!((e.next() - 1.0).abs() < 1e-6);
        e.next(); // decay collapses to sustain
        assert!((e.value() - 0.25).abs() < 1e-3);
        e.note_off();
        e.next();
        assert!(e.value() < 1e-3);
    }
}
