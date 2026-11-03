//! `cloud` effect — line-for-line port of the `sf-cloud` AudioWorklet
//! (public/sf-cloud.js), the canonical DSP for this device. Any change there
//! must land here too (CLAUDE.md: render-path parity).
//!
//! Granular delay: a 4 s stereo buffer records input + tanh-clipped feedback
//! (unless Freeze). Grains spawn at `density`/s (interval jittered ±50 %),
//! each reading `delay` + spray behind the write head, pitched
//! 2^((pitch ± scatter)/12), optionally reversed, Hann-windowed over `size`,
//! constant-power panned within `spread`. 32 preallocated grains; the sum is
//! normalised by 1/√(density × size). Seeded xorshift32.
//!
//! Schema (schema.ts "cloud"): size 10..500 ms (80), density 1..100 /s (20),
//! delay 0..2000 ms (250), spray 0..1 (0.3), pitch ±24 st (0), scatter
//! 0..12 st (0), reverse 0..1 (0), spread 0..1 (0.6), feedback 0..0.95
//! (0.3), freeze Off/On, mix 0..1 (0.5), gain ±24 dB.

use super::stutter::xorshift;
use crate::traits::EffectDsp;
use core::f64::consts::PI;

const MAX_GRAINS: usize = 32;
const SEED: u32 = 0x6d2b_79f5;

#[derive(Clone, Copy)]
struct Params {
    size: f64,
    density: f64,
    delay: f64,
    spray: f64,
    pitch: f64,
    scatter: f64,
    reverse: f64,
    spread: f64,
    feedback: f64,
    freeze: f64,
    mix: f64,
    gain: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            size: 80.0,
            density: 20.0,
            delay: 250.0,
            spray: 0.3,
            pitch: 0.0,
            scatter: 0.0,
            reverse: 0.0,
            spread: 0.6,
            feedback: 0.3,
            freeze: 0.0,
            mix: 0.5,
            gain: 0.0,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Grain {
    act: bool,
    pos: f64,
    inc: f64,
    age: f64,
    dur: f64,
    gl: f64,
    gr: f64,
}

/// Per-block spawn settings (already clamped).
struct Spawn {
    size_s: f64,
    delay_s: f64,
    spray: f64,
    pitch: f64,
    scatter: f64,
    reverse: f64,
    spread: f64,
}

pub struct CloudFx {
    p: Params,
    sr: f64,
    buf_l: Vec<f64>,
    buf_r: Vec<f64>,
    w: usize,
    grains: [Grain; MAX_GRAINS],
    until_next: f64,
    rng: u32,
    fb_l: f64,
    fb_r: f64,
}

impl CloudFx {
    pub fn new() -> Self {
        let mut c = Self {
            p: Params::default(),
            sr: 44_100.0,
            buf_l: Vec::new(),
            buf_r: Vec::new(),
            w: 0,
            grains: [Grain::default(); MAX_GRAINS],
            until_next: 0.0,
            rng: SEED,
            fb_l: 0.0,
            fb_r: 0.0,
        };
        c.prepare(44_100.0);
        c
    }

    fn rand(&mut self) -> f64 {
        xorshift(&mut self.rng)
    }

