// Pure data: parameter specs for every instrument & effect module.
// The device rack UI, preset system and audio factories all read from here,
// so adding a module = add a spec + a factory. No tone imports allowed.

export type ParamSpec = {
  key: string
  label: string
  min: number
  max: number
  def: number
  exp?: boolean            // exponential knob response
  int?: boolean            // integer steps
  steps?: string[]         // enum knob: value is an index into steps
  rateSwitch?: boolean     // Rate knob with Sync / Low / High modes (see RATE_* specs)
  fmt?: (v: number) => string
}

export const fmtHz = (v: number) => (v >= 1000 ? `${(v / 1000).toFixed(1)}k` : `${Math.round(v)}Hz`)
// Low-mode rate: 2 decimals below 10Hz, 1 decimal up to 300Hz
export const fmtRate = (v: number) => `${v < 10 ? v.toFixed(2) : v.toFixed(1)}Hz`
export const fmtSec = (v: number) => (v < 1 ? `${Math.round(v * 1000)}ms` : `${v.toFixed(2)}s`)
// Sub-millisecond resolution (gate attack goes down to 0.1 ms, where fmtSec reads "0ms").
export const fmtSecFine = (v: number) => (v < 0.01 ? `${(v * 1000).toFixed(1)}ms` : fmtSec(v))
export const fmtDb = (v: number) => `${v > 0 ? '+' : ''}${v.toFixed(1)}dB`
export const fmtPct = (v: number) => `${Math.round(v * 100)}%`
export const fmtSemi = (v: number) => `${v > 0 ? '+' : ''}${Math.round(v)}st`
export const fmtX = (v: number) => `${v.toFixed(2)}x`

// ---- value ↔ position mapping --------------------------------------------
// Single source of truth shared by knobs, automation lanes and the engine, so
// all three agree. `exp` params (frequency, time, LFO rate) map on a LOG curve:
// the perceptual midpoint sits at the geometric mean, so e.g. a filter-cutoff
// automation lane is logarithmic in its vertical scale, matching the knob.
const clamp01 = (n: number) => (n < 0 ? 0 : n > 1 ? 1 : n)
/** True when a spec uses a logarithmic curve (needs min/max > 0 for the log). */
export function isLogSpec(s: ParamSpec): boolean { return !!s.exp && s.min > 0 && s.max > 0 }

/** Raw value → normalized [0,1] along the spec's curve (log for `exp` params). */
export function normFromSpec(s: ParamSpec, v: number): number {
  if (s.max === s.min) return 0
  if (isLogSpec(s)) return clamp01((Math.log(v) - Math.log(s.min)) / (Math.log(s.max) - Math.log(s.min)))
  return clamp01((v - s.min) / (s.max - s.min))
}

/** Normalized [0,1] → raw value along the spec's curve (log for `exp` params). */
export function valueFromSpec(s: ParamSpec, n: number): number {
  const c = clamp01(n)
  if (isLogSpec(s)) return Math.exp(Math.log(s.min) + c * (Math.log(s.max) - Math.log(s.min)))
  return s.min + c * (s.max - s.min)
}

const adsr = (a: number, d: number, s: number, r: number): ParamSpec[] => [
  { key: 'attack', label: 'Attack', min: 0.001, max: 2, def: a, exp: true, fmt: fmtSec },
  { key: 'decay', label: 'Decay', min: 0.01, max: 2, def: d, exp: true, fmt: fmtSec },
  { key: 'sustain', label: 'Sustain', min: 0, max: 1, def: s, fmt: fmtPct },
  { key: 'release', label: 'Release', min: 0.01, max: 4, def: r, exp: true, fmt: fmtSec },
]

// Built-in per-synth LFO (wired to the filter cutoff in devices.ts). Amount 0
// keeps it silent AND stopped, so existing projects sound unchanged.
export const INST_LFO_SHAPES = ['Sin', 'Tri', 'Saw', 'Sqr']
const instLfo = (): ParamSpec[] => [
  { key: 'lfoShape', label: 'LFO', min: 0, max: INST_LFO_SHAPES.length - 1, def: 0, int: true, steps: INST_LFO_SHAPES },
  { key: 'lfoRate', label: 'LFO Rate', min: 0.05, max: 16, def: 2, exp: true, fmt: v => `${v.toFixed(2)}Hz` },
  { key: 'lfoAmt', label: 'LFO Amt', min: 0, max: 1, def: 0, fmt: fmtPct },
]

export const WAVES = ['sawtooth', 'square', 'triangle', 'sine', 'fatsawtooth', 'fatsquare', 'fattriangle']
const WAVE_LABELS = ['Saw', 'Sqr', 'Tri', 'Sin', 'FatSaw', 'FatSqr', 'FatTri']

export const NOTE_NAMES = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B']

export const DELAY_DIVS = ['1/32', '1/16', '1/8', '3/16', '1/4', '1/2']
export const DELAY_FRACTIONS = [1 / 32, 1 / 16, 1 / 8, 3 / 16, 1 / 4, 1 / 2] // of a whole note

