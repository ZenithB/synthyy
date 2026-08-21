//! `delay` effect — port of the Tone.FeedbackDelay composite built by
//! `makeEffect("delay")` in src/audio/devices.ts.
//!
//! Behavior spec: per-channel delay line with feedback tapped from the wet
//! output back into the delay input; wet/dry via Tone's CrossFade (equal-power
//! cos/sin law). `time` is a stepped note-division index into
//! DELAY_FRACTIONS (schema.ts), converted to seconds from the transport bpm
//! (`delaySeconds()` in devices.ts); bpm arrives through `tick()`.

use crate::traits::EffectDsp;
use crate::util::{DelayLine, Smoother};

/// schema.ts DELAY_FRACTIONS — fractions of a whole note, indexed by `time`.
const FRACTIONS: [f32; 6] = [
    1.0 / 32.0,
    1.0 / 16.0,
    1.0 / 8.0,
    3.0 / 16.0,
    1.0 / 4.0,
    1.0 / 2.0,
];

/// Tone.FeedbackDelay `{ maxDelay: 4 }`.
const MAX_DELAY_SEC: f32 = 4.0;

/// devices.ts ramps time changes: `node.delayTime.rampTo(..., 0.05)`.
const TIME_RAMP_MS: f32 = 50.0;

pub struct DelayFx {
    sample_rate: f32,
    bpm: f64,
    time_idx: usize,
    /// Delay time in samples (50 ms glide, mirroring the rampTo).
    delay_smp: Smoother,
    /// Feedback amount (devices.ts caps at 0.9; schema max is 0.85).
    fb: Smoother,
    /// Wet/dry mix, 0..1 (Tone `wet`).
    mix: Smoother,
    line_l: DelayLine,
    line_r: DelayLine,
}

impl DelayFx {
    pub fn new() -> Self {
        let sample_rate = 44_100.0;
        let bpm = 120.0;
        // Schema defaults: time=2 (1/8 note), fb=0.35, mix=0.35.
        let time_idx = 2;
        let mut fx = Self {
            sample_rate,
            bpm,
            time_idx,
            delay_smp: Smoother::new(1.0),
            fb: Smoother::new(0.35),
            mix: Smoother::new(0.35),
            line_l: DelayLine::new((MAX_DELAY_SEC * sample_rate) as usize + 4),
            line_r: DelayLine::new((MAX_DELAY_SEC * sample_rate) as usize + 4),
        };
        fx.delay_smp.set_time_ms(TIME_RAMP_MS);
        fx.delay_smp.snap(fx.delay_samples());
        fx
    }

    /// devices.ts `delaySeconds(idx)`: whole note = 240/bpm seconds.
    fn delay_samples(&self) -> f32 {
        let whole = (240.0 / self.bpm.max(1.0)) as f32;
        let secs = (whole * FRACTIONS[self.time_idx]).min(MAX_DELAY_SEC);
        (secs * self.sample_rate).max(1.0)
    }
}

