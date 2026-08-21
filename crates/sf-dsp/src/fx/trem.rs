//! `trem` effect — port of the Tone.Tremolo composite built by
//! `makeEffect("trem")` in src/audio/devices.ts.
//!
//! Behavior spec (Tone/effect/Tremolo.ts, `new Tone.Tremolo({ frequency, depth,
//! wet, spread: 60 })`): per-channel amplitude LFO. Each channel's gain is a
//! sine LFO through AudioToGain then Scale(min: 1, max: 0), i.e.
//! `g(t) = (1 − depth·sin(2πft + φ)) / 2` — note the wet path sits around a
//! 0.5 midpoint by construction (Tone's min/max mapping), oscillating between
//! (1−depth)/2 and (1+depth)/2. Tone's spread setter puts the LFO phases at
//! 90 ∓ spread/2 degrees, so spread 60 → φL = 60°, φR = 120°. Wet/dry is
//! Tone's CrossFade (equal-power cos/sin law).
//! The rate knob is switchable (makeRate/rateHzOf in devices.ts/schema.ts):
//! rateMode 0 = Sync (tempo division index in rateSync, retuned from the
//! transport bpm via `tick()`), 1 = Low (`rate` Hz), 2 = High (`rateHi` Hz).
//!
//! Deviation from the browser device: devices.ts stops the LFO entirely when
//! depth or mix falls below 0.001 (`lfoGate`, a CPU optimization) which in
//! Tone freezes the wet gain at the LFO's initial value instead of 0.5. We
//! keep the LFO running — the depth → 0 limit stays continuous at gain 0.5.

use crate::traits::EffectDsp;
use crate::util::Smoother;

/// makeEffect("trem"): `spread: 60` → Tone LFO phases 90 ∓ 30 degrees,
/// expressed here in cycles.
const PHASE_L: f64 = 60.0 / 360.0;
const PHASE_R: f64 = 120.0 / 360.0;

/// schema.ts LFO_DIV_TICKS — cycle length in transport ticks per rateSync
/// index (PPQ = 96 ⇒ quarter = 96, bar = 384).
const DIV_TICKS: [f64; 9] = [3072.0, 1536.0, 768.0, 384.0, 192.0, 96.0, 48.0, 32.0, 24.0];

pub struct TremFx {
    sample_rate: f32,
    bpm: f64,
    // Switchable rate state (makeRate defaults: mode 1/Low, rateHi 440, sync 5).
    rate_mode: i32,
    rate_lo: f32,
    rate_hi: f32,
    rate_sync: usize,
    /// Shared LFO phase in cycles [0, 1); per-channel offsets applied at
    /// evaluation time. Step = hz / sample_rate.
    phase: f64,
    step: f64,
    /// Modulation depth 0..1 (Tone LFO amplitude), ~15 ms smoothed.
    depth: Smoother,
    /// Wet/dry mix, 0..1 (Tone `wet`).
    mix: Smoother,
}