export const DUCK_DIVS = ['1/2', '1/4', '1/4T', '1/8', '1/8T', '1/16']
export const DUCK_DIV_TICKS = [192, 96, 64, 48, 32, 24] // PPQ=96 ⇒ bar=384

// ----------------- instruments -----------------
export type InstrumentSchema = { type: string; label: string; icon: string; params: ParamSpec[] }

const PAD_NAMES = ['Kick', 'Snare', 'Clap', 'Cl Hat', 'Op Hat', 'Lo Tom', 'Perc', 'Crash']
const PAD_DECAYS = [0.42, 0.18, 0.28, 0.06, 0.4, 0.32, 0.12, 1.3]

function drumParams(): ParamSpec[] {
  const out: ParamSpec[] = []
  PAD_NAMES.forEach((name, i) => {
    out.push(
      { key: `p${i}_tune`, label: `${name} Tune`, min: -12, max: 12, def: 0, fmt: fmtSemi },
      { key: `p${i}_decay`, label: `${name} Decay`, min: 0.03, max: 2, def: PAD_DECAYS[i], exp: true, fmt: fmtSec },
      { key: `p${i}_level`, label: `${name} Level`, min: -24, max: 6, def: 0, fmt: fmtDb },
    )
  })
  return out
}

export const INSTRUMENTS: InstrumentSchema[] = [
  {
    type: 'poly', label: 'Analog Poly', icon: 'wave',
    params: [
      { key: 'wave', label: 'Wave', min: 0, max: WAVE_LABELS.length - 1, def: 0, int: true, steps: WAVE_LABELS },
      { key: 'spread', label: 'Spread', min: 0, max: 60, def: 18, fmt: v => `${Math.round(v)}ct` },
      { key: 'cutoff', label: 'Cutoff', min: 80, max: 14000, def: 7000, exp: true, fmt: fmtHz },
      { key: 'res', label: 'Res', min: 0, max: 10, def: 0.7, fmt: v => v.toFixed(1) },
      { key: 'slope', label: 'Slope', min: 0, max: 2, def: 0, int: true, steps: ['12dB', '24dB', '48dB'] },
      ...adsr(0.01, 0.15, 0.6, 0.4),
      ...instLfo(),
    ],
  },
  {
    type: 'duo', label: 'Duo Thick', icon: 'duo',
    params: [
      { key: 'harm', label: 'Interval', min: 0.5, max: 3, def: 1.5, fmt: fmtX },
      { key: 'vibAmt', label: 'Vibrato', min: 0, max: 0.6, def: 0.12, fmt: fmtPct },
      { key: 'vibRate', label: 'Vib Rate', min: 0.5, max: 10, def: 4.5, exp: true, fmt: v => `${v.toFixed(1)}Hz` },
      ...adsr(0.02, 0.3, 0.6, 0.6),
    ],
  },
  {
    type: 'fm', label: 'FM Synth', icon: 'bell',
    params: [
      { key: 'harm', label: 'Ratio', min: 0.25, max: 8, def: 3, fmt: fmtX },
      { key: 'modIdx', label: 'FM Amt', min: 0.5, max: 40, def: 10, exp: true, fmt: v => v.toFixed(1) },
      ...adsr(0.005, 0.3, 0.4, 0.6),
    ],
  },
  {
    type: 'mono', label: 'Mono Bass', icon: 'bass',
    params: [
      { key: 'wave', label: 'Wave', min: 0, max: 3, def: 0, int: true, steps: WAVE_LABELS },
      { key: 'cutoff', label: 'Cutoff', min: 40, max: 8000, def: 900, exp: true, fmt: fmtHz },
      { key: 'res', label: 'Res', min: 0, max: 10, def: 2, fmt: v => v.toFixed(1) },
      // def 1 = 24dB: MonoSynth's filter has always run at -24 rolloff
      { key: 'slope', label: 'Slope', min: 0, max: 2, def: 1, int: true, steps: ['12dB', '24dB', '48dB'] },
      { key: 'envAmt', label: 'Env Amt', min: 0, max: 6, def: 2.5, fmt: v => v.toFixed(1) },
      { key: 'glide', label: 'Glide', min: 0, max: 0.3, def: 0.02, exp: false, fmt: fmtSec },
      ...adsr(0.003, 0.2, 0.45, 0.25),
      ...instLfo(),
    ],
  },
  {
    type: 'pluck', label: 'Pluck', icon: 'pluck',
    params: [
      { key: 'dampen', label: 'Tone', min: 500, max: 12000, def: 4000, exp: true, fmt: fmtHz },
      { key: 'res', label: 'Sustain', min: 0.3, max: 0.98, def: 0.93, fmt: fmtPct },
    ],
  },
  {
    type: 'keys', label: 'Dream Keys', icon: 'keys',
    params: [
      { key: 'harm', label: 'Color', min: 0.5, max: 4, def: 2, fmt: fmtX },
      ...adsr(0.01, 0.4, 0.5, 0.8),
    ],
  },
  {
    type: 'sampler', label: 'Sampler', icon: 'sampler',
    params: [
      { key: 'tune', label: 'Tune', min: -24, max: 24, def: 0, fmt: fmtSemi },
      { key: 'attack', label: 'Attack', min: 0.001, max: 1, def: 0.005, exp: true, fmt: fmtSec },
      { key: 'release', label: 'Release', min: 0.01, max: 3, def: 0.4, exp: true, fmt: fmtSec },
    ],
  },
  {
    // Musical, keyboard-based single-sample instrument (one sound pitched across
    // the keyboard). `root` is the note the loaded sample sounds at unpitched.
    type: 'ksampler', label: 'Key Sampler', icon: 'sampler',
    params: [
      { key: 'root', label: 'Root', min: 0, max: 11, def: 0, int: true, steps: NOTE_NAMES },
      { key: 'tune', label: 'Fine', min: -24, max: 24, def: 0, fmt: fmtSemi },
      { key: 'attack', label: 'Attack', min: 0.001, max: 2, def: 0.004, exp: true, fmt: fmtSec },
      { key: 'release', label: 'Release', min: 0.01, max: 4, def: 0.4, exp: true, fmt: fmtSec },
    ],
  },
  {
    // Granular synthesis engine: a cloud of short sample "grains" pitched by the
    // keyboard. Parameter set mirrors the modern granular standard (Granulator/
    // Quanta/Portal): size, density, position + spray, pitch + jitter, reverse
    // probability, grain window shape, stereo spread, and an amp envelope.
    type: 'granular', label: 'Granular', icon: 'grain',
    params: [
      { key: 'size', label: 'Grain', min: 0.02, max: 0.5, def: 0.12, exp: true, fmt: fmtSec },
      { key: 'dens', label: 'Density', min: 2, max: 80, def: 18, exp: true, fmt: v => `${v.toFixed(0)}/s` },
      { key: 'pos', label: 'Position', min: 0, max: 1, def: 0.15, fmt: fmtPct },
      { key: 'spray', label: 'Spray', min: 0, max: 1, def: 0.05, fmt: fmtPct },
      { key: 'pitch', label: 'Pitch', min: -24, max: 24, def: 0, fmt: fmtSemi },
      { key: 'pjit', label: 'P.Jitter', min: 0, max: 12, def: 0, fmt: v => `${v.toFixed(1)}st` },
      { key: 'rev', label: 'Reverse', min: 0, max: 1, def: 0, fmt: fmtPct },
      { key: 'shape', label: 'Shape', min: 0, max: 1, def: 0.5, fmt: fmtPct },
      { key: 'spread', label: 'Spread', min: 0, max: 1, def: 0.6, fmt: fmtPct },
      { key: 'attack', label: 'Attack', min: 0.001, max: 2, def: 0.05, exp: true, fmt: fmtSec },
      { key: 'release', label: 'Release', min: 0.01, max: 4, def: 0.4, exp: true, fmt: fmtSec },
    ],
  },
  { type: 'drum', label: 'Drum Kit', icon: 'drum', params: drumParams() },
  { type: 'audiobus', label: 'Audio', icon: 'sampler', params: [] },
]

