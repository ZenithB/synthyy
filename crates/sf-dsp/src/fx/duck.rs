//! `duck` effect — port of the sidechain ducker built by `makeEffect("duck")`
//! in src/audio/devices.ts.
//!
//! Behavior spec: a single gain node pumped by an envelope. Two modes:
//!  · tempo (default): the gain dips at every division boundary and recovers
//!    over the cycle, phase derived from the transport tick position.
//!  · trigger (engine pushes `srcOn` = 1): a source track/pad note calls
//!    `trigger()`; the dip starts there and recovers over one Rate cycle
//!    (cycle length in seconds from the current bpm).
//! Recovery shape: `rec = phase^(0.35 + curve·1.6)`, gain
//! `g = (1 − amount) + amount·rec`. devices.ts writes the gain via
//! `setTargetAtTime(g, now, 0.004)` — a 4 ms one-pole — which we mirror with
//! a 4 ms smoother on the gain itself (the envelope's inherent slew; the
//! continuous params get the standard ~15 ms smoothing on top).
//! There is no wet/dry mix param — the device IS the gain (schema: rate /
//! amount / curve only; `fxNeutral` treats amount ≈ 0 as passthrough, which
//! falls out naturally here since g → 1).
//!
//! PARITY NOTE (fixture fx-duck, ΔRMS ≈ −4.3 dB): the golden WAV contains NO
//! ducking at all — it is NOT a depth/curve reference. In the browser's
//! offline export the duck is accidentally inert: Tone's `Offline()`
//! (node_modules/tone/build/esm/core/context/Offline.js) restores the global
//! context (`setContext(originalContext)`) BEFORE awaiting the render, so the
//! `transport.scheduleRepeat` tick callbacks in src/audio/render.ts fire with
//! `Tone.now()` reading the ONLINE context's clock. The duck tick's
//! `node.gain.setTargetAtTime(g, Tone.now(), 0.004)` (devices.ts, case 'duck')
//! therefore schedules every gain event far beyond the offline timeline and
//! the gain never leaves 1. Evidence: golden fx-duck.wav has full-level note
//! onsets and no recovery ramp, and golden RMS (−12.88 dBFS) equals our
//! rendered RMS (−17.14) minus the exact mean duck attenuation for amount=1,
//! curve=0.3 (10·log10(2·0.83+1) = 4.25 dB). This port follows the browser's
//! LIVE tick path (engine-tone.ts frame tick), which does duck. Do NOT
//! calibrate depth against this golden; fix render.ts to pass the callback's
//! `time` into `setTargetAtTime`, regold, then re-measure.

use crate::traits::EffectDsp;
use crate::util::Smoother;

/// schema.ts DUCK_DIV_TICKS — cycle length in transport ticks per rate index
/// (PPQ = 96 ⇒ bar = 384): ['1/2','1/4','1/4T','1/8','1/8T','1/16'].
const DIV_TICKS: [f64; 6] = [192.0, 96.0, 64.0, 48.0, 32.0, 24.0];

/// devices.ts: `setTargetAtTime(g, now, 0.004)` — 4 ms gain time constant.
const GAIN_TC_MS: f32 = 4.0;

pub struct DuckFx {
    sample_rate: f32,
    bpm: f64,
    playing: bool,
    /// Transport position in ticks; resynced by `tick()` at block boundaries,
    /// advanced per sample in between so the pump is sample-continuous.
    pos_ticks: f64,
    /// Rate division index into DIV_TICKS (schema `rate`, stepped).
    rate_idx: usize,
    /// Engine-pushed mode flag (fx map srcTrack): trigger mode when set.
    src_on: bool,
    /// Trigger mode: has a source note fired yet? (JS `trigAt < 0` = no.)
    trig_active: bool,
    /// Samples elapsed since the last `trigger()` (wall-clock in JS, so it
    /// advances regardless of transport state).
    trig_elapsed: f64,
    amount: Smoother,
    curve: Smoother,
    /// The pumped gain itself, 4 ms (see module docs).
    gain: Smoother,
}

impl DuckFx {
    pub fn new() -> Self {
        // Schema defaults: rate=1 (1/4), amount=0.7, curve=0.5.
        let mut gain = Smoother::new(1.0);
        gain.set_time_ms(GAIN_TC_MS);
        Self {
            sample_rate: 44_100.0,
            bpm: 120.0,
            playing: false,
            pos_ticks: 0.0,
            rate_idx: 1,
            src_on: false,
            trig_active: false,
            trig_elapsed: 0.0,
            amount: Smoother::new(0.7),
            curve: Smoother::new(0.5),
            gain,
        }
    }

