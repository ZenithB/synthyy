// Tone.js implementations of every instrument & effect module.
// Factories build into whatever Tone context is current, so the same code
// powers live playback AND offline WAV rendering (engine swaps the context).
// The app runs its context at 2x sample rate (88.2kHz) so FM/waveshaping
// stay clean; Drive additionally oversamples its waveshaper 4x.

import * as Tone from 'tone'
import { WAVES, DELAY_FRACTIONS, DUCK_DIV_TICKS, EQ7_BANDS, EQ7_FREQS, eq7BandType, rateHzOf } from './schema'
import { snapToScale, getScale } from '../theory'
import { meta } from '../state/doc'
import { clamp } from '../types'

/** Project scale → root-relative pitch-class bitmask (bit n = degree n allowed). */
const scaleMask = (scaleId: string) => getScale(scaleId).ivs.reduce((m, iv) => m | (1 << (((iv % 12) + 12) % 12)), 0)

/**
 * True when an effect's params make it audibly transparent (its dry path is
 * bit-exact passthrough), so the engine can splice it out of the live chain
 * entirely — a wet-0 reverb still convolves, a mix-0 autotune still granulates.
 * Only types whose neutrality is statically decidable are listed; dynamics
 * (comp/opto/mbcomp), filter and the EQs depend on signal or are too cheap to
 * matter, so they never report neutral.
 */
export function fxNeutral(type: string, p: Record<string, number>): boolean {
  const z = (v: number | undefined) => (v ?? 0) <= 0.0001
  switch (type) {
    case 'delay': case 'reverb': case 'plate': case 'chorus': case 'dist':
    case 'crush': case 'phaser': case 'pingpong': case 'autofilt': case 'trem':
    case 'vib': case 'cheby': case 'shift': case 'autotune':
      return z(p.mix)
    case 'autopan': return z(p.depth)
    case 'duck': return z(p.amount)
    case 'widen': return Math.abs((p.width ?? 0.5) - 0.5) <= 0.0001
    default: return false
  }
}

// Monophonic pitch detector (YIN cumulative-mean-normalized difference) used by
// Auto-Tune. Returns the fundamental in Hz, or 0 if no clear pitch.
function detectPitchHz(buf: Float32Array, sr: number): number {
  const n = buf.length
  const maxTau = Math.min(n - 256, Math.floor(sr / 70))   // down to ~70 Hz
  const minTau = Math.max(2, Math.floor(sr / 1100))        // up to ~1100 Hz
  if (maxTau <= minTau + 2) return 0
  const cmnd = new Float32Array(maxTau)
  let running = 0
  for (let tau = minTau; tau < maxTau; tau++) {
    let sum = 0
    for (let i = 0; i < n - maxTau; i++) { const d = buf[i] - buf[i + tau]; sum += d * d }
    running += sum
    cmnd[tau] = running > 0 ? (sum * (tau - minTau + 1)) / running : 1
  }
  const thr = 0.15
  let tau = -1
  for (let t = minTau + 1; t < maxTau - 1; t++) {
    if (cmnd[t] < thr) { while (t + 1 < maxTau && cmnd[t + 1] < cmnd[t]) t++; tau = t; break }
  }
  if (tau < 0) {
    let best = 1, bt = -1
    for (let t = minTau; t < maxTau; t++) if (cmnd[t] < best) { best = cmnd[t]; bt = t }
    if (best > 0.4) return 0
    tau = bt
  }
  return tau > 0 ? sr / tau : 0
}

export type Inst = {
  out: Tone.ToneAudioNode
  set: (key: string, v: number) => void
  noteOn: (p: number, vel: number) => void
  noteOff: (p: number) => void
  trigger: (p: number, durSec: number, time: number, vel: number) => void
  dispose: () => void
}

// ---- switchable Rate knobs (Sync / Low / High) ----
// Effects with a rate-switchable knob store rateMode/rate/rateHi/rateSync in
// their params; the device re-derives its LFO frequency whenever one of those
// keys (or the tempo) changes. Live devices register a re-push callback so a
// bpm change retunes every synced rate; offline (export) devices skip the
// registry — the render sets the bpm before building and never changes it.
let rateBpm = 120
const rateFns = new Set<() => void>()
export function setRateBpm(bpm: number) {
  if (!bpm || bpm === rateBpm) return
  rateBpm = bpm
  rateFns.forEach(f => f())
}
function makeRate(p: Record<string, number>, apply: (hz: number) => void) {
  const st: Record<string, number> = {
    rateMode: p.rateMode ?? 1, rate: p.rate ?? 1, rateHi: p.rateHi ?? 440, rateSync: p.rateSync ?? 5,
  }
  const push = () => apply(rateHzOf(k => st[k], rateBpm))
  const offline = Tone.getContext().rawContext?.constructor?.name?.includes('Offline')
  if (!offline) rateFns.add(push)
  return {
    hz0: rateHzOf(k => st[k], rateBpm),
    /** Returns true when it consumed the key (one of the four rate fields). */
    set: (k: string, v: number): boolean => {
      if (!(k in st)) return false
      st[k] = v
      push()
      return true
    },
    dispose: () => rateFns.delete(push),
  }
}

export type Fx = {
  node: Tone.ToneAudioNode
  set: (key: string, v: number) => void
  // `time` is the audio-context time the tick applies to. It MUST be passed on
  // the offline render path: Tone.Offline restores the global context to the
  // LIVE one before awaiting the render, so Tone.now() there returns live
  // wall-clock — scheduling past the end of the bounce (the inert-duck bug).
  tick?: (posTicks: number, playing: boolean, bpm: number, time?: number) => void
  // Effects that need to analyse their *input* (e.g. Auto-Tune pitch detection)
  // expose a raw AnalyserNode; the engine taps the pre-effect signal into it.
  detect?: AnalyserNode
  // Live gain reduction in dB (≤0) for a GR meter — single value (comp/opto) or
  // per-band (multiband). Read each frame by the UI; absent on non-dynamics fx.
  gr?: () => number
  grBands?: () => number[]
  // Sidechain: fire the duck envelope at `when` (audio time; defaults to now).
  // The engine calls this from note events on the configured source track/pad.
  trigger?: (when?: number) => void
  dispose: () => void
}

const midiHz = (p: number) => Tone.Frequency(p, 'midi').toFrequency()

// Timestamp for notes the player triggers LIVE (MIDI/keyboard), right now.
// Tone's default ~100ms context `lookAhead` keeps *sequenced* clips jitter-free,
// but for a live keypress it's pure latency — there's no sequence to stay in sync
// with. So we schedule live notes a hair (5ms) ahead of the raw audio clock via
// `immediate()` (which excludes lookAhead) instead of `now()` (which adds it),
// taking key→sound from ~100ms down to roughly buffer-bound. The small 5ms margin
// avoids the "scheduled in the past" edge case while staying imperceptible.
// The scheduled `trigger()` path (clip playback) keeps its exact time untouched.
const liveTime = () => Tone.immediate() + 0.005

// ---------------- instruments ----------------

// slope index (12/24/48 dB) → biquad-cascade rolloff
const SLOPE_ROLLOFFS = [-12, -24, -48] as const
const slopeRolloff = (i: number) => SLOPE_ROLLOFFS[Math.max(0, Math.min(2, i | 0))]

