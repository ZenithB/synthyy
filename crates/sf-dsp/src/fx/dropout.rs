//! `dropout` effect — line-for-line port of the `sf-dropout` AudioWorklet
//! (public/sf-dropout.js), the canonical DSP for this device. Any change
//! there must land here too (CLAUDE.md: render-path parity).
//!
//! Codec / packet-loss emulation: 512-sample sine-windowed MDCT frames (hop
//! M = 256, perfect reconstruction when nothing is degraded). Per frame:
//! bandwidth cut, per-band quantiser (16 log bands, step = band RMS ×
//! (24/kbps)^1.5, optional dither "warble", band energy restored), and
//! Gilbert–Elliott packet loss (4-frame packets) concealed by silence,
//! repeat-with-decay or random-sign noise. Latency 2M = 512 samples; the dry
//! path is delayed to match.
//!
//! Schema (schema.ts "dropout"): bitrate 8..320 kbps (48), warble 0..1 (0.3),
//! bw 1..20 kHz (16000), loss 0..1 (0), burst 0..1 (0.3), conceal
//! Silence/Repeat/Noise (Repeat), mix 0..1 (1), gain ±24 dB.

use super::stutter::xorshift;
use crate::traits::EffectDsp;
use core::f64::consts::PI;

const M: usize = 256;
const N: usize = 2 * M;
const BANDS: usize = 16;
const PACKET: u64 = 4;
const SEED: u32 = 0x2545_f491;

#[derive(Clone, Copy)]
struct Params {
    bitrate: f64,
    warble: f64,
    bw: f64,
    loss: f64,
    burst: f64,
    conceal: f64,
    mix: f64,
    gain: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self { bitrate: 48.0, warble: 0.3, bw: 16000.0, loss: 0.0, burst: 0.3, conceal: 1.0, mix: 1.0, gain: 0.0 }
    }
}

pub struct DropoutFx {
    p: Params,
    sr: f64,
    win: Vec<f64>,
    t: Vec<f64>,
    edges: [usize; BANDS + 1],
    prev_in: [Vec<f64>; 2],
    new_in: [Vec<f64>; 2],
    out_q: [Vec<f64>; 2],
    tail: [Vec<f64>; 2],
    last: [Vec<f64>; 2],
    x: Vec<f64>,
    wb: Vec<f64>,
    y: Vec<f64>,
    k: usize,
    frame: u64,
    lost: bool,
    lost_run: i32,
    rng: u32,
    dry: [Vec<f64>; 2],
    dw: usize,
}

impl DropoutFx {
    pub fn new() -> Self {
        let mut win = vec![0.0; N];
        for (n, w) in win.iter_mut().enumerate() {
            *w = libm::sin((PI * (n as f64 + 0.5)) / N as f64);
        }
        let mut t = vec![0.0; M * N];
        for k in 0..M {
            for n in 0..N {
                t[k * N + n] = libm::cos((PI / M as f64) * (n as f64 + 0.5 + (M / 2) as f64) * (k as f64 + 0.5));
            }
        }
        let mut edges = [0usize; BANDS + 1];
        for (b, e) in edges.iter_mut().enumerate() {
            *e = libm::floor((M as f64 * (libm::pow(2.0, (8 * b) as f64 / BANDS as f64) - 1.0)) / 255.0) as usize;
        }
        let z = || vec![0.0; M];
        let mut d = Self {
            p: Params::default(),
            sr: 44_100.0,
            win,
            t,
            edges,
            prev_in: [z(), z()],
            new_in: [z(), z()],
            out_q: [z(), z()],
            tail: [z(), z()],
            last: [z(), z()],
            x: z(),
            wb: vec![0.0; N],
            y: vec![0.0; N],
            k: 0,
            frame: 0,
            lost: false,
            lost_run: 0,
            rng: SEED,
            dry: [vec![0.0; N], vec![0.0; N]],
            dw: 0,
        };
        d.prepare(44_100.0);
        d
    }

    fn rand(&mut self) -> f64 {
        xorshift(&mut self.rng)
    }

