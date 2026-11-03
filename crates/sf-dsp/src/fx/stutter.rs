//! `stutter` effect — line-for-line port of the `sf-stutter` AudioWorklet
//! (public/sf-stutter.js), the canonical DSP for this device. Any change
//! there must land here too (CLAUDE.md: render-path parity).
//!
//! Tempo-synced beat repeat: on each `interval` boundary a seeded die
//! (`chance`) starts a stutter lasting `gate` × interval that repeats a
//! `grid`-length slice captured at the trigger. Repeat n shrinks by
//! (1 − decay/2)^n, plays at 2^(pitch·n/12), reverses with probability
//! `reverse` and fades by (1 − fade·0.3)^n; 2 ms crossfades everywhere.
//! Stopped transport ⇒ dry. The xorshift32 RNG is reseeded on every
//! transport start so live playback and exports agree.
//!
//! Schema (schema.ts "stutter"): interval 1/4,1/2,1 bar,2 bars (1 bar),
//! chance 0..1 (1), gate 0..1 (0.5), grid 1/4…1/64,1/8T (1/16), decay 0..1,
//! pitch −12..12 st, reverse 0..1, fade 0..1, mix 0..1 (1), gain ±24 dB.

use crate::traits::EffectDsp;

const PPQ: f64 = 96.0;
const IVL_TICKS: [f64; 4] = [96.0, 192.0, 384.0, 768.0];
const GRID_TICKS: [f64; 6] = [96.0, 48.0, 24.0, 12.0, 6.0, 32.0];
const MIN_SLICE_TICKS: f64 = 3.0;
const SEED: u32 = 0x9e37_79b9;

#[derive(Clone, Copy)]
struct Params {
    interval: f64,
    chance: f64,
    gate: f64,
    grid: f64,
    decay: f64,
    pitch: f64,
    reverse: f64,
    fade: f64,
    mix: f64,
    gain: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            interval: 2.0,
            chance: 1.0,
            gate: 0.5,
            grid: 2.0,
            decay: 0.0,
            pitch: 0.0,
            reverse: 0.0,
            fade: 0.0,
            mix: 1.0,
            gain: 0.0,
        }
    }
}

/// xorshift32, identical to the worklet's (`>>> 0` ⇒ u32 wrapping).
#[inline]
pub(crate) fn xorshift(state: &mut u32) -> f64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x as f64 / 4_294_967_296.0
}

/// JS `Math.round` for the non-negative values used here.
#[inline]
fn round_js(x: f64) -> f64 {
    libm::floor(x + 0.5)
}

pub struct StutterFx {
    p: Params,
    sr: f64,
    buf_l: Vec<f64>,
    buf_r: Vec<f64>,
    w: usize,
    fade_len: f64,
    rng: u32,
    active: bool,
    env: f64,
    left: f64,
    start: usize,
    rep: i32,
    rp: f64,
    rep_len: f64,
    rate: f64,
    rev: bool,
    rep_gain: f64,
    base_slice: f64,
    pos: f64,
    bpm: f64,
    playing: bool,
}

impl StutterFx {
    pub fn new() -> Self {
        let mut s = Self {
            p: Params::default(),
            sr: 44_100.0,
            buf_l: Vec::new(),
            buf_r: Vec::new(),
            w: 0,
            fade_len: 1.0,
            rng: SEED,
            active: false,
            env: 0.0,
            left: 0.0,
            start: 0,
            rep: 0,
            rp: 0.0,
            rep_len: 0.0,
            rate: 1.0,
            rev: false,
            rep_gain: 1.0,
            base_slice: 0.0,
            pos: 0.0,
            bpm: 120.0,
            playing: false,
        };
        s.prepare(44_100.0);
        s
    }

    fn rand(&mut self) -> f64 {
        xorshift(&mut self.rng)
    }

    fn begin_repeat(&mut self) {
        let p = self.p;
        let n = self.rep;
        let shrink = libm::pow(1.0 - p.decay.clamp(0.0, 1.0) * 0.5, n as f64);
        let min_len = (MIN_SLICE_TICKS * 60.0 / (self.bpm * PPQ)) * self.sr;
        self.rep_len = min_len.max(self.base_slice * shrink);
        self.rate = libm::pow(2.0, (p.pitch * n as f64) / 12.0);
        self.rev = n > 0 && self.rand() < p.reverse;
        self.rep_gain = libm::pow(1.0 - p.fade.clamp(0.0, 1.0) * 0.3, n as f64);
        self.rp = 0.0;
    }

    fn read_slice(&self, right: bool, q: f64) -> f64 {
        let buf = if right { &self.buf_r } else { &self.buf_l };
        let len = buf.len();
        let mut pos = self.start as f64 + q;
        while pos >= len as f64 {
            pos -= len as f64;
        }
        let fl = libm::floor(pos);
        let fr = pos - fl;
        let mut i0 = fl as usize;
        if i0 >= len {
            i0 -= len;
        }
        let i1 = if i0 + 1 == len { 0 } else { i0 + 1 };
        buf[i0] + (buf[i1] - buf[i0]) * fr
    }
}

