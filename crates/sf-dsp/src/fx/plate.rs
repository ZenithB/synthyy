//! `plate` effect — plate reverb (port of `PlateVerb` in src/audio/devices.ts).
//!
//! Browser topology: pre-delay → convolver holding a synthesized plate IR →
//! damping lowpass → wet gain, with dry = 1 − mix in parallel. The IR is
//! decorrelated stereo noise with a ~4 ms onset ramp, an exponential decay
//! reaching −60 dB at `decay` seconds, and a first-difference high tilt
//! (d[i] − 0.3·d[i−1]) for the metallic sheen. A real plate has no discrete
//! early reflections — echo density is maximal almost immediately.
//!
//! Rust variant: an 8-line Householder FDN behind a 4-stage allpass diffuser
//! instead of a convolver (no IR regeneration, RT-safe). Per-line feedback
//! gains place −60 dB at `decay` seconds; the diffuser makes the onset dense
//! within a few ms (taps from its last two stages start the wet path before
//! the shortest FDN line speaks, mirroring the IR's 4 ms ramp). The metallic
//! tilt and damping lowpass sit on the wet path — both are linear, so this
//! matches baking them into the IR. Wet level is scaled by 1/√decay to mimic
//! `convolver.normalize = true` (loudness roughly independent of decay).

use crate::traits::EffectDsp;
use crate::util::{DelayLine, Smoother, Svf, SvfMode};

/// FDN order.
const N: usize = 8;
/// FDN line lengths (ms) — short and mutually non-harmonic for plate density.
const LINE_MS: [f32; N] = [7.9, 11.3, 13.7, 17.9, 21.3, 26.1, 31.7, 38.3];
/// Input diffuser allpass delays (ms) — instant echo density (~4 ms onset).
const DIFF_MS: [f32; 4] = [0.73, 1.31, 2.19, 3.41];
/// Diffuser allpass coefficient.
const DIFF_G: f32 = 0.65;
/// Metallic tilt: wet[n] − TILT·wet[n−1] (the IR's first-difference blend
/// d·0.7 + (d − d₋₁)·0.3 reduces to exactly this).
const TILT: f32 = 0.3;
/// Diffuser → wet early taps (pre-FDN onset).
const EARLY_GAIN: f32 = 0.3;
/// Per-line wet tap gain.
const TAP_GAIN: f32 = 0.5;
/// Diffused input → FDN injection gain.
const INJECT: f32 = 0.5;
/// Wet level at decay = 1 s (scaled by 1/√decay, see module docs).
const WET_NORM: f32 = 0.5;
/// Pre-delay allocation (s); schema max is 0.2 (browser allocates 0.5).
const PRE_MAX_SEC: f32 = 0.25;
/// `makeIr` floors decay at 0.15 s.
const MIN_DECAY: f32 = 0.15;

/// Schroeder allpass with a fixed integer delay.
struct Allpass {
    line: DelayLine,
    delay: usize,
}

impl Allpass {
    fn new(delay: usize) -> Self {
        Self {
            line: DelayLine::new(delay + 1),
            delay: delay.max(1),
        }
    }

    #[inline]
    fn tick(&mut self, x: f32) -> f32 {
        let d = self.line.read(self.delay);
        let v = x + DIFF_G * d;
        self.line.write(v);
        d - DIFF_G * v
    }
}

pub struct PlateFx {
    sample_rate: f32,
    pre_l: DelayLine,
    pre_r: DelayLine,
    diff: [Allpass; 4],
    lines: [DelayLine; N],
    line_len: [usize; N],
    gains: [f32; N],
    wet_norm: f32,
    tilt_prev_l: f32,
    tilt_prev_r: f32,
    damp_l: Svf,
    damp_r: Svf,
    // smoothed params (schema keys: decay, predelay, damp, mix)
    decay: Smoother,
    predelay: Smoother, // seconds
    damp: Smoother,     // Hz
    mix: Smoother,
}