// ----------------- effects -----------------
// `group` is UI-only: it buckets the add-effect menu into submenus so the list
// of 23 doesn't arrive as one wall. It is NOT part of the project schema (the
// Rust registry dump reads type + params only) — but it is required, so a new
// effect can't be added without deciding where in the menu it belongs.
//   Dynamics     — level over time (compression, ducking)
//   Equalisation — level by frequency (EQs, filters)
//   Harmonic     — adds or moves frequency content (drive, bitcrush, pitch)
//   Temporal     — delay- or phase-based, incl. LFO-modulated (verb, echo, mod)
//   Other        — everything else (stereo utilities)
export const FX_GROUPS = ['Dynamics', 'Equalisation', 'Harmonic', 'Temporal', 'Other'] as const
export type FxGroup = (typeof FX_GROUPS)[number]

export type EffectSchema = { type: string; label: string; icon: string; group: FxGroup; params: ParamSpec[] }

// 7-band parametric EQ: band 0 = low shelf, 1–5 = peaking, 6 = high shelf.
// Flat b{i}_* keys (like the drum kit / multiband) so presets, automation,
// macros and LFOs address every band with zero extra plumbing.
export const EQ7_FREQS = [60, 150, 400, 1000, 2500, 6000, 12000]
export const EQ7_BANDS = EQ7_FREQS.length
export function eq7BandType(i: number): 'lowshelf' | 'peaking' | 'highshelf' {
  return i === 0 ? 'lowshelf' : i === EQ7_BANDS - 1 ? 'highshelf' : 'peaking'
}
function eq7Params(): ParamSpec[] {
  const out: ParamSpec[] = []
  EQ7_FREQS.forEach((f, i) => {
    const name = i === 0 ? 'LoShelf' : i === EQ7_BANDS - 1 ? 'HiShelf' : `Band ${i + 1}`
    out.push(
      { key: `b${i}_freq`, label: `${name} Freq`, min: 20, max: 20000, def: f, exp: true, fmt: fmtHz },
      { key: `b${i}_gain`, label: `${name} Gain`, min: -15, max: 15, def: 0, fmt: fmtDb },
      { key: `b${i}_q`, label: `${name} Q`, min: 0.3, max: 12, def: 1, exp: true, fmt: v => v.toFixed(2) },
    )
  })
  return out
}

