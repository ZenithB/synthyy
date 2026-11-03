//! Param registry mirroring the FROZEN device schema (`src/audio/schema.ts`):
//! key/label/min/max/def plus the `exp` (log curve), `int`, `steps` and
//! `rateSwitch` flags. Tables transcribed from `fixtures/schema-dump.json`
//! (regenerate with `node scripts/dump-schema.mjs`); the `schema_dump` test
//! validates every entry 1:1 against that dump. Display-only `fmt` closures
//! are intentionally not mirrored.

/// Knob/automation response curve. `Exp` mirrors schema.ts `exp: true`; like
/// `isLogSpec` there, the log mapping only applies when `min > 0 && max > 0`
/// (see [`ParamSpec::is_log`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Curve {
    Linear,
    Exp,
}

/// One parameter spec (schema.ts `ParamSpec`, minus `fmt`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub min: f32,
    pub max: f32,
    pub def: f32,
    pub curve: Curve,
    /// Integer steps (`int: true`).
    pub int: bool,
    /// Enum knob: value is an index into these labels (`steps`).
    pub steps: Option<&'static [&'static str]>,
    /// Rate knob with Sync / Low / High modes (`rateSwitch: true`).
    pub rate_switch: bool,
}

impl ParamSpec {
    /// schema.ts `isLogSpec`: true when the spec maps on a log curve.
    pub fn is_log(&self) -> bool {
        self.curve == Curve::Exp && self.min > 0.0 && self.max > 0.0
    }

    /// schema.ts `normFromSpec`: raw value → normalized [0,1] along the curve.
    pub fn norm(&self, v: f32) -> f32 {
        if self.max == self.min {
            return 0.0;
        }
        if self.is_log() {
            ((v.ln() - self.min.ln()) / (self.max.ln() - self.min.ln())).clamp(0.0, 1.0)
        } else {
            ((v - self.min) / (self.max - self.min)).clamp(0.0, 1.0)
        }
    }

    /// schema.ts `valueFromSpec`: normalized [0,1] → raw value along the curve.
    pub fn value(&self, n: f32) -> f32 {
        let c = n.clamp(0.0, 1.0);
        if self.is_log() {
            (self.min.ln() + c * (self.max.ln() - self.min.ln())).exp()
        } else {
            self.min + c * (self.max - self.min)
        }
    }
}