    /// One hop: MDCT both channels, degrade, IMDCT, overlap-add into out_q.
    fn run_frame(&mut self) {
        let p = self.p;
        let bitrate = p.bitrate.clamp(8.0, 320.0);
        let qf = libm::pow(24.0 / bitrate, 1.5);
        let warble = p.warble.clamp(0.0, 1.0);
        let cut = libm::floor((p.bw.clamp(1000.0, 20000.0) * N as f64) / self.sr - 0.5) + 1.0;
        let cut_bin = cut.clamp(0.0, M as f64) as usize;
        let loss = p.loss.clamp(0.0, 1.0);
        let burst = p.burst.clamp(0.0, 1.0);
        let conceal = (p.conceal as i64).clamp(0, 2);

        if self.frame.is_multiple_of(PACKET) {
            let pl = if self.lost { loss + burst * (1.0 - loss) } else { loss };
            self.lost = self.rand() < pl;
        }
        self.frame += 1;
        if self.lost {
            self.lost_run += 1;
        } else {
            self.lost_run = 0;
        }
        let decay = libm::pow(0.85, self.lost_run as f64);

        for ch in 0..2 {
            for n in 0..M {
                self.wb[n] = self.win[n] * self.prev_in[ch][n];
                self.wb[M + n] = self.win[M + n] * self.new_in[ch][n];
            }
            for k in 0..M {
                let row = &self.t[k * N..k * N + N];
                let mut acc = 0.0;
                for (w, t) in self.wb.iter().zip(row) {
                    acc += w * t;
                }
                self.x[k] = acc;
            }
            if !self.lost {
                for b in 0..BANDS {
                    let lo = self.edges[b];
                    let hi = self.edges[b + 1];
                    if hi <= lo {
                        continue;
                    }
                    let top = hi.min(cut_bin);
                    let mut e = 0.0;
                    for k in lo..top {
                        e += self.x[k] * self.x[k];
                    }
                    for k in top.max(lo)..hi {
                        self.x[k] = 0.0;
                    }
                    if top <= lo {
                        continue;
                    }
                    let step = libm::sqrt(e / (top - lo) as f64) * qf;
                    let mut eq = 0.0;
                    for k in lo..top {
                        if step <= 0.0 {
                            self.x[k] = 0.0;
                            continue;
                        }
                        let mut u = self.x[k] / step;
                        if warble > 0.0 {
                            u += (self.rand() - 0.5) * warble;
                        }
                        self.x[k] = libm::floor(u + 0.5) * step;
                        eq += self.x[k] * self.x[k];
                    }
                    if eq > 0.0 {
                        let g = libm::sqrt(e / eq);
                        for k in lo..top {
                            self.x[k] *= g;
                        }
                    }
                }
                for k in self.edges[BANDS]..M {
                    self.x[k] = 0.0;
                }
                self.last[ch].copy_from_slice(&self.x);
            } else if conceal == 0 {
                self.x.fill(0.0);
            } else if conceal == 1 {
                for k in 0..M {
                    self.x[k] = self.last[ch][k] * decay;
                }
            } else {
                for k in 0..M {
                    let m = self.last[ch][k].abs();
                    self.x[k] = (if self.rand() < 0.5 { -m } else { m }) * decay;
                }
            }
            // ---- IMDCT + overlap-add ----
            for n in 0..N {
                let mut acc = 0.0;
                for k in 0..M {
                    acc += self.x[k] * self.t[k * N + n];
                }
                self.y[n] = self.win[n] * (2.0 / M as f64) * acc;
            }
            for n in 0..M {
                self.out_q[ch][n] = self.tail[ch][n] + self.y[n];
                self.tail[ch][n] = self.y[M + n];
            }
            let (prev, cur) = (&mut self.prev_in, &self.new_in);
            prev[ch].copy_from_slice(&cur[ch]);
        }
    }
}

