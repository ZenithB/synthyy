//! `autotune` effect — port of the `sf-autotune` AudioWorklet in
//! public/sf-fx.js (CANONICAL worklet DSP; see docs/RUST_ARCH.md).
//!
//! Topology (worklet `SFAutotune`): a dual-tap "tape splice" granular pitch
//! shifter (`ShiftCore`, 60 ms window) driven by an in-band YIN detector.
//! The detector runs on a 2×-decimated mono mix every DETECT_HOP input
//! samples over a DETECT_N decimated-sample window; the detected MIDI pitch
//! is snapped to the project key/scale (or the nearest semitone in chromatic
//! mode) and the correction in semitones glides toward the target with a
//! `speed`-ms time constant. Wet/dry is Tone.CrossFade's equal-power law.
//!
//! Schema params (src/audio/schema.ts "autotune"): amount 0..1 (def 1),
//! speed 1..200 ms (def 20), mix 0..1 (def 1), mode 0=Key/1=Chr (def 0).
//! The worklet additionally receives `root` (pitch class, def 9 = A) and
//! `mask` (root-relative scale pitch-class bitmask, def 0x5AD = minor) as
//! port messages driven by the project key — accepted here as extra
//! `set_param` keys with the same defaults.
//!
//! `amount` and `speed` act on the detector/glide path, which is already
//! smoothed by the `speed` glide itself (a correction change can never
//! zipper); `mode`/`root`/`mask` are inherently stepped. Only `mix` is an
//! audible continuous gain, smoothed ~15 ms per contract.

use crate::traits::EffectDsp;
use crate::util::{DelayLine, Smoother, MAX_BLOCK};

/// Worklet constants: analysis window / hop, in samples.
const DETECT_N: usize = 1024; // decimated samples (≈46 ms @ 44.1k)
const DETECT_HOP: usize = 512; // input samples between detections (~12 ms)
const RING_LEN: usize = 4096; // 2×-decimated mono detection ring
const RING_MASK: usize = RING_LEN - 1;

/// YIN pitch detection (cumulative-mean-normalised difference), verbatim from
/// `yinDetect` in sf-fx.js: early exit once the first sub-threshold dip is
/// walked to its local minimum. `sr` is the (decimated) sample rate.
fn yin_detect(buf: &[f32; DETECT_N], sr: f32) -> f32 {
    let n = DETECT_N;
    let max_tau = (n - 256).min(libm::floorf(sr / 70.0) as usize); // down to ~70 Hz
    let min_tau = 2usize.max(libm::floorf(sr / 1100.0) as usize); // up to ~1100 Hz
    if max_tau <= min_tau + 2 {
        return 0.0;
    }
    let lim = n - max_tau;
    let mut running = 0.0f64;
    let mut best = 1.0f64;
    let mut bt: i64 = -1;
    let mut prev = 1.0f64;
    let mut found: i64 = -1;
    for tau in min_tau..max_tau {
        let mut sum = 0.0f64;
        for (&a, &b) in buf[..lim].iter().zip(&buf[tau..tau + lim]) {
            let d = (a - b) as f64;
            sum += d * d;
        }
        running += sum;
        let c = if running > 0.0 {
            sum * (tau - min_tau + 1) as f64 / running
        } else {
            1.0
        };
        if found >= 0 {
            if c < prev {
                // still descending
                found = tau as i64;
                prev = c;
                continue;
            }
            break; // local minimum found
        }
        if c < 0.15 {
            found = tau as i64;
            prev = c;
            continue;
        }
        if c < best {
            best = c;
            bt = tau as i64;
        }
        prev = c;
    }
    let mut tau = found;
    if tau < 0 {
        if best > 0.4 {
            return 0.0;
        }
        tau = bt;
    }
    if tau > 0 {
        sr / tau as f32
    } else {
        0.0
    }
}

/// Dual-tap granular ("tape splice") shifter — `ShiftCore` in sf-fx.js: two
/// taps half a window apart whose delay ramps 0→window at rate (1−ratio);
/// each tap sin-fades to zero exactly when its delay wraps.
struct ShiftCore {
    win: f32, // window, seconds (autotune fixes it at 0.06)
    ratio: f32, // current (smoothed) playback ratio
    target: f32,
    ph: f64, // tap phase 0..1 (delay = ph·window)
    dl_l: DelayLine,
    dl_r: DelayLine,
}

impl ShiftCore {
    fn new(window_sec: f32, sample_rate: f32) -> Self {
        Self {
            win: window_sec.clamp(0.03, 0.25),
            ratio: 1.0,
            target: 1.0,
            ph: 0.0,
            // worklet: Math.ceil(sampleRate * 0.6) samples per channel
            dl_l: DelayLine::new(libm::ceilf(sample_rate * 0.6) as usize),
            dl_r: DelayLine::new(libm::ceilf(sample_rate * 0.6) as usize),
        }
    }