impl PlateFx {
    pub fn new() -> Self {
        let sr = 44_100.0;
        let line_len = Self::line_lengths(sr);
        let mut fx = Self {
            sample_rate: sr,
            pre_l: DelayLine::new((PRE_MAX_SEC * sr) as usize + 2),
            pre_r: DelayLine::new((PRE_MAX_SEC * sr) as usize + 2),
            diff: core::array::from_fn(|i| Allpass::new(Self::ms_to_samples(DIFF_MS[i], sr))),
            lines: core::array::from_fn(|i| DelayLine::new(line_len[i] + 2)),
            line_len,
            gains: [0.0; N],
            wet_norm: WET_NORM,
            tilt_prev_l: 0.0,
            tilt_prev_r: 0.0,
            damp_l: Svf::new(SvfMode::Lowpass),
            damp_r: Svf::new(SvfMode::Lowpass),
            // schema defaults
            decay: Smoother::new(1.8),
            predelay: Smoother::new(0.02),
            damp: Smoother::new(6_000.0),
            mix: Smoother::new(0.3),
        };
        fx.update_decay_gains(fx.decay.current());
        fx
    }

    fn ms_to_samples(ms: f32, sr: f32) -> usize {
        ((ms * 1e-3 * sr) as usize).max(1)
    }

    fn line_lengths(sr: f32) -> [usize; N] {
        core::array::from_fn(|i| Self::ms_to_samples(LINE_MS[i], sr))
    }

    /// Per-line feedback so the tail hits −60 dB at `decay` seconds
    /// (IR: exp(−6.9·t/decay)), plus the normalize-mimicking wet scale.
    fn update_decay_gains(&mut self, decay_sec: f32) {
        let t = decay_sec.max(MIN_DECAY);
        for (g, len) in self.gains.iter_mut().zip(self.line_len) {
            *g = libm::powf(10.0, -3.0 * len as f32 / (self.sample_rate * t)).min(0.999_5);
        }
        self.wet_norm = WET_NORM / libm::sqrtf(t);
    }
}