impl Default for DropoutFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for DropoutFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sr = sample_rate.max(1.0) as f64;
        self.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        let v = value as f64;
        if !v.is_finite() {
            return;
        }
        let p = &mut self.p;
        let slot = match key {
            "bitrate" => &mut p.bitrate,
            "warble" => &mut p.warble,
            "bw" => &mut p.bw,
            "loss" => &mut p.loss,
            "burst" => &mut p.burst,
            "conceal" => &mut p.conceal,
            "mix" => &mut p.mix,
            "gain" => &mut p.gain,
            _ => return,
        };
        *slot = v;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let mix = self.p.mix.clamp(0.0, 1.0);
        let out_lin = libm::pow(10.0, self.p.gain / 20.0);
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let k = self.k;
            let xl = *ls as f64;
            let xr = *rs as f64;
            let dl = self.dry[0][self.dw];
            let dr = self.dry[1][self.dw];
            self.dry[0][self.dw] = xl;
            self.dry[1][self.dw] = xr;
            self.dw = if self.dw + 1 == N { 0 } else { self.dw + 1 };
            let wl = self.out_q[0][k];
            let wr = self.out_q[1][k];
            self.new_in[0][k] = xl;
            self.new_in[1][k] = xr;
            if k + 1 == M {
                self.run_frame();
                self.k = 0;
            } else {
                self.k = k + 1;
            }
            *ls = ((wl * mix + dl * (1.0 - mix)) * out_lin) as f32;
            *rs = ((wr * mix + dr * (1.0 - mix)) * out_lin) as f32;
        }
    }

    fn reset(&mut self) {
        for ch in 0..2 {
            self.prev_in[ch].fill(0.0);
            self.new_in[ch].fill(0.0);
            self.out_q[ch].fill(0.0);
            self.tail[ch].fill(0.0);
            self.last[ch].fill(0.0);
            self.dry[ch].fill(0.0);
        }
        self.k = 0;
        self.frame = 0;
        self.lost = false;
        self.lost_run = 0;
        self.rng = SEED;
        self.dw = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn run(fx: &mut DropoutFx, x: &[f32]) -> Vec<f32> {
        let mut l = x.to_vec();
        let mut r = x.to_vec();
        for (cl, cr) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            fx.process(cl, cr);
        }
        l
    }

    fn sig(n: usize) -> Vec<f32> {
        (0..n).map(|i| 0.4 * libm::sinf(i as f32 * 0.0144) + 0.2 * libm::sinf(i as f32 * 0.173)).collect()
    }

    /// Error of `y` against `x` delayed by the codec latency, in dB.
    fn err_db(x: &[f32], y: &[f32]) -> f64 {
        let (mut e, mut s) = (0.0f64, 0.0f64);
        for i in 4096..x.len() {
            let d = (y[i] - x[i - N]) as f64;
            e += d * d;
            s += (x[i - N] as f64).powi(2);
        }
        10.0 * (e / s).log10()
    }

    #[test]
    fn high_bitrate_is_transparent_after_latency() {
        let mut fx = DropoutFx::new();
        fx.prepare(SR);
        fx.set_param("bitrate", 320.0);
        fx.set_param("warble", 0.0);
        fx.set_param("bw", 20000.0);
        let x = sig(24_000);
        let y = run(&mut fx, &x);
        assert!(err_db(&x, &y) < -40.0, "err {}", err_db(&x, &y));
    }

    #[test]
    fn total_loss_with_silence_concealment_is_silent() {
        let mut fx = DropoutFx::new();
        fx.prepare(SR);
        fx.set_param("loss", 1.0);
        fx.set_param("conceal", 0.0);
        let x = sig(24_000);
        let y = run(&mut fx, &x);
        assert!(y[2048..].iter().all(|s| s.abs() < 1e-9));
    }

    #[test]
    fn low_bitrate_degrades_but_stays_bounded() {
        let mut fx = DropoutFx::new();
        fx.prepare(SR);
        fx.set_param("bitrate", 8.0);
        fx.set_param("warble", 1.0);
        let x = sig(24_000);
        let y = run(&mut fx, &x);
        assert!(err_db(&x, &y) > -10.0);
        assert!(y.iter().all(|s| s.is_finite() && s.abs() < 4.0));
    }
}
