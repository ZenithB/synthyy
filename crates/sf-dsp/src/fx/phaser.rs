//! `phaser` effect — port of the Tone.Phaser composite built by
//! `makeEffect("phaser")` in src/audio/devices.ts.
//!
//! Behavior spec (Tone/effect/Phaser.ts, `new Tone.Phaser({ frequency, octaves,
//! baseFrequency: 350, wet })`): per channel, a serial chain of `stages` (Tone
//! default 10) biquad *allpass* filters, all sharing one center frequency that
//! a sine LFO sweeps linearly (in Hz) from `baseFrequency` up to
//! `baseFrequency · 2^octaves`. Q is the Tone default 10 on every stage. The
//! right channel's LFO runs at phase 180° (antiphase). Wet/dry is Tone's
//! CrossFade (equal-power cos/sin law) — the notches themselves come from the
//! dry+wet sum, since the allpass chain alone is magnitude-flat.
//! The rate knob is switchable (makeRate/rateHzOf in devices.ts/schema.ts):
//! rateMode 0 = Sync (tempo division index in rateSync, retuned from the
//! transport bpm via `tick()`), 1 = Low (`rate` Hz), 2 = High (`rateHi` Hz).

use crate::traits::EffectDsp;
use crate::util::Smoother;

/// makeEffect("phaser"): `baseFrequency: 350` (Hz, fixed — not a device param).
const BASE_FREQ: f32 = 350.0;

/// Tone.Phaser defaults: 10 allpass stages per channel, Q = 10.
const STAGES: usize = 10;
const Q: f32 = 10.0;

/// schema.ts LFO_DIV_TICKS — cycle length in transport ticks per rateSync
/// index (PPQ = 96 ⇒ quarter = 96, bar = 384).
const DIV_TICKS: [f64; 9] = [3072.0, 1536.0, 768.0, 384.0, 192.0, 96.0, 48.0, 32.0, 24.0];

/// Damping for the SVF core: k = 1/Q.
const K: f32 = 1.0 / Q;

/// One 2nd-order allpass stage on the same Simper trapezoidal SVF core as
/// `util::Svf` (allpass output = v0 − 2k·v1). A direct-form biquad is NOT
/// usable here: the LFO modulates the center frequency per sample — up to
/// 1 kHz in rateMode High — and DF1/DF2 biquads blow up under audio-rate
/// coefficient modulation, while the trapezoidal SVF stays bounded. State is
/// per stage; the (g, k)-derived coefficients are identical for all 10 stages
/// of a channel, so they are computed once per channel per sample and shared
/// (which util::Svf, recomputing tan per instance, cannot do).
#[derive(Clone, Copy, Default)]
struct Allpass {
    ic1: f32,
    ic2: f32,
}

impl Allpass {
    #[inline]
    fn run(&mut self, v0: f32, a1: f32, a2: f32, a3: f32) -> f32 {
        let v3 = v0 - self.ic2;
        let v1 = a1 * self.ic1 + a2 * v3;
        let v2 = self.ic2 + a2 * self.ic1 + a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        v0 - 2.0 * K * v1
    }

    #[inline]
    fn clear(&mut self) {
        *self = Self::default();
    }
}

pub struct PhaserFx {
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
    /// Sweep span above base in Hz: base·(2^octaves − 1). Stepped param
    /// (schema `octaves` is int), so no smoother — matches Tone's plain set.
    span: f32,
    /// Wet/dry mix, 0..1 (Tone `wet`), ~15 ms smoothed.
    mix: Smoother,
    stages_l: [Allpass; STAGES],
    stages_r: [Allpass; STAGES],
}