impl TremFx {
    pub fn new() -> Self {
        // Schema defaults: rate=5 Hz, depth=0.6, mix=1.
        let mut fx = Self {
            sample_rate: 44_100.0,
            bpm: 120.0,
            rate_mode: 1,
            rate_lo: 5.0,
            rate_hi: 440.0,
            rate_sync: 5,
            phase: 0.0,
            step: 0.0,
            depth: Smoother::new(0.6),
            mix: Smoother::new(1.0),
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

    /// Tone LFO with min 1 / max 0: gain = (1 − depth·sin(θ)) / 2.
    #[inline]
    fn lfo_gain(depth: f32, cycles: f64) -> f32 {
        0.5 * (1.0 - depth * libm::sinf(core::f32::consts::TAU * (cycles as f32)))
    }
}

impl Default for TremFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for TremFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
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
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let depth = self.depth.next();
            let mix = self.mix.next();
            // Tone's wet control is a CrossFade: equal-power cos/sin.
            let dry_g = libm::cosf(mix * core::f32::consts::FRAC_PI_2);
            let wet_g = libm::sinf(mix * core::f32::consts::FRAC_PI_2);
            let gl = Self::lfo_gain(depth, self.phase + PHASE_L);
            let gr = Self::lfo_gain(depth, self.phase + PHASE_R);
            self.phase += self.step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            *ls = *ls * dry_g + *ls * gl * wet_g;
            *rs = *rs * dry_g + *rs * gr * wet_g;
        }
    }

    fn reset(&mut self) {
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

    fn run_blocks(fx: &mut TremFx, l: &mut [f32], r: &mut [f32]) {
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

    /// Sign changes of `x − level` (the amplitude-envelope oscillation
    /// counter). Samples exactly at `level` carry no sign — at 48 kHz an
    /// integer-Hz LFO lands crossings exactly on samples.
    fn crossings(x: &[f32], level: f32) -> usize {
        let mut count = 0;
        let mut last = 0i32;
        for &v in x {
            let d = v - level;
            let s = if d > 0.0 {
                1
            } else if d < 0.0 {
                -1
            } else {
                0
            };
            if s != 0 {
                if last != 0 && s != last {
                    count += 1;
                }
                last = s;
            }
        }
        count
    }

    /// DC-input run: with input ≡ 1 and mix 1 the output IS the wet gain
    /// curve, so amplitude-envelope checks read it directly.
    fn dc_gain_curve(fx: &mut TremFx, seconds: f32) -> (Vec<f32>, Vec<f32>) {
        let n = (seconds * SR) as usize;
        let mut l = vec![1.0f32; n];
        let mut r = vec![1.0f32; n];
        run_blocks(fx, &mut l, &mut r);
        (l, r)
    }

    #[test]
    fn two_seconds_no_nan_inf_and_wet_non_silent() {
        let mut fx = TremFx::new(); // schema defaults: rate 5, depth 0.6, mix 1
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

        let n = (2.0 * SR) as usize;
        let src = noise(n);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        // Fully-wet output is non-silent and differs from dry (0.5 midpoint
        // plus modulation).
        let rms: f32 = (l.iter().map(|v| v * v).sum::<f32>() / n as f32).sqrt();
        assert!(rms > 0.05, "wet output near-silent (rms {rms})");
        let diff: f32 = l.iter().zip(&src).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 1.0, "output identical to dry (diff {diff})");
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = TremFx::new();
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
        // Unknown keys are ignored; mix=0 is a dry passthrough.
        fx.set_param("nope", 3.0);
        fx.set_param("mix", 0.0);
        fx.reset();
        let mut l = [0.25f32; BLOCK];
        let mut r = [-0.25f32; BLOCK];
        fx.process(&mut l, &mut r);
        assert!(l.iter().all(|v| (v - 0.25).abs() < 1e-6));
        assert!(r.iter().all(|v| (v + 0.25).abs() < 1e-6));
    }

    #[test]
    fn amplitude_envelope_oscillates_at_set_rate() {
        // depth 1, mix 1, 4 Hz: gain = (1 − sin(2π·4t + φ))/2 — a 4 Hz curve
        // spanning 0..1 around a 0.5 midpoint. Verify rate two ways over the
        // last integer second: midpoint crossings (2 per cycle → 8) and
        // Goertzel energy at 4 Hz vs an off-rate bin.
        let mut fx = TremFx::new();
        fx.set_param("rateMode", 1.0);
        fx.set_param("rate", 4.0);
        fx.set_param("depth", 1.0);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);

        let (l, _r) = dc_gain_curve(&mut fx, 1.5);
        let tail = &l[l.len() - SR as usize..];

        let min = tail.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = tail.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(min < 0.01 && max > 0.99, "full depth should span 0..1 ({min}..{max})");

        let n = crossings(tail, 0.5);
        assert!((7..=9).contains(&n), "expected ~8 midpoint crossings at 4 Hz, got {n}");

        let at_rate = goertzel(tail, 4.0);
        let off_rate = goertzel(tail, 6.0);
        assert!((at_rate - 0.5).abs() < 0.05, "4 Hz component should be ~0.5 ({at_rate})");
        assert!(at_rate > 10.0 * off_rate, "envelope not at set rate ({at_rate} vs {off_rate})");
    }

    #[test]
    fn synced_rate_follows_bpm() {
        // Sync mode, rateSync 5 (1/4 note): 120 bpm → 2 Hz envelope
        // (rateHzOf: bpm·96 / (60·96)); 90 bpm → 1.5 Hz.
        for (bpm, hz) in [(120.0, 2.0f32), (90.0, 1.5)] {
            let mut fx = TremFx::new();
            fx.set_param("rateMode", 0.0);
            fx.set_param("rateSync", 5.0);
            fx.set_param("depth", 1.0);
            fx.set_param("mix", 1.0);
            fx.prepare(SR);
            fx.tick(0.0, true, bpm);

            let (l, _r) = dc_gain_curve(&mut fx, 3.0);
            let tail = &l[l.len() - 2 * SR as usize..]; // 2 whole seconds
            let n = crossings(tail, 0.5);
            let expected = (2.0 * 2.0 * hz) as usize; // 2 s · 2 crossings/cycle
            assert!(
                n.abs_diff(expected) <= 1,
                "bpm {bpm}: expected ~{expected} crossings, got {n}"
            );
        }
    }

    #[test]
    fn stereo_spread_offsets_channels_and_depth_zero_is_half_gain() {
        // spread 60°: R runs 60° behind L — same rate, visibly different
        // instantaneous gain (peak |L−R| = sin(30°) = 0.5 at depth 1).
        let mut fx = TremFx::new();
        fx.set_param("rate", 4.0);
        fx.set_param("depth", 1.0);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);
        let (l, r) = dc_gain_curve(&mut fx, 1.5);
        let tail_l = &l[l.len() - SR as usize..];
        let tail_r = &r[r.len() - SR as usize..];
        let peak_diff = tail_l
            .iter()
            .zip(tail_r)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!((peak_diff - 0.5).abs() < 0.02, "spread-60 peak L/R diff ~0.5 ({peak_diff})");
        let n_l = crossings(tail_l, 0.5);
        let n_r = crossings(tail_r, 0.5);
        assert!(n_l.abs_diff(n_r) <= 1, "channels at different rates ({n_l} vs {n_r})");

        // depth 0, mix 1: Tone's min-1/max-0 LFO mapping pins the wet gain at
        // the 0.5 midpoint — a constant −6 dB, not unity.
        let mut fx = TremFx::new();
        fx.set_param("depth", 0.0);
        fx.set_param("mix", 1.0);
        fx.prepare(SR);
        let (l, r) = dc_gain_curve(&mut fx, 0.5);
        assert!(l[l.len() - 1..].iter().all(|v| (v - 0.5).abs() < 1e-3));
        assert!(r[r.len() - 1..].iter().all(|v| (v - 0.5).abs() < 1e-3));
    }
}

