//! Metronome blip synth (engine-tone scheduleMetronome): a sine ping every
//! beat of the current meter, accent on bar starts — 1760 Hz @ 0.5 velocity
//! accented, 1175 Hz @ 0.25 otherwise, ~30 ms long. Gated by the engine's
//! metronome flag; the blip mixes into the master input like the live synth.

pub struct Metronome {
    sample_rate: f32,
    phase: f32,
    step: f32,
    amp: f32,
    decay: f32,
    remaining: u32,
}

impl Metronome {
    pub fn new() -> Self {
        Self {
            sample_rate: 44_100.0,
            phase: 0.0,
            step: 0.0,
            amp: 0.0,
            decay: 0.0,
            remaining: 0,
        }
    }

    pub fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.remaining = 0;
        self.amp = 0.0;
    }

    /// Fire a blip (accent = bar start).
    pub fn trigger(&mut self, accent: bool) {
        let (freq, vel) = if accent { (1760.0, 0.5) } else { (1175.0, 0.25) };
        self.phase = 0.0;
        self.step = freq / self.sample_rate;
        self.amp = vel;
        // 30 ms nominal length: exponential decay τ = 8 ms, hard stop at 60 ms
        self.decay = (-1.0 / (0.008 * self.sample_rate)).exp();
        self.remaining = (0.06 * self.sample_rate) as u32;
    }

    /// Mix the blip into l/r (additive).
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if self.remaining == 0 {
            return;
        }
        for i in 0..l.len() {
            if self.remaining == 0 {
                break;
            }
            let s = (self.phase * core::f32::consts::TAU).sin() * self.amp;
            self.phase += self.step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            self.amp *= self.decay;
            self.remaining -= 1;
            l[i] += s;
            r[i] += s;
        }
    }
}

impl Default for Metronome {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blip_is_short_and_bounded() {
        let mut m = Metronome::new();
        m.prepare(44_100.0);
        m.trigger(true);
        let mut l = vec![0.0f32; 8_192];
        let mut r = vec![0.0f32; 8_192];
        for (cl, cr) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            m.process(cl, cr);
        }
        assert!(l[..1_000].iter().any(|s| s.abs() > 0.05));
        assert!(l[4_000..].iter().all(|s| s.abs() < 1e-3), "blip too long");
        assert!(l.iter().all(|s| s.is_finite() && s.abs() <= 0.5));
    }
}
