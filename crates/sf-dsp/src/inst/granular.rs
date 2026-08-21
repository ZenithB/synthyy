//! `granular` instrument — port of `makeGranular()` in src/audio/devices.ts.
//! Each held/triggered note runs a grain clock at `dens` grains/s; every tick
//! spawns one grain: a `size`-second slice from `pos` (± `spray`) of the
//! sample (slot 0), pitched by the note (−60 = middle C reference, + `pitch`
//! ± `pjit` jitter), reversed with probability `rev`, through a triangular
//! 0→0.8→0 gain window skewed by `shape`, a random stereo pan (± `spread`,
//! Web-Audio StereoPanner law), and the voice's amp envelope
//! (`attack`/`release`, velocity-scaled). Randomness uses an inline
//! xorshift64* PRNG with a fixed seed (deterministic per run — browser parity
//! for the random stream is not required). Output gain 0.9 mirrors
//! `new Tone.Gain(0.9)`. No sample → silence, never panic, no allocation in
//! the process/note paths (fixed voice + grain pools).

use std::sync::Arc;

use crate::sample::SampleBuf;
use crate::traits::InstrumentDsp;
use crate::util::MAX_BLOCK;

const MAX_VOICES: usize = 10; // browser device's MAX_VOICES
const MAX_GRAINS: usize = 256; // shared pool; spawns skip when exhausted
const OUT_GAIN: f32 = 0.9;
const WIN_PEAK: f32 = 0.8;
const LEVEL_FLOOR: f32 = 1e-4;
const RNG_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// xorshift64* — same generator family the engine scheduler uses.
struct XorShift(u64);

impl XorShift {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 { RNG_SEED } else { seed })
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    #[inline]
    fn next_f32(&mut self) -> f32 {
        ((self.next_u64() >> 40) as f32) / (1u64 << 24) as f32
    }

    /// Uniform in [-1, 1).
    #[inline]
    fn next_pm1(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
    }
}

/// Web-Audio StereoPannerNode stereo pan law (grains pan post-window).
#[inline]
fn stereo_pan(l: f32, r: f32, pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    if p == 0.0 {
        return (l, r);
    }
    let x = if p <= 0.0 { p + 1.0 } else { p };
    let (gl, gr) = (
        libm::cosf(x * core::f32::consts::FRAC_PI_2),
        libm::sinf(x * core::f32::consts::FRAC_PI_2),
    );
    if p <= 0.0 {
        (l + r * gl, r * gr)
    } else {
        (l * gl, r + l * gr)
    }
}

#[inline]
fn clampf(v: f32, lo: f32, hi: f32) -> f32 {
    v.clamp(lo, hi)
}

#[derive(Clone, Copy)]
struct Voice {
    active: bool,
    pitch: u8,
    vel: f32,
    held: bool,
    elapsed: f64, // engine samples since start
    atk: f64,     // samples
    /// Trigger path: grain clock + sustain end (∞ while held).
    dur: f64,
    rel: f64, // samples
    releasing: bool,
    rel_end: f64,    // elapsed when grain spawning stops (noteOff path)
    exp_coeff: f32,  // noteOff: setTargetAtTime τ = rel/3
    cur_level: f32,  // tracked level (exp release decays this)
    spawn_next: f64, // elapsed time of the next grain
    age: u64,
}

impl Voice {
    fn idle() -> Self {
        Self {
            active: false,
            pitch: 0,
            vel: 0.0,
            held: false,
            elapsed: 0.0,
            atk: 1.0,
            dur: 0.0,
            rel: 1.0,
            releasing: false,
            rel_end: 0.0,
            exp_coeff: 0.0,
            cur_level: 0.0,
            spawn_next: 0.0,
            age: 0,
        }
    }
}

#[derive(Clone, Copy)]
struct Grain {
    active: bool,
    voice: u8,
    pos: f64,  // source frame (signed step handles reverse)
    step: f64, // source frames per engine sample
    t: f64,    // engine samples since grain start (starts negative for mid-chunk spawns)
    len: f64,  // engine samples
    peak: f64, // engine samples to the window peak
    pan: f32,
}

impl Grain {
    fn idle() -> Self {
        Self { active: false, voice: 0, pos: 0.0, step: 0.0, t: 0.0, len: 1.0, peak: 0.5, pan: 0.0 }
    }
}

pub struct GranularInst {
    sample_rate: f32,
    buf: Option<Arc<SampleBuf>>,
    // frozen schema params
    size: f32,
    dens: f32,
    pos: f32,
    spray: f32,
    pitch: f32,
    pjit: f32,
    rev: f32,
    shape: f32,
    spread: f32,
    attack: f32,
    release: f32,
    voices: [Voice; MAX_VOICES],
    grains: [Grain; MAX_GRAINS],
    /// Per-voice level curves for the current sub-block (voice-major).
    levels: Vec<f32>,
    rng: XorShift,
    age: u64,
}