impl Default for PlateFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for PlateFx {
    fn prepare(&mut self, sample_rate: f32) {
        let sr = sample_rate.max(1.0);
        self.sample_rate = sr;
        // prepare() may allocate; process() never does
        self.pre_l = DelayLine::new((PRE_MAX_SEC * sr) as usize + 2);
        self.pre_r = DelayLine::new((PRE_MAX_SEC * sr) as usize + 2);
        self.diff = core::array::from_fn(|i| Allpass::new(Self::ms_to_samples(DIFF_MS[i], sr)));
        self.line_len = Self::line_lengths(sr);
        self.lines = core::array::from_fn(|i| DelayLine::new(self.line_len[i] + 2));
        self.damp_l.prepare(sr);
        self.damp_r.prepare(sr);
        for s in [&mut self.decay, &mut self.predelay, &mut self.damp, &mut self.mix] {
            s.prepare(sr);
        }
        self.tilt_prev_l = 0.0;
        self.tilt_prev_r = 0.0;
        self.update_decay_gains(self.decay.current());
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            // browser regenerates the IR (debounced) — here just a gain retune
            "decay" => self.decay.set_target(value.clamp(MIN_DECAY, 20.0)),
            "predelay" => self.predelay.set_target(value.clamp(0.0, 0.2)),
            "damp" => self.damp.set_target(value.clamp(100.0, 18_000.0)),
            "mix" => self.mix.set_target(value.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        if n == 0 {
            return;
        }
        // Block-rate params (decay retune costs 8 powf; damp a tan) — stepped
        // at ≤ MAX_BLOCK ≈ 3 ms, below the 15 ms smoothing time constant.
        for _ in 0..n {
            self.decay.next();
            self.damp.next();
        }
        self.update_decay_gains(self.decay.current());
        let fc = self.damp.current().clamp(100.0, self.sample_rate * 0.45);
        let q = core::f32::consts::FRAC_1_SQRT_2;
        self.damp_l.set_cutoff_q(fc, q);
        self.damp_r.set_cutoff_q(fc, q);

        for (sl, sr_) in l.iter_mut().zip(r.iter_mut()).take(n) {
            let (xl, xr) = (*sl, *sr_);

            // pre-delay (browser: DelayNode before the convolver)
            let pd = (self.predelay.next() * self.sample_rate).max(1.0);
            self.pre_l.write(xl);
            self.pre_r.write(xr);
            let pl = self.pre_l.read_frac(pd);
            let pr = self.pre_r.read_frac(pd);

            // allpass diffusion — instantly dense, like the plate IR
            let a0 = self.diff[0].tick(0.5 * (pl + pr));
            let a1 = self.diff[1].tick(a0);
            let a2 = self.diff[2].tick(a1);
            let a3 = self.diff[3].tick(a2);

            // FDN read; per-line decay gain applied to the recirculating signal
            let mut d = [0.0f32; N];
            let mut sum_g = 0.0f32;
            for ((di, line), (len, g)) in d
                .iter_mut()
                .zip(&self.lines)
                .zip(self.line_len.iter().zip(self.gains))
            {
                *di = line.read(*len);
                sum_g += *di * g;
            }

            // Householder feedback (I − 2/N·ones) + alternating-sign injection
            let house = sum_g * (2.0 / N as f32);
            let mut sign = 1.0f32;
            for ((line, di), g) in self.lines.iter_mut().zip(d).zip(self.gains) {
                line.write(di * g - house + sign * INJECT * a3);
                sign = -sign;
            }

            // decorrelated wet taps + early diffuser taps (pre-FDN onset)
            let wet_l = TAP_GAIN * (d[0] - d[2] + d[4] - d[6]) + EARLY_GAIN * a2;
            let wet_r = TAP_GAIN * (d[1] - d[3] + d[5] - d[7]) + EARLY_GAIN * a3;

            // metallic tilt (1 − TILT·z⁻¹) then damping lowpass, as in the IR
            let tl = wet_l - TILT * self.tilt_prev_l;
            self.tilt_prev_l = wet_l;
            let tr = wet_r - TILT * self.tilt_prev_r;
            self.tilt_prev_r = wet_r;
            let wl = self.damp_l.process_sample(tl) * self.wet_norm;
            let wr = self.damp_r.process_sample(tr) * self.wet_norm;

            // wet = mix, dry = 1 − mix (PlateVerb's wet/dry Gain pair)
            let m = self.mix.next();
            *sl = (1.0 - m) * xl + m * wl;
            *sr_ = (1.0 - m) * xr + m * wr;
        }
    }

    fn reset(&mut self) {
        self.pre_l.clear();
        self.pre_r.clear();
        for ap in &mut self.diff {
            ap.line.clear();
        }
        for line in &mut self.lines {
            line.clear();
        }
        self.damp_l.reset();
        self.damp_r.reset();
        self.tilt_prev_l = 0.0;
        self.tilt_prev_r = 0.0;
        for s in [&mut self.decay, &mut self.predelay, &mut self.damp, &mut self.mix] {
            let t = s.target();
            s.snap(t);
        }
        self.update_decay_gains(self.decay.current());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 44_100.0;

    fn make(decay: f32, predelay: f32, damp: f32, mix: f32) -> PlateFx {
        let mut fx = PlateFx::new();
        fx.prepare(SR);
        fx.set_param("decay", decay);
        fx.set_param("predelay", predelay);
        fx.set_param("damp", damp);
        fx.set_param("mix", mix);
        fx.reset(); // snap smoothers so tests start at the target values
        fx
    }

    /// Impulse response: unit impulse into both channels, `secs` of output.
    fn render_ir(fx: &mut PlateFx, secs: f32) -> (Vec<f32>, Vec<f32>) {
        let n = (secs * SR) as usize;
        let (mut out_l, mut out_r) = (Vec::with_capacity(n), Vec::with_capacity(n));
        let mut first = true;
        let mut pos = 0;
        while pos < n {
            let len = 128.min(n - pos);
            let mut l = vec![0.0f32; len];
            let mut r = vec![0.0f32; len];
            if first {
                l[0] = 1.0;
                r[0] = 1.0;
                first = false;
            }
            fx.process(&mut l, &mut r);
            out_l.extend_from_slice(&l);
            out_r.extend_from_slice(&r);
            pos += len;
        }
        (out_l, out_r)
    }

    fn rms(x: &[f32], from_sec: f32, to_sec: f32) -> f64 {
        let a = (from_sec * SR) as usize;
        let b = ((to_sec * SR) as usize).min(x.len());
        let sum: f64 = x[a..b].iter().map(|s| (*s as f64) * (*s as f64)).sum();
        (sum / (b - a) as f64).sqrt()
    }

    fn db(x: f64) -> f64 {
        20.0 * x.max(1e-30).log10()
    }

    /// RMS spectral frequency via Parseval (derivative energy over energy) —
    /// a monotone brightness measure standing in for the spectral centroid.
    fn rms_freq(x: &[f32]) -> f64 {
        let mut e = 0.0f64;
        let mut de = 0.0f64;
        for w in x.windows(2) {
            let d = (w[1] - w[0]) as f64;
            de += d * d;
        }
        for s in x {
            e += (*s as f64) * (*s as f64);
        }
        SR as f64 / core::f64::consts::TAU * (de / e.max(1e-30)).sqrt()
    }

    fn xorshift(state: &mut u32) -> f32 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *state = x;
        (x as f32) * (2.0 / u32::MAX as f32) - 1.0
    }

    #[test]
    fn two_seconds_no_nan_and_wet_tail_rings() {
        // schema defaults, wet half in
        let mut fx = make(1.8, 0.02, 6_000.0, 0.5);
        let (l, r) = render_ir(&mut fx, 2.0);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
        // instantly dense onset: energy right after the 20 ms pre-delay …
        assert!(rms(&l, 0.025, 0.06) > 1e-6, "no early wet energy");
        // … and a tail long after the impulse (wet output is non-silent)
        assert!(rms(&l, 0.3, 0.8) > 1e-7, "left tail silent");
        assert!(rms(&r, 0.3, 0.8) > 1e-7, "right tail silent");
        // decorrelated stereo: L and R tails are not the same signal
        let a = (0.1 * SR) as usize;
        let b = (0.8 * SR) as usize;
        let diff: f64 = l[a..b]
            .iter()
            .zip(&r[a..b])
            .map(|(x, y)| ((x - y) as f64).powi(2))
            .sum();
        assert!(diff > 1e-8, "L/R tails identical");
    }

    #[test]
    fn param_sweep_stays_bounded() {
        // grid over the full schema ranges with noise input
        let mut rng = 0x1234_5678u32;
        for decay in [0.3, 1.8, 8.0] {
            for predelay in [0.0, 0.2] {
                for damp in [500.0, 6_000.0, 16_000.0] {
                    for mix in [0.0, 0.5, 1.0] {
                        let mut fx = make(decay, predelay, damp, mix);
                        for _ in 0..40 {
                            let mut l = [0.0f32; 128];
                            let mut r = [0.0f32; 128];
                            for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                                *a = 0.5 * xorshift(&mut rng);
                                *b = 0.5 * xorshift(&mut rng);
                            }
                            fx.process(&mut l, &mut r);
                            for s in l.iter().chain(r.iter()) {
                                assert!(s.is_finite() && s.abs() < 20.0, "blew up: {s}");
                            }
                        }
                    }
                }
            }
        }
        // continuous sweep on one instance (smoothers under moving targets)
        let mut fx = make(0.3, 0.0, 500.0, 0.0);
        for i in 0..400 {
            let t = i as f32 / 399.0;
            fx.set_param("decay", 0.3 + t * (8.0 - 0.3));
            fx.set_param("predelay", 0.2 * t);
            fx.set_param("damp", 500.0 + t * (16_000.0 - 500.0));
            fx.set_param("mix", t);
            let mut l = [0.0f32; 128];
            let mut r = [0.0f32; 128];
            for (a, b) in l.iter_mut().zip(r.iter_mut()) {
                *a = 0.5 * xorshift(&mut rng);
                *b = 0.5 * xorshift(&mut rng);
            }
            fx.process(&mut l, &mut r);
            for s in l.iter().chain(r.iter()) {
                assert!(s.is_finite() && s.abs() < 20.0, "sweep blew up: {s}");
            }
        }
    }

