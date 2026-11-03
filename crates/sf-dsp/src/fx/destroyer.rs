//! `destroyer` effect — "distortion plus". Line-for-line port of the
//! `sf-destroyer` AudioWorklet (public/sf-destroyer.js), the canonical DSP.
//! Every expression keeps the worklet's operation order and f64 precision so
//! the two are bit-identical (verified against the worklet run in Node);
//! change both together (CLAUDE.md render-path parity).
//!
//!   in ─► Flatten ─┬─► [Pre filters] ─► Drive ─► Clipper ─► DC block ─► [Post filters] ─┐
//!                  └─► [Side filters, latency-aligned] ───────────────────────────────────┴─► Tone ─► Mix ─► Out ─► Limiter
//!
//! Flatten: one-knob stereo-linked leveller. F1/F2: TPT SVF (Off/LP/BP/HP/BR)
//! placed Pre / Post / Side by `route` (3 × 3 = 9 routings); F3 = Tone, always
//! last. Clipper: six curves with bias, 1×/2×/4× oversampled through
//! windowed-sinc half-band FIRs (latency 0/15/23 samples, matched on the dry
//! and Side paths), 5 Hz DC blocker. Output: mix, gain, optional limiter.

use crate::traits::EffectDsp;

const PI: f64 = core::f64::consts::PI;
const FLAT_REF: f64 = 0.25;
const FLAT_MIN: f64 = 1.0 / 16.0;
const FLAT_MAX: f64 = 4.0;
const LIM_CEIL: f64 = 0.97;
const TAPS_A: usize = 31;
const TAPS_B: usize = 33;
const DLEN: usize = 32;
/// route → [F1 role, F2 role]; 0 = Pre, 1 = Post, 2 = Side.
const ROUTES: [[u8; 2]; 9] = [[0, 0], [0, 1], [0, 2], [1, 0], [1, 1], [1, 2], [2, 0], [2, 1], [2, 2]];

#[inline]
fn db(v: f64) -> f64 {
    libm::pow(10.0, v / 20.0)
}

fn halfband<const N: usize>() -> [f64; N] {
    let mut h = [0.0f64; N];
    let c = (N as f64 - 1.0) / 2.0;
    let mut sum = 0.0;
    for (i, hi) in h.iter_mut().enumerate() {
        let x = (i as f64 - c) * 0.5;
        let sinc = if x == 0.0 { 1.0 } else { libm::sin(PI * x) / (PI * x) };
        let w = 0.42 - 0.5 * libm::cos((2.0 * PI * i as f64) / (N as f64 - 1.0))
            + 0.08 * libm::cos((4.0 * PI * i as f64) / (N as f64 - 1.0));
        *hi = 0.5 * sinc * w;
        sum += *hi;
    }
    for hi in h.iter_mut() {
        *hi /= sum;
    }
    h
}

#[derive(Clone, Copy)]
struct Fir<const N: usize> {
    buf: [f64; N],
    pos: usize,
}

impl<const N: usize> Fir<N> {
    const fn new() -> Self {
        Self { buf: [0.0; N], pos: 0 }
    }
    fn reset(&mut self) {
        self.buf = [0.0; N];
        self.pos = 0;
    }
    #[inline]
    fn push(&mut self, x: f64) {
        self.pos = if self.pos + 1 == N { 0 } else { self.pos + 1 };
        self.buf[self.pos] = x;
    }
    #[inline]
    fn out(&self, h: &[f64; N]) -> f64 {
        let mut acc = 0.0;
        let mut j = self.pos;
        for hi in h.iter() {
            acc += hi * self.buf[j];
            j = if j == 0 { N - 1 } else { j - 1 };
        }
        acc
    }
}

#[derive(Clone, Copy)]
struct ClipChan {
    up_a: Fir<TAPS_A>,
    dn_a: Fir<TAPS_A>,
    up_b: Fir<TAPS_B>,
    dn_b: Fir<TAPS_B>,
    dcx: f64,
    dcy: f64,
}

