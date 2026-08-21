//! `autopan` effect — port of the Tone.AutoPanner composite built by
//! `makeEffect("autopan")` in src/audio/devices.ts.
//!
//! Behavior spec (Tone/effect/AutoPanner.ts, `new Tone.AutoPanner({ frequency,
//! depth })`): a sine LFO drives a StereoPannerNode's `pan` param, so
//! pan(t) = depth·sin(2π·f·t). The device schema exposes no mix knob and the
//! Tone wet stays at its default 1 (fully wet) — depth ≈ 0 is the neutral
//! state (`fxNeutral` in devices.ts), and pan 0 through the stereo panner law
//! below is exact passthrough, matching that contract.
//! The pan law is the Web Audio StereoPannerNode *stereo* algorithm (the
//! chain is planar stereo): the retreating channel bleeds into the other side
//! with an equal-power cos/sin split.
//! The rate knob is switchable (makeRate/rateHzOf in devices.ts/schema.ts):
//! rateMode 0 = Sync (tempo division index in rateSync, retuned from the
//! transport bpm via `tick()`), 1 = Low (`rate` Hz), 2 = High (`rateHi` Hz).

use crate::traits::EffectDsp;
use crate::util::Smoother;

/// schema.ts LFO_DIV_TICKS — cycle length in transport ticks per rateSync
/// index (PPQ = 96 ⇒ quarter = 96, bar = 384).
const DIV_TICKS: [f64; 9] = [3072.0, 1536.0, 768.0, 384.0, 192.0, 96.0, 48.0, 32.0, 24.0];

/// Web Audio StereoPannerNode, stereo-input algorithm: for pan ≤ 0 the right
/// channel folds into the left through an equal-power cos/sin pair (and
/// mirrored for pan > 0). Pan 0 is bit-exact passthrough.
#[inline]
fn pan_stereo(pan: f32, l: f32, r: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    if p <= 0.0 {
        let x = (p + 1.0) * core::f32::consts::FRAC_PI_2;
        (l + r * libm::cosf(x), r * libm::sinf(x))
    } else {
        let x = p * core::f32::consts::FRAC_PI_2;
        (l * libm::cosf(x), r + l * libm::sinf(x))
    }
}

pub struct AutopanFx {
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
    /// Pan LFO amplitude 0..1 (Tone `depth`), ~15 ms smoothed.
    depth: Smoother,
}