// The synths' built-in LFO (schema instLfo): a Tone.LFO summed into the filter
// cutoff through a depth gain (±cutoff·amt). Stopped whenever amt ≈ 0, so it
// costs nothing until the LFO Amt knob comes up.
const INST_LFO_TYPES = ['sine', 'triangle', 'sawtooth', 'square'] as const
function makeInstLfo(p: Record<string, number>, freqParam: Tone.Signal<'frequency'> | Tone.Param<'frequency'>, cutoff0: number) {
  let cutoff = cutoff0
  let amt = p.lfoAmt ?? 0
  const lfo = new Tone.LFO({ frequency: p.lfoRate ?? 2, min: -1, max: 1 })
  lfo.type = INST_LFO_TYPES[Math.max(0, Math.min(3, (p.lfoShape ?? 0) | 0))]
  const depth = new Tone.Gain(cutoff * amt)
  lfo.connect(depth)
  depth.connect(freqParam as any)
  let running = false
  const gate = () => {
    const want = amt > 0.001
    if (want === running) return
    running = want
    try { want ? lfo.start() : lfo.stop() } catch { /* ok */ }
  }
  gate()
  return {
    /** handle lfoShape/lfoRate/lfoAmt + cutoff changes; true = handled */
    set(k: string, v: number): boolean {
      if (k === 'lfoRate') { lfo.frequency.value = v; return true }
      if (k === 'lfoShape') { lfo.type = INST_LFO_TYPES[Math.max(0, Math.min(3, v | 0))]; return true }
      if (k === 'lfoAmt') { amt = v; depth.gain.rampTo(cutoff * amt, 0.03); gate(); return true }
      if (k === 'cutoff') { cutoff = v; depth.gain.rampTo(cutoff * amt, 0.03) }  // not handled — caller also moves the base
      return false
    },
    dispose() { lfo.dispose(); depth.dispose() },
  }
}

function makePoly(p: Record<string, number>): Inst {
  let wave = p.wave | 0
  let spread = p.spread ?? 18
  const oscOpts = () => {
    const type = WAVES[wave] as any
    return type.startsWith('fat') ? { type, spread, count: 3 } : { type }
  }
  const synth = new Tone.PolySynth(Tone.Synth, {
    maxPolyphony: 16,
    oscillator: oscOpts(),
    envelope: { attack: p.attack, decay: p.decay, sustain: p.sustain, release: p.release },
  } as any)
  const filt = new Tone.Filter(p.cutoff, 'lowpass')
  filt.Q.value = p.res
  filt.rolloff = slopeRolloff(p.slope ?? 0)
  synth.connect(filt)
  const lfo = makeInstLfo(p, filt.frequency, p.cutoff)
  return {
    out: filt,
    set: (k, v) => {
      if (lfo.set(k, v)) return
      if (k === 'wave') { wave = v | 0; synth.set({ oscillator: oscOpts() } as any) }
      else if (k === 'spread') { spread = v; if (WAVES[wave].startsWith('fat')) synth.set({ oscillator: { spread: v } } as any) }
      else if (k === 'cutoff') filt.frequency.rampTo(v, 0.03)
      else if (k === 'res') filt.Q.value = v
      else if (k === 'slope') filt.rolloff = slopeRolloff(v)
      else synth.set({ envelope: { [k]: v } } as any)
    },
    noteOn: (pp, vel) => synth.triggerAttack(midiHz(pp), liveTime(), vel),
    noteOff: pp => synth.triggerRelease(midiHz(pp), liveTime()),
    trigger: (pp, dur, time, vel) => synth.triggerAttackRelease(midiHz(pp), dur, time, vel),
    dispose: () => { synth.dispose(); filt.dispose(); lfo.dispose() },
  }
}

function makeDuo(p: Record<string, number>): Inst {
  const synth = new Tone.PolySynth(Tone.DuoSynth as any, {
    maxPolyphony: 6,
    harmonicity: p.harm,
    vibratoAmount: p.vibAmt,
    vibratoRate: p.vibRate,
    voice0: { envelope: { attack: p.attack, decay: p.decay, sustain: p.sustain, release: p.release } },
    voice1: { envelope: { attack: p.attack, decay: p.decay, sustain: p.sustain, release: p.release } },
  } as any)
  return {
    out: synth as any,
    set: (k, v) => {
      if (k === 'harm') synth.set({ harmonicity: v } as any)
      else if (k === 'vibAmt') synth.set({ vibratoAmount: v } as any)
      else if (k === 'vibRate') synth.set({ vibratoRate: v } as any)
      else synth.set({ voice0: { envelope: { [k]: v } }, voice1: { envelope: { [k]: v } } } as any)
    },
    noteOn: (pp, vel) => (synth as any).triggerAttack(midiHz(pp), liveTime(), vel),
    noteOff: pp => (synth as any).triggerRelease(midiHz(pp), liveTime()),
    trigger: (pp, dur, time, vel) => (synth as any).triggerAttackRelease(midiHz(pp), dur, time, vel),
    dispose: () => synth.dispose(),
  }
}

function makeFm(p: Record<string, number>): Inst {
  const synth = new Tone.PolySynth(Tone.FMSynth, {
    maxPolyphony: 12,
    harmonicity: p.harm,
    modulationIndex: p.modIdx,
    envelope: { attack: p.attack, decay: p.decay, sustain: p.sustain, release: p.release },
    modulationEnvelope: { attack: 0.005, decay: 0.4, sustain: 0.6, release: 0.4 },
  } as any)
  return {
    out: synth,
    set: (k, v) => {
      if (k === 'harm') synth.set({ harmonicity: v } as any)
      else if (k === 'modIdx') synth.set({ modulationIndex: v } as any)
      else synth.set({ envelope: { [k]: v } } as any)
    },
    noteOn: (pp, vel) => synth.triggerAttack(midiHz(pp), liveTime(), vel),
    noteOff: pp => synth.triggerRelease(midiHz(pp), liveTime()),
    trigger: (pp, dur, time, vel) => synth.triggerAttackRelease(midiHz(pp), dur, time, vel),
    dispose: () => synth.dispose(),
  }
}

function makeMono(p: Record<string, number>): Inst {
  const synth = new Tone.MonoSynth({
    oscillator: { type: WAVES[p.wave | 0] as any },
    filter: { Q: p.res, type: 'lowpass', rolloff: slopeRolloff(p.slope ?? 1) },
    filterEnvelope: { attack: 0.004, decay: 0.18, sustain: 0.4, release: 0.3, baseFrequency: p.cutoff, octaves: p.envAmt },
    envelope: { attack: p.attack, decay: p.decay, sustain: p.sustain, release: p.release },
    portamento: p.glide,
  } as any)
  // built-in LFO sums into the filter cutoff on top of the filter envelope
  const lfo = makeInstLfo(p, synth.filter.frequency, p.cutoff)
  return {
    out: synth,
    set: (k, v) => {
      if (lfo.set(k, v)) return
      if (k === 'wave') synth.set({ oscillator: { type: WAVES[v | 0] as any } } as any)
      else if (k === 'cutoff') synth.set({ filterEnvelope: { baseFrequency: v } } as any)
      else if (k === 'res') synth.set({ filter: { Q: v } } as any)
      else if (k === 'slope') synth.filter.rolloff = slopeRolloff(v)
      else if (k === 'envAmt') synth.set({ filterEnvelope: { octaves: v } } as any)
      else if (k === 'glide') synth.portamento = v
      else synth.set({ envelope: { [k]: v } } as any)
    },
    noteOn: (pp, vel) => synth.triggerAttack(midiHz(pp), liveTime(), vel),
    noteOff: () => synth.triggerRelease(liveTime()),
    trigger: (pp, dur, time, vel) => synth.triggerAttackRelease(midiHz(pp), dur, time, vel),
    dispose: () => { synth.dispose(); lfo.dispose() },
  }
}

