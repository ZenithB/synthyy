//! `vib` effect — port of the Tone.Vibrato composite built by
//! `makeEffect("vib")` in src/audio/devices.ts.
//!
//! Behavior spec (Tone/effect/Vibrato.ts, `new Tone.Vibrato({ frequency, depth,
//! wet })`): one delay line per channel whose delay time is driven by a shared
//! sine LFO — a pitch vibrato, identical on both channels (no stereo spread).
//! Tone's LFO spans min 0 to max `maxDelay` (default 5 ms) with `depth` as its
//! amplitude and a −90° phase offset, so
//! `delay(t) = maxDelay/2 · (1 + depth · sin(2πft − π/2))`, starting at the
//! shortest delay. Wet/dry is Tone's CrossFade (equal-power cos/sin law).
//! The rate knob is switchable (makeRate/rateHzOf in devices.ts/schema.ts):
//! rateMode 0 = Sync (tempo division index in rateSync, retuned from the
//! transport bpm via `tick()`), 1 = Low (`rate` Hz), 2 = High (`rateHi` Hz).

use crate::traits::EffectDsp;
use crate::util::{DelayLine, Smoother};

/// Tone.Vibrato default `maxDelay` (seconds); the LFO sweeps 0..this.
const MAX_DELAY_SEC: f32 = 0.005;

/// The LFO rest position (depth 0): half of `maxDelay`.
const CENTER_DELAY_SEC: f32 = MAX_DELAY_SEC * 0.5;

/// schema.ts LFO_DIV_TICKS — cycle length in transport ticks per rateSync
/// index (PPQ = 96 ⇒ quarter = 96, bar = 384).
const DIV_TICKS: [f64; 9] = [3072.0, 1536.0, 768.0, 384.0, 192.0, 96.0, 48.0, 32.0, 24.0];

pub struct VibFx {
    sample_rate: f32,
    bpm: f64,
    // Switchable rate state (makeRate defaults: mode 1/Low, rateHi 440, sync 5).
    rate_mode: i32,
    rate_lo: f32,
    rate_hi: f32,
    rate_sync: usize,
    /// LFO phase in cycles [0, 1); step = hz / sample_rate.
    phase: f64,
    step: f64,
    /// LFO amplitude (Tone `depth`, 0..1), ~15 ms smoothed.
    depth: Smoother,
    /// Wet/dry mix, 0..1 (Tone `wet`).
    mix: Smoother,
    line_l: DelayLine,
    line_r: DelayLine,
}

impl VibFx {
    pub fn new() -> Self {
        let sample_rate = 44_100.0;
        // Schema defaults: rate=5 Hz, depth=0.15, mix=1.
        let mut fx = Self {
            sample_rate,
            bpm: 120.0,
            rate_mode: 1,
            rate_lo: 5.0,
            rate_hi: 440.0,
            rate_sync: 5,
            phase: 0.0,
            step: 0.0,
            depth: Smoother::new(0.15),
            mix: Smoother::new(1.0),
            line_l: DelayLine::new((MAX_DELAY_SEC * sample_rate) as usize + 4),
            line_r: DelayLine::new((MAX_DELAY_SEC * sample_rate) as usize + 4),
        };
        fx.update_step();
        fx
    }

    /// schema.ts `rateHzOf()`: effective LFO frequency for the stored quad.
    fn rate_hz(&self) -> f32 {
        match self.rate_mode {
            2 => self.rate_hi.clamp(301.0, 1000.0),
            0 => {
                let ticks = DIV_TICKS[self.rate_sync.min(DIV_TICKS.len() - 1)];
                ((self.bpm.max(1.0) * 96.0) / (60.0 * ticks)) as f32
            }
            _ => self.rate_lo.clamp(0.01, 300.0),
        }
    }

    /// Frequency changes are phase-continuous (no discontinuity in the LFO
    /// output), so the step needs no smoother.
    fn update_step(&mut self) {
        self.step = self.rate_hz() as f64 / self.sample_rate.max(1.0) as f64;
    }

    fn center_samples(&self) -> f32 {
        CENTER_DELAY_SEC * self.sample_rate
    }
}