impl PhaserFx {
    pub fn new() -> Self {
        // Schema defaults: rate=0.8 Hz, octaves=3, mix=0.5.
        let mut fx = Self {
            sample_rate: 44_100.0,
            bpm: 120.0,
            rate_mode: 1,
            rate_lo: 0.8,
            rate_hi: 440.0,
            rate_sync: 5,
            phase: 0.0,
            step: 0.0,
            span: BASE_FREQ * ((1u32 << 3) as f32 - 1.0),
            mix: Smoother::new(0.5),
            stages_l: [Allpass::default(); STAGES],
            stages_r: [Allpass::default(); STAGES],
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

    /// Shared SVF coefficients (a1, a2, a3) for a center frequency. The
    /// browser context runs at 2× (88.2 kHz) where 350·2^6 = 22.4 kHz is
    /// legal; at engine rates we clamp just below Nyquist (as util::Svf does)
    /// to keep the prewarp finite.
    #[inline]
    fn coeffs(&self, freq: f32) -> (f32, f32, f32) {
        let f = freq.clamp(1.0, 0.49 * self.sample_rate);
        let g = libm::tanf(core::f32::consts::PI * f / self.sample_rate);
        let a1 = 1.0 / (1.0 + g * (g + K));
        let a2 = g * a1;
        (a1, a2, g * a2)
    }
}

impl Default for PhaserFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for PhaserFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.mix.prepare(self.sample_rate);
        self.mix.snap(self.mix.target());
        self.update_step();
        self.phase = 0.0;
        for s in self.stages_l.iter_mut().chain(self.stages_r.iter_mut()) {
            s.clear();
        }
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
            // Tone: lfo max = base·2^octaves (devices.ts truncates: p.octaves|0).
            "octaves" => {
                let oct = (value as i32).clamp(1, 6) as u32;
                self.span = BASE_FREQ * ((1u32 << oct) as f32 - 1.0);
            }
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
            let mix = self.mix.next();
            // Tone's wet control is a CrossFade: equal-power cos/sin.
            let dry_g = libm::cosf(mix * core::f32::consts::FRAC_PI_2);
            let wet_g = libm::sinf(mix * core::f32::consts::FRAC_PI_2);
            // Tone.LFO min..max maps the sine linearly in Hz; right at 180°:
            // sin(θ+π) = −sin θ.
            let s = libm::sinf(core::f32::consts::TAU * self.phase as f32);
            self.phase += self.step;
            if self.phase >= 1.0 {
                self.phase -= 1.0;
            }
            let f_l = BASE_FREQ + (0.5 + 0.5 * s) * self.span;
            let f_r = BASE_FREQ + (0.5 - 0.5 * s) * self.span;
            let (a1l, a2l, a3l) = self.coeffs(f_l);
            let (a1r, a2r, a3r) = self.coeffs(f_r);
            let mut wl = *ls;
            for st in &mut self.stages_l {
                wl = st.run(wl, a1l, a2l, a3l);
            }
            let mut wr = *rs;
            for st in &mut self.stages_r {
                wr = st.run(wr, a1r, a2r, a3r);
            }
            *ls = *ls * dry_g + wl * wet_g;
            *rs = *rs * dry_g + wr * wet_g;
        }
    }

    fn reset(&mut self) {
        self.mix.snap(self.mix.target());
        self.phase = 0.0;
        for s in self.stages_l.iter_mut().chain(self.stages_r.iter_mut()) {
            s.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;

    fn run_blocks(fx: &mut PhaserFx, l: &mut [f32], r: &mut [f32]) {
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

    /// Transfer-shape estimate over a window: wet/dry magnitude ratio at a
    /// log-spaced bank inside the sweep range, normalized to unit sum. Taking
    /// the ratio against the SAME dry window cancels the noise realization,
    /// so what remains is the filter's response shape at that time.
    fn transfer_profile(wet: &[f32], dry: &[f32]) -> Vec<f32> {
        let bank: Vec<f32> = (0..24)
            .map(|i| 300.0 * libm::powf(2.0, i as f32 * 5.0 / 24.0)) // 300 Hz..9.6 kHz
            .collect();
        let h: Vec<f32> = bank
            .iter()
            .map(|&f| goertzel(wet, f) / goertzel(dry, f).max(1e-9))
            .collect();
        let sum: f32 = h.iter().sum::<f32>().max(1e-12);
        h.iter().map(|m| m / sum).collect()
    }

    /// L1 distance between the transfer shapes of two windows of a run.
    fn shape_delta(wet: &[f32], dry: &[f32], t0: usize, t1: usize, w: usize) -> f32 {
        let p0 = transfer_profile(&wet[t0..t0 + w], &dry[t0..t0 + w]);
        let p1 = transfer_profile(&wet[t1..t1 + w], &dry[t1..t1 + w]);
        p0.iter().zip(&p1).map(|(x, y)| (x - y).abs()).sum()
    }

    #[test]
    fn two_seconds_no_nan_inf_and_wet_non_silent() {
        let mut fx = PhaserFx::new(); // schema defaults: rate 0.8, octaves 3, mix 0.5
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
        // Antiphase L/R LFOs: channels must decorrelate.
        let lr: f32 = l.iter().zip(&r).map(|(a, b)| (a - b).abs()).sum();
        assert!(lr > 1.0, "L/R identical (diff {lr})");
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = PhaserFx::new();
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
                fx.set_param("octaves", 1.0 + t * 5.0);
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
        // Unknown keys are ignored; mix=0 returns to (near) dry.
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
    fn notches_move_over_time_on_white_noise() {
        // mix 0.5 sums dry with the swept allpass chain → moving comb of
        // notches. With a 0.5 Hz LFO, windows 1 s apart sit at opposite sweep
        // extremes, so the transfer shape at t0 must differ from t1 — far
        // more than in a control run whose LFO (0.01 Hz) is effectively
        // frozen over the same span.
        let render = |rate: f32| -> (Vec<f32>, Vec<f32>) {
            let mut fx = PhaserFx::new();
            fx.set_param("rateMode", 1.0);
            fx.set_param("rate", rate);
            fx.set_param("octaves", 4.0);
            fx.set_param("mix", 0.5);
            fx.prepare(SR);
            let n = (2.0 * SR) as usize;
            let src = noise(n);
            let mut l = src.clone();
            let mut r = src.clone();
            run_blocks(&mut fx, &mut l, &mut r);
            (l, src)
        };
        let w = (0.4 * SR) as usize;
        let t0 = (0.2 * SR) as usize;
        let t1 = (1.2 * SR) as usize; // half a 0.5 Hz cycle later
        let (wet, dry) = render(0.5);
        let d_move = shape_delta(&wet, &dry, t0, t1, w);
        let (wet_s, dry_s) = render(0.01);
        let d_static = shape_delta(&wet_s, &dry_s, t0, t1, w);
        assert!(
            d_move > 3.0 * d_static && d_move > 0.1,
            "notches did not move: sweeping delta {d_move}, static control {d_static}"
        );
    }

    #[test]
    fn allpass_chain_is_magnitude_flat_at_full_wet() {
        // At mix=1 the output is the bare allpass chain: magnitude ≈ input's.
        // (The notches exist only in the dry+wet sum — Tone.Phaser topology.)
        let mut fx = PhaserFx::new();
        fx.set_param("rate", 0.01); // near-static sweep
        fx.set_param("mix", 1.0);
        fx.prepare(SR);

        let n = SR as usize;
        let src = noise(n);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        let tail = SR as usize / 2; // skip the filter settling
        let rms = |x: &[f32]| {
            libm::sqrtf(x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32)
        };
        let (a, b) = (rms(&l[tail..]), rms(&src[tail..]));
        assert!(
            (a / b - 1.0).abs() < 0.1,
            "allpass chain not magnitude-flat: wet rms {a} vs dry {b}"
        );
    }

    #[test]
    fn synced_rate_follows_bpm() {
        // Sync mode, rateSync 5 (1/4 note): 120 bpm → 2 Hz sweep. The spectral
        // shape must repeat one LFO period (0.5 s) later, but differ half a
        // period later.
        let mut fx = PhaserFx::new();
        fx.set_param("rateMode", 0.0);
        fx.set_param("rateSync", 5.0);
        fx.set_param("octaves", 4.0);
        fx.set_param("mix", 0.5);
        fx.prepare(SR);
        fx.tick(0.0, true, 120.0);

        let n = (2.0 * SR) as usize;
        let src = noise(n);
        let mut l = src.clone();
        let mut r = src.clone();
        run_blocks(&mut fx, &mut l, &mut r);

        let w = (0.1 * SR) as usize;
        let t0 = (0.5 * SR) as usize;
        let same = (1.0 * SR) as usize; // +1 full period at 2 Hz
        let opp = (0.75 * SR) as usize; // +half period
        let d_same = shape_delta(&l, &src, t0, same, w);
        let d_opp = shape_delta(&l, &src, t0, opp, w);
        assert!(
            d_opp > d_same,
            "synced sweep period wrong: opposite-phase delta {d_opp} <= same-phase delta {d_same}"
        );
    }
}