    fn set_pitch(&mut self, semi: f32) {
        self.target = libm::powf(2.0, semi / 12.0);
    }

    fn reset(&mut self) {
        self.ratio = 1.0;
        self.target = 1.0;
        self.ph = 0.0;
        self.dl_l.clear();
        self.dl_r.clear();
    }

    /// Process one stereo block, writing the WET signal.
    fn block(
        &mut self,
        in_l: &[f32],
        in_r: &[f32],
        wet_l: &mut [f32],
        wet_r: &mut [f32],
        sample_rate: f32,
    ) {
        let win_s = self.win * sample_rate;
        let mut ph = self.ph;
        for i in 0..in_l.len() {
            // glide the ratio (~2 ms time constant) so pitch changes don't zipper
            self.ratio += (self.target - self.ratio) * 0.01;
            let inc = (1.0 - self.ratio as f64) / win_s as f64;
            self.dl_l.write(in_l[i]);
            self.dl_r.write(in_r[i]);
            ph += inc;
            ph -= libm::floor(ph); // wrap to [0,1)
            let ph_b = (ph + 0.5) % 1.0;
            // worklet DelayLine reads d back from the JUST-written sample
            // (d = 0 → newest); util::DelayLine's newest sample is delay 1.
            let d_a = ph as f32 * win_s + 1.0;
            let d_b = ph_b as f32 * win_s + 1.0;
            let g_a = libm::sinf(core::f32::consts::PI * ph as f32); // 0 at tap A's wrap
            let g_b = libm::sinf(core::f32::consts::PI * ph_b as f32); // 0 at tap B's wrap
            wet_l[i] = self.dl_l.read_frac(d_a) * g_a + self.dl_l.read_frac(d_b) * g_b;
            wet_r[i] = self.dl_r.read_frac(d_a) * g_a + self.dl_r.read_frac(d_b) * g_b;
        }
        self.ph = ph;
    }
}

pub struct AutotuneFx {
    sample_rate: f32,
    core: ShiftCore,
    amount: f32,
    speed_ms: f32,
    mix: Smoother,
    mode: i32, // 0 = snap to key/scale, 1 = chromatic
    root: i32, // pitch class of the key root (def 9 = A)
    mask: u32, // root-relative scale pitch-class bitmask (def 0x5AD = minor)
    cur: f32, // current correction (semitones)
    desired: f32,
    // 2×-decimated mono ring for detection
    ring: Vec<f32>,
    rw: usize,
    decim_pending: bool, // pending sample for pairwise averaging
    decim_hold: f32,
    since_hop: usize,
    ana: [f32; DETECT_N],
}

impl AutotuneFx {
    pub fn new() -> Self {
        Self {
            sample_rate: 44_100.0,
            core: ShiftCore::new(0.06, 44_100.0),
            amount: 1.0,
            speed_ms: 20.0,
            mix: Smoother::new(1.0),
            mode: 0,
            root: 9,
            mask: 0x5AD,
            cur: 0.0,
            desired: 0.0,
            ring: vec![0.0; RING_LEN],
            rw: 0,
            decim_pending: false,
            decim_hold: 0.0,
            since_hop: 0,
            ana: [0.0; DETECT_N],
        }
    }

    fn in_mask(&self, pitch: i32) -> bool {
        (self.mask >> (pitch - self.root).rem_euclid(12)) & 1 == 1
    }

    fn snap(&self, pitch: i32) -> i32 {
        if self.mode == 1 || self.in_mask(pitch) {
            return pitch;
        }
        for off in 1..=6 {
            if self.in_mask(pitch - off) {
                return pitch - off;
            }
            if self.in_mask(pitch + off) {
                return pitch + off;
            }
        }
        pitch
    }

    fn detect(&mut self) {
        // copy the latest DETECT_N decimated samples out of the ring, oldest first
        let start = (self.rw + RING_LEN - DETECT_N) & RING_MASK;
        for (i, a) in self.ana.iter_mut().enumerate() {
            *a = self.ring[(start + i) & RING_MASK];
        }
        let mut rms = 0.0f64;
        for &a in self.ana.iter() {
            rms += (a * a) as f64;
        }
        let rms = libm::sqrt(rms / DETECT_N as f64);
        if rms <= 0.004 {
            return; // silence: hold the correction
        }
        let f = yin_detect(&self.ana, self.sample_rate * 0.5);
        if f <= 0.0 {
            return;
        }
        let midi = 69.0 + 12.0 * libm::log2f(f / 440.0);
        let target = self.snap(libm::roundf(midi) as i32);
        self.desired = ((target as f32 - midi) * self.amount).clamp(-12.0, 12.0);
    }

