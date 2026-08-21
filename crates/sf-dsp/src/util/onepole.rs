//! One-pole lowpass/highpass (6 dB/oct) — tone-shaping and control filtering.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OnePoleMode {
    #[default]
    Lowpass,
    Highpass,
}

pub struct OnePole {
    mode: OnePoleMode,
    a: f32, // smoothing coefficient
    state: f32,
    sample_rate: f32,
    cutoff: f32,
}

impl OnePole {
    pub fn new(mode: OnePoleMode) -> Self {
        let mut f = Self {
            mode,
            a: 1.0,
            state: 0.0,
            sample_rate: 44_100.0,
            cutoff: 1_000.0,
        };
        f.update();
        f
    }

    pub fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.update();
        self.reset();
    }

    pub fn set_mode(&mut self, mode: OnePoleMode) {
        self.mode = mode;
    }

    pub fn set_cutoff(&mut self, cutoff_hz: f32) {
        self.cutoff = cutoff_hz.clamp(0.01, self.sample_rate * 0.49);
        self.update();
    }

    fn update(&mut self) {
        // exact pole placement: a = 1 − e^(−2π·fc/sr)
        self.a = 1.0 - libm::expf(-core::f32::consts::TAU * self.cutoff / self.sample_rate);
    }

    pub fn reset(&mut self) {
        self.state = 0.0;
    }

    #[inline]
    pub fn process_sample(&mut self, x: f32) -> f32 {
        self.state += self.a * (x - self.state);
        match self.mode {
            OnePoleMode::Lowpass => self.state,
            OnePoleMode::Highpass => x - self.state,
        }
    }

    pub fn process(&mut self, buf: &mut [f32]) {
        for s in buf {
            *s = self.process_sample(*s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_rms_through(mode: OnePoleMode, cutoff: f32, freq: f32) -> f32 {
        let sr = 44_100.0;
        let mut f = OnePole::new(mode);
        f.prepare(sr);
        f.set_cutoff(cutoff);
        let n = sr as usize;
        let mut acc = 0.0f32;
        let mut counted = 0usize;
        for i in 0..n {
            let x = libm::sinf(core::f32::consts::TAU * freq * i as f32 / sr);
            let y = f.process_sample(x);
            if i >= n / 2 {
                acc += y * y;
                counted += 1;
            }
        }
        libm::sqrtf(acc / counted as f32)
    }

    #[test]
    fn lowpass_slope() {
        let full = core::f32::consts::FRAC_1_SQRT_2;
        // ~ -3 dB at cutoff, ~ -20 dB a decade up
        let at_fc = 20.0 * libm::log10f(sine_rms_through(OnePoleMode::Lowpass, 500.0, 500.0) / full);
        assert!((at_fc + 3.0).abs() < 1.0, "at cutoff: {at_fc:.2} dB");
        let decade = 20.0 * libm::log10f(sine_rms_through(OnePoleMode::Lowpass, 200.0, 2_000.0) / full);
        assert!(decade < -17.0 && decade > -26.0, "decade up: {decade:.1} dB");
    }

    #[test]
    fn highpass_blocks_dc() {
        let mut f = OnePole::new(OnePoleMode::Highpass);
        f.prepare(44_100.0);
        f.set_cutoff(50.0);
        let mut y = 0.0;
        for _ in 0..44_100 {
            y = f.process_sample(1.0);
        }
        assert!(y.abs() < 1e-4, "DC leaked: {y}");
    }
}
