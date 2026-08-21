//! `mono` instrument — port of `makeMono()` in src/audio/devices.ts:
//! Tone.MonoSynth topology. One PolyBLEP oscillator → lowpass (biquad-cascade
//! rolloff 12/24/48 dB via `slope`, Q from `res` in dB like a Web-Audio
//! lowpass) → amp ADSR. The filter cutoff is driven by a dedicated envelope
//! (attack 0.004, decay 0.18, sustain 0.4, release 0.3) mapped
//! `freq = base + (base·2^envAmt − base) · e²` (Tone.FrequencyEnvelope with
//! exponent 2, baseFrequency = `cutoff`, octaves = `envAmt`), plus the built-in
//! instrument LFO (`lfoShape/lfoRate/lfoAmt`, devices.ts makeInstLfo) summed
//! into the cutoff as ±cutoff·amt. `glide` is Tone's portamento: an
//! exponential pitch ramp over `glide` seconds when a note is retriggered
//! while the previous one is still sounding.

use crate::traits::InstrumentDsp;
use crate::util::{midi_hz, Adsr, Lfo, PolyBlepOsc, Smoother, Svf, SvfMode, Wave};

/// Tone's SLOPE_ROLLOFFS = [-12, -24, -48] as cascaded biquad stages.
const SLOPE_STAGES: [usize; 3] = [1, 2, 4];
const MAX_STAGES: usize = 4;

/// devices.ts INST_LFO_TYPES = ['sine', 'triangle', 'sawtooth', 'square'] →
/// the frozen lfo_shape_value indices (0 sine, 1 tri, 2 saw↑, 4 square).
const LFO_SHAPE_MAP: [u32; 4] = [0, 1, 2, 4];

pub struct MonoInst {
    sample_rate: f32,
    osc: PolyBlepOsc,
    filt: [Svf; MAX_STAGES],
    stages: usize,
    amp_env: Adsr,
    filt_env: Adsr,
    lfo: Lfo,

    // params
    cutoff: f32,
    res: f32,
    env_amt: f32,
    glide_s: f32,
    lfo_amt: f32,
    attack: f32,
    decay: f32,
    sustain: f32,
    release: f32,

    // voice state
    vel: Smoother,
    freq_log2: f32,        // current (possibly gliding) pitch in log2 Hz
    freq_target_log2: f32, // glide destination
    glide_step: f32,       // per-sample log2 increment (0 = arrived)
    gate_remaining: i64,   // samples until the scheduled note_off (-1 = held)
    active: bool,
    last_filter_hz: f32,
}

impl MonoInst {
    pub fn new() -> Self {
        let mut m = Self {
            sample_rate: 44_100.0,
            osc: PolyBlepOsc::new(Wave::Saw),
            filt: [
                Svf::new(SvfMode::Lowpass),
                Svf::new(SvfMode::Lowpass),
                Svf::new(SvfMode::Lowpass),
                Svf::new(SvfMode::Lowpass),
            ],
            stages: SLOPE_STAGES[1], // schema default slope = 1 (24 dB)
            amp_env: Adsr::new(),
            filt_env: Adsr::new(),
            lfo: Lfo::new(),
            cutoff: 900.0,
            res: 2.0,
            env_amt: 2.5,
            glide_s: 0.02,
            lfo_amt: 0.0,
            attack: 0.003,
            decay: 0.2,
            sustain: 0.45,
            release: 0.25,
            vel: Smoother::new(1.0),
            freq_log2: 0.0,
            freq_target_log2: 0.0,
            glide_step: 0.0,
            gate_remaining: -1,
            active: false,
            last_filter_hz: -1.0,
        };
        m.vel.set_time_ms(2.0);
        // makeMono's fixed filter envelope
        m.filt_env.set_adsr(0.004, 0.18, 0.4, 0.3);
        m.sync_amp_env();
        m
    }

    fn sync_amp_env(&mut self) {
        self.amp_env
            .set_adsr(self.attack, self.decay, self.sustain, self.release);
    }

    /// Q from `res`: Web-Audio lowpass Q is in dB (Tone.Filter passes it raw).
    #[inline]
    fn q_linear(&self) -> f32 {
        libm::powf(10.0, self.res * 0.05).max(0.05)
    }

    #[inline]
    fn set_filter_hz(&mut self, hz: f32) {
        // quantized guard so we only re-derive coefficients when the target moved
        if (hz - self.last_filter_hz).abs() > self.last_filter_hz * 1e-3 + 0.01 {
            self.last_filter_hz = hz;
            let q = self.q_linear();
            let hz = hz.clamp(10.0, self.sample_rate * 0.45);
            for f in self.filt.iter_mut().take(self.stages) {
                f.set_cutoff_q(hz, q);
            }
        }
    }