    #[test]
    fn tail_decay_tracks_decay_param() {
        // −60 dB at `decay` seconds ⇒ the short setting must fall much faster
        // over the same interval, and the long tail must outlive the short one.
        let drop_of = |decay: f32| {
            let mut fx = make(decay, 0.0, 16_000.0, 1.0);
            let (l, _) = render_ir(&mut fx, 2.0);
            let early = db(rms(&l, 0.05, 0.25));
            let late = db(rms(&l, 1.2, 1.5));
            (early - late, rms(&l, 1.2, 1.5))
        };
        let (drop_short, late_short) = drop_of(0.5);
        let (drop_long, late_long) = drop_of(4.0);
        // ideal: ~144 dB vs ~18 dB over the ~1.2 s between window centers
        assert!(
            drop_short > drop_long + 30.0,
            "short {drop_short:.1} dB vs long {drop_long:.1} dB"
        );
        assert!(drop_long > 5.0, "long tail should still decay: {drop_long:.1} dB");
        assert!(
            late_long > 10.0 * late_short,
            "decay=4 tail should outlive decay=0.5 ({late_long:e} vs {late_short:e})"
        );
    }

    #[test]
    fn spectral_centroid_above_untilted_reverb() {
        // The metallic tilt (1 − 0.3·z⁻¹) is what separates the plate from a
        // generic decaying-noise reverb (Tone.Reverb's default IR model). The
        // reference is exponentially decaying white noise through the SAME
        // damping lowpass but without the tilt — the plate must sit brighter.
        let mut fx = make(1.8, 0.0, 6_000.0, 1.0);
        let (l, _) = render_ir(&mut fx, 1.0);
        let plate_f = rms_freq(&l[..l.len()]);

        let n = SR as usize;
        let mut lp = Svf::new(SvfMode::Lowpass);
        lp.prepare(SR);
        lp.set_cutoff_q(6_000.0, core::f32::consts::FRAC_1_SQRT_2);
        let mut rng = 0xdead_beefu32;
        let mut reference = Vec::with_capacity(n);
        for i in 0..n {
            let env = libm::expf(-6.9 * i as f32 / (1.8 * SR));
            reference.push(lp.process_sample(xorshift(&mut rng) * env));
        }
        let ref_f = rms_freq(&reference);
        assert!(
            plate_f > ref_f * 1.03,
            "plate centroid {plate_f:.0} Hz not above untilted reverb {ref_f:.0} Hz"
        );
    }

