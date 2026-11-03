//! `orbit` effect — line-for-line port of the `sf-orbit` AudioWorklet
//! (public/sf-orbit.js), the canonical DSP for this device. Any change there
//! must land here too (CLAUDE.md: render-path parity).
//!
//! Binaural 3D mover: the mono sum of the input travels a tempo-synced path
//! around the listener. Per sample: path phase → position → per-ear delay
//! (Doppler propagation + Woodworth ITD) read from a mono ring with linear
//! interpolation → Brown–Duda spherical-head shadow per ear → one-pole
//! low-pass (air absorption + behind-the-head darkening) → 1/r gain.
//!
//! Precision: the worklet runs in JS doubles, so all state is f64 here.
//!
//! Schema (schema.ts "orbit"): path Circle/Figure 8/Fly-by/Pendulum/Drift,
//! rate 8 bars…1/8, dist 0.5..10 m (2), height −1..1 (0), width 0..1 (1),
//! doppler 0..1 (0.5), mix 0..1 (1), gain −24..24 dB (0).

use crate::traits::EffectDsp;
use core::f64::consts::PI;

const PPQ: f64 = 96.0;
const C: f64 = 343.0;
const HEAD: f64 = 0.0875;
const W0: f64 = C / HEAD;
const ALPHA_MIN: f64 = 0.1;
const THETA_MIN: f64 = (150.0 * PI) / 180.0;
const RATE_TICKS: [f64; 7] = [3072.0, 1536.0, 768.0, 384.0, 192.0, 96.0, 48.0];

#[derive(Clone, Copy)]
struct Params {
    path: f64,
    rate: f64,
    dist: f64,
    height: f64,
    width: f64,
    doppler: f64,
    mix: f64,
    gain: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self { path: 0.0, rate: 3.0, dist: 2.0, height: 0.0, width: 1.0, doppler: 0.5, mix: 1.0, gain: 0.0 }
    }
}

pub struct OrbitFx {
    p: Params,
    sr: f64,
    ring: Vec<f64>,
    w: usize,
    d_l: f64,
    d_r: f64,
    hs_l: [f64; 2],
    hs_r: [f64; 2],
    lp_l: f64,
    lp_r: f64,
    first: bool,
    smooth: f64,
    pos: f64,
    bpm: f64,
    playing: bool,
}

/// JS `x | 0` for a small finite value (truncate toward zero).
#[inline]
fn trunc_i(x: f64) -> i64 {
    x as i64
}

impl OrbitFx {
    pub fn new() -> Self {
        let mut o = Self {
            p: Params::default(),
            sr: 44_100.0,
            ring: Vec::new(),
            w: 0,
            d_l: 0.0,
            d_r: 0.0,
            hs_l: [0.0; 2],
            hs_r: [0.0; 2],
            lp_l: 0.0,
            lp_r: 0.0,
            first: true,
            smooth: 0.0,
            pos: 0.0,
            bpm: 120.0,
            playing: false,
        };
        o.prepare(44_100.0);
        o
    }

    fn path_xy(path: i64, f: f64) -> (f64, f64) {
        let t = 2.0 * PI;
        match path {
            0 => (libm::sin(t * f), libm::cos(t * f)),
            1 => (libm::sin(t * f), libm::sin(2.0 * t * f)),
            2 => (2.0 * (1.0 - 4.0 * (f - 0.5).abs()), 0.35),
            3 => (libm::sin(t * f), 1.0),
            _ => (
                0.6 * libm::sin(t * f) + 0.4 * libm::sin(t * 2.31 * f + 1.0),
                0.6 * libm::cos(t * 1.37 * f) + 0.4 * libm::cos(t * 0.73 * f + 2.0),
            ),
        }
    }

    /// Brown–Duda head shadow, bilinear-transformed → (b0, b1, a1).
    fn shadow(&self, theta_inc: f64) -> (f64, f64, f64) {
        let a = 1.0 + ALPHA_MIN / 2.0 + (1.0 - ALPHA_MIN / 2.0) * libm::cos((theta_inc / THETA_MIN) * PI);
        let k = 2.0 * self.sr;
        let den = 2.0 * W0 + k;
        ((2.0 * W0 + a * k) / den, (2.0 * W0 - a * k) / den, (2.0 * W0 - k) / den)
    }

    fn read_ring(&self, d: f64) -> f64 {
        let len = self.ring.len();
        let mut pos = self.w as f64 - d;
        while pos < 0.0 {
            pos += len as f64;
        }
        let fl = libm::floor(pos);
        let fr = pos - fl;
        let mut i0 = fl as usize;
        if i0 >= len {
            i0 -= len;
        }
        let i1 = if i0 + 1 == len { 0 } else { i0 + 1 };
        self.ring[i0] + (self.ring[i1] - self.ring[i0]) * fr
    }
}