export const EFFECTS: EffectSchema[] = [
  {
    type: 'eq', label: 'EQ Three', icon: 'eq', group: 'Equalisation',
    params: [
      { key: 'low', label: 'Low', min: -12, max: 12, def: 0, fmt: fmtDb },
      { key: 'mid', label: 'Mid', min: -12, max: 12, def: 0, fmt: fmtDb },
      { key: 'high', label: 'High', min: -12, max: 12, def: 0, fmt: fmtDb },
    ],
  },
  {
    type: 'filter', label: 'Filter', icon: 'filter', group: 'Equalisation',
    params: [
      { key: 'ftype', label: 'Type', min: 0, max: 2, def: 0, int: true, steps: ['LP', 'HP', 'BP'] },
      { key: 'freq', label: 'Freq', min: 40, max: 18000, def: 2000, exp: true, fmt: fmtHz },
      { key: 'q', label: 'Res', min: 0, max: 12, def: 1, fmt: v => v.toFixed(1) },
      // Web Audio biquads only do 12/24/48 dB/oct — def 0 (12dB) keeps the old sound.
      { key: 'slope', label: 'Slope', min: 0, max: 2, def: 0, int: true, steps: ['12dB', '24dB', '48dB'] },
    ],
  },
  {
    type: 'delay', label: 'Echo', icon: 'echo', group: 'Temporal',
    params: [
      { key: 'time', label: 'Time', min: 0, max: DELAY_DIVS.length - 1, def: 2, int: true, steps: DELAY_DIVS },
      { key: 'fb', label: 'Feedback', min: 0, max: 0.85, def: 0.35, fmt: fmtPct },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 0.35, fmt: fmtPct },
    ],
  },
  {
    type: 'reverb', label: 'Reverb', icon: 'reverb', group: 'Temporal',
    params: [
      { key: 'size', label: 'Size', min: 0.2, max: 10, def: 2.2, exp: true, fmt: fmtSec },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 0.3, fmt: fmtPct },
    ],
  },
  {
    // Plate model: instantly-dense synthesized plate IR through a convolver,
    // with pre-delay and a damping filter on the wet path (see devices.ts).
    type: 'plate', label: 'Plate Verb', icon: 'plate', group: 'Temporal',
    params: [
      { key: 'decay', label: 'Decay', min: 0.3, max: 8, def: 1.8, exp: true, fmt: fmtSec },
      { key: 'predelay', label: 'PreDelay', min: 0, max: 0.2, def: 0.02, fmt: v => `${Math.round(v * 1000)}ms` },
      { key: 'damp', label: 'Damp', min: 500, max: 16000, def: 6000, exp: true, fmt: fmtHz },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 0.3, fmt: fmtPct },
    ],
  },
  {
    // 7-band parametric EQ: low shelf, 5 peaking bands, high shelf. Custom
    // graphic card (draggable dots + response curve + RTA) in DeviceRack.
    type: 'eq7', label: 'EQ Seven', icon: 'eq', group: 'Equalisation',
    params: eq7Params(),
  },
  {
    type: 'chorus', label: 'Chorus', icon: 'chorus', group: 'Temporal',
    params: [
      { key: 'rate', label: 'Rate', min: 0.01, max: 300, def: 1.5, exp: true, rateSwitch: true, fmt: fmtRate },
      { key: 'depth', label: 'Depth', min: 0, max: 1, def: 0.5, fmt: fmtPct },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 0.5, fmt: fmtPct },
    ],
  },
  {
    type: 'dist', label: 'Drive', icon: 'bolt', group: 'Harmonic',
    params: [
      { key: 'amt', label: 'Drive', min: 0, max: 1, def: 0.4, fmt: fmtPct },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 1, fmt: fmtPct },
    ],
  },
  {
    type: 'crush', label: 'Crusher', icon: 'crush', group: 'Harmonic',
    params: [
      { key: 'bits', label: 'Bits', min: 1, max: 16, def: 8, int: true, fmt: v => `${Math.round(v)}bit` },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 1, fmt: fmtPct },
    ],
  },
  {
    type: 'comp', label: 'Compressor', icon: 'comp', group: 'Dynamics',
    params: [
      { key: 'thresh', label: 'Thresh', min: -60, max: 0, def: -20, fmt: fmtDb },
      { key: 'ratio', label: 'Ratio', min: 1, max: 20, def: 4, fmt: v => `${v.toFixed(0)}:1` },
      { key: 'attack', label: 'Attack', min: 0.001, max: 0.3, def: 0.01, exp: true, fmt: fmtSec },
      { key: 'release', label: 'Release', min: 0.02, max: 1, def: 0.2, exp: true, fmt: fmtSec },
    ],
  },
  {
    // LA-2A-style optical compressor: soft-knee, gentle program-dependent
    // levelling. Just Peak Reduction + makeup Gain + a Comp/Limit switch, like
    // the hardware (ratio/attack/release are fixed by the "opto cell" model).
    type: 'opto', label: 'Opto Comp', icon: 'opto', group: 'Dynamics',
    params: [
      { key: 'reduction', label: 'Peak Redux', min: 0, max: 1, def: 0.4, fmt: fmtPct },
      { key: 'gain', label: 'Gain', min: -12, max: 24, def: 0, fmt: fmtDb },
      { key: 'mode', label: 'Mode', min: 0, max: 1, def: 0, int: true, steps: ['Comp', 'Limit'] },
    ],
  },
  {
    // 3-band multiband dynamics with a Comp ⇄ Expand switch. Custom graphical UI
    // (per-band threshold/ratio handles + GR meters) in DeviceRack; the engine
    // runs it as a single AudioWorklet node (sf-mbdyn). Band params are flat keys
    // (b{i}_*) like the drum kit, so presets/automation/macros all just work.
    type: 'mbcomp', label: 'Multiband', icon: 'mbcomp', group: 'Dynamics',
    params: [
      { key: 'mode', label: 'Mode', min: 0, max: 1, def: 0, int: true, steps: ['Comp', 'Expand'] },
      { key: 'xlo', label: 'Lo×', min: 60, max: 1000, def: 250, exp: true, fmt: fmtHz },
      { key: 'xhi', label: 'Hi×', min: 1000, max: 12000, def: 2500, exp: true, fmt: fmtHz },
      { key: 'attack', label: 'Attack', min: 0.001, max: 0.3, def: 0.02, exp: true, fmt: fmtSec },
      { key: 'release', label: 'Release', min: 0.02, max: 1, def: 0.18, exp: true, fmt: fmtSec },
      { key: 'b0_thresh', label: 'Lo Thr', min: -60, max: 0, def: -24, fmt: fmtDb },
      { key: 'b0_ratio', label: 'Lo Ratio', min: 1, max: 20, def: 2, fmt: v => `${v.toFixed(1)}:1` },
      { key: 'b0_gain', label: 'Lo Gain', min: -24, max: 24, def: 0, fmt: fmtDb },
      { key: 'b1_thresh', label: 'Mid Thr', min: -60, max: 0, def: -24, fmt: fmtDb },
      { key: 'b1_ratio', label: 'Mid Ratio', min: 1, max: 20, def: 2, fmt: v => `${v.toFixed(1)}:1` },
      { key: 'b1_gain', label: 'Mid Gain', min: -24, max: 24, def: 0, fmt: fmtDb },
      { key: 'b2_thresh', label: 'Hi Thr', min: -60, max: 0, def: -24, fmt: fmtDb },
      { key: 'b2_ratio', label: 'Hi Ratio', min: 1, max: 20, def: 2, fmt: v => `${v.toFixed(1)}:1` },
      { key: 'b2_gain', label: 'Hi Gain', min: -24, max: 24, def: 0, fmt: fmtDb },
    ],
  },
  {
    // Advanced gate / downward expander, one AudioWorklet node (sf-gate).
    //
    // Detector: key = a channel selection (Stereo/L/R/Mid/Side) through a
    // bandpass pair (hpf..lpf) — "listen" monitors that signal so the band can
    // be dialled by ear. Peak or RMS.
    //
    // Open/close use SEPARATE thresholds, both as dB offsets from `thresh`:
    // the gate opens when the key rises past thresh+up and closes when it
    // falls below thresh+dn. dn < up is classic hysteresis (no chatter);
    // up = dn = 0 is a plain expander; dn > up crosses the two, so the gate
    // closes on the way down at a level it would not yet have opened at on the
    // way up — it clips note sustains and re-opens on transients.
    //
    // While closed the signal is expanded downward by `ratio` (1:1 … 20:1,
    // where 20 reads "Gate" = infinite) and never pushed further than `range`.
    // `inverse` mirrors the key level about the threshold, so the gate passes
    // what is BELOW it and clamps what is above.
    type: 'gate', label: 'Gate', icon: 'gate', group: 'Dynamics',
    params: [
      { key: 'thresh', label: 'Thresh', min: -80, max: 0, def: -40, fmt: fmtDb },
      { key: 'up', label: 'Up Thr', min: -40, max: 40, def: 0, fmt: fmtDb },
      { key: 'dn', label: 'Dn Thr', min: -40, max: 40, def: -6, fmt: fmtDb },
      { key: 'range', label: 'Range', min: -100, max: 0, def: -60, fmt: fmtDb },
      { key: 'ratio', label: 'Ratio', min: 1, max: 20, def: 20, fmt: v => (v >= 19.95 ? 'Gate' : `${v.toFixed(1)}:1`) },
      { key: 'knee', label: 'Knee', min: 0, max: 24, def: 3, fmt: v => `${v.toFixed(1)}dB` },
      { key: 'attack', label: 'Attack', min: 0.0001, max: 0.3, def: 0.002, exp: true, fmt: fmtSecFine },
      { key: 'hold', label: 'Hold', min: 0.001, max: 2, def: 0.05, exp: true, fmt: fmtSecFine },
      { key: 'release', label: 'Release', min: 0.005, max: 3, def: 0.15, exp: true, fmt: fmtSecFine },
      { key: 'look', label: 'Look', min: 0, max: 10, def: 0, fmt: v => `${v.toFixed(1)}ms` },
      { key: 'hpf', label: 'Key HP', min: 20, max: 2000, def: 20, exp: true, fmt: fmtHz },
      { key: 'lpf', label: 'Key LP', min: 200, max: 20000, def: 20000, exp: true, fmt: fmtHz },
      { key: 'key', label: 'Key', min: 0, max: 4, def: 0, int: true, steps: ['Stereo', 'Left', 'Right', 'Mid', 'Side'] },
      { key: 'det', label: 'Detect', min: 0, max: 1, def: 0, int: true, steps: ['Peak', 'RMS'] },
      { key: 'listen', label: 'Listen', min: 0, max: 1, def: 0, int: true, steps: ['Off', 'On'] },
      { key: 'inverse', label: 'Inverse', min: 0, max: 1, def: 0, int: true, steps: ['Off', 'On'] },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 1, fmt: fmtPct },
      { key: 'gain', label: 'Output', min: -24, max: 24, def: 0, fmt: fmtDb },
    ],
  },
  {
    type: 'phaser', label: 'Phaser', icon: 'phaser', group: 'Temporal',
    params: [
      { key: 'rate', label: 'Rate', min: 0.01, max: 300, def: 0.8, exp: true, rateSwitch: true, fmt: fmtRate },
      { key: 'octaves', label: 'Sweep', min: 1, max: 6, def: 3, int: true, fmt: v => `${Math.round(v)}oct` },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 0.5, fmt: fmtPct },
    ],
  },
  {
    type: 'pingpong', label: 'Ping Pong', icon: 'pingpong', group: 'Temporal',
    params: [
      { key: 'time', label: 'Time', min: 0, max: DELAY_DIVS.length - 1, def: 2, int: true, steps: DELAY_DIVS },
      { key: 'fb', label: 'Feedback', min: 0, max: 0.85, def: 0.4, fmt: fmtPct },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 0.3, fmt: fmtPct },
    ],
  },
  {
    type: 'autofilt', label: 'Auto Filter', icon: 'autofilt', group: 'Equalisation',
    params: [
      { key: 'rate', label: 'Rate', min: 0.01, max: 300, def: 1, exp: true, rateSwitch: true, fmt: fmtRate },
      { key: 'depth', label: 'Depth', min: 0, max: 1, def: 0.7, fmt: fmtPct },
      { key: 'base', label: 'Base', min: 80, max: 4000, def: 350, exp: true, fmt: fmtHz },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 1, fmt: fmtPct },
    ],
  },
  {
    type: 'trem', label: 'Tremolo', icon: 'trem', group: 'Temporal',
    params: [
      { key: 'rate', label: 'Rate', min: 0.01, max: 300, def: 5, exp: true, rateSwitch: true, fmt: fmtRate },
      { key: 'depth', label: 'Depth', min: 0, max: 1, def: 0.6, fmt: fmtPct },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 1, fmt: fmtPct },
    ],
  },
  {
    type: 'autopan', label: 'Auto Pan', icon: 'autopan', group: 'Temporal',
    params: [
      { key: 'rate', label: 'Rate', min: 0.01, max: 300, def: 1.5, exp: true, rateSwitch: true, fmt: fmtRate },
      { key: 'depth', label: 'Depth', min: 0, max: 1, def: 0.8, fmt: fmtPct },
    ],
  },
  {
    type: 'vib', label: 'Vibrato', icon: 'vib', group: 'Temporal',
    params: [
      { key: 'rate', label: 'Rate', min: 0.01, max: 300, def: 5, exp: true, rateSwitch: true, fmt: fmtRate },
      { key: 'depth', label: 'Depth', min: 0, max: 0.6, def: 0.15, fmt: fmtPct },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 1, fmt: fmtPct },
    ],
  },
  {
    type: 'cheby', label: 'Heat', icon: 'heat', group: 'Harmonic',
    params: [
      { key: 'order', label: 'Order', min: 2, max: 14, def: 3, int: true, fmt: v => `${Math.round(v)}` },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 0.35, fmt: fmtPct },
    ],
  },
  {
    type: 'widen', label: 'Widener', icon: 'widen', group: 'Other',
    params: [
      { key: 'width', label: 'Width', min: 0, max: 1, def: 0.8, fmt: fmtPct },
    ],
  },
  {
    type: 'shift', label: 'Freq Shift', icon: 'shift', group: 'Harmonic',
    params: [
      { key: 'amt', label: 'Shift', min: -400, max: 400, def: 60, fmt: v => `${Math.round(v)}Hz` },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 0.4, fmt: fmtPct },
    ],
  },
  {
    type: 'duck', label: 'Sidechain', icon: 'duck', group: 'Dynamics',
    params: [
      { key: 'rate', label: 'Rate', min: 0, max: DUCK_DIVS.length - 1, def: 1, int: true, steps: DUCK_DIVS },
      { key: 'amount', label: 'Amount', min: 0, max: 1, def: 0.7, fmt: fmtPct },
      { key: 'curve', label: 'Release', min: 0.1, max: 1, def: 0.5, fmt: fmtPct },
    ],
  },
  {
    type: 'autotune', label: 'Auto-Tune', icon: 'autotune', group: 'Harmonic',
    params: [
      { key: 'amount', label: 'Amount', min: 0, max: 1, def: 1, fmt: fmtPct },
      { key: 'speed', label: 'Speed', min: 1, max: 200, def: 20, exp: true, fmt: v => `${Math.round(v)}ms` },
      { key: 'mix', label: 'Mix', min: 0, max: 1, def: 1, fmt: fmtPct },
      { key: 'mode', label: 'Snap', min: 0, max: 1, def: 0, int: true, steps: ['Key', 'Chr'] },
    ],
  },
]