impl GranularInst {
    pub fn new() -> Self {
        Self {
            sample_rate: 44_100.0,
            buf: None,
            size: 0.12,
            dens: 18.0,
            pos: 0.15,
            spray: 0.05,
            pitch: 0.0,
            pjit: 0.0,
            rev: 0.0,
            shape: 0.5,
            spread: 0.6,
            attack: 0.05,
            release: 0.4,
            voices: [Voice::idle(); MAX_VOICES],
            grains: [Grain::idle(); MAX_GRAINS],
            levels: vec![0.0; MAX_VOICES * MAX_BLOCK],
            rng: XorShift::new(RNG_SEED),
            age: 0,
        }
    }

    fn alloc_voice(&mut self) -> Option<usize> {
        // browser device: at MAX_VOICES live voices new notes are dropped
        if let Some(i) = self.voices.iter().position(|v| !v.active) {
            return Some(i);
        }
        None
    }

    fn start_voice(&mut self, pitch: u8, vel: f32, dur_sec: f32) {
        if self.buf.is_none() {
            return;
        }
        let Some(vi) = self.alloc_voice() else { return };
        let sr = self.sample_rate as f64;
        let held = dur_sec < 0.0;
        let atk_s = if held {
            (self.attack).max(0.003)
        } else {
            (self.attack.max(0.003)).min(dur_sec * 0.5)
        };
        let rel_s = self.release.max(0.02);
        self.age += 1;
        let v = &mut self.voices[vi];
        v.active = true;
        v.pitch = pitch;
        v.vel = vel.clamp(0.0, 1.0);
        v.held = held;
        v.elapsed = 0.0;
        v.atk = (atk_s as f64 * sr).max(1.0);
        v.dur = if held { f64::INFINITY } else { dur_sec as f64 * sr };
        v.rel = (rel_s as f64 * sr).max(1.0);
        v.releasing = false;
        v.rel_end = f64::INFINITY;
        // setTargetAtTime τ = rel/3 (noteOff path)
        v.exp_coeff = libm::expf(-3.0 / (rel_s * self.sample_rate.max(1.0)));
        v.cur_level = 0.0;
        v.spawn_next = 0.0; // Tone.Clock ticks at its start time
        v.age = self.age;
    }

    /// Spawn one grain for voice `vi`, starting `delay` engine samples into
    /// the current sub-block (t starts negative).
    fn spawn_grain(&mut self, vi: usize, delay: f64) {
        let Some(buf) = self.buf.as_ref() else { return };
        let frames = buf.frames();
        if frames == 0 {
            return;
        }
        let Some(gi) = self.grains.iter().position(|g| !g.active) else {
            return; // pool exhausted: skip this grain, keep running
        };
        let src_sr = buf.sample_rate.max(1.0) as f64;
        let buf_dur = frames as f64 / src_sr;
        let eng_sr = self.sample_rate.max(1.0) as f64;

        let size = clampf(self.size, 0.02, 0.5) as f64;
        let p = self.voices[vi].pitch as f32;
        let rate =
            libm::powf(2.0, (p - 60.0 + self.pitch + self.rng.next_pm1() * self.pjit) / 12.0)
                as f64;
        let mut offset =
            self.pos as f64 * buf_dur + self.rng.next_pm1() as f64 * self.spray as f64 * buf_dur * 0.5;
        let rev = self.rng.next_f32() < self.rev;
        if rev {
            offset = buf_dur - offset - size * rate;
        }
        let start_sec = offset.clamp(0.0, (buf_dur - 0.02).max(0.0));
        let (pos0, step) = if rev {
            // reversed-buffer index j maps to original frame (N−1−j)
            ((frames as f64 - 1.0) - start_sec * src_sr, -(rate * src_sr / eng_sr))
        } else {
            (start_sec * src_sr, rate * src_sr / eng_sr)
        };
        let shape = clampf(self.shape, 0.03, 0.97) as f64;
        let len = (size * eng_sr).max(2.0);
        let g = &mut self.grains[gi];
        g.active = true;
        g.voice = vi as u8;
        g.pos = pos0;
        g.step = step;
        g.t = -delay;
        g.len = len;
        g.peak = (len * shape).clamp(1.0, len - 1.0);
        g.pan = self.rng.next_pm1() * self.spread;
    }