impl ClipChan {
    const fn new() -> Self {
        Self { up_a: Fir::new(), dn_a: Fir::new(), up_b: Fir::new(), dn_b: Fir::new(), dcx: 0.0, dcy: 0.0 }
    }
    fn reset(&mut self) {
        self.up_a.reset();
        self.dn_a.reset();
        self.up_b.reset();
        self.dn_b.reset();
        self.dcx = 0.0;
        self.dcy = 0.0;
    }
}

#[derive(Clone, Copy, Default)]
struct Svf {
    ic1: f64,
    ic2: f64,
}

#[derive(Clone, Copy, Default)]
struct Slot {
    ty: i32,
    k: f64,
    a1: f64,
    a2: f64,
    a3: f64,
    vol: f64,
}

/// Clipper curves; slope 1 at 0 for every curve.
#[inline]
fn shape(curve: i32, u: f64) -> f64 {
    match curve {
        0 => libm::tanh(u),
        1 => u.clamp(-1.0, 1.0),
        2 => {
            if u >= 0.0 {
                1.0 - libm::exp(-u)
            } else {
                -0.7 * (1.0 - libm::exp(u / 0.7))
            }
        }
        3 => {
            if u >= 0.0 {
                u / (1.0 + u)
            } else {
                u / (1.0 - 1.5 * u)
            }
        }
        4 => libm::sin(u),
        _ => {
            let t = libm::tanh(u);
            0.5 * t + 0.5 * t.abs()
        }
    }
}

/// Raw schema params (JS `this.p`).
#[derive(Clone, Copy)]
struct Params {
    drive: f64,
    flat: f64,
    curve: f64,
    bias: f64,
    thresh: f64,
    route: f64,
    f: [[f64; 4]; 3], // per filter: type, freq, res, vol
    os: f64,
    limit: f64,
    mix: f64,
    gain: f64,
}

impl Default for Params {
    /// Schema defaults (schema.ts "destroyer").
    fn default() -> Self {
        Self {
            drive: 18.0,
            flat: 0.3,
            curve: 2.0,
            bias: 0.0,
            thresh: 0.0,
            route: 0.0,
            f: [[0.0, 800.0, 0.2, 0.0], [0.0, 3000.0, 0.2, 0.0], [0.0, 8000.0, 0.1, 0.0]],
            os: 1.0,
            limit: 1.0,
            mix: 1.0,
            gain: 0.0,
        }
    }
}

pub struct DestroyerFx {
    p: Params,
    sr: f64,
    dirty: bool,
    h_a: [f64; TAPS_A],
    h_b: [f64; TAPS_B],
    clip: [ClipChan; 2],
    svf: [[Svf; 2]; 3],
    fc: [Slot; 3],
    env: f64,
    lenv: f64,
    d_dry: [[f64; DLEN]; 2],
    d_flat: [[f64; DLEN]; 2],
    widx: usize,
    os_now: i32,
    // derived
    role1: u8,
    role2: u8,
    curve: i32,
    bias: f64,
    bias_out: f64,
    thr_lin: f64,
    drive_gain: f64,
    flat: f64,
    flat_att: f64,
    flat_rel: f64,
    lim_rel: f64,
    dc_r: f64,
    mix: f64,
    out_lin: f64,
    limit: bool,
    latency: usize,
}

impl DestroyerFx {
    pub fn new() -> Self {
        let mut d = Self {
            p: Params::default(),
            sr: 44_100.0,
            dirty: true,
            h_a: halfband::<TAPS_A>(),
            h_b: halfband::<TAPS_B>(),
            clip: [ClipChan::new(); 2],
            svf: [[Svf::default(); 2]; 3],
            fc: [Slot::default(); 3],
            env: 0.0,
            lenv: 0.0,
            d_dry: [[0.0; DLEN]; 2],
            d_flat: [[0.0; DLEN]; 2],
            widx: 0,
            os_now: -1,
            role1: 0,
            role2: 0,
            curve: 2,
            bias: 0.0,
            bias_out: 0.0,
            thr_lin: 1.0,
            drive_gain: 1.0,
            flat: 0.0,
            flat_att: 0.0,
            flat_rel: 0.0,
            lim_rel: 0.0,
            dc_r: 1.0,
            mix: 1.0,
            out_lin: 1.0,
            limit: true,
            latency: 0,
        };
        d.recompute();
        d
    }