    /// One ≤ MAX_BLOCK chunk — mirrors one worklet `process()` quantum.
    fn process_chunk(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        // feed the decimated detection ring (mono mix, average adjacent pairs)
        for (&xl, &xr) in l.iter().zip(r.iter()) {
            let x = (xl + xr) * 0.5;
            if self.decim_pending {
                self.ring[self.rw] = (self.decim_hold + x) * 0.5;
                self.rw = (self.rw + 1) & RING_MASK;
                self.decim_pending = false;
            } else {
                self.decim_hold = x;
                self.decim_pending = true;
            }
        }
        self.since_hop += n;
        if self.since_hop >= DETECT_HOP {
            self.since_hop = 0;
            self.detect();
        }
        // glide toward the desired correction (time constant = speed ms)
        let coeff = 1.0
            - libm::exp(
                -(n as f64 / self.sample_rate as f64) / (self.speed_ms as f64 / 1000.0).max(0.001),
            ) as f32;
        self.cur += (self.desired - self.cur) * coeff;
        self.core.set_pitch(self.cur);
        let mut wet_l = [0.0f32; MAX_BLOCK];
        let mut wet_r = [0.0f32; MAX_BLOCK];
        self.core
            .block(l, r, &mut wet_l[..n], &mut wet_r[..n], self.sample_rate);
        // equal-power dry/wet (Tone.CrossFade): dry·cos(m·π/2) + wet·sin(m·π/2)
        for i in 0..n {
            let m = self.mix.next().clamp(0.0, 1.0);
            let dry = libm::cosf(m * core::f32::consts::FRAC_PI_2);
            let wet = libm::sinf(m * core::f32::consts::FRAC_PI_2);
            l[i] = l[i] * dry + wet_l[i] * wet;
            r[i] = r[i] * dry + wet_r[i] * wet;
        }
    }
}

