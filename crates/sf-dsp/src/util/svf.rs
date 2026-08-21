//! State-variable filter — Andrew Simper's trapezoidal-integrator SVF
//! ("Solving the continuous SVF equations using trapezoidal integration",
//! cytomic technical paper). Stable across the audio band at any sane Q.

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SvfMode {
    #[default]
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
}

pub struct Svf {
    mode: SvfMode,
    g: f32,
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    ic1eq: f32,
    ic2eq: f32,
    sample_rate: f32,
    cutoff: f32,
    q: f32,
}

impl Svf {
    pub fn new(mode: SvfMode) -> Self {
        let mut f = Self {
            mode,
            g: 0.0,
            k: 0.0,
            a1: 0.0,
            a2: 0.0,
            a3: 0.0,
            ic1eq: 0.0,
            ic2eq: 0.0,
            sample_rate: 44_100.0,
            cutoff: 1_000.0,
            q: core::f32::consts::FRAC_1_SQRT_2,
        };
        f.update();
        f
    }

    pub fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.update();
        self.reset();
    }

    pub fn set_mode(&mut self, mode: SvfMode) {
        self.mode = mode;
    }

    pub fn set_cutoff_q(&mut self, cutoff_hz: f32, q: f32) {
        // keep the prewarp finite: cap just below Nyquist
        self.cutoff = cutoff_hz.clamp(1.0, self.sample_rate * 0.49);
        self.q = q.max(0.025);
        self.update();
    }

    fn update(&mut self) {
        self.g = libm::tanf(core::f32::consts::PI * self.cutoff / self.sample_rate);
        self.k = 1.0 / self.q;
        self.a1 = 1.0 / (1.0 + self.g * (self.g + self.k));
        self.a2 = self.g * self.a1;
        self.a3 = self.g * self.a2;
    }

    pub fn reset(&mut self) {
        self.ic1eq = 0.0;
        self.ic2eq = 0.0;
    }

    #[inline]
    pub fn process_sample(&mut self, v0: f32) -> f32 {
        let v3 = v0 - self.ic2eq;
        let v1 = self.a1 * self.ic1eq + self.a2 * v3;
        let v2 = self.ic2eq + self.a2 * self.ic1eq + self.a3 * v3;
        self.ic1eq = 2.0 * v1 - self.ic1eq;
        self.ic2eq = 2.0 * v2 - self.ic2eq;
        match self.mode {
            SvfMode::Lowpass => v2,
            SvfMode::Bandpass => v1,
            SvfMode::Highpass => v0 - self.k * v1 - v2,
            SvfMode::Notch => v0 - self.k * v1,
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

    fn sine_rms_through(mode: SvfMode, cutoff: f32, q: f32, freq: f32, sr: f32) -> f32 {
        let mut f = Svf::new(mode);
        f.prepare(sr);
        f.set_cutoff_q(cutoff, q);
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
    fn lowpass_attenuates_4x_above_cutoff() {
        let sr = 44_100.0;
        let sine_rms = core::f32::consts::FRAC_1_SQRT_2;
        let out = sine_rms_through(SvfMode::Lowpass, 500.0, 0.707, 2_000.0, sr);
        let db = 20.0 * libm::log10f(out / sine_rms);
        assert!(db < -12.0, "LP at 4x cutoff only {db:.1} dB");
    }

    #[test]
    fn lowpass_passes_below_cutoff() {
        let out = sine_rms_through(SvfMode::Lowpass, 2_000.0, 0.707, 100.0, 44_100.0);
        let db = 20.0 * libm::log10f(out / core::f32::consts::FRAC_1_SQRT_2);
        assert!(db.abs() < 1.0, "LP passband off by {db:.2} dB");
    }

    #[test]
    fn highpass_mirrors() {
        let out = sine_rms_through(SvfMode::Highpass, 2_000.0, 0.707, 500.0, 44_100.0);
        let db = 20.0 * libm::log10f(out / core::f32::consts::FRAC_1_SQRT_2);
        assert!(db < -12.0, "HP at cutoff/4 only {db:.1} dB");
    }

    #[test]
    fn notch_kills_center() {
        let out = sine_rms_through(SvfMode::Notch, 1_000.0, 2.0, 1_000.0, 44_100.0);
        let db = 20.0 * libm::log10f((out / core::f32::consts::FRAC_1_SQRT_2).max(1e-9));
        assert!(db < -20.0, "notch center only {db:.1} dB");
    }

    #[test]
    fn stable_at_20khz_and_high_q() {
        let sr = 44_100.0;
        let mut f = Svf::new(SvfMode::Lowpass);
        f.prepare(sr);
        f.set_cutoff_q(20_000.0, 10.0);
        let mut peak = 0.0f32;
        for i in 0..(sr as usize) {
            let x = if i == 0 { 1.0 } else { 0.0 }; // impulse
            let y = f.process_sample(x);
            assert!(y.is_finite());
            peak = peak.max(y.abs());
        }
        assert!(peak < 20.0, "impulse response blew up: {peak}");
    }
}