function makePluck(p: Record<string, number>): Inst {
  const out = new Tone.Gain(0.9)
  const pool = Array.from({ length: 4 }, () =>
    new Tone.PluckSynth({ dampening: p.dampen, resonance: p.res, attackNoise: 1 }).connect(out))
  let i = 0
  const next = () => pool[i++ % pool.length]
  return {
    out,
    set: (k, v) => pool.forEach(s => {
      if (k === 'dampen') (s as any).dampening = v
      else if (k === 'res') s.resonance = v as any
    }),
    noteOn: (pp, vel) => next().triggerAttack(midiHz(pp), liveTime()),
    noteOff: () => {},
    trigger: (pp, _dur, time, _vel) => next().triggerAttack(midiHz(pp), time),
    dispose: () => { pool.forEach(s => s.dispose()); out.dispose() },
  }
}

function makeKeys(p: Record<string, number>): Inst {
  const synth = new Tone.PolySynth(Tone.AMSynth, {
    maxPolyphony: 12,
    harmonicity: p.harm,
    envelope: { attack: p.attack, decay: p.decay, sustain: p.sustain, release: p.release },
    modulation: { type: 'sine' },
  } as any)
  return {
    out: synth,
    set: (k, v) => {
      if (k === 'harm') synth.set({ harmonicity: v } as any)
      else synth.set({ envelope: { [k]: v } } as any)
    },
    noteOn: (pp, vel) => synth.triggerAttack(midiHz(pp), liveTime(), vel),
    noteOff: pp => synth.triggerRelease(midiHz(pp), liveTime()),
    trigger: (pp, dur, time, vel) => synth.triggerAttackRelease(midiHz(pp), dur, time, vel),
    dispose: () => synth.dispose(),
  }
}

// ---- drum kit: 8 synthesized pads ----

type Pad = {
  trig: (time: number, vel: number) => void
  set: (which: 'tune' | 'decay' | 'level', v: number) => void
  dispose: () => void
}

function noisePad(mix: Tone.Gain, kind: 'snare' | 'clap' | 'clhat' | 'ophat' | 'crash', decay0: number): Pad {
  const centers: Record<string, number> = { snare: 1800, clap: 1100, clhat: 9000, ophat: 8000, crash: 6500 }
  const types: Record<string, BiquadFilterType> = { snare: 'bandpass', clap: 'bandpass', clhat: 'highpass', ophat: 'highpass', crash: 'highpass' }
  let tune = 0
  let decay = decay0
  const level = new Tone.Gain(1).connect(mix)
  const filt = new Tone.Filter(centers[kind], types[kind]).connect(level)
  if (kind === 'snare') filt.Q.value = 0.8
  const noise = new Tone.NoiseSynth({
    noise: { type: kind === 'crash' ? 'pink' : 'white' },
    envelope: { attack: 0.001, decay: decay0, sustain: 0, release: 0.03 },
  }).connect(filt)
  const retune = () => filt.frequency.setValueAtTime(centers[kind] * Math.pow(2, tune / 12), Tone.now())
  return {
    trig: (time, vel) => {
      if (kind === 'clap') {
        // three quick bursts ≈ a clap
        noise.triggerAttackRelease(decay, time, vel * 0.7)
        noise.triggerAttackRelease(decay, time + 0.012, vel * 0.85)
        noise.triggerAttackRelease(decay, time + 0.026, vel)
      } else {
        noise.triggerAttackRelease(decay, time, vel)
      }
    },
    set: (which, v) => {
      if (which === 'tune') { tune = v; retune() }
      else if (which === 'decay') { decay = v; noise.set({ envelope: { decay: v } } as any) }
      else level.gain.value = Tone.dbToGain(v)
    },
    dispose: () => { noise.dispose(); filt.dispose(); level.dispose() },
  }
}

function membranePad(mix: Tone.Gain, baseMidi: number, decay0: number, pitchDecay: number): Pad {
  let tune = 0
  let decay = decay0
  const level = new Tone.Gain(1).connect(mix)
  const synth = new Tone.MembraneSynth({
    pitchDecay,
    octaves: 6,
    envelope: { attack: 0.001, decay: decay0, sustain: 0, release: 0.05 },
  }).connect(level)
  return {
    trig: (time, vel) => synth.triggerAttackRelease(midiHz(baseMidi + tune), Math.max(0.05, decay), time, vel),
    set: (which, v) => {
      if (which === 'tune') tune = v
      else if (which === 'decay') { decay = v; synth.set({ envelope: { decay: v } } as any) }
      else level.gain.value = Tone.dbToGain(v)
    },
    dispose: () => { synth.dispose(); level.dispose() },
  }
}

function blipPad(mix: Tone.Gain, baseHz: number, decay0: number): Pad {
  let tune = 0
  let decay = decay0
  const level = new Tone.Gain(1).connect(mix)
  const synth = new Tone.Synth({
    oscillator: { type: 'triangle' },
    envelope: { attack: 0.001, decay: decay0, sustain: 0, release: 0.03 },
  }).connect(level)
  return {
    trig: (time, vel) => synth.triggerAttackRelease(baseHz * Math.pow(2, tune / 12), Math.max(0.04, decay), time, vel),
    set: (which, v) => {
      if (which === 'tune') tune = v
      else if (which === 'decay') { decay = v; synth.set({ envelope: { decay: v } } as any) }
      else level.gain.value = Tone.dbToGain(v)
    },
    dispose: () => { synth.dispose(); level.dispose() },
  }
}

// A drum pad backed by an audio sample (one-shot). Polyphonic + velocity via
// Tone.Sampler (same engine the Sampler instrument uses); `tune` repitches in
// semitones, `level` is the pad gain. The full sample always plays (decay is a
// no-op here). Drops into the same Pad slot as the synth voices.
function samplePad(mix: Tone.Gain, buffer: AudioBuffer, level0: number, tune0: number): Pad {
  let tune = tune0
  const level = new Tone.Gain(Tone.dbToGain(level0)).connect(mix)
  const sampler = new Tone.Sampler({
    urls: { C3: new Tone.ToneAudioBuffer(buffer) },
    attack: 0.001, release: 0.06,
  }).connect(level)
  const note = () => Tone.Frequency(48 + tune, 'midi').toFrequency() // C3 = midi 48 → natural pitch
  return {
    trig: (time, vel) => { try { sampler.triggerAttack(note(), time, vel) } catch { /* overlapping retrigger */ } },
    set: (which, v) => {
      if (which === 'tune') tune = v
      else if (which === 'level') level.gain.value = Tone.dbToGain(v)
      // 'decay' has no effect on a sampled pad (the whole sample plays)
    },
    dispose: () => { sampler.dispose(); level.dispose() },
  }
}

function makeDrum(p: Record<string, number>, padBuffers?: Map<number, AudioBuffer>): Inst {
  const mix = new Tone.Gain(1)
  // Each pad is synthesized unless a sample has been dropped on it, in which
  // case that one pad plays the sample while the rest stay synths.
  const padOr = (i: number, synth: () => Pad): Pad => {
    const buf = padBuffers?.get(i)
    return buf ? samplePad(mix, buf, p[`p${i}_level`] ?? 0, p[`p${i}_tune`] ?? 0) : synth()
  }
  const pads: Pad[] = [
    padOr(0, () => membranePad(mix, 36, p.p0_decay, 0.05)),  // kick
    padOr(1, () => noisePad(mix, 'snare', p.p1_decay)),      // snare
    padOr(2, () => noisePad(mix, 'clap', p.p2_decay)),       // clap
    padOr(3, () => noisePad(mix, 'clhat', p.p3_decay)),      // closed hat
    padOr(4, () => noisePad(mix, 'ophat', p.p4_decay)),      // open hat
    padOr(5, () => membranePad(mix, 48, p.p5_decay, 0.03)),  // lo tom
    padOr(6, () => blipPad(mix, 1100, p.p6_decay)),          // perc
    padOr(7, () => noisePad(mix, 'crash', p.p7_decay)),      // crash
  ]
  pads.forEach((pad, i) => {
    pad.set('tune', p[`p${i}_tune`] ?? 0)
    pad.set('level', p[`p${i}_level`] ?? 0)
  })
  return {
    out: mix,
    set: (k, v) => {
      const m = k.match(/^p(\d)_(tune|decay|level)$/)
      if (m) pads[+m[1]]?.set(m[2] as any, v)
    },
    noteOn: (pp, vel) => pads[pp % pads.length]?.trig(liveTime(), vel),
    noteOff: () => {},
    trigger: (pp, _dur, time, vel) => pads[pp % pads.length]?.trig(time, vel),
    dispose: () => { pads.forEach(pad => pad.dispose()); mix.dispose() },
  }
}