impl Default for AutotuneFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for AutotuneFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.core = ShiftCore::new(0.06, self.sample_rate);
        self.mix.prepare(self.sample_rate);
        self.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "amount" => self.amount = value,
            "speed" => self.speed_ms = value,
            "mix" => self.mix.set_target(value),
            "mode" => self.mode = value as i32,
            // project key, pushed like the worklet's root/mask port messages
            "root" => self.root = value as i32,
            "mask" => self.mask = value as u32,
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let mut i = 0;
        while i < n {
            let m = (n - i).min(MAX_BLOCK);
            let (lc, rc) = (&mut l[i..i + m], &mut r[i..i + m]);
            self.process_chunk(lc, rc);
            i += m;
        }
    }

    fn reset(&mut self) {
        self.core.reset();
        self.cur = 0.0;
        self.desired = 0.0;
        self.ring.fill(0.0);
        self.rw = 0;
        self.decim_pending = false;
        self.decim_hold = 0.0;
        self.since_hop = 0;
        self.mix.snap(self.mix.target());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_sine(fx: &mut AutotuneFx, hz: f32, sr: f32, secs: f32, out_l: &mut Vec<f32>) {
        let total = (sr * secs) as usize;
        let mut t = 0usize;
        while t < total {
            let n = MAX_BLOCK.min(total - t);
            let mut l = [0.0f32; MAX_BLOCK];
            let mut r = [0.0f32; MAX_BLOCK];
            for i in 0..n {
                let s = 0.5 * libm::sinf(2.0 * core::f32::consts::PI * hz * (t + i) as f32 / sr);
                l[i] = s;
                r[i] = s;
            }
            fx.process(&mut l[..n], &mut r[..n]);
            out_l.extend_from_slice(&l[..n]);
            t += n;
        }
    }

    /// Naive DFT magnitude at `hz` over `buf`.
    fn dft_mag(buf: &[f32], hz: f32, sr: f32) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        let w = 2.0 * core::f64::consts::PI * hz as f64 / sr as f64;
        for (i, &x) in buf.iter().enumerate() {
            let ph = w * i as f64;
            re += x as f64 * libm::cos(ph);
            im += x as f64 * libm::sin(ph);
        }
        libm::sqrt(re * re + im * im)
    }

    fn peak_hz(buf: &[f32], sr: f32, lo: f32, hi: f32, step: f32) -> f32 {
        let mut best_hz = lo;
        let mut best_mag = -1.0f64;
        let mut hz = lo;
        while hz <= hi {
            let m = dft_mag(buf, hz, sr);
            if m > best_mag {
                best_mag = m;
                best_hz = hz;
            }
            hz += step;
        }
        best_hz
    }

    #[test]
    fn two_seconds_no_nan_and_wet_non_silent() {
        let sr = 44_100.0;
        let mut fx = AutotuneFx::new();
        fx.prepare(sr);
        fx.set_param("mix", 1.0); // pure wet
        let mut out = Vec::new();
        run_sine(&mut fx, 220.0, sr, 2.0, &mut out);
        assert!(out.iter().all(|s| s.is_finite()));
        let tail = &out[out.len() / 2..];
        let rms = libm::sqrt(tail.iter().map(|&s| (s * s) as f64).sum::<f64>() / tail.len() as f64);
        assert!(rms > 0.05, "wet output should be non-silent: rms {rms}");
    }

    #[test]
    fn param_sweep_stays_bounded() {
        let sr = 48_000.0;
        for &amount in &[0.0f32, 0.5, 1.0] {
            for &speed in &[1.0f32, 20.0, 200.0] {
                for &mix in &[0.0f32, 0.5, 1.0] {
                    for &mode in &[0.0f32, 1.0] {
                        for &(root, mask) in &[(0.0f32, 0.0f32), (9.0, 0x5AD as f32), (11.0, 0xFFF as f32)] {
                            let mut fx = AutotuneFx::new();
                            fx.prepare(sr);
                            fx.set_param("amount", amount);
                            fx.set_param("speed", speed);
                            fx.set_param("mix", mix);
                            fx.set_param("mode", mode);
                            fx.set_param("root", root);
                            fx.set_param("mask", mask);
                            let mut out = Vec::new();
                            run_sine(&mut fx, 445.0, sr, 0.25, &mut out);
                            assert!(
                                out.iter().all(|s| s.is_finite() && s.abs() < 4.0),
                                "unbounded at amount={amount} speed={speed} mix={mix} mode={mode}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn mix_zero_is_dry_passthrough() {
        let sr = 44_100.0;
        let mut fx = AutotuneFx::new();
        fx.prepare(sr);
        fx.set_param("mix", 0.0);
        fx.reset(); // snap the smoother so the whole run is dry
        let mut out = Vec::new();
        run_sine(&mut fx, 445.0, sr, 0.5, &mut out);
        // regenerate the input to compare (equal-power dry gain at mix 0 is 1)
        let mut expect = Vec::with_capacity(out.len());
        for i in 0..out.len() {
            expect.push(0.5 * libm::sinf(2.0 * core::f32::consts::PI * 445.0 * i as f32 / sr));
        }
        let max_err = out
            .iter()
            .zip(&expect)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_err < 1e-4, "mix 0 should be dry: max err {max_err}");
    }

    #[test]
    fn detuned_sine_pulled_toward_440() {
        // 445 Hz in, strength 1, chromatic snap → nearest semitone is A4;
        // the corrected output's spectral peak must sit closer to 440 than
        // the 445 Hz input does.
        let sr = 44_100.0;
        let mut fx = AutotuneFx::new();
        fx.prepare(sr);
        fx.set_param("amount", 1.0);
        fx.set_param("mode", 1.0); // chromatic
        fx.set_param("mix", 1.0);
        let mut out = Vec::new();
        run_sine(&mut fx, 445.0, sr, 2.0, &mut out);
        let tail = &out[out.len() - 22_050..]; // settled last 0.5 s
        let peak = peak_hz(tail, sr, 420.0, 460.0, 0.5);
        // The worklet's YIN uses integer tau (no parabolic interpolation):
        // at the 22.05 kHz decimated rate 445 Hz lands on tau 50 → detected
        // ≈441 Hz → correction −0.039 st → ~444 Hz out. The gate is the
        // spec's: the corrected peak sits closer to 440 than the input does.
        assert!(
            (peak - 440.0).abs() < (445.0f32 - 440.0).abs(),
            "peak {peak} Hz should be closer to 440 than the 445 Hz input"
        );
    }

    #[test]
    fn key_snap_uses_root_and_mask() {
        // F#4 = MIDI 66 is not in A minor (0x5AD from root 9); nearest scale
        // degrees are F (65) and G (67), one semitone either side — the
        // downward candidate wins (worklet checks -off before +off).
        let fx = AutotuneFx::new();
        assert_eq!(fx.snap(66), 65);
        assert_eq!(fx.snap(69), 69); // A is the root, always in mask
        // chromatic mode leaves the rounded pitch alone
        let mut chr = AutotuneFx::new();
        chr.set_param("mode", 1.0);
        assert_eq!(chr.snap(66), 66);
    }
}