    /// Advance voice `vi` one sample, returning its envelope level (or None
    /// when the voice just died).
    #[inline]
    fn voice_level(v: &mut Voice) -> Option<f32> {
        let lvl = if v.releasing {
            v.cur_level *= v.exp_coeff;
            if v.cur_level < LEVEL_FLOOR {
                return None;
            }
            v.cur_level
        } else if v.elapsed < v.atk {
            v.cur_level = v.vel * (v.elapsed / v.atk) as f32;
            v.cur_level
        } else if v.elapsed <= v.dur {
            v.cur_level = v.vel;
            v.vel
        } else {
            // trigger path: linear release from vel to 0 over rel
            let t = (v.elapsed - v.dur) / v.rel;
            if t >= 1.0 {
                return None;
            }
            v.cur_level = v.vel * (1.0 - t) as f32;
            v.cur_level
        };
        v.elapsed += 1.0;
        Some(lvl)
    }
}

impl Default for GranularInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for GranularInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.reset();
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "size" => self.size = value,
            "dens" => self.dens = value,
            "pos" => self.pos = value.clamp(0.0, 1.0),
            "spray" => self.spray = value.clamp(0.0, 1.0),
            "pitch" => self.pitch = value,
            "pjit" => self.pjit = value.max(0.0),
            "rev" => self.rev = value.clamp(0.0, 1.0),
            "shape" => self.shape = value,
            "spread" => self.spread = value.clamp(0.0, 1.0),
            "attack" => self.attack = value.max(0.0),
            "release" => self.release = value.max(0.0),
            _ => {}
        }
    }

    fn note_on(&mut self, pitch: u8, vel: f32) {
        // browser: held.has(pp) → ignore retrigger of a held pitch
        if self.voices.iter().any(|v| v.active && v.held && !v.releasing && v.pitch == pitch) {
            return;
        }
        self.start_voice(pitch, vel, -1.0);
    }

    fn note_off(&mut self, pitch: u8) {
        for v in &mut self.voices {
            if v.active && v.held && !v.releasing && v.pitch == pitch {
                v.releasing = true;
                v.rel_end = v.elapsed + v.rel; // clock.stop(now + rel)
            }
        }
    }

    fn trigger(&mut self, pitch: u8, dur_sec: f32, vel: f32) {
        self.start_voice(pitch, vel, dur_sec.max(0.0));
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if self.buf.is_none() {
            return;
        }
        let total = l.len().min(r.len());
        let mut done = 0usize;
        while done < total {
            let m = (total - done).min(MAX_BLOCK);
            self.process_block(&mut l[done..done + m], &mut r[done..done + m]);
            done += m;
        }
    }

    fn set_sample(&mut self, slot: u32, buf: Option<Arc<SampleBuf>>) {
        if slot != 0 {
            return;
        }
        for v in &mut self.voices {
            v.active = false;
        }
        for g in &mut self.grains {
            g.active = false;
        }
        self.buf = buf;
    }

    fn reset(&mut self) {
        for v in &mut self.voices {
            v.active = false;
        }
        for g in &mut self.grains {
            g.active = false;
        }
        self.rng = XorShift::new(RNG_SEED);
    }
}