    fn set_filter(&mut self, slot: usize, ty: f64, freq: f64, res: f64) {
        let sr = self.sr;
        let g = libm::tan((PI * freq.clamp(10.0, sr * 0.49)) / sr);
        let k = 2.0 - 1.96 * res.clamp(0.0, 1.0);
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;
        let f = &mut self.fc[slot];
        f.ty = ty as i32;
        f.k = k;
        f.a1 = a1;
        f.a2 = a2;
        f.a3 = a3;
    }

    fn recompute(&mut self) {
        self.dirty = false;
        let p = self.p;
        let sr = self.sr;
        for s in 0..3 {
            self.set_filter(s, p.f[s][0], p.f[s][1], p.f[s][2]);
        }
        for s in 0..3 {
            self.fc[s].vol = db(p.f[s][3]);
        }
        let r = ROUTES[(p.route as i32).clamp(0, 8) as usize];
        self.role1 = r[0];
        self.role2 = r[1];
        self.curve = (p.curve as i32).clamp(0, 5);
        self.bias = p.bias.clamp(-1.0, 1.0);
        self.bias_out = shape(self.curve, self.bias);
        self.thr_lin = db(p.thresh.min(0.0));
        self.drive_gain = db(p.drive) / self.thr_lin;
        self.flat = p.flat.clamp(0.0, 1.0);
        self.flat_att = 1.0 - libm::exp(-1.0 / (0.001 * sr));
        self.flat_rel = 1.0 - libm::exp(-1.0 / (0.08 * sr));
        self.lim_rel = libm::exp(-1.0 / (0.05 * sr));
        self.dc_r = 1.0 - (2.0 * PI * 5.0) / sr;
        self.mix = p.mix.clamp(0.0, 1.0);
        self.out_lin = db(p.gain);
        self.limit = p.limit >= 0.5;
        let os = (p.os as i32).clamp(0, 2);
        if os != self.os_now {
            self.os_now = os;
            for c in self.clip.iter_mut() {
                c.reset();
            }
            self.d_dry = [[0.0; DLEN]; 2];
            self.d_flat = [[0.0; DLEN]; 2];
            self.widx = 0;
        }
        self.latency = match os {
            0 => 0,
            1 => 15,
            _ => 23,
        };
    }

    /// One filter slot on one channel; Off returns the input (series bypass).
    #[inline]
    fn filt(&mut self, slot: usize, ch: usize, x: f64) -> f64 {
        let f = self.fc[slot];
        if f.ty == 0 {
            return x;
        }
        let s = &mut self.svf[slot][ch];
        let v3 = x - s.ic2;
        let v1 = f.a1 * s.ic1 + f.a2 * v3;
        let v2 = s.ic2 + f.a2 * s.ic1 + f.a3 * v3;
        s.ic1 = 2.0 * v1 - s.ic1;
        s.ic2 = 2.0 * v2 - s.ic2;
        let y = match f.ty {
            1 => v2,
            2 => f.k * v1,
            3 => x - f.k * v1 - v2,
            _ => x - f.k * v1,
        };
        y * f.vol
    }