static INST_POLY: [ParamSpec; 12] = [
    ParamSpec { key: "wave", label: "Wave", min: 0.0, max: 6.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Saw", "Sqr", "Tri", "Sin", "FatSaw", "FatSqr", "FatTri"]), rate_switch: false },
    ParamSpec { key: "spread", label: "Spread", min: 0.0, max: 60.0, def: 18.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "cutoff", label: "Cutoff", min: 80.0, max: 14000.0, def: 7000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "res", label: "Res", min: 0.0, max: 10.0, def: 0.7, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "slope", label: "Slope", min: 0.0, max: 2.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["12dB", "24dB", "48dB"]), rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 2.0, def: 0.01, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "decay", label: "Decay", min: 0.01, max: 2.0, def: 0.15, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "sustain", label: "Sustain", min: 0.0, max: 1.0, def: 0.6, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.01, max: 4.0, def: 0.4, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "lfoShape", label: "LFO", min: 0.0, max: 3.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Sin", "Tri", "Saw", "Sqr"]), rate_switch: false },
    ParamSpec { key: "lfoRate", label: "LFO Rate", min: 0.05, max: 16.0, def: 2.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "lfoAmt", label: "LFO Amt", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static INST_DUO: [ParamSpec; 7] = [
    ParamSpec { key: "harm", label: "Interval", min: 0.5, max: 3.0, def: 1.5, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "vibAmt", label: "Vibrato", min: 0.0, max: 0.6, def: 0.12, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "vibRate", label: "Vib Rate", min: 0.5, max: 10.0, def: 4.5, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 2.0, def: 0.02, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "decay", label: "Decay", min: 0.01, max: 2.0, def: 0.3, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "sustain", label: "Sustain", min: 0.0, max: 1.0, def: 0.6, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.01, max: 4.0, def: 0.6, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
];

static INST_FM: [ParamSpec; 6] = [
    ParamSpec { key: "harm", label: "Ratio", min: 0.25, max: 8.0, def: 3.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "modIdx", label: "FM Amt", min: 0.5, max: 40.0, def: 10.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 2.0, def: 0.005, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "decay", label: "Decay", min: 0.01, max: 2.0, def: 0.3, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "sustain", label: "Sustain", min: 0.0, max: 1.0, def: 0.4, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.01, max: 4.0, def: 0.6, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
];

static INST_MONO: [ParamSpec; 13] = [
    ParamSpec { key: "wave", label: "Wave", min: 0.0, max: 3.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Saw", "Sqr", "Tri", "Sin", "FatSaw", "FatSqr", "FatTri"]), rate_switch: false },
    ParamSpec { key: "cutoff", label: "Cutoff", min: 40.0, max: 8000.0, def: 900.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "res", label: "Res", min: 0.0, max: 10.0, def: 2.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "slope", label: "Slope", min: 0.0, max: 2.0, def: 1.0, curve: Curve::Linear, int: true, steps: Some(&["12dB", "24dB", "48dB"]), rate_switch: false },
    ParamSpec { key: "envAmt", label: "Env Amt", min: 0.0, max: 6.0, def: 2.5, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "glide", label: "Glide", min: 0.0, max: 0.3, def: 0.02, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 2.0, def: 0.003, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "decay", label: "Decay", min: 0.01, max: 2.0, def: 0.2, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "sustain", label: "Sustain", min: 0.0, max: 1.0, def: 0.45, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.01, max: 4.0, def: 0.25, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "lfoShape", label: "LFO", min: 0.0, max: 3.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Sin", "Tri", "Saw", "Sqr"]), rate_switch: false },
    ParamSpec { key: "lfoRate", label: "LFO Rate", min: 0.05, max: 16.0, def: 2.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "lfoAmt", label: "LFO Amt", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static INST_PLUCK: [ParamSpec; 2] = [
    ParamSpec { key: "dampen", label: "Tone", min: 500.0, max: 12000.0, def: 4000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "res", label: "Sustain", min: 0.3, max: 0.98, def: 0.93, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static INST_KEYS: [ParamSpec; 5] = [
    ParamSpec { key: "harm", label: "Color", min: 0.5, max: 4.0, def: 2.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 2.0, def: 0.01, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "decay", label: "Decay", min: 0.01, max: 2.0, def: 0.4, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "sustain", label: "Sustain", min: 0.0, max: 1.0, def: 0.5, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.01, max: 4.0, def: 0.8, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
];

static INST_SAMPLER: [ParamSpec; 3] = [
    ParamSpec { key: "tune", label: "Tune", min: -24.0, max: 24.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 1.0, def: 0.005, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.01, max: 3.0, def: 0.4, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
];

static INST_KSAMPLER: [ParamSpec; 4] = [
    ParamSpec { key: "root", label: "Root", min: 0.0, max: 11.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"]), rate_switch: false },
    ParamSpec { key: "tune", label: "Fine", min: -24.0, max: 24.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 2.0, def: 0.004, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.01, max: 4.0, def: 0.4, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
];

static INST_GRANULAR: [ParamSpec; 11] = [
    ParamSpec { key: "size", label: "Grain", min: 0.02, max: 0.5, def: 0.12, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "dens", label: "Density", min: 2.0, max: 80.0, def: 18.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "pos", label: "Position", min: 0.0, max: 1.0, def: 0.15, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "spray", label: "Spray", min: 0.0, max: 1.0, def: 0.05, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "pitch", label: "Pitch", min: -24.0, max: 24.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "pjit", label: "P.Jitter", min: 0.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "rev", label: "Reverse", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "shape", label: "Shape", min: 0.0, max: 1.0, def: 0.5, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "spread", label: "Spread", min: 0.0, max: 1.0, def: 0.6, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 2.0, def: 0.05, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.01, max: 4.0, def: 0.4, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
];

static INST_DRUM: [ParamSpec; 24] = [
    ParamSpec { key: "p0_tune", label: "Kick Tune", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p0_decay", label: "Kick Decay", min: 0.03, max: 2.0, def: 0.42, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p0_level", label: "Kick Level", min: -24.0, max: 6.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p1_tune", label: "Snare Tune", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p1_decay", label: "Snare Decay", min: 0.03, max: 2.0, def: 0.18, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p1_level", label: "Snare Level", min: -24.0, max: 6.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p2_tune", label: "Clap Tune", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p2_decay", label: "Clap Decay", min: 0.03, max: 2.0, def: 0.28, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p2_level", label: "Clap Level", min: -24.0, max: 6.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p3_tune", label: "Cl Hat Tune", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p3_decay", label: "Cl Hat Decay", min: 0.03, max: 2.0, def: 0.06, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p3_level", label: "Cl Hat Level", min: -24.0, max: 6.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p4_tune", label: "Op Hat Tune", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p4_decay", label: "Op Hat Decay", min: 0.03, max: 2.0, def: 0.4, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p4_level", label: "Op Hat Level", min: -24.0, max: 6.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p5_tune", label: "Lo Tom Tune", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p5_decay", label: "Lo Tom Decay", min: 0.03, max: 2.0, def: 0.32, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p5_level", label: "Lo Tom Level", min: -24.0, max: 6.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p6_tune", label: "Perc Tune", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p6_decay", label: "Perc Decay", min: 0.03, max: 2.0, def: 0.12, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p6_level", label: "Perc Level", min: -24.0, max: 6.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p7_tune", label: "Crash Tune", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p7_decay", label: "Crash Decay", min: 0.03, max: 2.0, def: 1.3, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "p7_level", label: "Crash Level", min: -24.0, max: 6.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static INST_AUDIOBUS: [ParamSpec; 0] = [
];

/// Every instrument type, in schema.ts order.
pub static INSTRUMENTS: [(&str, &[ParamSpec]); 11] = [
    ("poly", &INST_POLY),
    ("duo", &INST_DUO),
    ("fm", &INST_FM),
    ("mono", &INST_MONO),
    ("pluck", &INST_PLUCK),
    ("keys", &INST_KEYS),
    ("sampler", &INST_SAMPLER),
    ("ksampler", &INST_KSAMPLER),
    ("granular", &INST_GRANULAR),
    ("drum", &INST_DRUM),
    ("audiobus", &INST_AUDIOBUS),
];

static FX_EQ: [ParamSpec; 3] = [
    ParamSpec { key: "low", label: "Low", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mid", label: "Mid", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "high", label: "High", min: -12.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_FILTER: [ParamSpec; 4] = [
    ParamSpec { key: "ftype", label: "Type", min: 0.0, max: 2.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["LP", "HP", "BP"]), rate_switch: false },
    ParamSpec { key: "freq", label: "Freq", min: 40.0, max: 18000.0, def: 2000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "q", label: "Res", min: 0.0, max: 12.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "slope", label: "Slope", min: 0.0, max: 2.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["12dB", "24dB", "48dB"]), rate_switch: false },
];

static FX_DELAY: [ParamSpec; 3] = [
    ParamSpec { key: "time", label: "Time", min: 0.0, max: 5.0, def: 2.0, curve: Curve::Linear, int: true, steps: Some(&["1/32", "1/16", "1/8", "3/16", "1/4", "1/2"]), rate_switch: false },
    ParamSpec { key: "fb", label: "Feedback", min: 0.0, max: 0.85, def: 0.35, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 0.35, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_REVERB: [ParamSpec; 2] = [
    ParamSpec { key: "size", label: "Size", min: 0.2, max: 10.0, def: 2.2, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 0.3, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_PLATE: [ParamSpec; 4] = [
    ParamSpec { key: "decay", label: "Decay", min: 0.3, max: 8.0, def: 1.8, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "predelay", label: "PreDelay", min: 0.0, max: 0.2, def: 0.02, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "damp", label: "Damp", min: 500.0, max: 16000.0, def: 6000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 0.3, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_EQ7: [ParamSpec; 21] = [
    ParamSpec { key: "b0_freq", label: "LoShelf Freq", min: 20.0, max: 20000.0, def: 60.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b0_gain", label: "LoShelf Gain", min: -15.0, max: 15.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b0_q", label: "LoShelf Q", min: 0.3, max: 12.0, def: 1.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b1_freq", label: "Band 2 Freq", min: 20.0, max: 20000.0, def: 150.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b1_gain", label: "Band 2 Gain", min: -15.0, max: 15.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b1_q", label: "Band 2 Q", min: 0.3, max: 12.0, def: 1.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b2_freq", label: "Band 3 Freq", min: 20.0, max: 20000.0, def: 400.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b2_gain", label: "Band 3 Gain", min: -15.0, max: 15.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b2_q", label: "Band 3 Q", min: 0.3, max: 12.0, def: 1.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b3_freq", label: "Band 4 Freq", min: 20.0, max: 20000.0, def: 1000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b3_gain", label: "Band 4 Gain", min: -15.0, max: 15.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b3_q", label: "Band 4 Q", min: 0.3, max: 12.0, def: 1.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b4_freq", label: "Band 5 Freq", min: 20.0, max: 20000.0, def: 2500.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b4_gain", label: "Band 5 Gain", min: -15.0, max: 15.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b4_q", label: "Band 5 Q", min: 0.3, max: 12.0, def: 1.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b5_freq", label: "Band 6 Freq", min: 20.0, max: 20000.0, def: 6000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b5_gain", label: "Band 6 Gain", min: -15.0, max: 15.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b5_q", label: "Band 6 Q", min: 0.3, max: 12.0, def: 1.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b6_freq", label: "HiShelf Freq", min: 20.0, max: 20000.0, def: 12000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b6_gain", label: "HiShelf Gain", min: -15.0, max: 15.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b6_q", label: "HiShelf Q", min: 0.3, max: 12.0, def: 1.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
];

static FX_CHORUS: [ParamSpec; 3] = [
    ParamSpec { key: "rate", label: "Rate", min: 0.01, max: 300.0, def: 1.5, curve: Curve::Exp, int: false, steps: None, rate_switch: true },
    ParamSpec { key: "depth", label: "Depth", min: 0.0, max: 1.0, def: 0.5, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 0.5, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_DIST: [ParamSpec; 2] = [
    ParamSpec { key: "amt", label: "Drive", min: 0.0, max: 1.0, def: 0.4, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_CRUSH: [ParamSpec; 2] = [
    ParamSpec { key: "bits", label: "Bits", min: 1.0, max: 16.0, def: 8.0, curve: Curve::Linear, int: true, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_COMP: [ParamSpec; 4] = [
    ParamSpec { key: "thresh", label: "Thresh", min: -60.0, max: 0.0, def: -20.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "ratio", label: "Ratio", min: 1.0, max: 20.0, def: 4.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 0.3, def: 0.01, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.02, max: 1.0, def: 0.2, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
];

static FX_OPTO: [ParamSpec; 3] = [
    ParamSpec { key: "reduction", label: "Peak Redux", min: 0.0, max: 1.0, def: 0.4, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "gain", label: "Gain", min: -12.0, max: 24.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mode", label: "Mode", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Comp", "Limit"]), rate_switch: false },
];

static FX_MBCOMP: [ParamSpec; 14] = [
    ParamSpec { key: "mode", label: "Mode", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Comp", "Expand"]), rate_switch: false },
    ParamSpec { key: "xlo", label: "Lo×", min: 60.0, max: 1000.0, def: 250.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "xhi", label: "Hi×", min: 1000.0, max: 12000.0, def: 2500.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.001, max: 0.3, def: 0.02, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.02, max: 1.0, def: 0.18, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b0_thresh", label: "Lo Thr", min: -60.0, max: 0.0, def: -24.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b0_ratio", label: "Lo Ratio", min: 1.0, max: 20.0, def: 2.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b0_gain", label: "Lo Gain", min: -24.0, max: 24.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b1_thresh", label: "Mid Thr", min: -60.0, max: 0.0, def: -24.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b1_ratio", label: "Mid Ratio", min: 1.0, max: 20.0, def: 2.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b1_gain", label: "Mid Gain", min: -24.0, max: 24.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b2_thresh", label: "Hi Thr", min: -60.0, max: 0.0, def: -24.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b2_ratio", label: "Hi Ratio", min: 1.0, max: 20.0, def: 2.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "b2_gain", label: "Hi Gain", min: -24.0, max: 24.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_GATE: [ParamSpec; 18] = [
    ParamSpec { key: "thresh", label: "Thresh", min: -80.0, max: 0.0, def: -40.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "up", label: "Up Thr", min: -40.0, max: 40.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "dn", label: "Dn Thr", min: -40.0, max: 40.0, def: -6.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "range", label: "Range", min: -100.0, max: 0.0, def: -60.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "ratio", label: "Ratio", min: 1.0, max: 20.0, def: 20.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "knee", label: "Knee", min: 0.0, max: 24.0, def: 3.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "attack", label: "Attack", min: 0.0001, max: 0.3, def: 0.002, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "hold", label: "Hold", min: 0.001, max: 2.0, def: 0.05, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "release", label: "Release", min: 0.005, max: 3.0, def: 0.15, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "look", label: "Look", min: 0.0, max: 10.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "hpf", label: "Key HP", min: 20.0, max: 2000.0, def: 20.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "lpf", label: "Key LP", min: 200.0, max: 20000.0, def: 20000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "key", label: "Key", min: 0.0, max: 4.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Stereo", "Left", "Right", "Mid", "Side"]), rate_switch: false },
    ParamSpec { key: "det", label: "Detect", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Peak", "RMS"]), rate_switch: false },
    ParamSpec { key: "listen", label: "Listen", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Off", "On"]), rate_switch: false },
    ParamSpec { key: "inverse", label: "Inverse", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Off", "On"]), rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "gain", label: "Output", min: -24.0, max: 24.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_PHASER: [ParamSpec; 3] = [
    ParamSpec { key: "rate", label: "Rate", min: 0.01, max: 300.0, def: 0.8, curve: Curve::Exp, int: false, steps: None, rate_switch: true },
    ParamSpec { key: "octaves", label: "Sweep", min: 1.0, max: 6.0, def: 3.0, curve: Curve::Linear, int: true, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 0.5, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_PINGPONG: [ParamSpec; 3] = [
    ParamSpec { key: "time", label: "Time", min: 0.0, max: 5.0, def: 2.0, curve: Curve::Linear, int: true, steps: Some(&["1/32", "1/16", "1/8", "3/16", "1/4", "1/2"]), rate_switch: false },
    ParamSpec { key: "fb", label: "Feedback", min: 0.0, max: 0.85, def: 0.4, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 0.3, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_AUTOFILT: [ParamSpec; 4] = [
    ParamSpec { key: "rate", label: "Rate", min: 0.01, max: 300.0, def: 1.0, curve: Curve::Exp, int: false, steps: None, rate_switch: true },
    ParamSpec { key: "depth", label: "Depth", min: 0.0, max: 1.0, def: 0.7, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "base", label: "Base", min: 80.0, max: 4000.0, def: 350.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_TREM: [ParamSpec; 3] = [
    ParamSpec { key: "rate", label: "Rate", min: 0.01, max: 300.0, def: 5.0, curve: Curve::Exp, int: false, steps: None, rate_switch: true },
    ParamSpec { key: "depth", label: "Depth", min: 0.0, max: 1.0, def: 0.6, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_AUTOPAN: [ParamSpec; 2] = [
    ParamSpec { key: "rate", label: "Rate", min: 0.01, max: 300.0, def: 1.5, curve: Curve::Exp, int: false, steps: None, rate_switch: true },
    ParamSpec { key: "depth", label: "Depth", min: 0.0, max: 1.0, def: 0.8, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_VIB: [ParamSpec; 3] = [
    ParamSpec { key: "rate", label: "Rate", min: 0.01, max: 300.0, def: 5.0, curve: Curve::Exp, int: false, steps: None, rate_switch: true },
    ParamSpec { key: "depth", label: "Depth", min: 0.0, max: 0.6, def: 0.15, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_CHEBY: [ParamSpec; 2] = [
    ParamSpec { key: "order", label: "Order", min: 2.0, max: 14.0, def: 3.0, curve: Curve::Linear, int: true, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 0.35, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_WIDEN: [ParamSpec; 1] = [
    ParamSpec { key: "width", label: "Width", min: 0.0, max: 1.0, def: 0.8, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_SHIFT: [ParamSpec; 2] = [
    ParamSpec { key: "amt", label: "Shift", min: -400.0, max: 400.0, def: 60.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 0.4, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_DUCK: [ParamSpec; 3] = [
    ParamSpec { key: "rate", label: "Rate", min: 0.0, max: 5.0, def: 1.0, curve: Curve::Linear, int: true, steps: Some(&["1/2", "1/4", "1/4T", "1/8", "1/8T", "1/16"]), rate_switch: false },
    ParamSpec { key: "amount", label: "Amount", min: 0.0, max: 1.0, def: 0.7, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "curve", label: "Release", min: 0.1, max: 1.0, def: 0.5, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static FX_AUTOTUNE: [ParamSpec; 4] = [
    ParamSpec { key: "amount", label: "Amount", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "speed", label: "Speed", min: 1.0, max: 200.0, def: 20.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "mode", label: "Snap", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Key", "Chr"]), rate_switch: false },
];

static FX_DESTROYER: [ParamSpec; 22] = [
    ParamSpec { key: "drive", label: "Drive", min: 0.0, max: 48.0, def: 18.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "flat", label: "Flatten", min: 0.0, max: 1.0, def: 0.3, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "curve", label: "Curve", min: 0.0, max: 5.0, def: 2.0, curve: Curve::Linear, int: true, steps: Some(&["Soft", "Hard", "Diode", "Tube", "Fold", "Rectify"]), rate_switch: false },
    ParamSpec { key: "bias", label: "Bias", min: -1.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "thresh", label: "Clip Lvl", min: -24.0, max: 0.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "route", label: "Routing", min: 0.0, max: 8.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["All Pre", "1 Pre 2 Post", "1 Pre 2 Side", "1 Post 2 Pre", "All Post", "1 Post 2 Side", "1 Side 2 Pre", "1 Side 2 Post", "All Side"]), rate_switch: false },
    ParamSpec { key: "f1type", label: "F1 Type", min: 0.0, max: 4.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Off", "LP", "BP", "HP", "BR"]), rate_switch: false },
    ParamSpec { key: "f1freq", label: "F1 Freq", min: 20.0, max: 20000.0, def: 800.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "f1res", label: "F1 Res", min: 0.0, max: 1.0, def: 0.2, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "f1vol", label: "F1 Vol", min: -24.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "f2type", label: "F2 Type", min: 0.0, max: 4.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Off", "LP", "BP", "HP", "BR"]), rate_switch: false },
    ParamSpec { key: "f2freq", label: "F2 Freq", min: 20.0, max: 20000.0, def: 3000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "f2res", label: "F2 Res", min: 0.0, max: 1.0, def: 0.2, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "f2vol", label: "F2 Vol", min: -24.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "f3type", label: "Tone Type", min: 0.0, max: 4.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Off", "LP", "BP", "HP", "BR"]), rate_switch: false },
    ParamSpec { key: "f3freq", label: "Tone Freq", min: 20.0, max: 20000.0, def: 8000.0, curve: Curve::Exp, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "f3res", label: "Tone Res", min: 0.0, max: 1.0, def: 0.1, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "f3vol", label: "Tone Vol", min: -24.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "os", label: "Quality", min: 0.0, max: 2.0, def: 1.0, curve: Curve::Linear, int: true, steps: Some(&["1x", "2x", "4x"]), rate_switch: false },
    ParamSpec { key: "limit", label: "Limiter", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: true, steps: Some(&["Off", "On"]), rate_switch: false },
    ParamSpec { key: "mix", label: "Mix", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "gain", label: "Output", min: -24.0, max: 12.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

/// Every effect type, in schema.ts order.
pub static EFFECTS: [(&str, &[ParamSpec]); 25] = [
    ("eq", &FX_EQ),
    ("filter", &FX_FILTER),
    ("delay", &FX_DELAY),
    ("reverb", &FX_REVERB),
    ("plate", &FX_PLATE),
    ("eq7", &FX_EQ7),
    ("chorus", &FX_CHORUS),
    ("dist", &FX_DIST),
    ("crush", &FX_CRUSH),
    ("comp", &FX_COMP),
    ("opto", &FX_OPTO),
    ("mbcomp", &FX_MBCOMP),
    ("gate", &FX_GATE),
    ("phaser", &FX_PHASER),
    ("pingpong", &FX_PINGPONG),
    ("autofilt", &FX_AUTOFILT),
    ("trem", &FX_TREM),
    ("autopan", &FX_AUTOPAN),
    ("vib", &FX_VIB),
    ("cheby", &FX_CHEBY),
    ("widen", &FX_WIDEN),
    ("shift", &FX_SHIFT),
    ("duck", &FX_DUCK),
    ("autotune", &FX_AUTOTUNE),
    ("destroyer", &FX_DESTROYER),
];

static MIDIFX_SCALE: [ParamSpec; 0] = [
];

static MIDIFX_CHORD: [ParamSpec; 3] = [
    ParamSpec { key: "i1", label: "+ Semi 1", min: -12.0, max: 24.0, def: 4.0, curve: Curve::Linear, int: true, steps: None, rate_switch: false },
    ParamSpec { key: "i2", label: "+ Semi 2", min: -12.0, max: 24.0, def: 7.0, curve: Curve::Linear, int: true, steps: None, rate_switch: false },
    ParamSpec { key: "i3", label: "+ Semi 3", min: -12.0, max: 24.0, def: 0.0, curve: Curve::Linear, int: true, steps: None, rate_switch: false },
];

static MIDIFX_ARP: [ParamSpec; 5] = [
    ParamSpec { key: "rate", label: "Rate", min: 0.0, max: 5.0, def: 3.0, curve: Curve::Linear, int: true, steps: Some(&["1/4", "1/8", "1/8T", "1/16", "1/16T", "1/32"]), rate_switch: false },
    ParamSpec { key: "mode", label: "Mode", min: 0.0, max: 3.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Up", "Down", "Up-Dn", "Rand"]), rate_switch: false },
    ParamSpec { key: "pat", label: "Pattern", min: 0.0, max: 3.0, def: 0.0, curve: Curve::Linear, int: true, steps: Some(&["Octave", "5ths", "Scale", "Select"]), rate_switch: false },
    ParamSpec { key: "oct", label: "Octaves", min: 1.0, max: 4.0, def: 1.0, curve: Curve::Linear, int: true, steps: None, rate_switch: false },
    ParamSpec { key: "gate", label: "Gate", min: 0.1, max: 1.0, def: 0.8, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static MIDIFX_VELO: [ParamSpec; 2] = [
    ParamSpec { key: "scale", label: "Scale", min: 0.0, max: 2.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "rand", label: "Random", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

static MIDIFX_RAND: [ParamSpec; 2] = [
    ParamSpec { key: "chance", label: "Chance", min: 0.0, max: 1.0, def: 1.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
    ParamSpec { key: "octave", label: "Oct Jump", min: 0.0, max: 1.0, def: 0.0, curve: Curve::Linear, int: false, steps: None, rate_switch: false },
];

/// Every MIDI fx type, in schema.ts order.
pub static MIDI_FX: [(&str, &[ParamSpec]); 5] = [
    ("scale", &MIDIFX_SCALE),
    ("chord", &MIDIFX_CHORD),
    ("arp", &MIDIFX_ARP),
    ("velo", &MIDIFX_VELO),
    ("rand", &MIDIFX_RAND),
];

fn lookup(table: &'static [(&'static str, &'static [ParamSpec])], ty: &str) -> &'static [ParamSpec] {
    // Unknown types fall back to the first entry, mirroring schema.ts
    // instSchema()/fxSchema()/midiFxSchema().
    table
        .iter()
        .find(|(t, _)| *t == ty)
        .unwrap_or(&table[0])
        .1
}

/// Param specs for an instrument type (schema.ts `instSchema(type).params`).
pub fn inst_params(ty: &str) -> &'static [ParamSpec] {
    lookup(&INSTRUMENTS, ty)
}

/// Param specs for an effect type (schema.ts `fxSchema(type).params`).
pub fn fx_params(ty: &str) -> &'static [ParamSpec] {
    lookup(&EFFECTS, ty)
}

/// Param specs for a MIDI fx type (schema.ts `midiFxSchema(type).params`).
pub fn midifx_params(ty: &str) -> &'static [ParamSpec] {
    lookup(&MIDI_FX, ty)
}

/// `(key, default)` pairs for a spec list (schema.ts `defaultsFor`).
pub fn default_params(
    specs: &'static [ParamSpec],
) -> impl Iterator<Item = (&'static str, f32)> {
    specs.iter().map(|s| (s.key, s.def))
}