export function instSchema(type: string) {
  return INSTRUMENTS.find(i => i.type === type) ?? INSTRUMENTS[0]
}
export function fxSchema(type: string) {
  return EFFECTS.find(e => e.type === type) ?? EFFECTS[0]
}
export function fxByGroup(): { group: FxGroup; items: EffectSchema[] }[] {
  return FX_GROUPS
    .map(group => ({ group, items: EFFECTS.filter(e => e.group === group) }))
    .filter(g => g.items.length > 0)
}
export function defaultsFor(specs: ParamSpec[]): Record<string, number> {
  const out: Record<string, number> = {}
  specs.forEach(s => { out[s.key] = s.def })
  return out
}

// ----------------- LFO (modulation source) -----------------
// Modelled on Ableton's LFO device: a low-frequency oscillator that modulates
// a mapped parameter around its manual value. Shape + rate (tempo-synced or
// free Hz) + depth + phase. Runs locally per client (see engine modulation loop).

export const LFO_SHAPES = ['Sine', 'Triangle', 'Saw ↑', 'Saw ↓', 'Square', 'S&H', 'Random']

export const LFO_DIVS = ['8 bar', '4 bar', '2 bar', '1 bar', '1/2', '1/4', '1/8', '1/8T', '1/16']
// cycle length in transport ticks (PPQ=96 ⇒ quarter = 96, bar = 384)
export const LFO_DIV_TICKS = [8 * 384, 4 * 384, 2 * 384, 384, 192, 96, 48, 32, 24]

