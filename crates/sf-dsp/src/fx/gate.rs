//! `gate` effect — line-for-line port of the `sf-gate` AudioWorklet
//! (public/sf-gate.js), the canonical DSP for this device. Any change there
//! must land here too (CLAUDE.md: render-path parity).
//!
//! Chain, per sample:
//!   key select → key filter (HP→LP) → detector (peak | RMS) → dB
//!   → hysteresis state machine (separate open / close thresholds)
//!   → expander curve (ratio, soft knee, clamped by range)
//!   → attack / hold / release ramp (linear in dB)
//!   → applied to the LOOKAHEAD-delayed audio → mix → output gain
//!
//! Open fires when the key rises past `thresh + up`, close when it falls below
//! `thresh + dn`: dn < up is classic hysteresis, up = dn = 0 a plain
//! expander, dn > up crosses the thresholds (percussive chop). `inverse`
//! mirrors the key about `thresh` so the gate passes what is BELOW it.
//!
//! Precision: the worklet runs in JS doubles, so the detector, state machine
//! and gain ramp are f64 here; only the audio buffers are f32.
//!
//! Schema (schema.ts "gate"): thresh −80..0 dB (−40), up/dn −40..40 dB
//! (0 / −6), range −100..0 dB (−60), ratio 1..20 (20 = "Gate", infinite),
//! knee 0..24 dB (3), attack 0.1 ms..0.3 s (2 ms), hold 1 ms..2 s (50 ms),
//! release 5 ms..3 s (150 ms), look 0..10 ms (0), hpf 20..2000 Hz (20),
//! lpf 200..20000 Hz (20000), key Stereo/Left/Right/Mid/Side, det Peak/RMS,
//! listen Off/On, inverse Off/On, mix 0..1 (1), gain −24..24 dB (0).

use crate::traits::EffectDsp;

const LOG2DB: f64 = 8.685_889_638_065_035; // 20/ln(10)
const DB_FLOOR: f64 = -160.0; // dB floor for log of near-silence
const MIN_LIN: f64 = 1e-8; // −160 dBFS
const RMS_WIN_SEC: f64 = 0.01; // RMS detector integration window
const PEAK_REL_SEC: f64 = 0.005; // peak detector release (instant attack)
const MAX_LOOK_MS: f64 = 10.0; // schema `look` max — sizes the delay line

#[inline]
fn db_of(v: f64) -> f64 {
    if v > MIN_LIN { libm::log(v) * LOG2DB } else { DB_FLOOR }
}

/// One-pole coefficient for a time constant in seconds (0 time ⇒ instant).
#[inline]
fn pole_of(sec: f64, sr: f64) -> f64 {
    if sec > 0.0 { libm::exp(-1.0 / (sec * sr)) } else { 0.0 }
}

/// Direct-Form-I biquad (one per channel per filter slot) — as sf-gate.js.
#[derive(Clone, Copy)]
struct Biquad {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
    c: [f64; 5],
}

impl Biquad {
    const fn new() -> Self {
        Self { x1: 0.0, x2: 0.0, y1: 0.0, y2: 0.0, c: [1.0, 0.0, 0.0, 0.0, 0.0] }
    }
    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
    #[inline]
    fn proc(&mut self, x: f64) -> f64 {
        let c = &self.c;
        let y = c[0] * x + c[1] * self.x1 + c[2] * self.x2 - c[3] * self.y1 - c[4] * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }
}

/// RBJ cookbook, Butterworth Q, normalized by a0.
fn lp_coeffs(f: f64, sr: f64) -> [f64; 5] {
    let w0 = 2.0 * core::f64::consts::PI * f.min(sr * 0.49) / sr;
    let cw = libm::cos(w0);
    let alpha = libm::sin(w0) / (2.0 * core::f64::consts::FRAC_1_SQRT_2);
    let a0 = 1.0 + alpha;
    [((1.0 - cw) / 2.0) / a0, (1.0 - cw) / a0, ((1.0 - cw) / 2.0) / a0, (-2.0 * cw) / a0, (1.0 - alpha) / a0]
}