    fn read(buf: &[f64], mut pos: f64) -> f64 {
        let len = buf.len();
        let lf = len as f64;
        while pos < 0.0 {
            pos += lf;
        }
        while pos >= lf {
            pos -= lf;
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

    fn spawn(&mut self, s: &Spawn) {
        let slot = self.grains.iter().position(|g| !g.act);
        let r_spray = self.rand();
        let r_pitch = self.rand();
        let r_rev = self.rand();
        let r_pan = self.rand();
        let Some(g) = slot else { return };
        let sr = self.sr;
        let dur = (s.size_s * sr).max(16.0);
        let inc = libm::pow(2.0, (s.pitch + s.scatter * (2.0 * r_pitch - 1.0)) / 12.0);
        let rev = r_rev < s.reverse;
        let mut back = s.delay_s * sr + s.spray * r_spray * 4.0 * s.size_s * sr;
        if !rev && inc > 1.0 {
            back += dur * (inc - 1.0);
        }
        if rev {
            back += dur * inc;
        }
        back = back.max(1.0).min(self.buf_l.len() as f64 - 2.0);
        let pan = (s.spread * (2.0 * r_pan - 1.0)).clamp(-1.0, 1.0);
        let a = (pan + 1.0) * 0.25 * PI;
        self.grains[g] = Grain {
            act: true,
            pos: self.w as f64 - back + if rev { dur * inc } else { 0.0 },
            inc: if rev { -inc } else { inc },
            age: 0.0,
            dur,
            gl: libm::cos(a),
            gr: libm::sin(a),
        };
    }
}

impl Default for CloudFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for CloudFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sr = sample_rate.max(1.0) as f64;
        let len = libm::ceil(4.0 * self.sr) as usize;
        self.buf_l = vec![0.0; len];
        self.buf_r = vec![0.0; len];
        self.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        let v = value as f64;
        if !v.is_finite() {
            return;
        }
        let p = &mut self.p;
        let slot = match key {
            "size" => &mut p.size,
            "density" => &mut p.density,
            "delay" => &mut p.delay,
            "spray" => &mut p.spray,
            "pitch" => &mut p.pitch,
            "scatter" => &mut p.scatter,
            "reverse" => &mut p.reverse,
            "spread" => &mut p.spread,
            "feedback" => &mut p.feedback,
            "freeze" => &mut p.freeze,
            "mix" => &mut p.mix,
            "gain" => &mut p.gain,
            _ => return,
        };
        *slot = v;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let p = self.p;
        let sr = self.sr;
        let len = self.buf_l.len();
        let s = Spawn {
            size_s: (p.size / 1000.0).clamp(0.01, 0.5),
            delay_s: (p.delay / 1000.0).clamp(0.0, 2.0),
            spray: p.spray.clamp(0.0, 1.0),
            pitch: p.pitch,
            scatter: p.scatter.clamp(0.0, 12.0),
            reverse: p.reverse.clamp(0.0, 1.0),
            spread: p.spread.clamp(0.0, 1.0),
        };
        let density = p.density.clamp(1.0, 100.0);
        let fb = p.feedback.clamp(0.0, 0.95);
        let freeze = p.freeze >= 0.5;
        let mix = p.mix.clamp(0.0, 1.0);
        let out_lin = libm::pow(10.0, p.gain / 20.0);
        let norm = 1.0 / libm::sqrt((density * s.size_s).max(1.0));
        let t = 2.0 * PI;

        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let xl = *ls as f64;
            let xr = *rs as f64;
            if !freeze {
                self.buf_l[self.w] = xl + libm::tanh(self.fb_l * fb);
                self.buf_r[self.w] = xr + libm::tanh(self.fb_r * fb);
            }

            // ---- scheduler ----
            self.until_next -= 1.0;
            if self.until_next <= 0.0 {
                self.spawn(&s);
                self.until_next += (sr / density) * (0.5 + self.rand());
            }

            // ---- grains ----
            let mut wl = 0.0;
            let mut wr = 0.0;
            for g in self.grains.iter_mut() {
                if !g.act {
                    continue;
                }
                let ph = g.age / g.dur;
                let env = 0.5 - 0.5 * libm::cos(t * ph);
                let v = (Self::read(&self.buf_l, g.pos) + Self::read(&self.buf_r, g.pos)) * 0.5 * env;
                wl += v * g.gl;
                wr += v * g.gr;
                g.pos += g.inc;
                g.age += 1.0;
                if g.age >= g.dur {
                    g.act = false;
                }
            }
            wl *= norm;
            wr *= norm;
            self.fb_l = wl;
            self.fb_r = wr;
            if !freeze {
                self.w = if self.w + 1 == len { 0 } else { self.w + 1 };
            }

            *ls = ((wl * mix + xl * (1.0 - mix)) * out_lin) as f32;
            *rs = ((wr * mix + xr * (1.0 - mix)) * out_lin) as f32;
        }
    }

    fn reset(&mut self) {
        self.buf_l.fill(0.0);
        self.buf_r.fill(0.0);
        self.w = 0;
        self.grains = [Grain::default(); MAX_GRAINS];
        self.until_next = 0.0;
        self.rng = SEED;
        self.fb_l = 0.0;
        self.fb_r = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(fx: &mut CloudFx, x: &[f32]) -> (Vec<f32>, Vec<f32>) {
        let mut l = x.to_vec();
        let mut r = x.to_vec();
        for (cl, cr) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            fx.process(cl, cr);
        }
        (l, r)
    }

    #[test]
    fn wet_appears_after_the_delay_and_is_bounded() {
        let mut fx = CloudFx::new();
        fx.prepare(SR);
        fx.set_param("mix", 1.0);
        fx.set_param("spray", 0.0);
        fx.set_param("delay", 500.0);
        fx.set_param("feedback", 0.0);
        // a burst in the first 0.1 s, then silence
        let x: Vec<f32> = (0..72_000).map(|i| if i < 4800 { libm::sinf(i as f32 * 0.05) * 0.5 } else { 0.0 }).collect();
        let (l, r) = run(&mut fx, &x);
        let early = l[..12_000].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let later = l[24_000..36_000].iter().chain(&r[24_000..36_000]).fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(early < 1e-6, "no grain may read before the delay, got {early}");
        assert!(later > 0.05, "the burst must come back after 0.5 s, got {later}");
        assert!(l.iter().chain(&r).all(|s| s.is_finite() && s.abs() < 2.0));
    }

    #[test]
    fn freeze_holds_the_buffer() {
        let mut fx = CloudFx::new();
        fx.prepare(SR);
        fx.set_param("mix", 1.0);
        let x: Vec<f32> = (0..48_000).map(|i| libm::sinf(i as f32 * 0.03) * 0.5).collect();
        run(&mut fx, &x);
        fx.set_param("freeze", 1.0);
        let silence = vec![0.0f32; 48_000];
        let (l, _) = run(&mut fx, &silence);
        let peak = l[24_000..].iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak > 0.05, "frozen cloud keeps sounding, got {peak}");
    }
}