// ---- switchable Rate knobs (effects with `rateSwitch` on their rate spec) ----
// Mode lives in the fx params map as rateMode: 0 = Sync (tempo division, index
// in rateSync), 1 = Low (Hz in `rate`), 2 = High (Hz in `rateHi`). The schema's
// own `rate` spec doubles as the Low knob; these two cover the other modes.
export const RATE_MODES = ['Sync', 'Low', 'High']
export const RATE_SYNC_SPEC: ParamSpec = { key: 'rateSync', label: 'Rate', min: 0, max: LFO_DIVS.length - 1, def: 5, int: true, steps: LFO_DIVS }
export const RATE_HI_SPEC: ParamSpec = { key: 'rateHi', label: 'Rate', min: 301, max: 1000, def: 440, exp: true, int: true, fmt: v => `${Math.round(v)}Hz` }
/** Effective LFO frequency (Hz) for a rate-switchable effect's stored params. */
export function rateHzOf(get: (k: string) => number | undefined, bpm: number): number {
  const mode = (get('rateMode') ?? 1) | 0
  if (mode === 2) return Math.min(1000, Math.max(301, get('rateHi') ?? 440))
  if (mode === 0) {
    const ticks = LFO_DIV_TICKS[(get('rateSync') ?? 5) | 0] || 96
    return (Math.max(1, bpm) * 96) / (60 * ticks)   // 1 / (ticks · secPerTick)
  }
  return Math.min(300, Math.max(0.01, get('rate') ?? 1))
}