function makeSampler(p: Record<string, number>, buffer?: AudioBuffer): Inst {
  const out = new Tone.Gain(0.9)
  let tune = p.tune ?? 0
  if (!buffer) {
    // no sample loaded (or not transferred to this peer yet) → silent but valid
    return { out, set: () => {}, noteOn: () => {}, noteOff: () => {}, trigger: () => {}, dispose: () => out.dispose() }
  }
  const sampler = new Tone.Sampler({
    urls: { C3: new Tone.ToneAudioBuffer(buffer) },
    attack: p.attack ?? 0.005,
    release: p.release ?? 0.4,
  }).connect(out)
  const hz = (pp: number) => Tone.Frequency(pp + tune, 'midi').toFrequency()
  return {
    out,
    set: (k, v) => {
      if (k === 'tune') tune = v
      else if (k === 'attack') sampler.attack = v
      else if (k === 'release') sampler.release = v
    },
    noteOn: (pp, vel) => sampler.triggerAttack(hz(pp), liveTime(), vel),
    noteOff: pp => sampler.triggerRelease(hz(pp), liveTime()),
    trigger: (pp, dur, time, vel) => sampler.triggerAttackRelease(hz(pp), dur, time, vel),
    dispose: () => { sampler.dispose(); out.dispose() },
  }
}

// Musical single-sample keyboard instrument: ONE buffer played chromatically,
// pitched up/down across the keyboard (Tone.Sampler handles polyphony + velocity
// + sample-accurate repitch). `root` (0..11 = C..B) is the note the sample sounds
// at unpitched, folded into the pitch math so it changes live with no rebuild.
function makeKSampler(p: Record<string, number>, buffer?: AudioBuffer): Inst {
  const out = new Tone.Gain(0.9)
  let tune = p.tune ?? 0
  let root = (p.root ?? 0) | 0
  if (!buffer) {
    return { out, set: () => {}, noteOn: () => {}, noteOff: () => {}, trigger: () => {}, dispose: () => out.dispose() }
  }
  const sampler = new Tone.Sampler({
    urls: { C3: new Tone.ToneAudioBuffer(buffer) },   // sample rooted at C3 (midi 48)
    attack: p.attack ?? 0.004,
    release: p.release ?? 0.4,
  }).connect(out)
  // Feed the C3-keyed sampler `note - root`, so playing the chosen root pitch
  // class (in octave 3) plays the sample at its natural pitch.
  const hz = (pp: number) => Tone.Frequency(pp - root + tune, 'midi').toFrequency()
  return {
    out,
    set: (k, v) => {
      if (k === 'tune') tune = v
      else if (k === 'root') root = v | 0
      else if (k === 'attack') sampler.attack = v
      else if (k === 'release') sampler.release = v
    },
    noteOn: (pp, vel) => sampler.triggerAttack(hz(pp), liveTime(), vel),
    noteOff: pp => sampler.triggerRelease(hz(pp), liveTime()),
    trigger: (pp, dur, time, vel) => sampler.triggerAttackRelease(hz(pp), dur, time, vel),
    dispose: () => { sampler.dispose(); out.dispose() },
  }
}

/**
 * Granular synthesis engine. Each held note runs a Tone.Clock at the grain
 * density; every tick schedules one grain — a raw AudioBufferSourceNode playing
 * a `size`-second slice from `pos` (± spray) of the sample, pitched by the note
 * (± jitter), through a triangular gain window (skewed by `shape`), a random
 * stereo pan (`spread`), and the voice's amp envelope. Clocks are context-time
 * driven, so sequenced playback AND offline export schedule sample-accurately.
 */
function makeGranular(p: Record<string, number>, buffer?: AudioBuffer): Inst {
  const out = new Tone.Gain(0.9)
  if (!buffer) {
    // no sample loaded (or not transferred to this peer yet) → silent but valid
    return { out, set: () => {}, noteOn: () => {}, noteOff: () => {}, trigger: () => {}, dispose: () => out.dispose() }
  }
  const rawCtx = Tone.getContext().rawContext as BaseAudioContext
  const g: Record<string, number> = { ...p }
  const bufDur = buffer.duration

  // reversed copy built once, on first reversed grain
  let revBuf: AudioBuffer | null = null
  const reversed = () => {
    if (!revBuf) {
      revBuf = rawCtx.createBuffer(buffer.numberOfChannels, buffer.length, buffer.sampleRate)
      for (let c = 0; c < buffer.numberOfChannels; c++) {
        const src = buffer.getChannelData(c), dst = revBuf.getChannelData(c)
        for (let i = 0, n = buffer.length; i < n; i++) dst[i] = src[n - 1 - i]
      }
    }
    return revBuf
  }

  type Voice = { p: number; env: Tone.Gain; clock: Tone.Clock; dead?: boolean }
  const held = new Map<number, Voice>()   // live (noteOn/noteOff) voices by pitch
  let liveCount = 0
  const MAX_VOICES = 10

  const spawnGrain = (v: Voice, time: number) => {
    if (v.dead) return
    const size = clamp(g.size ?? 0.12, 0.02, 0.5)
    const rate = Math.pow(2, (v.p - 60 + (g.pitch ?? 0) + (Math.random() * 2 - 1) * (g.pjit ?? 0)) / 12)
    let offset = (g.pos ?? 0) * bufDur + (Math.random() * 2 - 1) * (g.spray ?? 0) * bufDur * 0.5
    const rev = Math.random() < (g.rev ?? 0)
    const bs = rawCtx.createBufferSource()
    bs.buffer = rev ? reversed() : buffer
    if (rev) offset = bufDur - offset - size * rate
    bs.playbackRate.value = rate
    const win = rawCtx.createGain()
    const shape = clamp(g.shape ?? 0.5, 0.03, 0.97)   // 0 = percussive, 1 = swell
    win.gain.setValueAtTime(0, time)
    win.gain.linearRampToValueAtTime(0.8, time + size * shape)
    win.gain.linearRampToValueAtTime(0, time + size)
    const pan = rawCtx.createStereoPanner()
    pan.pan.value = (Math.random() * 2 - 1) * (g.spread ?? 0)
    bs.connect(win); win.connect(pan)
    Tone.connect(pan, v.env)
    bs.onended = () => { try { bs.disconnect(); win.disconnect(); pan.disconnect() } catch { /* ok */ } }
    try { bs.start(time, clamp(offset, 0, Math.max(0, bufDur - 0.02)), size * rate + 0.02) } catch { /* scheduling race */ }
  }

  const makeVoice = (pitch: number): Voice => {
    const env = new Tone.Gain(0).connect(out)
    const v: Voice = { p: pitch, env, clock: null as unknown as Tone.Clock }
    v.clock = new Tone.Clock(t => spawnGrain(v, t), clamp(g.dens ?? 18, 2, 80))
    liveCount++
    return v
  }
  const reap = (v: Voice, afterSec: number) => {
    setTimeout(() => {
      v.dead = true
      liveCount--
      try { v.clock.dispose(); v.env.dispose() } catch { /* context may be gone (offline) */ }
    }, Math.max(0, afterSec * 1000))
  }

  return {
    out,
    set: (k, v) => {
      g[k] = v
      if (k === 'dens') {
        const f = clamp(v, 2, 80)
        held.forEach(vc => { vc.clock.frequency.value = f })
      }
    },
    noteOn: (pp, vel) => {
      if (held.has(pp)) return
      if (liveCount >= MAX_VOICES) return
      const now = Tone.now()
      const v = makeVoice(pp)
      v.env.gain.setValueAtTime(0, now)
      v.env.gain.linearRampToValueAtTime(vel, now + Math.max(0.003, g.attack ?? 0.05))
      v.clock.start(now)
      held.set(pp, v)
    },
    noteOff: pp => {
      const v = held.get(pp)
      if (!v) return
      held.delete(pp)
      const now = Tone.now()
      const rel = Math.max(0.02, g.release ?? 0.4)
      v.env.gain.cancelScheduledValues(now)
      v.env.gain.setTargetAtTime(0, now, rel / 3)
      v.clock.stop(now + rel)
      reap(v, rel + 1.5)
    },
    trigger: (pp, dur, time, vel) => {
      if (liveCount >= MAX_VOICES) return
      const v = makeVoice(pp)
      const atk = Math.min(Math.max(0.003, g.attack ?? 0.05), dur * 0.5)
      const rel = Math.max(0.02, g.release ?? 0.4)
      v.env.gain.setValueAtTime(0, time)
      v.env.gain.linearRampToValueAtTime(vel, time + atk)
      v.env.gain.setValueAtTime(vel, time + dur)
      v.env.gain.linearRampToValueAtTime(0, time + dur + rel)
      try { v.clock.start(time).stop(time + dur) } catch { /* ok */ }
      reap(v, (time - Tone.now()) + dur + rel + 1.5)
    },
    dispose: () => {
      held.forEach(v => { v.dead = true; try { v.clock.dispose(); v.env.dispose() } catch { /* ok */ } })
      held.clear()
      out.dispose()
    },
  }
}