impl Default for DelayFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for DelayFx {
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
        self.delay_smp.prepare(self.sample_rate);
        self.delay_smp.set_time_ms(TIME_RAMP_MS);
        self.fb.prepare(self.sample_rate);
        self.mix.prepare(self.sample_rate);
        self.delay_smp.snap(self.delay_samples());
        self.fb.snap(self.fb.target());
        self.mix.snap(self.mix.target());
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            // Stepped division index — the glide happens on the resulting
            // time in samples, not on the index.
            "time" => {
                let hi = (FRACTIONS.len() - 1) as i32;
                self.time_idx = (value as i32).clamp(0, hi) as usize;
                self.delay_smp.set_target(self.delay_samples());
            }
            // devices.ts: Math.min(0.9, v)
            "fb" => self.fb.set_target(value.clamp(0.0, 0.9)),
            "mix" => self.mix.set_target(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn tick(&mut self, _pos_ticks: f64, _playing: bool, bpm: f64) {
        if bpm > 0.0 && bpm != self.bpm {
            self.bpm = bpm;
            self.delay_smp.set_target(self.delay_samples());
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let d = self.delay_smp.next();
            let fb = self.fb.next();
            let mix = self.mix.next();
            // Tone's wet control is a CrossFade: equal-power cos/sin.
            let dry_g = libm::cosf(mix * core::f32::consts::FRAC_PI_2);
            let wet_g = libm::sinf(mix * core::f32::consts::FRAC_PI_2);
            let wl = self.line_l.read_frac(d);
            let wr = self.line_r.read_frac(d);
            // FeedbackDelay: wet output feeds back into the delay input.
            self.line_l.write(*ls + wl * fb);
            self.line_r.write(*rs + wr * fb);
            *ls = *ls * dry_g + wl * wet_g;
            *rs = *rs * dry_g + wr * wet_g;
        }
    }

    fn reset(&mut self) {
        self.line_l.clear();
        self.line_r.clear();
        self.delay_smp.snap(self.delay_samples());
        self.fb.snap(self.fb.target());
        self.mix.snap(self.mix.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn run_blocks(fx: &mut DelayFx, l: &mut [f32], r: &mut [f32]) {
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

    #[test]
    fn impulse_echoes_at_expected_offset_bpm_120() {
        // time idx 2 = 1/8 of a whole note; whole = 240/120 = 2 s
        // → delay = 0.25 s = 12000 samples at 48 kHz.
        let mut fx = DelayFx::new();
        fx.set_param("time", 2.0);
        fx.set_param("fb", 0.0);
        fx.set_param("mix", 1.0); // wet only: echo amplitude ≈ 1
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

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
        let expected = 12_000usize;
        assert!(
            peak_idx.abs_diff(expected) <= 2,
            "echo at {peak_idx}, expected ~{expected}"
        );
        assert!(peak.abs() > 0.9, "echo amplitude too small: {peak}");
        assert!((l[peak_idx] - r[peak_idx]).abs() < 1e-6, "channels differ");
    }

    #[test]
    fn feedback_produces_decaying_echo_train() {
        let mut fx = DelayFx::new();
        fx.set_param("time", 0.0); // 1/32 → 62.5 ms at 120 bpm = 3000 smp
        fx.set_param("fb", 0.5);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);

        let n = 12_000;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        l[0] = 1.0;
        r[0] = 1.0;
        run_blocks(&mut fx, &mut l, &mut r);

        // Echoes at 3000/6000/9000 with amplitudes ~1, 0.5, 0.25.
        for (k, amp) in [(1usize, 1.0f32), (2, 0.5), (3, 0.25)] {
            let idx = 3_000 * k;
            let win = &l[idx - 3..=idx + 3];
            let got = win.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!(
                (got - amp).abs() < 0.05,
                "echo {k}: got {got}, expected ~{amp}"
            );
        }
    }

    #[test]
    fn two_seconds_no_nan_inf_and_wet_non_silent() {
        let mut fx = DelayFx::new(); // schema defaults: fb 0.35, mix 0.35
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

        let n = (2.0 * SR) as usize;
        let src = noise(n);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        // Wet path present: output must differ from the dry input.
        let diff: f32 = l.iter().zip(&src).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 1.0, "wet output is silent (diff {diff})");
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = DelayFx::new();
        fx.prepare(SR);
        let src = noise(BLOCK);
        let mut bpm = 120.0;
        for t in 0..=5 {
            for step in 0..=4 {
                let fb = 0.85 * step as f32 / 4.0;
                let mix = step as f32 / 4.0;
                fx.set_param("time", t as f32);
                fx.set_param("fb", fb);
                fx.set_param("mix", mix);
                bpm = if bpm > 150.0 { 60.0 } else { bpm + 37.0 };
                fx.tick(0.0, true, bpm);
                for _ in 0..40 {
                    let mut l = src.clone();
                    let mut r = src.clone();
                    fx.process(&mut l, &mut r);
                    assert!(
                        l.iter().chain(r.iter()).all(|v| v.is_finite() && v.abs() < 100.0),
                        "blew up at time={t} fb={fb} mix={mix} bpm={bpm}"
                    );
                }
            }
        }
        // Unknown keys are ignored, mix=0 returns to (near) dry.
        fx.set_param("nope", 3.0);
        fx.set_param("mix", 0.0);
        fx.set_param("fb", 0.0);
        fx.reset();
        let mut l = [0.25f32; BLOCK];
        let mut r = [-0.25f32; BLOCK];
        fx.process(&mut l, &mut r);
        assert!(l.iter().all(|v| (v - 0.25).abs() < 1e-3));
        assert!(r.iter().all(|v| (v + 0.25).abs() < 1e-3));
    }

    #[test]
    fn bpm_change_retunes_delay_time() {
        let mut fx = DelayFx::new();
        fx.set_param("time", 4.0); // 1/4 of a whole note
        fx.set_param("mix", 1.0);
        fx.set_param("fb", 0.0);
        fx.prepare(SR);
        fx.tick(0.0, true, 60.0); // whole = 4 s → 1/4 = 1 s = 48000 smp
        // Let the 50 ms time glide settle before the impulse (one-pole tail
        // needs several tau to bring a 36000-sample jump under 1 sample).
        let mut z = vec![0.0f32; (1.5 * SR) as usize];
        let mut z2 = z.clone();
        run_blocks(&mut fx, &mut z, &mut z2);

        let n = 60_000;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        l[0] = 1.0;
        r[0] = 1.0;
        run_blocks(&mut fx, &mut l, &mut r);
        let (peak_idx, _) = l
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
            .unwrap();
        // The f32 one-pole glide stalls within ~ulp(target)/2/(1-coeff) of the
        // target (~5 samples at 48000 ≈ 0.1 ms) — inaudible, so allow it.
        assert!(
            peak_idx.abs_diff(48_000) <= 8,
            "echo at {peak_idx}, expected ~48000"
        );
    }
}