export const LFO_PARAMS: ParamSpec[] = [
  { key: 'depth', label: 'Depth', min: 0, max: 1, def: 0.5, fmt: fmtPct },
  { key: 'phase', label: 'Phase', min: 0, max: 1, def: 0, fmt: v => `${Math.round(v * 360)}°` },
  { key: 'hz', label: 'Rate', min: 0.01, max: 30, def: 1, exp: true, fmt: v => `${v.toFixed(2)}Hz` },
]

// Deterministic [0,1) hash so Sample&Hold / Random match across collaborators.
function lfoHash(n: number) {
  const x = Math.sin(n * 127.1 + 311.7) * 43758.5453
  return x - Math.floor(x)
}

/** LFO waveform value in [-1, 1] for a given shape and (unwrapped) phase. */
export function lfoShapeValue(shape: number, phase: number): number {
  const f = phase - Math.floor(phase) // [0,1)
  switch (shape) {
    case 1: return f < 0.5 ? 4 * f - 1 : 3 - 4 * f          // triangle
    case 2: return 2 * f - 1                                 // saw up
    case 3: return 1 - 2 * f                                 // saw down
    case 4: return f < 0.5 ? 1 : -1                          // square
    case 5: return lfoHash(Math.floor(phase)) * 2 - 1        // sample & hold
    case 6: {                                                // smooth random
      const a = lfoHash(Math.floor(phase))
      const b = lfoHash(Math.floor(phase) + 1)
      const t = f * f * (3 - 2 * f)                          // smoothstep
      return (a + (b - a) * t) * 2 - 1
    }
    default: return Math.sin(f * Math.PI * 2)                // sine
  }
}