fn hp_coeffs(f: f64, sr: f64) -> [f64; 5] {
    let w0 = 2.0 * core::f64::consts::PI * f.min(sr * 0.49) / sr;
    let cw = libm::cos(w0);
    let alpha = libm::sin(w0) / (2.0 * core::f64::consts::FRAC_1_SQRT_2);
    let a0 = 1.0 + alpha;
    [((1.0 + cw) / 2.0) / a0, (-(1.0 + cw)) / a0, ((1.0 + cw) / 2.0) / a0, (-2.0 * cw) / a0, (1.0 - alpha) / a0]
}

/// Raw schema params (JS `this.p`).
#[derive(Clone, Copy)]
struct Params {
    thresh: f64,
    up: f64,
    dn: f64,
    range: f64,
    ratio: f64,
    knee: f64,
    attack: f64,
    hold: f64,
    release: f64,
    look: f64,
    hpf: f64,
    lpf: f64,
    key: f64,
    det: f64,
    listen: f64,
    inverse: f64,
    mix: f64,
    gain: f64,
}

impl Default for Params {
    /// Schema defaults (schema.ts "gate").
    fn default() -> Self {
        Self {
            thresh: -40.0,
            up: 0.0,
            dn: -6.0,
            range: -60.0,
            ratio: 20.0,
            knee: 3.0,
            attack: 0.002,
            hold: 0.05,
            release: 0.15,
            look: 0.0,
            hpf: 20.0,
            lpf: 20000.0,
            key: 0.0,
            det: 0.0,
            listen: 0.0,
            inverse: 0.0,
            mix: 1.0,
            gain: 0.0,
        }
    }
}

pub struct GateFx {
    p: Params,
    sr: f64,
    dirty: bool,
    hp: [Biquad; 2],
    lp: [Biquad; 2],
    peak_env: [f64; 2],
    ms_env: [f64; 2],
    open: bool,
    hold_left: u64,
    gain_db: f64,
    // derived (recompute)
    peak_rel: f64,
    rms_pole: f64,
    att_step: f64,
    rel_step: f64,
    hold_samples: u64,
    delay: usize,
    mix_wet: f64,
    out_lin: f64,
    range_db: f64,
    ratio: f64,
    infinite: bool,
    knee: f64,
    sign: f64,
    // lookahead delay lines (sized in prepare — never on the audio thread)
    dl: Vec<f32>,
    dr: Vec<f32>,
    widx: usize,
    // GR meter: peak reduction over a ~30 ms window, like the worklet's posts
    gr_max: f64,
    gr_report: f64,
    rep: usize,
    rep_every: usize,
}

impl GateFx {
    pub fn new() -> Self {
        let mut g = Self {
            p: Params::default(),
            sr: 44_100.0,
            dirty: true,
            hp: [Biquad::new(); 2],
            lp: [Biquad::new(); 2],
            peak_env: [0.0; 2],
            ms_env: [0.0; 2],
            open: false,
            hold_left: 0,
            gain_db: 0.0,
            peak_rel: 0.0,
            rms_pole: 0.0,
            att_step: 0.0,
            rel_step: 0.0,
            hold_samples: 0,
            delay: 0,
            mix_wet: 1.0,
            out_lin: 1.0,
            range_db: -60.0,
            ratio: 20.0,
            infinite: true,
            knee: 3.0,
            sign: 1.0,
            dl: Vec::new(),
            dr: Vec::new(),
            widx: 0,
            gr_max: 0.0,
            gr_report: 0.0,
            rep: 0,
            rep_every: 1323,
        };
        g.prepare(44_100.0);
        g
    }

