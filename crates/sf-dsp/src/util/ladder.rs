//! Cheap Moog-style ladder lowpass: four cascaded one-pole stages with
//! resonance feedback, soft-clipped input (single tanh-approx, not per-stage)
//! for mono-bass duty. 12 or 24 dB/oct via the output tap.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LadderMode {
    Db12,
    #[default]
    Db24,
}

pub struct Ladder {
    mode: LadderMode,
    g: f32, // per-stage one-pole coefficient
    k: f32, // feedback amount, 0..~4
    s: [f32; 4],
    sample_rate: f32,
    cutoff: f32,
    res: f32,
}

impl Ladder {
    pub fn new() -> Self {
        let mut l = Self {
            mode: LadderMode::Db24,
            g: 0.0,
            k: 0.0,
            s: [0.0; 4],
            sample_rate: 44_100.0,
            cutoff: 1_000.0,
            res: 0.0,
        };
        l.update();
        l
    }

    pub fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.update();
        self.reset();
    }

    pub fn set_mode(&mut self, mode: LadderMode) {
        self.mode = mode;
    }

    /// `res` in [0, 1]; ~1.0 approaches self-oscillation (k → 4, clamped).
    pub fn set_cutoff_res(&mut self, cutoff_hz: f32, res: f32) {
        self.cutoff = cutoff_hz.clamp(10.0, self.sample_rate * 0.45);
        self.res = res.clamp(0.0, 1.0);
        self.update();
    }

    fn update(&mut self) {
        // bilinear-ish warp keeps the corner honest toward Nyquist
        let wc = core::f32::consts::PI * self.cutoff / self.sample_rate;
        let t = libm::tanf(wc);
        self.g = t / (1.0 + t);
        self.k = 3.98 * self.res;
    }

    pub fn reset(&mut self) {
        self.s = [0.0; 4];
    }

    /// Cheap odd saturator (tanh-free): keeps resonance feedback bounded.
    #[inline]
    fn sat(x: f32) -> f32 {
        let x = x.clamp(-3.0, 3.0);
        x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
    }

    #[inline]
    pub fn process_sample(&mut self, x: f32) -> f32 {
        let input = Self::sat(x - self.k * self.s[3]);
        let mut v = input;
        for st in &mut self.s {
            *st += self.g * (v - *st);
            v = *st;
        }
        match self.mode {
            LadderMode::Db12 => self.s[1],
            LadderMode::Db24 => self.s[3],
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for s in buf {
            *s = self.process_sample(*s);
        }
    }
}

impl Default for Ladder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_rms_through(mode: LadderMode, cutoff: f32, freq: f32) -> f32 {
        let sr = 44_100.0;
        let mut f = Ladder::new();
        f.prepare(sr);
        f.set_mode(mode);
        f.set_cutoff_res(cutoff, 0.0);
        let n = sr as usize;
        let mut acc = 0.0f32;
        let mut counted = 0usize;
        for i in 0..n {
            let x = 0.5 * libm::sinf(core::f32::consts::TAU * freq * i as f32 / sr);
            let y = f.process_sample(x);
            if i >= n / 2 {
                acc += y * y;
                counted += 1;
            }
        }
        libm::sqrtf(acc / counted as f32)
    }

    #[test]
    fn rolloff_24_steeper_than_12() {
        let input_rms = 0.5 * core::f32::consts::FRAC_1_SQRT_2;
        let lp24 = sine_rms_through(LadderMode::Db24, 300.0, 2_400.0);
        let lp12 = sine_rms_through(LadderMode::Db12, 300.0, 2_400.0);
        let db24 = 20.0 * libm::log10f(lp24 / input_rms);
        let db12 = 20.0 * libm::log10f(lp12 / input_rms);
        assert!(db24 < -40.0, "24 dB mode 3 octaves up: {db24:.1} dB");
        assert!(db12 < -20.0, "12 dB mode 3 octaves up: {db12:.1} dB");
        assert!(db24 < db12 - 10.0, "24 ({db24:.1}) not steeper than 12 ({db12:.1})");
    }

    #[test]
    fn passband_roughly_unity() {
        let input_rms = 0.5 * core::f32::consts::FRAC_1_SQRT_2;
        let out = sine_rms_through(LadderMode::Db24, 4_000.0, 100.0);
        let db = 20.0 * libm::log10f(out / input_rms);
        assert!(db.abs() < 1.5, "passband off by {db:.2} dB");
    }

    #[test]
    fn stable_at_max_resonance() {
        let sr = 44_100.0;
        let mut f = Ladder::new();
        f.prepare(sr);
        f.set_cutoff_res(8_000.0, 1.0);
        for i in 0..(2 * sr as usize) {
            let x = if i % 1_000 == 0 { 1.0 } else { 0.0 };
            let y = f.process_sample(x);
            assert!(y.is_finite() && y.abs() < 10.0, "unstable at sample {i}: {y}");
        }
    }
}