/** Audio tracks have no synth — just a stereo passthrough bus that audio-clip
 *  players connect into, so the signal runs through the track's fx + mixer. */
function makeAudioBus(): Inst {
  const bus = new Tone.Gain(1)
  return { out: bus, set: () => {}, noteOn: () => {}, noteOff: () => {}, trigger: () => {}, dispose: () => bus.dispose() }
}

export function makeInstrument(type: string, params: Record<string, number>, buffer?: AudioBuffer, padBuffers?: Map<number, AudioBuffer>): Inst {
  switch (type) {
    case 'fm': return makeFm(params)
    case 'mono': return makeMono(params)
    case 'pluck': return makePluck(params)
    case 'keys': return makeKeys(params)
    case 'duo': return makeDuo(params)
    case 'sampler': return makeSampler(params, buffer)
    case 'ksampler': return makeKSampler(params, buffer)
    case 'granular': return makeGranular(params, buffer)
    case 'audiobus': return makeAudioBus()
    case 'drum': return makeDrum(params, padBuffers)
    default: return makePoly(params)
  }
}

// ---------------- effects ----------------

function delaySeconds(idx: number) {
  const bpm = Tone.getTransport().bpm.value
  const whole = (60 / bpm) * 4
  return whole * DELAY_FRACTIONS[Math.max(0, Math.min(DELAY_FRACTIONS.length - 1, idx | 0))]
}

// LA-2A-style optical compressor. The opto "cell" gives soft-knee, gentle,
// program-dependent levelling — modelled here on the native DynamicsCompressor
// with a wide knee, low ratio and slow release (Comp), or a tighter/harder
// curve (Limit). Composite (compressor → makeup gain) so it wires as one
// in=out effect node; exposes live gain reduction for the GR meter.
class OptoComp extends Tone.ToneAudioNode {
  readonly name = 'OptoComp'
  readonly input: Tone.Compressor
  readonly output: Tone.Gain
  private mode = 0
  constructor() {
    super()
    this.input = new Tone.Compressor({ ratio: 3, knee: 30, attack: 0.01, release: 0.45, threshold: -16 })
    this.output = new Tone.Gain(1)
    this.input.connect(this.output)
  }
  get reduction(): number { return (this.input as any).reduction ?? 0 }
  private applyMode() {
    this.input.ratio.value = this.mode ? 10 : 3
    this.input.knee.value = this.mode ? 12 : 30
    this.input.attack.value = this.mode ? 0.005 : 0.01
    this.input.release.value = this.mode ? 0.3 : 0.45
  }
  // Peak Reduction (0..1) lowers the threshold (0 → -40 dB), like the hardware.
  setReduction(v: number) { this.input.threshold.value = -40 * Math.max(0, Math.min(1, v)) }
  setGain(db: number) { this.output.gain.value = Tone.dbToGain(db) }
  setMode(m: number) { this.mode = m | 0; this.applyMode() }
  configure(mode: number, reduction: number, gainDb: number) {
    this.mode = mode | 0; this.applyMode(); this.setReduction(reduction); this.setGain(gainDb)
  }
  dispose() { super.dispose(); this.input.dispose(); this.output.dispose(); return this }
}

// Gate an effect's internal LFO. Chorus/AutoFilter/Tremolo/AutoPanner run a
// source oscillator forever once .start()ed — even at wet 0 or depth 0, where
// the modulation is inaudible (a stopped Tone.LFO rests at its mid value, which
// at depth≈0 equals the unmodulated setting). Start it only while it matters.
const LFO_EPS = 0.001
function lfoGate(node: { start: () => any; stop: () => any }) {
  let running = false
  return (want: boolean) => {
    if (want === running) return
    running = want
    try { want ? node.start() : node.stop() } catch { /* ok */ }
  }
}

// Plate reverb: a synthesized plate impulse response through a raw Convolver.
// A real plate has NO discrete early reflections — echo density is maximal
// almost immediately — so the IR is decorrelated stereo noise with a ~4ms onset
// ramp, an exponential decay, and a first-difference high-tilt for the metallic
// sheen. Pre-delay sits before the convolver, a damping lowpass after it.
// Crucially there is no nested offline render (unlike Tone.Reverb), so the same
// code path works inside Tone.Offline exports.
class PlateVerb extends Tone.ToneAudioNode {
  readonly name = 'PlateVerb'
  readonly input: Tone.Gain
  readonly output: Tone.Gain
  private pre: DelayNode
  private conv: ConvolverNode
  private dampF: BiquadFilterNode
  private wet: Tone.Gain
  private dry: Tone.Gain
  private regen: ReturnType<typeof setTimeout> | null = null
  constructor(p: Record<string, number>) {
    super()
    const raw = this.context.rawContext as BaseAudioContext
    this.input = new Tone.Gain(1)
    this.output = new Tone.Gain(1)
    this.pre = raw.createDelay(0.5)
    this.pre.delayTime.value = p.predelay ?? 0.02
    this.conv = raw.createConvolver()
    this.conv.normalize = true
    this.dampF = raw.createBiquadFilter()
    this.dampF.type = 'lowpass'
    this.dampF.frequency.value = p.damp ?? 6000
    const mix = p.mix ?? 0.3
    this.wet = new Tone.Gain(mix)
    this.dry = new Tone.Gain(1 - mix)
    this.input.connect(this.dry); this.dry.connect(this.output)
    Tone.connect(this.input, this.pre)
    this.pre.connect(this.conv); this.conv.connect(this.dampF)
    Tone.connect(this.dampF, this.wet)
    this.wet.connect(this.output)
    this.makeIr(p.decay ?? 1.8)
  }
  makeIr(decay: number) {
    const raw = this.context.rawContext as BaseAudioContext
    const sr = raw.sampleRate
    const len = Math.max(64, Math.floor(Math.max(0.15, decay) * sr))
    const ir = raw.createBuffer(2, len, sr)
    for (let c = 0; c < 2; c++) {
      const d = ir.getChannelData(c)
      for (let i = 0; i < len; i++) {
        const onset = Math.min(1, (i / sr) / 0.004)
        d[i] = (Math.random() * 2 - 1) * onset * Math.exp((-6.9 * i) / len)   // −60dB by the tail
      }
      // metallic tilt: blend in the first difference (a gentle high emphasis)
      for (let i = len - 1; i > 0; i--) d[i] = d[i] * 0.7 + (d[i] - d[i - 1]) * 0.3
    }
    this.conv.buffer = ir
  }
  setParam(k: string, v: number) {
    if (k === 'decay') {
      if (this.regen) clearTimeout(this.regen)
      this.regen = setTimeout(() => { this.regen = null; this.makeIr(v) }, 150)
    } else if (k === 'predelay') this.pre.delayTime.setTargetAtTime(v, this.now(), 0.02)
    else if (k === 'damp') this.dampF.frequency.setTargetAtTime(v, this.now(), 0.02)
    else if (k === 'mix') { this.wet.gain.rampTo(v, 0.03); this.dry.gain.rampTo(1 - v, 0.03) }
  }
  dispose() {
    super.dispose()
    if (this.regen) clearTimeout(this.regen)
    this.input.dispose(); this.output.dispose(); this.wet.dispose(); this.dry.dispose()
    try { this.pre.disconnect(); this.conv.disconnect(); this.dampF.disconnect() } catch { /* ok */ }
    return this
  }
}