    fn start_note(&mut self, pitch: u8, vel: f32) {
        let target = libm::log2f(midi_hz(pitch as f32));
        // Tone.Monophonic portamento: glide when the previous note still sounds
        let audible = self.active && self.amp_env.value() > 0.05;
        if audible && self.glide_s > 1e-4 {
            self.freq_target_log2 = target;
            let n = (self.glide_s * self.sample_rate).max(1.0);
            self.glide_step = (target - self.freq_log2) / n;
        } else {
            self.freq_log2 = target;
            self.freq_target_log2 = target;
            self.glide_step = 0.0;
        }
        self.vel.set_target(vel.clamp(0.0, 1.0));
        if !self.active {
            self.vel.snap(vel.clamp(0.0, 1.0));
        }
        self.amp_env.note_on();
        self.filt_env.note_on();
        self.active = true;
    }
}

impl Default for MonoInst {
    fn default() -> Self {
        Self::new()
    }
}

impl InstrumentDsp for MonoInst {
    fn prepare(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate.max(1.0);
        self.osc.prepare(self.sample_rate);
        for f in &mut self.filt {
            f.prepare(self.sample_rate);
        }
        self.amp_env.prepare(self.sample_rate);
        self.filt_env.prepare(self.sample_rate);
        self.lfo.prepare(self.sample_rate);
        self.vel.prepare(self.sample_rate);
        self.last_filter_hz = -1.0;
    }

    fn set_param(&mut self, key: &str, value: f32) {
        match key {
            "wave" => self.osc.set_wave(Wave::from_index(value as usize)),
            "cutoff" => {
                self.cutoff = value.max(1.0);
                self.last_filter_hz = -1.0;
            }
            "res" => {
                self.res = value;
                self.last_filter_hz = -1.0;
            }
            "slope" => {
                let idx = (value as usize).min(2);
                let stages = SLOPE_STAGES[idx];
                if stages != self.stages {
                    self.stages = stages;
                    self.last_filter_hz = -1.0;
                }
            }
            "envAmt" => self.env_amt = value,
            "glide" => self.glide_s = value.max(0.0),
            "attack" => {
                self.attack = value.max(0.0);
                self.sync_amp_env();
            }
            "decay" => {
                self.decay = value.max(0.0);
                self.sync_amp_env();
            }
            "sustain" => {
                self.sustain = value.clamp(0.0, 1.0);
                self.sync_amp_env();
            }
            "release" => {
                self.release = value.max(0.0);
                self.sync_amp_env();
            }
            "lfoShape" => {
                let idx = (value as usize).min(3);
                self.lfo.set_shape(LFO_SHAPE_MAP[idx]);
            }
            "lfoRate" => self.lfo.set_freq(value),
            "lfoAmt" => self.lfo_amt = value.clamp(0.0, 1.0),
            _ => {}
        }
    }

    fn note_on(&mut self, pitch: u8, vel: f32) {
        self.gate_remaining = -1;
        self.start_note(pitch, vel);
    }

    fn note_off(&mut self, _pitch: u8) {
        self.amp_env.note_off();
        self.filt_env.note_off();
        self.gate_remaining = -1;
    }

    fn trigger(&mut self, pitch: u8, dur_sec: f32, vel: f32) {
        self.start_note(pitch, vel);
        self.gate_remaining = (dur_sec.max(0.0) * self.sample_rate) as i64;
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        if !self.active {
            return;
        }
        debug_assert_eq!(l.len(), r.len());
        let base = self.cutoff;
        let span = base * (libm::powf(2.0, self.env_amt) - 1.0);
        for i in 0..l.len() {
            // scheduled gate off
            if self.gate_remaining == 0 {
                self.amp_env.note_off();
                self.filt_env.note_off();
                self.gate_remaining = -1;
            } else if self.gate_remaining > 0 {
                self.gate_remaining -= 1;
            }
            // portamento glide in log2-frequency (Tone's exponential ramp)
            if self.glide_step != 0.0 {
                self.freq_log2 += self.glide_step;
                if (self.glide_step > 0.0 && self.freq_log2 >= self.freq_target_log2)
                    || (self.glide_step < 0.0 && self.freq_log2 <= self.freq_target_log2)
                {
                    self.freq_log2 = self.freq_target_log2;
                    self.glide_step = 0.0;
                }
            }
            self.osc.set_freq(libm::exp2f(self.freq_log2));

            // filter cutoff: envelope (exponent 2) + LFO offset (±cutoff·amt)
            let e = self.filt_env.next();
            let lfo_v = self.lfo.next();
            let mut hz = base + span * e * e;
            if self.lfo_amt > 0.001 {
                hz += lfo_v * base * self.lfo_amt;
            }
            self.set_filter_hz(hz.max(10.0));

            let mut s = self.osc.next();
            for f in self.filt.iter_mut().take(self.stages) {
                s = f.process_sample(s);
            }
            s *= self.amp_env.next() * self.vel.next();
            l[i] += s;
            r[i] += s;

            if !self.amp_env.is_active() {
                self.active = false;
                for f in self.filt.iter_mut().take(self.stages) {
                    f.reset();
                }
                return;
            }
        }
    }