    fn recompute(&mut self) {
        self.dirty = false;
        let p = self.p;
        let sr = self.sr;
        // Keep the key band ordered — a crossed HP/LP would key off silence.
        let hp_f = p.hpf.min(p.lpf * 0.98);
        let lp_f = p.lpf.max(p.hpf * 1.02);
        let hc = hp_coeffs(hp_f, sr);
        let lc = lp_coeffs(lp_f, sr);
        for c in 0..2 {
            self.hp[c].c = hc;
            self.lp[c].c = lc;
        }
        self.peak_rel = pole_of(PEAK_REL_SEC, sr);
        self.rms_pole = pole_of(RMS_WIN_SEC, sr);
        // Ramps are linear in dB and span the full `range`, so "attack" is
        // literally the time to open from fully closed (and release to close).
        let span = p.range.abs().max(6.0);
        self.att_step = span / (p.attack.max(0.0001) * sr);
        self.rel_step = span / (p.release.max(0.0005) * sr);
        self.hold_samples = libm::round(p.hold.max(0.0) * sr) as u64;
        let dlen = self.dl.len().max(1);
        self.delay = ((libm::round((p.look / 1000.0) * sr)).max(0.0) as usize).min(dlen - 1);
        self.mix_wet = p.mix.clamp(0.0, 1.0);
        self.out_lin = libm::pow(10.0, p.gain / 20.0);
        self.range_db = p.range.min(0.0);
        self.ratio = p.ratio.max(1.0);
        self.infinite = self.ratio >= 19.95;
        self.knee = p.knee.max(0.0);
        self.sign = if p.inverse >= 0.5 { -1.0 } else { 1.0 };
    }

    /// Reduction (dB ≥ 0) the expander asks for at `x` dB past the threshold.
    #[inline]
    fn curve_red(&self, x: f64) -> f64 {
        if self.infinite {
            return -self.range_db;
        }
        let k = self.knee;
        let slope = self.ratio - 1.0;
        let red = if x >= k * 0.5 {
            0.0
        } else if x <= -k * 0.5 {
            slope * -x
        } else {
            let d = x - k * 0.5;
            (slope * d * d) / (2.0 * k)
        };
        red.min(-self.range_db)
    }

    fn reset_state(&mut self) {
        for c in 0..2 {
            self.hp[c].reset();
            self.lp[c].reset();
        }
        self.peak_env = [0.0; 2];
        self.ms_env = [0.0; 2];
        self.open = false;
        self.hold_left = 0;
        self.gain_db = 0.0;
        self.dl.iter_mut().for_each(|v| *v = 0.0);
        self.dr.iter_mut().for_each(|v| *v = 0.0);
        self.widx = 0;
        self.gr_max = 0.0;
        self.gr_report = 0.0;
        self.rep = 0;
    }
}

impl Default for GateFx {
    fn default() -> Self {
        Self::new()
    }
}