impl AutopanFx {
    pub fn new() -> Self {
        // Schema defaults: rate = 1.5 Hz, depth = 0.8.
        let mut fx = Self {
            sample_rate: 44_100.0,
            bpm: 120.0,
            rate_mode: 1,
            rate_lo: 1.5,
            rate_hi: 440.0,
            rate_sync: 5,
            phase: 0.0,
            step: 0.0,
            depth: Smoother::new(0.8),
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
}

impl Default for AutopanFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for AutopanFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.depth.prepare(self.sample_rate);
        self.depth.snap(self.depth.target());
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
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let pan = self.depth.next() * libm::sinf(core::f32::consts::TAU * self.phase as f32);
            self.phase += self.step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            let (ol, or) = pan_stereo(pan, *ls, *rs);
            *ls = ol;
            *rs = or;
        }
    }

    fn reset(&mut self) {
        self.depth.snap(self.depth.target());
        self.phase = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn run_blocks(fx: &mut AutopanFx, l: &mut [f32], r: &mut [f32]) {
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

    /// Windowed RMS envelope: one value per `win` samples.
    fn envelope(x: &[f32], win: usize) -> Vec<f32> {
        x.chunks_exact(win)
            .map(|c| {
                let e: f32 = c.iter().map(|v| v * v).sum();
                libm::sqrtf(e / win as f32)
            })
            .collect()
    }

    /// Pearson correlation coefficient of two equal-length series.
    fn pearson(a: &[f32], b: &[f32]) -> f32 {
        let n = a.len() as f64;
        let ma = a.iter().map(|v| *v as f64).sum::<f64>() / n;
        let mb = b.iter().map(|v| *v as f64).sum::<f64>() / n;
        let (mut num, mut da, mut db) = (0.0f64, 0.0f64, 0.0f64);
        for (x, y) in a.iter().zip(b) {
            let (x, y) = (*x as f64 - ma, *y as f64 - mb);
            num += x * y;
            da += x * x;
            db += y * y;
        }
        (num / libm::sqrt((da * db).max(1e-30))) as f32
    }

    /// Goertzel magnitude at `freq` for a series sampled at `sr`.
    fn goertzel(x: &[f32], freq: f32, sr: f32) -> f32 {
        let w = core::f64::consts::TAU * freq as f64 / sr as f64;
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
        let mut fx = AutopanFx::new(); // schema defaults: rate 1.5, depth 0.8
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

        let n = (2.0 * SR) as usize;
        let src = noise(n);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        // Panning active: output must differ from the dry input and be non-silent.
        let diff: f32 = l.iter().zip(&src).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 1.0, "pan modulation absent (diff {diff})");
        let energy: f32 = l.iter().chain(r.iter()).map(|v| v * v).sum();
        assert!(energy > 1.0, "output silent (energy {energy})");
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = AutopanFx::new();
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
                fx.set_param("depth", t);
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
        // Unknown keys are ignored; depth 0 is exact passthrough (fxNeutral).
        fx.set_param("nope", 3.0);
        fx.set_param("depth", 0.0);
        fx.reset();
        let mut l = [0.25f32; BLOCK];
        let mut r = [-0.25f32; BLOCK];
        fx.process(&mut l, &mut r);
        assert!(l.iter().all(|v| (v - 0.25).abs() < 1e-6));
        assert!(r.iter().all(|v| (v + 0.25).abs() < 1e-6));
    }

    #[test]
    fn lr_envelopes_anticorrelated_at_lfo_rate() {
        // Identical 1 kHz sines on both channels through a full-depth 2 Hz
        // pan: energy must slosh L↔R, so the channel RMS envelopes are
        // anti-correlated and modulated at exactly the LFO rate.
        let mut fx = AutopanFx::new();
        fx.set_param("rateMode", 1.0);
        fx.set_param("rate", 2.0);
        fx.set_param("depth", 1.0);
        fx.prepare(SR);

        let n = (2.0 * SR) as usize;
        let src = sine(n, 1_000.0, 0.5);
        let mut l = src.clone();
        let mut r = src;
        run_blocks(&mut fx, &mut l, &mut r);

        // 5 ms RMS windows → 200 Hz envelope rate; analyze the last 1.5 s
        // (integer LFO cycles, past any smoother settle).
        let win = (0.005 * SR) as usize;
        let skip = (0.5 * SR) as usize;
        let env_l = envelope(&l[skip..], win);
        let env_r = envelope(&r[skip..], win);
        let corr = pearson(&env_l, &env_r);
        assert!(corr < -0.8, "L/R envelopes not anti-correlated (r = {corr})");

        // Envelope spectrum peaks at the LFO rate, not elsewhere.
        let env_sr = SR / win as f32;
        let mean = env_l.iter().sum::<f32>() / env_l.len() as f32;
        let ac: Vec<f32> = env_l.iter().map(|v| v - mean).collect();
        let at_rate = goertzel(&ac, 2.0, env_sr);
        let off_rate = goertzel(&ac, 7.0, env_sr);
        assert!(at_rate > 0.05, "no envelope modulation at 2 Hz ({at_rate})");
        assert!(
            at_rate > 5.0 * off_rate,
            "envelope not dominated by the LFO rate ({at_rate} vs {off_rate} at 7 Hz)"
        );
    }

    #[test]
    fn synced_rate_follows_bpm() {
        // Sync mode, rateSync 5 (1/4 note): 120 bpm → 2 Hz pan LFO. Verify
        // via the envelope spectrum, and that a bpm change retunes it.
        let mut fx = AutopanFx::new();
        fx.set_param("rateMode", 0.0);
        fx.set_param("rateSync", 5.0);
        fx.set_param("depth", 1.0);
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

        let n = (2.0 * SR) as usize;
        let src = sine(n, 1_000.0, 0.5);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        let win = (0.005 * SR) as usize;
        let env_sr = SR / win as f32;
        let env = envelope(&l[(0.5 * SR) as usize..], win);
        let mean = env.iter().sum::<f32>() / env.len() as f32;
        let ac: Vec<f32> = env.iter().map(|v| v - mean).collect();
        let at_2 = goertzel(&ac, 2.0, env_sr);
        let at_3 = goertzel(&ac, 3.0, env_sr);
        assert!(at_2 > 0.05 && at_2 > 3.0 * at_3, "not synced to 2 Hz ({at_2} vs {at_3})");

        // 90 bpm → 1.5 Hz.
        fx.reset();
        fx.tick(0.0, true, 90.0);
        let mut l = src.clone();
        let mut r = src;
        run_blocks(&mut fx, &mut l, &mut r);
        let env = envelope(&l[(0.5 * SR) as usize..], win);
        let mean = env.iter().sum::<f32>() / env.len() as f32;
        let ac: Vec<f32> = env.iter().map(|v| v - mean).collect();
        let at_15 = goertzel(&ac, 1.5, env_sr);
        let at_2 = goertzel(&ac, 2.0, env_sr);
        assert!(
            at_15 > 0.05 && at_15 > 3.0 * at_2,
            "bpm change did not retune ({at_15} at 1.5 Hz vs {at_2} at 2 Hz)"
        );
    }
}