    /// Oversampled clipper for one channel's base-rate sample.
    #[inline]
    fn clip_one(&mut self, ch: usize, x: f64) -> f64 {
        let v = x * self.drive_gain;
        let (cv, b, bo) = (self.curve, self.bias, self.bias_out);
        let (h_a, h_b) = (&self.h_a, &self.h_b);
        let c = &mut self.clip[ch];
        let mut y = match self.os_now {
            0 => shape(cv, v + b) - bo,
            1 => {
                c.up_a.push(v * 2.0);
                let u0 = c.up_a.out(h_a);
                c.up_a.push(0.0);
                let u1 = c.up_a.out(h_a);
                c.dn_a.push(shape(cv, u0 + b) - bo);
                c.dn_a.push(shape(cv, u1 + b) - bo);
                c.dn_a.out(h_a)
            }
            _ => {
                c.up_a.push(v * 2.0);
                let a0 = c.up_a.out(h_a);
                c.up_a.push(0.0);
                let a1 = c.up_a.out(h_a);
                c.up_b.push(a0 * 2.0);
                let q0 = c.up_b.out(h_b);
                c.up_b.push(0.0);
                let q1 = c.up_b.out(h_b);
                c.up_b.push(a1 * 2.0);
                let q2 = c.up_b.out(h_b);
                c.up_b.push(0.0);
                let q3 = c.up_b.out(h_b);
                c.dn_b.push(shape(cv, q0 + b) - bo);
                c.dn_b.push(shape(cv, q1 + b) - bo);
                let d0 = c.dn_b.out(h_b);
                c.dn_b.push(shape(cv, q2 + b) - bo);
                c.dn_b.push(shape(cv, q3 + b) - bo);
                let d1 = c.dn_b.out(h_b);
                c.dn_a.push(d0);
                c.dn_a.push(d1);
                c.dn_a.out(h_a)
            }
        };
        y *= self.thr_lin;
        // DC blocker (bias and Rectify leave a moving offset)
        let o = y - c.dcx + self.dc_r * c.dcy;
        c.dcx = y;
        c.dcy = o;
        o
    }

    fn reset_state(&mut self) {
        for c in self.clip.iter_mut() {
            c.reset();
        }
        self.svf = [[Svf::default(); 2]; 3];
        self.env = 0.0;
        self.lenv = 0.0;
        self.d_dry = [[0.0; DLEN]; 2];
        self.d_flat = [[0.0; DLEN]; 2];
        self.widx = 0;
    }
}