impl EffectDsp for GateFx {
    fn prepare(&mut self, sample_rate: f32) {
        self.sr = sample_rate.max(1.0) as f64;
        let max_delay = libm::ceil((MAX_LOOK_MS / 1000.0) * self.sr) as usize + 2;
        self.dl = vec![0.0; max_delay];
        self.dr = vec![0.0; max_delay];
        self.rep_every = 256usize.max(libm::floor(self.sr * 0.03) as usize);
        self.reset_state();
        self.dirty = true;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        let v = value as f64;
        if !v.is_finite() {
            return;
        }
        let p = &mut self.p;
        let slot = match key {
            "thresh" => &mut p.thresh,
            "up" => &mut p.up,
            "dn" => &mut p.dn,
            "range" => &mut p.range,
            "ratio" => &mut p.ratio,
            "knee" => &mut p.knee,
            "attack" => &mut p.attack,
            "hold" => &mut p.hold,
            "release" => &mut p.release,
            "look" => &mut p.look,
            "hpf" => &mut p.hpf,
            "lpf" => &mut p.lpf,
            "key" => &mut p.key,
            "det" => &mut p.det,
            "listen" => &mut p.listen,
            "inverse" => &mut p.inverse,
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
        let p = self.p;
        let mode = p.key as i32;
        let rms = p.det >= 0.5;
        let listen = p.listen >= 0.5;
        let stereo_key = mode == 0;
        let open_at = p.thresh + p.up;
        let close_at = p.thresh + p.dn;
        let dry = 1.0 - self.mix_wet;
        let dlen = self.dl.len();
        let n = l.len().min(r.len());

        for i in 0..n {
            let xl = l[i] as f64;
            let xr = r[i] as f64;

            // ---- key signal (pre-filter) ----
            let (mut k0, mut k1) = match mode {
                0 => (xl, xr),
                1 => (xl, 0.0),
                2 => (xr, 0.0),
                3 => ((xl + xr) * 0.5, 0.0),
                _ => ((xl - xr) * 0.5, 0.0),
            };

            // ---- key filter: HP then LP ----
            k0 = self.lp[0].proc(self.hp[0].proc(k0));
            if stereo_key {
                k1 = self.lp[1].proc(self.hp[1].proc(k1));
            }

            // ---- detector ----
            let lvl = if rms {
                self.ms_env[0] += (k0 * k0 - self.ms_env[0]) * (1.0 - self.rms_pole);
                let mut ms = self.ms_env[0];
                if stereo_key {
                    self.ms_env[1] += (k1 * k1 - self.ms_env[1]) * (1.0 - self.rms_pole);
                    if self.ms_env[1] > ms {
                        ms = self.ms_env[1];
                    }
                }
                libm::sqrt(ms.max(0.0))
            } else {
                let a0 = k0.abs();
                self.peak_env[0] =
                    if a0 > self.peak_env[0] { a0 } else { a0 + (self.peak_env[0] - a0) * self.peak_rel };
                let mut lvl = self.peak_env[0];
                if stereo_key {
                    let a1 = k1.abs();
                    self.peak_env[1] =
                        if a1 > self.peak_env[1] { a1 } else { a1 + (self.peak_env[1] - a1) * self.peak_rel };
                    if self.peak_env[1] > lvl {
                        lvl = self.peak_env[1];
                    }
                }
                lvl
            };
            let lvl_db = db_of(lvl);

            // ---- hysteresis state machine ----
            let sg = self.sign;
            if self.open {
                if sg * lvl_db < sg * close_at {
                    // Hold keeps the gate wide open for its full time first.
                    if self.hold_left > 0 {
                        self.hold_left -= 1;
                    } else {
                        self.open = false;
                    }
                } else {
                    self.hold_left = self.hold_samples;
                }
            } else if sg * lvl_db > sg * open_at {
                self.open = true;
                self.hold_left = self.hold_samples;
            }

            // ---- target gain ----
            let target_db = if self.open { 0.0 } else { -self.curve_red(sg * (lvl_db - p.thresh)) };

            // ---- attack / release ramp (linear in dB) ----
            if target_db > self.gain_db {
                self.gain_db = target_db.min(self.gain_db + self.att_step);
            } else if target_db < self.gain_db {
                self.gain_db = target_db.max(self.gain_db - self.rel_step);
            }
            let g = libm::pow(10.0, self.gain_db * 0.05);
            if -self.gain_db > self.gr_max {
                self.gr_max = -self.gain_db;
            }

            // ---- lookahead delay (audio only; the detector ran early) ----
            self.dl[self.widx] = xl as f32;
            self.dr[self.widx] = xr as f32;
            let ridx = if self.widx >= self.delay { self.widx - self.delay } else { self.widx + dlen - self.delay };
            let d_l = self.dl[ridx] as f64;
            let d_r = self.dr[ridx] as f64;
            self.widx = if self.widx + 1 == dlen { 0 } else { self.widx + 1 };

            if listen {
                // Audition the key band, ungated (ignores mix on purpose).
                let m = if stereo_key { (k0 + k1) * 0.5 } else { k0 };
                l[i] = (m * self.out_lin) as f32;
                r[i] = (m * self.out_lin) as f32;
            } else {
                l[i] = ((d_l * g * self.mix_wet + d_l * dry) * self.out_lin) as f32;
                r[i] = ((d_r * g * self.mix_wet + d_r * dry) * self.out_lin) as f32;
            }
        }

        self.rep += n;
        if self.rep >= self.rep_every {
            self.rep = 0;
            self.gr_report = self.gr_max;
            self.gr_max = 0.0;
        }
    }

    /// dB ≤ 0, the peak reduction of the last ~30 ms window (the worklet's
    /// posted `gr`, negated as devices.ts does).
    fn gr_db(&self) -> f32 {
        -(self.gr_report.max(self.gr_max)) as f32
    }

    fn reset(&mut self) {
        self.reset_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SR: f32 = 48_000.0;

    fn sine(amp: f32, hz: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * libm::sinf(core::f32::consts::TAU * hz * i as f32 / SR)).collect()
    }

    fn rms(v: &[f32]) -> f32 {
        libm::sqrtf(v.iter().map(|x| x * x).sum::<f32>() / v.len().max(1) as f32)
    }

    fn run(g: &mut GateFx, input: &[f32]) -> Vec<f32> {
        let mut l = input.to_vec();
        let mut r = input.to_vec();
        for (cl, cr) in l.chunks_mut(128).zip(r.chunks_mut(128)) {
            g.process(cl, cr);
        }
        l
    }

    fn gate() -> GateFx {
        let mut g = GateFx::new();
        g.prepare(SR);
        g
    }

    #[test]
    fn loud_signal_passes_after_attack() {
        let mut g = gate();
        let x = sine(0.5, 440.0, 24_000); // ≈ −9 dBFS peak, far above −40
        let y = run(&mut g, &x);
        let tail = 4_800..24_000; // after the 2 ms attack
        let ratio = rms(&y[tail.clone()]) / rms(&x[tail]);
        assert!((ratio - 1.0).abs() < 1e-3, "open gate must be unity, got {ratio}");
    }

    #[test]
    fn quiet_signal_is_cut_by_range() {
        let mut g = gate();
        let x = sine(0.001, 440.0, 48_000); // −60 dBFS, below −40
        let y = run(&mut g, &x);
        let tail = 24_000..48_000; // well past the release
        let att_db = 20.0 * libm::log10f(rms(&y[tail.clone()]) / rms(&x[tail]));
        assert!((att_db + 60.0).abs() < 0.5, "closed gate must sit at range (−60 dB), got {att_db}");
        assert!(g.gr_db() < -59.0, "meter shows the reduction: {}", g.gr_db());
    }

    #[test]
    fn inverse_passes_quiet_and_cuts_loud() {
        let mut g = gate();
        g.set_param("inverse", 1.0);
        let quiet = run(&mut g, &sine(0.001, 440.0, 24_000));
        assert!(rms(&quiet[12_000..]) > 0.0006, "inverse passes the quiet signal");
        let mut g = gate();
        g.set_param("inverse", 1.0);
        let loud = run(&mut g, &sine(0.5, 440.0, 48_000));
        assert!(rms(&loud[24_000..]) < 0.5 * 1e-2, "inverse cuts the loud signal");
    }

    #[test]
    fn hysteresis_holds_state_between_thresholds() {
        // open at −40, close below −46: a level of −43 dB keeps whatever
        // state the gate is in
        let mut g = gate();
        let between = 0.00708; // ≈ −43 dBFS
        let y = run(&mut g, &sine(between, 440.0, 24_000));
        assert!(rms(&y[12_000..]) < between * 0.01, "starts closed → stays closed");
        let mut g = gate();
        run(&mut g, &sine(0.5, 440.0, 4_800)); // open it
        let y = run(&mut g, &sine(between, 440.0, 24_000));
        let ratio = rms(&y[12_000..]) / (between / core::f32::consts::SQRT_2);
        assert!((ratio - 1.0).abs() < 0.02, "opened → stays open, got {ratio}");
    }

    #[test]
    fn lookahead_delays_the_audio() {
        let mut g = gate();
        g.set_param("look", 5.0); // 5 ms = 240 samples at 48 kHz
        g.set_param("thresh", -80.0);
        let mut x = vec![0.0f32; 1_000];
        x[100] = 1.0;
        let y = run(&mut g, &x);
        let peak = y.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
        assert_eq!(peak, 340);
    }

    #[test]
    fn mix_zero_and_output_gain() {
        let mut g = gate();
        g.set_param("mix", 0.0);
        g.set_param("gain", -6.0);
        let x = sine(0.001, 440.0, 24_000); // would be gated
        let y = run(&mut g, &x);
        let ratio = rms(&y[12_000..]) / rms(&x[12_000..]);
        assert!((ratio - 0.501).abs() < 0.01, "dry path × −6 dB, got {ratio}");
    }

    #[test]
    fn expander_ratio_is_gentler_than_gate() {
        let mut g = gate();
        g.set_param("ratio", 2.0); // 2:1 expander, −60 range
        let x = sine(0.001, 440.0, 48_000); // −60 dBFS peak → 20 dB under −40
        let y = run(&mut g, &x);
        let att_db = 20.0 * libm::log10f(rms(&y[24_000..]) / rms(&x[24_000..]));
        assert!(att_db < -10.0 && att_db > -40.0, "2:1 expands partially, got {att_db}");
    }
}