impl Default for StutterFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for StutterFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sr = sample_rate.max(1.0) as f64;
        let len = libm::ceil(4.0 * self.sr) as usize;
        self.buf_l = vec![0.0; len];
        self.buf_r = vec![0.0; len];
        self.fade_len = round_js(0.002 * self.sr).max(1.0);
        self.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        let v = value as f64;
        if !v.is_finite() {
            return;
        }
        let p = &mut self.p;
        let slot = match key {
            "interval" => &mut p.interval,
            "chance" => &mut p.chance,
            "gate" => &mut p.gate,
            "grid" => &mut p.grid,
            "decay" => &mut p.decay,
            "pitch" => &mut p.pitch,
            "reverse" => &mut p.reverse,
            "fade" => &mut p.fade,
            "mix" => &mut p.mix,
            "gain" => &mut p.gain,
            _ => return,
        };
        *slot = v;
    }

    fn tick(&mut self, pos_ticks: f64, playing: bool, bpm: f64) {
        self.bpm = if bpm > 0.0 { bpm } else { 120.0 };
        if !playing {
            self.playing = false;
            self.active = false;
            return;
        }
        if !self.playing {
            self.rng = SEED;
            self.active = false;
            self.env = 0.0;
        }
        if !self.playing || (pos_ticks - self.pos).abs() > 2.0 {
            self.pos = pos_ticks;
        }
        self.playing = true;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let p = self.p;
        let sr = self.sr;
        let len = self.buf_l.len();
        let fl = self.fade_len;
        let ivl = IVL_TICKS[(p.interval as i64).clamp(0, 3) as usize];
        let grid = GRID_TICKS[(p.grid as i64).clamp(0, 5) as usize];
        let mix = p.mix.clamp(0.0, 1.0);
        let out_lin = libm::pow(10.0, p.gain / 20.0);
        let tps = (self.bpm / 60.0) * PPQ / sr;
        let spt = sr * 60.0 / (self.bpm * PPQ);

        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let xl = *ls as f64;
            let xr = *rs as f64;
            self.buf_l[self.w] = xl;
            self.buf_r[self.w] = xr;

            // ---- trigger on interval boundaries ----
            if self.playing {
                let prev = self.pos;
                self.pos += tps;
                if libm::floor(self.pos / ivl) != libm::floor(prev / ivl) && self.rand() < p.chance {
                    self.active = true;
                    self.start = self.w;
                    self.left = round_js(p.gate.clamp(0.0, 1.0) * ivl * spt).max(1.0);
                    self.base_slice = grid * spt;
                    self.rep = 0;
                    self.begin_repeat();
                }
            }

            // ---- stutter voice (runs through the release crossfade) ----
            let mut sl = 0.0;
            let mut sr2 = 0.0;
            if self.active || self.env > 0.0 {
                if self.rp >= self.rep_len {
                    self.rep += 1;
                    self.begin_repeat();
                }
                let q = if self.rev { self.rep_len - self.rp } else { self.rp };
                let edge = 1.0f64.min(self.rp / fl).min((self.rep_len - self.rp) / fl);
                let g = self.rep_gain * if self.rep == 0 { 1.0 } else { edge.max(0.0) };
                let qc = q.min(self.rep_len).max(0.0);
                sl = self.read_slice(false, qc) * g;
                sr2 = self.read_slice(true, qc) * g;
                self.rp += if self.rep == 0 { 1.0 } else { self.rate };
                if self.active {
                    self.left -= 1.0;
                    if self.left <= 0.0 {
                        self.active = false;
                    }
                }
            }
            let target = if self.active { 1.0 } else { 0.0 };
            self.env +=
                if target > self.env { (1.0 / fl).min(target - self.env) } else { -(1.0 / fl).min(self.env - target) };
            let e = self.env * mix;
            self.w = if self.w + 1 == len { 0 } else { self.w + 1 };

            *ls = ((sl * e + xl * (1.0 - e)) * out_lin) as f32;
            *rs = ((sr2 * e + xr * (1.0 - e)) * out_lin) as f32;
        }
    }

    fn reset(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.w = 0;
        self.rng = SEED;
        self.active = false;
        self.env = 0.0;
        self.left = 0.0;
        self.rep = 0;
        self.rp = 0.0;
        self.rep_len = 0.0;
        self.pos = 0.0;
        self.playing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(fx: &mut StutterFx, x: &[f32], playing: bool) -> Vec<f32> {
        let mut l = x.to_vec();
        let mut r = x.to_vec();
        let mut pos = 0.0;
        for (cl, cr) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            fx.tick(pos, playing, 120.0);
            fx.process(cl, cr);
            pos += 128.0 * 192.0 / SR as f64;
        }
        l
    }

    #[test]
    fn stopped_transport_is_dry() {
        let mut fx = StutterFx::new();
        fx.prepare(SR);
        let x: Vec<f32> = (0..48_000).map(|i| libm::sinf(i as f32 * 0.01)).collect();
        let y = run(&mut fx, &x, false);
        assert!(y.iter().zip(&x).all(|(a, b)| (a - b).abs() < 1e-6));
    }

    #[test]
    fn repeats_the_slice_after_a_bar_boundary() {
        // 1/4-note interval, 1/16 grid, gate 1: the second beat (from 0.5 s)
        // is the first 1/16 (6000 samples at 120 bpm) of that beat repeated.
        let mut fx = StutterFx::new();
        fx.prepare(SR);
        fx.set_param("interval", 0.0);
        fx.set_param("gate", 1.0);
        let x: Vec<f32> = (0..48_000).map(|i| (i as f32 * 1e-5).sin()).collect();
        let y = run(&mut fx, &x, true);
        let b = 24_000; // first boundary at tick 96
        for k in [200usize, 1000, 3000] {
            let a = y[b + k];
            let c = y[b + 6000 + k];
            assert!((a - c).abs() < 1e-3, "repeat mismatch at {k}: {a} vs {c}");
        }
        assert!(y.iter().all(|s| s.is_finite()));
    }
}