impl Default for OrbitFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for OrbitFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sr = sample_rate.max(1.0) as f64;
        let len = libm::ceil(0.1 * self.sr) as usize + 4;
        self.ring = vec![0.0; len];
        self.smooth = 1.0 - libm::exp(-1.0 / (0.02 * self.sr));
        self.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        let v = value as f64;
        if !v.is_finite() {
            return;
        }
        let p = &mut self.p;
        let slot = match key {
            "path" => &mut p.path,
            "rate" => &mut p.rate,
            "dist" => &mut p.dist,
            "height" => &mut p.height,
            "width" => &mut p.width,
            "doppler" => &mut p.doppler,
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
            return;
        }
        if !self.playing || (pos_ticks - self.pos).abs() > 2.0 {
            self.pos = pos_ticks;
        }
        self.playing = true;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let p = self.p;
        let sr = self.sr;
        let path = trunc_i(p.path).clamp(0, 4);
        let cyc = RATE_TICKS[trunc_i(p.rate).clamp(0, 6) as usize];
        let dist = p.dist.clamp(0.5, 10.0);
        let width = p.width.clamp(0.0, 1.0);
        let height = p.height.clamp(-1.0, 1.0);
        let dop = p.doppler.clamp(0.0, 1.0);
        let mix = p.mix.clamp(0.0, 1.0);
        let out_lin = libm::pow(10.0, p.gain / 20.0);
        let tps = (self.bpm / 60.0) * PPQ / sr;
        let len = self.ring.len();
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let xl = *ls as f64;
            let xr = *rs as f64;
            self.ring[self.w] = (xl + xr) * 0.5;

            // ---- position ----
            let mut f = (self.pos / cyc) % 1.0;
            if f < 0.0 {
                f += 1.0;
            }
            self.pos += tps;
            let (px, py) = Self::path_xy(path, f);
            let x = px * width * dist;
            let y = py * dist;
            let z = height * dist;
            let rr = libm::sqrt(x * x + y * y + z * z).max(0.3);
            let lat = libm::asin((x / rr).clamp(-1.0, 1.0));
            let front = y / rr;

            // ---- per-ear delays ----
            let prop = (dop * rr / C) * sr;
            let itd = (HEAD / C) * (lat.abs() + libm::sin(lat.abs())) * sr;
            let t_l = prop + if lat > 0.0 { itd } else { 0.0 };
            let t_r = prop + if lat < 0.0 { itd } else { 0.0 };
            if self.first {
                self.d_l = t_l;
                self.d_r = t_r;
                self.first = false;
            }
            self.d_l += (t_l - self.d_l) * self.smooth;
            self.d_r += (t_r - self.d_r) * self.smooth;

            let mut s_l = self.read_ring(self.d_l);
            let mut s_r = self.read_ring(self.d_r);
            self.w = if self.w + 1 == len { 0 } else { self.w + 1 };

            // ---- head shadow per ear ----
            let (b0, b1, a1) = self.shadow(PI / 2.0 + lat);
            let yv = b0 * s_l + b1 * self.hs_l[0] - a1 * self.hs_l[1];
            self.hs_l = [s_l, yv];
            s_l = yv;
            let (b0, b1, a1) = self.shadow(PI / 2.0 - lat);
            let yv = b0 * s_r + b1 * self.hs_r[0] - a1 * self.hs_r[1];
            self.hs_r = [s_r, yv];
            s_r = yv;

            // ---- air absorption + behind-the-head darkening ----
            let fc_air = 20000.0 / (1.0 + rr * 0.15);
            let fc_back = 6000.0 + 14000.0 * (0.5 + 0.5 * front);
            let fc = fc_air.min(fc_back).min(sr * 0.45);
            let g = 1.0 - libm::exp((-2.0 * PI * fc) / sr);
            self.lp_l += (s_l - self.lp_l) * g;
            self.lp_r += (s_r - self.lp_r) * g;

            let dg = (1.0 / rr).min(2.0);
            let wl = self.lp_l * dg;
            let wr = self.lp_r * dg;
            *ls = ((wl * mix + xl * (1.0 - mix)) * out_lin) as f32;
            *rs = ((wr * mix + xr * (1.0 - mix)) * out_lin) as f32;
        }
    }

    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.w = 0;
        self.d_l = 0.0;
        self.d_r = 0.0;
        self.hs_l = [0.0; 2];
        self.hs_r = [0.0; 2];
        self.lp_l = 0.0;
        self.lp_r = 0.0;
        self.first = true;
        self.pos = 0.0;
        self.playing = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(fx: &mut OrbitFx, x: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut l = x.to_vec();
        let mut r = x.to_vec();
        let mut pos = 0.0;
        for (cl, cr) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            fx.tick(pos, true, 120.0);
            fx.process(cl, cr);
            pos += 128.0 * 192.0 / SR as f64;
        }
        (l, r)
    }

    fn rms(v: &[f32]) -> f32 {
        libm::sqrtf(v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32)
    }

    #[test]
    fn source_on_the_right_is_louder_in_the_right_ear() {
        // Pendulum at phase 0.25 sits hard right; 8-bar rate barely moves it.
        let mut fx = OrbitFx::new();
        fx.prepare(SR);
        fx.set_param("path", 3.0);
        fx.set_param("rate", 0.0);
        fx.tick(768.0, true, 120.0); // 0.25 of 3072 ticks
        let x: Vec<f32> = (0..9600).map(|i| libm::sinf(i as f32 * 0.37) * 0.5).collect();
        let mut l = x.clone();
        let mut r = x.clone();
        fx.process(&mut l, &mut r);
        assert!(rms(&r[4800..]) > rms(&l[4800..]) * 1.5);
    }

    #[test]
    fn dry_mix_is_passthrough_and_output_is_finite() {
        let mut fx = OrbitFx::new();
        fx.prepare(SR);
        fx.set_param("mix", 0.0);
        let x: Vec<f32> = (0..4800).map(|i| libm::sinf(i as f32 * 0.05)).collect();
        let (l, r) = run(&mut fx, &x);
        assert!(l.iter().zip(&x).all(|(a, b)| (a - b).abs() < 1e-6));
        assert!(r.iter().zip(&x).all(|(a, b)| (a - b).abs() < 1e-6));
        let mut fx = OrbitFx::new();
        fx.prepare(SR);
        fx.set_param("path", 4.0);
        fx.set_param("doppler", 0.0);
        fx.set_param("dist", 0.5);
        let (l, r) = run(&mut fx, &x);
        assert!(l.iter().chain(&r).all(|s| s.is_finite()));
    }
}