    /// devices.ts: `rec = phase^(0.35 + curve·1.6); g = (1−amount) + amount·rec`.
    #[inline]
    fn gain_of(phase: f32, amount: f32, curve: f32) -> f32 {
        let rec = libm::powf(phase.clamp(0.0, 1.0), 0.35 + curve * 1.6);
        ((1.0 - amount) + amount * rec).max(0.0)
    }
}

impl Default for DuckFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for DuckFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.amount.prepare(self.sample_rate);
        self.amount.snap(self.amount.target());
        self.curve.prepare(self.sample_rate);
        self.curve.snap(self.curve.target());
        self.gain.prepare(self.sample_rate);
        self.gain.set_time_ms(GAIN_TC_MS);
        self.gain.snap(1.0);
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "rate" => {
                self.rate_idx = (value as i32).clamp(0, DIV_TICKS.len() as i32 - 1) as usize;
            }
            "amount" => self.amount.set_target(value.clamp(0.0, 1.0)),
            "curve" => self.curve.set_target(value.clamp(0.1, 1.0)),
            // Engine-pushed mode flag, not a schema knob (stepped, no smoother).
            "srcOn" => self.src_on = value > 0.5,
            _ => {}
        }
    }

    fn tick(&mut self, pos_ticks: f64, playing: bool, bpm: f64) {
        self.pos_ticks = pos_ticks;
        self.playing = playing;
        if bpm > 0.0 {
            self.bpm = bpm;
        }
    }

    fn trigger(&mut self) {
        // JS keeps the newest trigger time; here "now" always restarts the dip.
        self.trig_active = true;
        self.trig_elapsed = 0.0;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let ticks_per_sample = self.bpm.max(1.0) * 96.0 / (60.0 * self.sample_rate as f64);
        let cyc = DIV_TICKS[self.rate_idx];
        // ticks → samples for the trigger-mode recovery window.
        let cyc_samples = cyc / ticks_per_sample;
        for (ls, rs) in l.iter_mut().zip(r.iter_mut()) {
            let amount = self.amount.next();
            let curve = self.curve.next();
            let target = if self.src_on {
                if self.trig_active {
                    let phase = (self.trig_elapsed / cyc_samples).min(1.0) as f32;
                    self.trig_elapsed += 1.0;
                    Self::gain_of(phase, amount, curve)
                } else {
                    1.0
                }
            } else {
                let phase = (self.pos_ticks.rem_euclid(cyc) / cyc) as f32;
                if self.playing {
                    self.pos_ticks += ticks_per_sample;
                }
                Self::gain_of(phase, amount, curve)
            };
            self.gain.set_target(target);
            let g = self.gain.next();
            *ls *= g;
            *rs *= g;
        }
    }

    fn reset(&mut self) {
        self.pos_ticks = 0.0;
        self.playing = false;
        self.trig_active = false;
        self.trig_elapsed = 0.0;
        self.amount.snap(self.amount.target());
        self.curve.snap(self.curve.target());
        self.gain.snap(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;
    const BLOCK: usize = 128;
    /// 120 bpm ⇒ 192 ticks per second.
    const TICKS_PER_SEC: f64 = 192.0;

    /// Process in engine-sized blocks with a transport tick before each block,
    /// mirroring the callback (pos advances at 120 bpm from `start_ticks`).
    fn run_ticked(fx: &mut DuckFx, l: &mut [f32], r: &mut [f32], start_ticks: f64) {
        let n = l.len();
        let mut i = 0;
        while i < n {
            let end = (i + BLOCK).min(n);
            fx.tick(start_ticks + i as f64 / SR as f64 * TICKS_PER_SEC, true, 120.0);
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

    fn rms(x: &[f32]) -> f32 {
        libm::sqrtf(x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32)
    }

    #[test]
    fn two_seconds_no_nan_inf_and_non_silent() {
        let mut fx = DuckFx::new(); // defaults: rate 1 (1/4), amount 0.7, curve 0.5
        fx.prepare(SR);
        let n = (2.0 * SR) as usize;
        let src = noise(n);
        let mut l = src.clone();
        let mut r = src.clone();
        run_ticked(&mut fx, &mut l, &mut r, 0.0);

        assert!(l.iter().chain(r.iter()).all(|v| v.is_finite()));
        assert!(rms(&l) > 1e-3, "wet output is silent");
        // amount 0.7 pumping must actually change the signal.
        let diff: f32 = l.iter().zip(&src).map(|(a, b)| (a - b).abs()).sum();
        assert!(diff > 1.0, "output identical to dry input (diff {diff})");
        // The gain never exceeds unity: |out| ≤ |in| everywhere.
        assert!(l.iter().zip(&src).all(|(a, b)| a.abs() <= b.abs() + 1e-6));
    }

    #[test]
    fn param_sweep_full_ranges_stays_bounded() {
        let mut fx = DuckFx::new();
        fx.prepare(SR);
        let src = noise(BLOCK);
        let mut bpm = 120.0;
        for rate in 0..DIV_TICKS.len() {
            for step in 0..=4 {
                let t = step as f32 / 4.0;
                for src_on in [0.0, 1.0] {
                    fx.set_param("rate", rate as f32);
                    fx.set_param("amount", t);
                    fx.set_param("curve", 0.1 + t * 0.9);
                    fx.set_param("srcOn", src_on);
                    bpm = if bpm > 200.0 { 1.0 } else { bpm + 61.0 };
                    if step % 2 == 0 {
                        fx.trigger();
                    }
                    for blk in 0..40 {
                        fx.tick((blk * BLOCK) as f64 / SR as f64 * TICKS_PER_SEC, true, bpm);
                        let mut l = src.clone();
                        let mut r = src.clone();
                        fx.process(&mut l, &mut r);
                        assert!(
                            l.iter().chain(r.iter()).all(|v| v.is_finite() && v.abs() <= 0.5 + 1e-6),
                            "blew up at rate={rate} t={t} srcOn={src_on} bpm={bpm}"
                        );
                    }
                }
            }
        }
    }

    /// Analytic: tempo mode, rate 1 = 1/4 note = 96 ticks = 0.5 s at 120 bpm.
    /// The gain must be near unity just before the division boundary and dip
    /// hard just after it.
    #[test]
    fn tick_driven_gain_dips_at_division_boundaries() {
        let mut fx = DuckFx::new();
        fx.set_param("rate", 1.0);
        fx.set_param("amount", 0.9);
        fx.set_param("curve", 0.5);
        fx.prepare(SR);

        let n = SR as usize; // 1 s = two 0.5 s cycles
        let mut l = vec![1.0f32; n];
        let mut r = vec![1.0f32; n];
        run_ticked(&mut fx, &mut l, &mut r, 0.0);
        assert!(l.iter().all(|v| v.is_finite()));

        let at = |sec: f32| l[(sec * SR) as usize];
        // Shortly after the start-of-cycle dip (30 ms in: past the 4 ms slew).
        assert!(at(0.03) < 0.4, "no dip after t=0: {}", at(0.03));
        // Just before the boundary at 0.5 s the envelope has recovered.
        assert!(at(0.498) > 0.9, "not recovered before boundary: {}", at(0.498));
        // And dips again right after the boundary.
        assert!(at(0.53) < 0.4, "no dip after boundary: {}", at(0.53));
        // amount = 0.9 floor: gain never goes below 1 − amount.
        assert!(l.iter().all(|v| *v >= 0.1 - 1e-3));
    }

    /// Analytic: trigger mode (srcOn) idles at unity gain until `trigger()`,
    /// then dips and recovers over one Rate cycle.
    #[test]
    fn trigger_causes_dip_then_recovers() {
        let mut fx = DuckFx::new();
        fx.set_param("rate", 1.0); // 1/4 = 0.5 s cycle at 120 bpm
        fx.set_param("amount", 0.9);
        fx.set_param("srcOn", 1.0);
        fx.prepare(SR);

        // No trigger yet: passthrough at unity.
        let idle = (0.2 * SR) as usize;
        let mut l = vec![1.0f32; idle];
        let mut r = vec![1.0f32; idle];
        run_ticked(&mut fx, &mut l, &mut r, 0.0);
        assert!(l[idle - 1] > 0.95, "idle gain not unity: {}", l[idle - 1]);

        // Note fires: dip starts now, recovers over the 0.5 s cycle.
        fx.trigger();
        let n = (0.5 * SR) as usize;
        let mut l = vec![1.0f32; n];
        let mut r = vec![1.0f32; n];
        run_ticked(&mut fx, &mut l, &mut r, 0.2 * TICKS_PER_SEC);
        assert!(l.iter().all(|v| v.is_finite()));
        let at = |sec: f32| l[(sec * SR) as usize];
        assert!(at(0.03) < 0.4, "trigger did not dip: {}", at(0.03));
        assert!(at(0.497) > 0.9, "did not recover by cycle end: {}", at(0.497));
    }
}