    #[test]
    fn mix_zero_is_dry_and_mix_follows_schema() {
        // wet = mix, dry = 1 − mix exactly (PlateVerb's Gain pair)
        let mut fx = make(1.8, 0.02, 6_000.0, 0.0);
        let mut l = [0.5f32; 128];
        let mut r = [-0.5f32; 128];
        fx.process(&mut l, &mut r);
        assert!(l.iter().all(|s| (*s - 0.5).abs() < 1e-6));
        assert!(r.iter().all(|s| (*s + 0.5).abs() < 1e-6));
        // mix = 1 leaves no dry: an impulse must not appear at sample 0
        let mut fx = make(1.8, 0.02, 6_000.0, 1.0);
        let (l, _) = render_ir(&mut fx, 0.05);
        assert!(l[0].abs() < 1e-6, "dry leaked at mix=1: {}", l[0]);
    }

    #[test]
    fn predelay_shifts_the_onset() {
        let onset_of = |pre: f32| {
            let mut fx = make(1.8, pre, 16_000.0, 1.0);
            let (l, _) = render_ir(&mut fx, 0.5);
            l.iter().position(|s| s.abs() > 1e-4).unwrap_or(l.len())
        };
        let near = onset_of(0.0);
        let far = onset_of(0.15);
        let shift = (far - near) as f32 / SR;
        assert!(
            (shift - 0.15).abs() < 0.01,
            "pre-delay shift {shift:.3}s, expected ~0.15s"
        );
    }
}