impl Default for DestroyerFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for DestroyerFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sr = sample_rate.max(1.0) as f64;
        self.reset_state();
        self.os_now = -1;
        self.dirty = true;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        let v = value as f64;
        if !v.is_finite() {
            return;
        }
        let p = &mut self.p;
        let slot = match key {
            "drive" => &mut p.drive,
            "flat" => &mut p.flat,
            "curve" => &mut p.curve,
            "bias" => &mut p.bias,
            "thresh" => &mut p.thresh,
            "route" => &mut p.route,
            "f1type" => &mut p.f[0][0],
            "f1freq" => &mut p.f[0][1],
            "f1res" => &mut p.f[0][2],
            "f1vol" => &mut p.f[0][3],
            "f2type" => &mut p.f[1][0],
            "f2freq" => &mut p.f[1][1],
            "f2res" => &mut p.f[1][2],
            "f2vol" => &mut p.f[1][3],
            "f3type" => &mut p.f[2][0],
            "f3freq" => &mut p.f[2][1],
            "f3res" => &mut p.f[2][2],
            "f3vol" => &mut p.f[2][3],
            "os" => &mut p.os,
            "limit" => &mut p.limit,
            "mix" => &mut p.mix,
            "gain" => &mut p.gain,
            _ => return,
        };
        *slot = v;
        self.dirty = true;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if self.dirty {
            self.recompute();
        }
        let (r1, r2, lat) = (self.role1, self.role2, self.latency);
        let (wet, dry) = (self.mix, 1.0 - self.mix);
        let n = l.len().min(r.len());
        for i in 0..n {
            let xl = l[i] as f64;
            let xr = r[i] as f64;

            // ---- flattener (stereo-linked) ----
            let a = xl.abs().max(xr.abs());
            self.env += (a - self.env) * if a > self.env { self.flat_att } else { self.flat_rel };
            let mut fg = 1.0;
            if self.flat > 0.0 {
                fg = libm::pow(FLAT_REF / self.env.max(1e-5), self.flat);
                fg = fg.clamp(FLAT_MIN, FLAT_MAX);
            }

            // ---- latency-alignment delay for the dry and Side paths ----
            let w = self.widx;
            let ridx = if w >= lat { w - lat } else { w + DLEN - lat };
            self.widx = if w + 1 == DLEN { 0 } else { w + 1 };

            let mut out = [0.0f64; 2];
            for (ch, o_ch) in out.iter_mut().enumerate() {
                let x = if ch == 0 { xl } else { xr };
                let fx = x * fg;
                self.d_dry[ch][w] = x;
                self.d_flat[ch][w] = fx;
                let d_dry = self.d_dry[ch][ridx];
                let d_flat = self.d_flat[ch][ridx];

                let mut s = fx;
                if r1 == 0 {
                    s = self.filt(0, ch, s);
                }
                if r2 == 0 {
                    s = self.filt(1, ch, s);
                }
                let mut y = self.clip_one(ch, s);
                if r1 == 1 {
                    y = self.filt(0, ch, y);
                }
                if r2 == 1 {
                    y = self.filt(1, ch, y);
                }
                if r1 == 2 && self.fc[0].ty != 0 {
                    y += self.filt(0, ch, d_flat);
                }
                if r2 == 2 && self.fc[1].ty != 0 {
                    y += self.filt(1, ch, d_flat);
                }
                y = self.filt(2, ch, y);
                *o_ch = (y * wet + d_dry * dry) * self.out_lin;
            }
            let (mut ol, mut orr) = (out[0], out[1]);

            // ---- limiter (stereo-linked, instant attack) ----
            if self.limit {
                let m = ol.abs().max(orr.abs());
                self.lenv = if m > self.lenv { m } else { m + (self.lenv - m) * self.lim_rel };
                if self.lenv > LIM_CEIL {
                    let g = LIM_CEIL / self.lenv;
                    ol *= g;
                    orr *= g;
                }
            }
            l[i] = ol as f32;
            r[i] = orr as f32;
        }
    }

    fn reset(&mut self) {
        self.reset_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(d: &mut DestroyerFx, l: &[f32], r: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let (mut l, mut r) = (l.to_vec(), r.to_vec());
        for (cl, cr) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            d.process(cl, cr);
        }
        (l, r)
    }

    fn sine(amp: f32, hz: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * libm::sinf(core::f32::consts::TAU * hz * i as f32 / SR)).collect()
    }

    fn fx() -> DestroyerFx {
        let mut d = DestroyerFx::new();
        d.prepare(SR);
        d
    }

    #[test]
    fn halfband_is_unity_at_dc_and_symmetric() {
        let h = halfband::<TAPS_A>();
        assert!((h.iter().sum::<f64>() - 1.0).abs() < 1e-12);
        for i in 0..TAPS_A {
            assert!((h[i] - h[TAPS_A - 1 - i]).abs() < 1e-15);
        }
        // half-band: every other tap away from the centre is ~0
        assert!(h[15 + 2].abs() < 1e-12 && h[15 - 4].abs() < 1e-12);
    }

    #[test]
    fn limiter_holds_the_ceiling_and_off_lets_it_through() {
        let x = sine(0.9, 110.0, 24_000);
        let mut d = fx();
        d.set_param("gain", 12.0);
        let (l, _) = run(&mut d, &x, &x);
        assert!(l.iter().all(|v| v.abs() <= 0.970_001), "limited");
        let mut d = fx();
        d.set_param("gain", 12.0);
        d.set_param("limit", 0.0);
        let (l, _) = run(&mut d, &x, &x);
        assert!(l.iter().any(|v| v.abs() > 1.5), "limiter off lets the boost through");
    }

    #[test]
    fn mix_zero_is_the_aligned_dry_signal() {
        for (os, lat) in [(0.0, 0usize), (1.0, 15), (2.0, 23)] {
            let mut d = fx();
            d.set_param("mix", 0.0);
            d.set_param("limit", 0.0);
            d.set_param("os", os);
            let x = sine(0.5, 440.0, 4_000);
            let (l, _) = run(&mut d, &x, &x);
            for i in lat..4_000 {
                assert_eq!(l[i], x[i - lat], "os {os}: dry delayed by exactly {lat}");
            }
        }
    }

    /// Magnitude of the `hz` component of `v` (sin/cos projection).
    fn tone_mag(v: &[f32], hz: f32) -> f32 {
        let (mut s, mut c) = (0.0f32, 0.0f32);
        for (i, x) in v.iter().enumerate() {
            let ph = core::f32::consts::TAU * hz * i as f32 / SR;
            s += x * libm::sinf(ph);
            c += x * libm::cosf(ph);
        }
        libm::sqrtf(s * s + c * c) * 2.0 / v.len() as f32
    }

    #[test]
    fn side_low_pass_adds_a_clean_parallel_low_end() {
        // "All Side", F1 = LP 150 Hz: the side path adds the CLEAN low band
        // in parallel with the clipper. With flattener and limiter off, the
        // difference (with − without F1) is just that filtered clean signal:
        // ≈ the full 55 Hz tone, almost none of the 3 kHz tone.
        let x: Vec<f32> = sine(0.3, 55.0, 48_000).iter().zip(sine(0.3, 3000.0, 48_000)).map(|(a, b)| a + b).collect();
        let common = [("route", 8.0), ("flat", 0.0), ("limit", 0.0), ("drive", 24.0)];
        let mut with = fx();
        for (k, v) in common.iter().chain([("f1type", 1.0), ("f1freq", 150.0)].iter()) {
            with.set_param(k, *v);
        }
        let mut without = fx();
        for (k, v) in common.iter() {
            without.set_param(k, *v);
        }
        let (a, _) = run(&mut with, &x, &x);
        let (b, _) = run(&mut without, &x, &x);
        let diff: Vec<f32> = a.iter().zip(&b).map(|(p, q)| p - q).collect();
        let tail = &diff[24_000..];
        let low = tone_mag(tail, 55.0);
        let high = tone_mag(tail, 3000.0);
        assert!((low - 0.3).abs() < 0.03, "clean 55 Hz passes the side LP at full level, got {low}");
        assert!(high < 0.01, "3 kHz is filtered out of the side path, got {high}");
    }

    #[test]
    fn every_curve_and_route_is_finite_and_bounded() {
        let x = sine(0.7, 220.0, 9_600);
        for curve in 0..6 {
            for route in 0..9 {
                let mut d = fx();
                for (k, v) in [
                    ("curve", curve as f32),
                    ("route", route as f32),
                    ("bias", 0.5),
                    ("f1type", 2.0),
                    ("f2type", 4.0),
                    ("f3type", 1.0),
                    ("os", 2.0),
                ] {
                    d.set_param(k, v);
                }
                let (l, r) = run(&mut d, &x, &x);
                assert!(l.iter().chain(r.iter()).all(|v| v.is_finite() && v.abs() <= 0.970_001));
            }
        }
    }

    #[test]
    fn bias_adds_even_harmonics_without_dc() {
        let x = sine(0.5, 1000.0, 48_000);
        let mut d = fx();
        d.set_param("bias", 0.6);
        d.set_param("curve", 0.0);
        let (l, _) = run(&mut d, &x, &x);
        let tail = &l[24_000..];
        let dc: f32 = tail.iter().sum::<f32>() / tail.len() as f32;
        assert!(dc.abs() < 1e-3, "DC blocked, got {dc}");
        // 2 kHz (2nd harmonic) present: correlate against a 2 kHz sine
        let h2: f32 = tail
            .iter()
            .enumerate()
            .map(|(i, v)| v * libm::sinf(core::f32::consts::TAU * 2000.0 * (i + 24_000) as f32 / SR + 0.0))
            .sum::<f32>()
            .abs()
            + tail
                .iter()
                .enumerate()
                .map(|(i, v)| v * libm::cosf(core::f32::consts::TAU * 2000.0 * (i + 24_000) as f32 / SR))
                .sum::<f32>()
                .abs();
        assert!(h2 > 50.0, "even harmonic from bias, got {h2}");
    }
}