// 7-band parametric EQ: low shelf → 5 peaking → high shelf, as a series of raw
// biquads inside one in=out node. Params are flat b{i}_(freq|gain|q) keys.
class Eq7 extends Tone.ToneAudioNode {
  readonly name = 'Eq7'
  readonly input: Tone.Gain
  readonly output: Tone.Gain
  readonly bands: BiquadFilterNode[] = []
  constructor(p: Record<string, number>) {
    super()
    const raw = this.context.rawContext as BaseAudioContext
    this.input = new Tone.Gain(1)
    this.output = new Tone.Gain(1)
    for (let i = 0; i < EQ7_BANDS; i++) {
      const bq = raw.createBiquadFilter()
      bq.type = eq7BandType(i)
      bq.frequency.value = p[`b${i}_freq`] ?? EQ7_FREQS[i]
      bq.gain.value = p[`b${i}_gain`] ?? 0
      bq.Q.value = p[`b${i}_q`] ?? 1
      if (i === 0) Tone.connect(this.input, bq)
      else this.bands[i - 1].connect(bq)
      this.bands.push(bq)
    }
    Tone.connect(this.bands[EQ7_BANDS - 1], this.output)
  }
  setParam(k: string, v: number) {
    const m = /^b(\d)_(freq|gain|q)$/.exec(k)
    if (!m) return
    const bq = this.bands[+m[1]]
    if (!bq) return
    if (m[2] === 'freq') bq.frequency.setTargetAtTime(v, this.now(), 0.02)
    else if (m[2] === 'gain') bq.gain.setTargetAtTime(v, this.now(), 0.02)
    else bq.Q.setTargetAtTime(Math.max(0.05, v), this.now(), 0.02)
  }
  dispose() {
    super.dispose()
    this.input.dispose(); this.output.dispose()
    this.bands.forEach(b => { try { b.disconnect() } catch { /* ok */ } })
    return this
  }
}

// Construct one of the consolidated sf-fx worklet processors (sf-chorus /
// sf-phaser / sf-shifter / sf-autotune — see public/sf-fx.js). Returns null
// when the module isn't registered on the current context (preload failed or
// timed out) — callers fall back to the original Tone composite so audio never
// breaks. Registered live by engine.doStart and per-render by render.ts.
// Tone's OFFLINE context is a standardized-audio-context wrapper, not a native
// BaseAudioContext, so `new AudioWorkletNode(rawContext, …)` throws
// "parameter 1 is not of type 'BaseAudioContext'" during an export and every
// worklet device silently degraded (mbcomp → passthrough) in the bounce.
// context.createAudioWorkletNode() picks the native constructor for a native
// context and the wrapper-aware one otherwise, so live and offline both work.
function fxWorkletNode(name: string, init: Record<string, number>): AudioWorkletNode | null {
  try {
    const node = Tone.getContext().createAudioWorkletNode(name, {
      numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [2],
      channelCount: 2, channelCountMode: 'explicit', channelInterpretation: 'speakers',
    }) as unknown as AudioWorkletNode
    node.port.postMessage({ init })
    return node
  } catch { return null }
}