impl Default for VibFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for VibFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        let max = (MAX_DELAY_SEC * self.sample_rate) as usize + 4;
        if max > self.line_l.max_delay() {
            self.line_l = DelayLine::new(max);
            self.line_r = DelayLine::new(max);
        } else {
            self.line_l.clear();
            self.line_r.clear();
        }
        self.depth.prepare(self.sample_rate);
        self.depth.snap(self.depth.target());
        self.mix.prepare(self.sample_rate);
        self.mix.snap(self.mix.target());
        self.update_step();
        self.phase = 0.0;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "rate" => {
                self.rate_lo = value;
                self.update_step();
            }
            "rateMode" => {
                self.rate_mode = (value as i32).clamp(0, 2);
                self.update_step();
            }
            "rateHi" => {
                self.rate_hi = value;
                self.update_step();
            }
            "rateSync" => {
                self.rate_sync = (value as i32).clamp(0, DIV_TICKS.len() as i32 - 1) as usize;
                self.update_step();
            }
            "depth" => self.depth.set_target(value.clamp(0.0, 1.0)),
            "mix" => self.mix.set_target(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn tick(&mut self, _pos_ticks: f64, _playing: bool, bpm: f64) {
        if bpm > 0.0 && bpm != self.bpm {
            self.bpm = bpm;
            if self.rate_mode == 0 {
                self.update_step();
            }
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let center = self.center_samples();
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let depth = self.depth.next();
            let mix = self.mix.next();
            // Tone's wet control is a CrossFade: equal-power cos/sin.
            let dry_g = libm::cosf(mix * core::f32::consts::FRAC_PI_2);
            let wet_g = libm::sinf(mix * core::f32::consts::FRAC_PI_2);
            // Tone LFO phase −90°: sin(2πφ − π/2), resting at the minimum.
            let s = libm::sinf(
                core::f32::consts::TAU * self.phase as f32 - core::f32::consts::FRAC_PI_2,
            );
            self.phase += self.step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            // delay = maxDelay/2 · (1 + depth·s); read_frac clamps at 1 sample.
            let d = center * (1.0 + depth * s);
            let wl = self.line_l.read_frac(d);
            let wr = self.line_r.read_frac(d);
            self.line_l.write(*ls);
            self.line_r.write(*rs);
            *ls = *ls * dry_g + wl * wet_g;
            *rs = *rs * dry_g + wr * wet_g;
        }
    }

    fn reset(&mut self) {
        self.line_l.clear();
        self.line_r.clear();
        self.depth.snap(self.depth.target());
        self.mix.snap(self.mix.target());
        self.phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn run_blocks(fx: &mut VibFx, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        let mut i = 0;
        while i < n {
            let end = (i + BLOCK).min(n);
            fx.process(&mut l[i..end], &mut r[i..end]);
            i = end;
        }
    }

    /// Deterministic pseudo-noise in [-0.5, 0.5].
    fn noise(n: usize) -> Vec<f32> {
        let mut s = 0x12345678u32;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1 << 24) as f32 - 0.5
            })
            .collect()
    }

    fn sine(n: usize, freq: f32, amp: f32) -> Vec<f32> {
        (0..n)
            .map(|i| amp * libm::sinf(core::f32::consts::TAU * freq * i as f32 / SR))
            .collect()
    }

    /// Goertzel magnitude at `freq`, normalized so a full-scale sine reads ~1.
    fn goertzel(x: &[f32], freq: f32) -> f32 {
        let w = core::f64::consts::TAU * freq as f64 / SR as f64;
        let c = 2.0 * libm::cos(w);
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s0 = v as f64 + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let p = s1 * s1 + s2 * s2 - c * s1 * s2;
        (2.0 * libm::sqrt(p.max(0.0)) / x.len() as f64) as f32
    }

    #[test]
    fn two_seconds_no_nan_inf_and_wet_non_silent() {
        let mut fx = VibFx::new(); // schema defaults: rate 5, depth 0.15, mix 1
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

        let n = (2.0 * SR) as usize;
        let src = noise(n);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        // Wet path present and non-silent: mix 1 replaces dry with the
        // modulated-delay signal, which must differ from the input.
        let energy: f32 = l.iter().map(|v| v * v).sum();
        assert!(energy > 1.0, "wet output is silent (energy {energy})");
        let diff: f32 = l.iter().zip(&src).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 1.0, "output identical to dry (diff {diff})");
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = VibFx::new();
        fx.prepare(SR);
        let src = noise(BLOCK);
        let mut bpm = 120.0;
        for mode in 0..=2 {
            for step in 0..=4 {
                let t = step as f32 / 4.0;
                fx.set_param("rateMode", mode as f32);
                fx.set_param("rate", 0.01 + t * (300.0 - 0.01));
                fx.set_param("rateHi", 301.0 + t * (1000.0 - 301.0));
                fx.set_param("rateSync", (step * 2) as f32);
                fx.set_param("depth", t * 0.6); // schema depth range 0..0.6
                fx.set_param("mix", 1.0 - t);
                bpm = if bpm > 150.0 { 60.0 } else { bpm + 37.0 };
                fx.tick(0.0, true, bpm);
                for _ in 0..40 {
                    let mut l = src.clone();
                    let mut r = src.clone();
                    fx.process(&mut l, &mut r);
                    assert!(
                        l.iter().chain(r.iter()).all(|v| v.is_finite() && v.abs() < 100.0),
                        "blew up at mode={mode} t={t} bpm={bpm}"
                    );
                }
            }
        }
        // Unknown keys are ignored; mix=0 is exactly dry after reset.
        fx.set_param("nope", 3.0);
        fx.set_param("mix", 0.0);
        fx.reset();
        let mut l = [0.25f32; BLOCK];
        let mut r = [-0.25f32; BLOCK];
        fx.process(&mut l, &mut r);
        assert!(l.iter().all(|v| (v - 0.25).abs() < 1e-3));
        assert!(r.iter().all(|v| (v + 0.25).abs() < 1e-3));
    }

    #[test]
    fn sine_in_instantaneous_frequency_wobbles() {
        // A modulated delay is phase modulation: a pure carrier must spread
        // into sidebands at f_c ± k·f_lfo (spectral spread beyond the input
        // line), and the carrier line itself must lose energy.
        let fc = 1_000.0f32;
        let fm = 8.0f32;
        let mut fx = VibFx::new();
        fx.set_param("rateMode", 1.0);
        fx.set_param("rate", fm);
        fx.set_param("depth", 0.5);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);

        let n = (1.5 * SR) as usize;
        let src = sine(n, fc, 0.5);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        // Mono LFO: both channels carry the identical vibrato.
        let lr_diff: f32 = l.iter().zip(&r).map(|(a, b)| (a - b).abs()).sum();
        assert!(lr_diff < 1e-3, "channels diverged (diff {lr_diff})");

        // Analyze the last integer second (integer-Hz bins are leakage-free).
        let tail = &l[n - SR as usize..];
        let dry_tail = &src[n - SR as usize..];
        let sidebands: f32 = (1..=5)
            .map(|k| goertzel(tail, fc + k as f32 * fm) + goertzel(tail, fc - k as f32 * fm))
            .sum();
        let dry_sidebands: f32 = (1..=5)
            .map(|k| goertzel(dry_tail, fc + k as f32 * fm) + goertzel(dry_tail, fc - k as f32 * fm))
            .sum();
        assert!(
            sidebands > 0.05,
            "no sideband energy around the carrier ({sidebands})"
        );
        assert!(
            sidebands > 20.0 * dry_sidebands,
            "sidebands not above dry floor ({sidebands} vs {dry_sidebands})"
        );
        let carrier = goertzel(tail, fc);
        let dry_carrier = goertzel(dry_tail, fc);
        assert!(
            carrier < 0.95 * dry_carrier,
            "carrier line kept all its energy ({carrier} vs {dry_carrier})"
        );
    }

    #[test]
    fn synced_rate_follows_bpm() {
        // Sync mode, rateSync 5 (1/4 note): 120 bpm → 2 Hz LFO. Verify the
        // modulation period via sideband spacing at f_c ± 2 Hz.
        let fc = 1_000.0f32;
        let mut fx = VibFx::new();
        fx.set_param("rateMode", 0.0);
        fx.set_param("rateSync", 5.0);
        fx.set_param("depth", 0.5);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

        let n = (2.0 * SR) as usize;
        let src = sine(n, fc, 0.5);
        let mut l = src.clone();
        let mut r = src;
        run_blocks(&mut fx, &mut l, &mut r);

        let tail = &l[n - SR as usize..];
        let at_sync: f32 = (1..=5)
            .map(|k| goertzel(tail, fc + 2.0 * k as f32) + goertzel(tail, fc - 2.0 * k as f32))
            .sum();
        assert!(at_sync > 0.05, "no sidebands at the synced rate ({at_sync})");
    }

    #[test]
    fn depth_zero_is_a_static_center_delay() {
        // depth 0: wet path is a fixed maxDelay/2 delay of the input (Tone LFO
        // amplitude 0 rests at the midpoint of [0, maxDelay]).
        let mut fx = VibFx::new();
        fx.set_param("depth", 0.0);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);

        let n = (0.5 * SR) as usize;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        l[0] = 1.0;
        r[0] = 1.0;
        run_blocks(&mut fx, &mut l, &mut r);
        let (peak_idx, peak) = l
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap();
        let expected = (CENTER_DELAY_SEC * SR) as usize; // 120 samples
        assert!(
            peak_idx.abs_diff(expected) <= 2,
            "static delay at {peak_idx}, expected ~{expected}"
        );
        assert!(peak.abs() > 0.9, "wet echo too small: {peak}");
    }
}