impl GranularInst {
    fn process_block(&mut self, l: &mut [f32], r: &mut [f32]) {
        let m = l.len();
        debug_assert!(m <= MAX_BLOCK);
        let eng_sr = self.sample_rate.max(1.0) as f64;
        let interval = eng_sr / clampf(self.dens, 2.0, 80.0) as f64;

        // 1) voice envelopes for the block + grain spawning
        for vi in 0..MAX_VOICES {
            if !self.voices[vi].active {
                continue;
            }
            // spawn every grain due within this block (clock at `dens` Hz);
            // spawning stops at dur (trigger) / rel_end (noteOff)
            loop {
                let v = &self.voices[vi];
                let spawn_stop = if v.held { v.rel_end } else { v.dur };
                let due = v.spawn_next;
                if due >= v.elapsed + m as f64 || due > spawn_stop {
                    break;
                }
                let delay = (due - v.elapsed).max(0.0);
                self.voices[vi].spawn_next = due + interval;
                self.spawn_grain(vi, delay);
            }
            let base = vi * MAX_BLOCK;
            let mut died_at: Option<usize> = None;
            for k in 0..m {
                match Self::voice_level(&mut self.voices[vi]) {
                    Some(lvl) => self.levels[base + k] = lvl,
                    None => {
                        died_at = Some(k);
                        break;
                    }
                }
            }
            if let Some(k0) = died_at {
                for k in k0..m {
                    self.levels[vi * MAX_BLOCK + k] = 0.0;
                }
                self.voices[vi].active = false;
                for g in &mut self.grains {
                    if g.active && g.voice as usize == vi {
                        g.active = false; // env is 0: cut the dead voice's grains
                    }
                }
            }
        }

        // 2) grain-major accumulation
        let Some(buf) = self.buf.as_ref() else { return };
        for g in &mut self.grains {
            if !g.active {
                continue;
            }
            let vi = g.voice as usize;
            let base = vi * MAX_BLOCK;
            for (k, (ol, or)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
                g.t += 1.0;
                let t = g.t;
                if t < 0.0 {
                    continue; // grain starts mid-block
                }
                if t >= g.len {
                    g.active = false;
                    break;
                }
                let win = if t < g.peak {
                    WIN_PEAK * (t / g.peak) as f32
                } else {
                    WIN_PEAK * (1.0 - (t - g.peak) / (g.len - g.peak)) as f32
                };
                let (sl, sr) = buf.read_lin(g.pos);
                g.pos += g.step;
                let (pl, pr) = stereo_pan(sl * win, sr * win, g.pan);
                let lvl = self.levels[base + k] * OUT_GAIN;
                *ol += pl * lvl;
                *or += pr * lvl;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine_buf(sr: f32, freq: f32, secs: f32) -> Arc<SampleBuf> {
        let n = (sr * secs) as usize;
        let l: Vec<f32> = (0..n)
            .map(|i| libm::sinf(core::f32::consts::TAU * freq * i as f32 / sr))
            .collect();
        Arc::new(SampleBuf { sample_rate: sr, l, r: vec![] })
    }

    fn render(d: &mut GranularInst, samples: usize) -> (Vec<f32>, Vec<f32>) {
        let mut l = vec![0.0f32; samples];
        let mut r = vec![0.0f32; samples];
        for (cl, cr) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            d.process(cl, cr);
        }
        (l, r)
    }

    fn rms(b: &[f32]) -> f32 {
        (b.iter().map(|s| s * s).sum::<f32>() / b.len().max(1) as f32).sqrt()
    }

    #[test]
    fn silent_without_sample_and_additive() {
        let mut d = GranularInst::new();
        d.prepare(48_000.0);
        d.note_on(60, 1.0);
        d.trigger(64, 0.25, 0.8);
        let mut l = [0.5f32; 8];
        let mut r = [0.5f32; 8];
        d.process(&mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.5));
        d.note_off(60);
        d.reset();
    }

    #[test]
    fn grain_cloud_sounds_and_trigger_ends() {
        let sr = 44_100.0;
        let mut d = GranularInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(sr, 330.0, 1.0)));
        d.set_param("pos", 0.4);
        d.set_param("dens", 40.0);
        d.set_param("attack", 0.01);
        d.set_param("release", 0.1);
        d.trigger(60, 0.5, 1.0);
        let (l, r) = render(&mut d, sr as usize * 3 / 2); // 1.5 s
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
        assert!(rms(&l[2_000..20_000]) > 1e-3, "grain cloud silent");
        // dur 0.5 s + rel 0.1 s (+ one grain tail ≤ 0.5 s): silent by 1.3 s
        let tail = &l[(sr * 1.3) as usize..];
        assert!(rms(tail) < 1e-4, "trigger did not end: {}", rms(tail));
    }

    #[test]
    fn note_off_releases() {
        let sr = 44_100.0;
        let mut d = GranularInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(sr, 220.0, 1.0)));
        d.set_param("release", 0.05);
        d.note_on(60, 1.0);
        let (head, _) = render(&mut d, 22_050);
        assert!(rms(&head[4_000..]) > 1e-3, "held voice silent");
        d.note_off(60);
        let (tail, _) = render(&mut d, 44_100);
        assert!(tail.iter().all(|s| s.is_finite()));
        assert!(rms(&tail[22_050..]) < 1e-4, "note_off did not fade out");
    }

    #[test]
    fn param_sweeps_do_not_blow_up() {
        let sr = 48_000.0;
        let mut d = GranularInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(sr, 500.0, 0.5)));
        for (k, v) in [
            ("size", 0.5),
            ("dens", 80.0),
            ("pos", 1.0),
            ("spray", 1.0),
            ("pitch", 24.0),
            ("pjit", 12.0),
            ("rev", 1.0),
            ("shape", 0.0),
            ("spread", 1.0),
        ] {
            d.set_param(k, v);
        }
        d.note_on(84, 1.0);
        d.trigger(36, 0.3, 1.0);
        let (l, r) = render(&mut d, 48_000);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
        assert!(l.iter().chain(r.iter()).all(|s| s.abs() < 32.0));
        d.note_off(84);
        let (l2, _) = render(&mut d, 24_000);
        assert!(l2.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn deterministic_per_reset() {
        let sr = 44_100.0;
        let run = |d: &mut GranularInst| {
            d.reset();
            d.set_param("spray", 0.5);
            d.set_param("pjit", 4.0);
            d.trigger(60, 0.2, 1.0);
            render(d, 16_384).0
        };
        let mut d = GranularInst::new();
        d.prepare(sr);
        d.set_sample(0, Some(sine_buf(sr, 330.0, 1.0)));
        let a = run(&mut d);
        let b = run(&mut d);
        assert_eq!(a, b);
    }
}