// ----------------- mixer pseudo-params (automation / macro targets) -----------------
export const MIX_SPECS: ParamSpec[] = [
  { key: 'gain', label: 'Volume', min: -48, max: 6, def: 0, fmt: fmtDb },
  { key: 'pan', label: 'Pan', min: -1, max: 1, def: 0, fmt: v => Math.abs(v) < 0.02 ? 'C' : v < 0 ? `${Math.round(-v * 50)}L` : `${Math.round(v * 50)}R` },
]
export function mixSpec(key: string) { return MIX_SPECS.find(s => s.key === key) }

// ----------------- live MIDI effects -----------------
export type MidiFxSchema = { type: string; label: string; icon: string; params: ParamSpec[] }
export const MIDI_FX: MidiFxSchema[] = [
  { type: 'scale', label: 'Scale', icon: 'note', params: [] }, // forces project scale
  {
    type: 'chord', label: 'Chord', icon: 'chord', params: [
      { key: 'i1', label: '+ Semi 1', min: -12, max: 24, def: 4, int: true, fmt: fmtSemi },
      { key: 'i2', label: '+ Semi 2', min: -12, max: 24, def: 7, int: true, fmt: fmtSemi },
      { key: 'i3', label: '+ Semi 3', min: -12, max: 24, def: 0, int: true, fmt: fmtSemi },
    ],
  },
  {
    type: 'arp', label: 'Arp', icon: 'arpUp', params: [
      { key: 'rate', label: 'Rate', min: 0, max: 5, def: 3, int: true, steps: ['1/4', '1/8', '1/8T', '1/16', '1/16T', '1/32'] },
      { key: 'mode', label: 'Mode', min: 0, max: 3, def: 0, int: true, steps: ['Up', 'Down', 'Up-Dn', 'Rand'] },
      // Pattern = which pitches each held note generates (Select adds a piano
      // toggle grid, rendered by MidiFxCard, driven by the `sel` bitmask param).
      { key: 'pat', label: 'Pattern', min: 0, max: 3, def: 0, int: true, steps: ['Octave', '5ths', 'Scale', 'Select'] },
      { key: 'oct', label: 'Octaves', min: 1, max: 4, def: 1, int: true, fmt: v => `${Math.round(v)}` },
      { key: 'gate', label: 'Gate', min: 0.1, max: 1, def: 0.8, fmt: fmtPct },
    ],
  },
  {
    type: 'velo', label: 'Velocity', icon: 'rampUp', params: [
      { key: 'scale', label: 'Scale', min: 0, max: 2, def: 1, fmt: fmtPct },
      { key: 'rand', label: 'Random', min: 0, max: 1, def: 0, fmt: fmtPct },
    ],
  },
  {
    type: 'rand', label: 'Random', icon: 'dice', params: [
      { key: 'chance', label: 'Chance', min: 0, max: 1, def: 1, fmt: fmtPct },
      { key: 'octave', label: 'Oct Jump', min: 0, max: 1, def: 0, fmt: fmtPct },
    ],
  },
]
export function midiFxSchema(type: string) { return MIDI_FX.find(m => m.type === type) ?? MIDI_FX[0] }

export const ARP_DIV_TICKS = [96, 48, 32, 24, 16, 12]

// ----------------- follow actions -----------------
export const FOLLOW_ACTIONS = ['Next', 'Prev', 'First', 'Any', 'Random', 'Stop']