    fn reset(&mut self) {
        self.amp_env.reset();
        self.filt_env.reset();
        for f in &mut self.filt {
            f.reset();
        }
        self.osc.reset_phase(0.0);
        self.active = false;
        self.gate_remaining = -1;
        self.glide_step = 0.0;
        self.last_filter_hz = -1.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(params: &[(&str, f32)]) -> MonoInst {
        let mut m = MonoInst::new();
        m.prepare(44_100.0);
        for (k, v) in params {
            m.set_param(k, *v);
        }
        m
    }

    fn render(m: &mut MonoInst, n: usize) -> Vec<f32> {
        let mut out = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        for (cl, cr) in out.chunks_mut(128).zip(r.chunks_mut(128)) {
            m.process(cl, cr);
        }
        out
    }

    fn rms(buf: &[f32]) -> f32 {
        libm::sqrtf(buf.iter().map(|s| s * s).sum::<f32>() / buf.len().max(1) as f32)
    }

    #[test]
    fn pitch_via_zero_crossings() {
        // sine wave + wide-open filter so the waveform is a clean sinusoid
        let mut m = make(&[
            ("wave", 3.0),
            ("cutoff", 8000.0),
            ("envAmt", 0.0),
            ("res", 0.0),
            ("glide", 0.0),
            ("sustain", 1.0),
            ("attack", 0.001),
        ]);
        m.note_on(69, 1.0); // A4 = 440 Hz
        let buf = render(&mut m, 44_100);
        let skip = 2_000; // let the attack/filter settle
        let zc = buf[skip..]
            .windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        let cycles = zc as f32 / 2.0;
        let expected = 440.0 * (44_100.0 - skip as f32) / 44_100.0;
        assert!(
            (cycles - expected).abs() < 12.0,
            "{cycles} cycles vs {expected}"
        );
    }

    #[test]
    fn cutoff_sweep_shapes_spectrum() {
        // saw through a closed vs open filter: high-frequency energy must grow
        let hf_energy = |cutoff: f32| {
            let mut m = make(&[
                ("wave", 0.0),
                ("cutoff", cutoff),
                ("envAmt", 0.0),
                ("res", 0.0),
                ("sustain", 1.0),
                ("lfoAmt", 0.0),
            ]);
            m.note_on(45, 1.0); // 110 Hz saw
            let buf = render(&mut m, 44_100);
            // crude HF tap: first difference boosts highs, kills lows
            let hf: Vec<f32> = buf.windows(2).map(|w| w[1] - w[0]).collect();
            rms(&hf[4_000..])
        };
        let closed = hf_energy(200.0);
        let open = hf_energy(6_000.0);
        assert!(
            open > closed * 3.0,
            "open {open} not brighter than closed {closed}"
        );
    }

    #[test]
    fn no_nan_across_param_extremes() {
        let mut m = make(&[
            ("wave", 1.0),
            ("cutoff", 8_000.0),
            ("res", 10.0),
            ("slope", 2.0),
            ("envAmt", 6.0),
            ("glide", 0.3),
            ("lfoShape", 3.0),
            ("lfoRate", 16.0),
            ("lfoAmt", 1.0),
        ]);
        m.trigger(24, 0.2, 1.0);
        m.trigger(96, 0.2, 1.0); // extreme glide jump
        let buf = render(&mut m, 22_050);
        assert!(buf.iter().all(|s| s.is_finite()));
        assert!(rms(&buf) > 1e-5, "should be audible");
    }

    #[test]
    fn silent_after_release() {
        let mut m = make(&[("release", 0.05), ("sustain", 0.5)]);
        m.trigger(60, 0.1, 0.9);
        let buf = render(&mut m, 44_100); // 1 s ≫ 0.1 s gate + 50 ms release
        let tail = &buf[22_050..];
        assert!(rms(tail) < 1e-4, "tail rms {}", rms(tail));
        // and fully inactive: additive process leaves the buffer untouched
        let mut l = [0.25f32; 64];
        let mut r = [0.25f32; 64];
        m.process(&mut l, &mut r);
        assert!(l.iter().all(|s| *s == 0.25));
    }

    /// Goertzel power at one frequency (steady-state tail of the buffer).
    fn tone_power(buf: &[f32], freq: f32, sr: f32) -> f32 {
        let w = core::f32::consts::TAU * freq / sr;
        let c = 2.0 * libm::cosf(w);
        let (mut s1, mut s2) = (0.0f32, 0.0f32);
        for &x in buf {
            let s0 = x + c * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        s1 * s1 + s2 * s2 - c * s1 * s2
    }

    #[test]
    fn slope_48_darker_than_12() {
        // saw at 110 Hz, cutoff 400: measure the 32nd harmonic (3520 Hz),
        // ~3 octaves into the stop band — 48 dB/oct must sink far below 12.
        let hf = |slope: f32| {
            let mut m = make(&[
                ("wave", 0.0),
                ("cutoff", 400.0),
                ("envAmt", 0.0),
                ("res", 0.0),
                ("slope", slope),
                ("sustain", 1.0),
            ]);
            m.note_on(45, 1.0);
            let buf = render(&mut m, 44_100);
            tone_power(&buf[22_050..], 3_520.0, 44_100.0)
        };
        let s12 = hf(0.0);
        let s48 = hf(2.0);
        assert!(
            s48 < s12 * 0.05,
            "48 dB ({s48}) should be far darker than 12 dB ({s12})"
        );
    }
}