export function makeEffect(type: string, p: Record<string, number>): Fx {
  switch (type) {
    case 'eq': {
      const node = new Tone.EQ3({ low: p.low, mid: p.mid, high: p.high })
      return {
        node,
        set: (k, v) => { (node as any)[k].value = v },
        dispose: () => node.dispose(),
      }
    }
    case 'filter': {
      const types: BiquadFilterType[] = ['lowpass', 'highpass', 'bandpass']
      // slope index → biquad rolloff (dB/oct). Web Audio only supports these three.
      const rolloffs = [-12, -24, -48] as const
      const rolloffOf = (i: number) => rolloffs[Math.max(0, Math.min(2, i | 0))]
      const node = new Tone.Filter(p.freq, types[p.ftype | 0])
      node.Q.value = p.q
      node.rolloff = rolloffOf(p.slope ?? 0)
      return {
        node,
        set: (k, v) => {
          if (k === 'ftype') node.type = types[v | 0]
          else if (k === 'freq') node.frequency.rampTo(v, 0.03)
          else if (k === 'slope') node.rolloff = rolloffOf(v)
          else node.Q.value = v
        },
        dispose: () => node.dispose(),
      }
    }
    case 'delay': {
      const node = new Tone.FeedbackDelay({ delayTime: delaySeconds(p.time), feedback: p.fb, wet: p.mix, maxDelay: 4 })
      return {
        node,
        set: (k, v) => {
          if (k === 'time') node.delayTime.rampTo(delaySeconds(v), 0.05)
          else if (k === 'fb') node.feedback.value = Math.min(0.9, v)
          else node.wet.value = v
        },
        dispose: () => node.dispose(),
      }
    }
    case 'reverb': {
      const node = new Tone.Reverb({ decay: p.size, preDelay: 0.02, wet: p.mix })
      node.generate().catch(() => {})
      let regen: ReturnType<typeof setTimeout> | null = null
      return {
        node,
        set: (k, v) => {
          if (k === 'size') {
            node.decay = v
            if (regen) clearTimeout(regen)
            regen = setTimeout(() => node.generate().catch(() => {}), 150)
          } else node.wet.value = v
        },
        dispose: () => node.dispose(),
      }
    }
    case 'plate': {
      const node = new PlateVerb(p)
      return {
        node,
        set: (k, v) => node.setParam(k, v),
        dispose: () => node.dispose(),
      }
    }
    case 'eq7': {
      const node = new Eq7(p)
      // pre-EQ tap for the card's real-time analyser (engine wires prev → detect)
      const ctx = Tone.getContext().rawContext as BaseAudioContext
      const detect = ctx.createAnalyser()
      detect.fftSize = 2048
      detect.smoothingTimeConstant = 0.82
      return {
        node,
        detect,
        set: (k, v) => node.setParam(k, v),
        dispose: () => { node.dispose(); try { detect.disconnect() } catch { /* ok */ } },
      }
    }
    case 'chorus': {
      // Deliberately the Tone composite, NOT an sf-fx worklet: a worklet port
      // was tried and reverted — even a faithful reimplementation subtly shifts
      // the sound of existing projects, and this device's character is baked
      // into people's mixes. (Auto-Tune is worklet-based because there the
      // change was a correctness fix.)
      const rate = makeRate(p, hz => { node.frequency.value = hz })
      const node = new Tone.Chorus({ frequency: rate.hz0, delayTime: 3, depth: p.depth, wet: p.mix })
      let depth = p.depth ?? 0, mix = p.mix ?? 0
      const gate = lfoGate(node)
      gate(depth > LFO_EPS && mix > LFO_EPS)
      return {
        node,
        set: (k, v) => {
          if (rate.set(k, v)) return
          if (k === 'depth') { depth = v; node.depth = v }
          else { mix = v; node.wet.value = v }
          gate(depth > LFO_EPS && mix > LFO_EPS)
        },
        dispose: () => { rate.dispose(); node.dispose() },
      }
    }
    case 'dist': {
      const node = new Tone.Distortion({ distortion: p.amt, oversample: '4x', wet: p.mix })
      return {
        node,
        set: (k, v) => {
          if (k === 'amt') node.distortion = v
          else node.wet.value = v
        },
        dispose: () => node.dispose(),
      }
    }
    case 'crush': {
      const node = new Tone.BitCrusher(p.bits | 0)
      node.wet.value = p.mix
      return {
        node,
        set: (k, v) => {
          if (k === 'bits') node.bits.value = Math.max(1, v | 0)
          else node.wet.value = v
        },
        dispose: () => node.dispose(),
      }
    }
    case 'comp': {
      const node = new Tone.Compressor({ threshold: p.thresh, ratio: p.ratio, attack: p.attack, release: p.release })
      return {
        node,
        set: (k, v) => {
          if (k === 'thresh') node.threshold.value = v
          else if (k === 'ratio') node.ratio.value = v
          else if (k === 'attack') node.attack.value = v
          else node.release.value = v
        },
        gr: () => (node as any).reduction ?? 0,   // DynamicsCompressor live GR (dB, ≤0)
        dispose: () => node.dispose(),
      }
    }
    case 'opto': {
      const opto = new OptoComp()
      opto.configure((p.mode ?? 0) | 0, p.reduction ?? 0.4, p.gain ?? 0)
      return {
        node: opto,
        set: (k, v) => {
          if (k === 'reduction') opto.setReduction(v)
          else if (k === 'gain') opto.setGain(v)
          else if (k === 'mode') opto.setMode(v | 0)
        },
        gr: () => opto.reduction,
        dispose: () => opto.dispose(),
      }
    }
    case 'mbcomp': {
      // Single AudioWorklet node (sf-mbdyn): 3-band crossover + per-band comp or
      // expander + sum. Module is preloaded at engine start; if it isn't ready
      // yet (or in a context without it), degrade to a clean passthrough.
      let grLast = [0, 0, 0]
      try {
        // context-aware factory: a raw `new AudioWorkletNode` throws on Tone's
        // offline (standardized-audio-context) wrapper — see fxWorkletNode.
        const node = Tone.getContext().createAudioWorkletNode('sf-mbdyn', {
          numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [2],
          channelCount: 2, channelCountMode: 'explicit', channelInterpretation: 'speakers',
        }) as unknown as AudioWorkletNode
        node.port.postMessage({ init: p })
        node.port.onmessage = e => { if (e.data?.gr) grLast = e.data.gr as number[] }
        return {
          // a raw AudioWorkletNode IS an AudioNode; Tone.connect handles it at
          // runtime (it only follows .input/.output on real ToneAudioNodes).
          node: node as unknown as Tone.ToneAudioNode,
          set: (k, v) => node.port.postMessage({ k, v }),
          grBands: () => grLast.map(g => -g),   // worklet posts positive dB of reduction
          dispose: () => { try { node.port.onmessage = null; node.disconnect() } catch { /* ok */ } },
        }
      } catch {
        const node = new Tone.Gain(1)
        return { node, set: () => {}, grBands: () => [0, 0, 0], dispose: () => node.dispose() }
      }
    }
    case 'gate': {
      // Single AudioWorklet node (sf-gate): key filter + dual-threshold state
      // machine + expander + lookahead. Module is preloaded at engine start and
      // per-render by render.ts; if it isn't registered (or the context has no
      // worklet support) the device degrades to a clean passthrough.
      let grLast = 0
      try {
        const node = Tone.getContext().createAudioWorkletNode('sf-gate', {
          numberOfInputs: 1, numberOfOutputs: 1, outputChannelCount: [2],
          channelCount: 2, channelCountMode: 'explicit', channelInterpretation: 'speakers',
          processorOptions: { init: p },
        }) as unknown as AudioWorkletNode
        node.port.onmessage = e => { if (typeof e.data?.gr === 'number') grLast = e.data.gr }
        return {
          node: node as unknown as Tone.ToneAudioNode,
          set: (k, v) => node.port.postMessage({ k, v }),
          gr: () => -grLast,                      // worklet posts positive dB of reduction
          dispose: () => { try { node.port.onmessage = null; node.disconnect() } catch { /* ok */ } },
        }
      } catch {
        const node = new Tone.Gain(1)
        return { node, set: () => {}, gr: () => 0, dispose: () => node.dispose() }
      }
    }
    case 'phaser': {
      // Tone composite on purpose — see the chorus note above (worklet port
      // tried and reverted for sounding different from the original device).
      const rate = makeRate(p, hz => { node.frequency.value = hz })
      const node = new Tone.Phaser({ frequency: rate.hz0, octaves: p.octaves | 0, baseFrequency: 350, wet: p.mix })
      return {
        node,
        set: (k, v) => {
          if (rate.set(k, v)) return
          if (k === 'octaves') node.octaves = v | 0
          else node.wet.value = v
        },
        dispose: () => { rate.dispose(); node.dispose() },
      }
    }
    case 'pingpong': {
      const node = new Tone.PingPongDelay({ delayTime: delaySeconds(p.time), feedback: p.fb, wet: p.mix, maxDelay: 4 })
      return {
        node,
        set: (k, v) => {
          if (k === 'time') node.delayTime.rampTo(delaySeconds(v), 0.05)
          else if (k === 'fb') node.feedback.value = Math.min(0.9, v)
          else node.wet.value = v
        },
        dispose: () => node.dispose(),
      }
    }
    case 'autofilt': {
      const rate = makeRate(p, hz => { node.frequency.value = hz })
      const node = new Tone.AutoFilter({ frequency: rate.hz0, depth: p.depth, baseFrequency: p.base, wet: p.mix, octaves: 3.5 })
      let depth = p.depth ?? 0, mix = p.mix ?? 0
      const gate = lfoGate(node)
      gate(depth > LFO_EPS && mix > LFO_EPS)
      return {
        node,
        set: (k, v) => {
          if (rate.set(k, v)) return
          if (k === 'depth') { depth = v; node.depth.value = v }
          else if (k === 'base') node.baseFrequency = v
          else { mix = v; node.wet.value = v }
          gate(depth > LFO_EPS && mix > LFO_EPS)
        },
        dispose: () => { rate.dispose(); node.dispose() },
      }
    }
    case 'trem': {
      const rate = makeRate(p, hz => { node.frequency.value = hz })
      const node = new Tone.Tremolo({ frequency: rate.hz0, depth: p.depth, wet: p.mix, spread: 60 })
      let depth = p.depth ?? 0, mix = p.mix ?? 0
      const gate = lfoGate(node)
      gate(depth > LFO_EPS && mix > LFO_EPS)
      return {
        node,
        set: (k, v) => {
          if (rate.set(k, v)) return
          if (k === 'depth') { depth = v; node.depth.value = v }
          else { mix = v; node.wet.value = v }
          gate(depth > LFO_EPS && mix > LFO_EPS)
        },
        dispose: () => { rate.dispose(); node.dispose() },
      }
    }
    case 'autopan': {
      const rate = makeRate(p, hz => { node.frequency.value = hz })
      const node = new Tone.AutoPanner({ frequency: rate.hz0, depth: p.depth })
      let depth = p.depth ?? 0
      const gate = lfoGate(node)
      gate(depth > LFO_EPS)
      return {
        node,
        set: (k, v) => {
          if (rate.set(k, v)) return
          depth = v; node.depth.value = v
          gate(depth > LFO_EPS)
        },
        dispose: () => { rate.dispose(); node.dispose() },
      }
    }
    case 'vib': {
      const rate = makeRate(p, hz => { node.frequency.value = hz })
      const node = new Tone.Vibrato({ frequency: rate.hz0, depth: p.depth, wet: p.mix })
      return {
        node,
        set: (k, v) => {
          if (rate.set(k, v)) return
          if (k === 'depth') node.depth.value = v
          else node.wet.value = v
        },
        dispose: () => { rate.dispose(); node.dispose() },
      }
    }
    case 'cheby': {
      const node = new Tone.Chebyshev({ order: Math.max(1, p.order | 0), wet: p.mix })
      try { (node as any).oversample = '4x' } catch { /* optional */ }
      return {
        node,
        set: (k, v) => {
          if (k === 'order') node.order = Math.max(1, v | 0)
          else node.wet.value = v
        },
        dispose: () => node.dispose(),
      }
    }
    case 'widen': {
      const node = new Tone.StereoWidener(p.width)
      return {
        node,
        set: (_k, v) => { node.width.value = v },
        dispose: () => node.dispose(),
      }
    }
    case 'shift': {
      const node = new Tone.FrequencyShifter({ frequency: p.amt, wet: p.mix })
      return {
        node,
        set: (k, v) => {
          if (k === 'amt') node.frequency.value = v
          else node.wet.value = v
        },
        dispose: () => node.dispose(),
      }
    }
    case 'duck': {
      // Volume ducking ("sidechain pump"). Two modes:
      //  · tempo (default): gain dips on each beat boundary and recovers over
      //    the cycle, driven by the engine frame tick — no clock drift.
      //  · trigger: a source track (or one drum pad) fires trigger(); the dip
      //    starts at that note's audio time and recovers over one Rate cycle.
      // srcOn is pushed by the engine from the fx map's srcTrack field.
      const node = new Tone.Gain(1)
      let amount = p.amount, curve = p.curve, rate = p.rate | 0
      let srcOn = 0
      let trigAt = -1   // audio time (s) of the most recent trigger
      return {
        node,
        set: (k, v) => {
          if (k === 'amount') amount = v
          else if (k === 'curve') curve = v
          else if (k === 'rate') rate = v | 0
          else if (k === 'srcOn') srcOn = v
        },
        trigger: when => {
          const at = when ?? Tone.now()
          if (at > trigAt) trigAt = at
        },
        tick: (posTicks, _playing, bpm, time) => {
          const at = time ?? Tone.now()
          const cyc = DUCK_DIV_TICKS[Math.max(0, Math.min(DUCK_DIV_TICKS.length - 1, rate))] || 96
          let phase: number
          if (srcOn) {
            if (trigAt < 0) { node.gain.setTargetAtTime(1, at, 0.004); return }
            const cycSec = cyc * (60 / (Math.max(1, bpm || 120) * 96))   // ticks → seconds
            phase = Math.min(1, Math.max(0, (at - trigAt) / cycSec))
          } else {
            phase = (((posTicks % cyc) + cyc) % cyc) / cyc
          }
          const rec = Math.pow(phase, 0.35 + curve * 1.6) // recovery shape
          const g = (1 - amount) + amount * rec
          node.gain.setTargetAtTime(Math.max(0, g), at, 0.004)
        },
        dispose: () => node.dispose(),
      }
    }
    case 'autotune': {
      // Monophonic auto-tune. Preferred path: the sf-autotune worklet — YIN
      // detection AND the pitch shifter run on the audio thread in one node
      // (the old version burned ~1M multiply-adds per animation frame on the
      // MAIN thread), and it works inside OfflineAudioContext, so exports and
      // track freezes finally reproduce Auto-Tune instead of skipping it.
      const wn = fxWorkletNode('sf-autotune', {
        amount: p.amount ?? 1, speed: p.speed ?? 20, mix: p.mix ?? 1, mode: (p.mode ?? 0) | 0,
        root: (meta.get('root') ?? 9) as number, mask: scaleMask((meta.get('scale') ?? 'minor') as string),
      })
      if (wn) {
        // follow live project key/scale changes (offline renders fix the key
        // at build time and never observe — the doc must not leak observers)
        const offline = Tone.getContext().rawContext?.constructor?.name?.includes('Offline')
        const pushKey = () => {
          wn.port.postMessage({ k: 'root', v: (meta.get('root') ?? 9) as number })
          wn.port.postMessage({ k: 'mask', v: scaleMask((meta.get('scale') ?? 'minor') as string) })
        }
        const onMeta = (ev: { keysChanged: Set<string> }) => {
          if (ev.keysChanged.has('root') || ev.keysChanged.has('scale')) pushKey()
        }
        if (!offline) meta.observe(onMeta as any)
        return {
          node: wn as unknown as Tone.ToneAudioNode,
          set: (k, v) => wn.port.postMessage({ k, v: k === 'mode' ? v | 0 : v }),
          dispose: () => {
            if (!offline) { try { meta.unobserve(onMeta as any) } catch { /* ok */ } }
            try { wn.disconnect() } catch { /* ok */ }
          },
        }
      }
      // fallback: main-thread detector driving Tone.PitchShift (worklet unavailable).
      // The analyser taps the DRY input (engine wires prev→detect).
      const ctx = Tone.getContext().rawContext as AudioContext
      const node = new Tone.PitchShift({ windowSize: 0.06, delayTime: 0, feedback: 0 })
      node.wet.value = p.mix ?? 1
      const detect = ctx.createAnalyser()
      detect.fftSize = 2048
      const buf = new Float32Array(detect.fftSize)
      const sr = ctx.sampleRate
      let amount = p.amount ?? 1, speedMs = p.speed ?? 20, mode = (p.mode ?? 0) | 0
      let cur = 0 // current shift (semitones), smoothed toward the target
      return {
        node,
        detect,
        set: (k, v) => {
          if (k === 'amount') amount = v
          else if (k === 'speed') speedMs = v
          else if (k === 'mix') node.wet.value = v
          else if (k === 'mode') mode = v | 0
        },
        tick: () => {
          detect.getFloatTimeDomainData(buf)
          let rms = 0
          for (let i = 0; i < buf.length; i++) rms += buf[i] * buf[i]
          rms = Math.sqrt(rms / buf.length)
          let desired = cur
          if (rms > 0.004) {
            const f = detectPitchHz(buf, sr)
            if (f > 0) {
              const midi = 69 + 12 * Math.log2(f / 440)
              let target: number
              if (mode === 1) target = Math.round(midi)
              else target = snapToScale(Math.round(midi), (meta.get('root') ?? 9) as number, (meta.get('scale') ?? 'minor') as string)
              desired = Math.max(-12, Math.min(12, (target - midi) * amount))
            }
          }
          // exponential glide toward target; ~speedMs time constant at 60fps
          const coeff = 1 - Math.exp(-1 / Math.max(1, (speedMs / 1000) * 60))
          cur += (desired - cur) * coeff
          node.pitch = cur
        },
        dispose: () => { node.dispose(); try { detect.disconnect() } catch { /* ok */ } },
      }
    }
    default: {
      const node = new Tone.Gain(1)
      return { node, set: () => {}, dispose: () => node.dispose() }
    }
  }
}
