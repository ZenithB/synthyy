// Offline audio export. Rebuilds the WHOLE project graph inside Tone.Offline at
// the engine's 2x rate (88.2kHz) — mirroring the live signal path so the bounce
// matches what you hear: instrument → per-device output gains → fx chain →
// pan → volume → master, plus send/return buses, audio clips, the sampler,
// live MIDI-fx, sidechain ducking, LFOs and (scene) automation envelopes.
// Then resamples to 44.1kHz and encodes to WAV or MP3, stereo / mono / stems.
//
// Note: Auto-Tune is NOT reproduced offline (its pitch detector needs a live
// AnalyserNode, which doesn't run in an OfflineAudioContext) — use Record
// Output for an auto-tuned bounce. Macros are already baked into param values.

import * as Tone from 'tone'
import { BAR, PPQ, Note, clamp } from '../types'
import { exportProject, arrEndTicks, meta as docMeta, ProjectJSON, TrackJSON, ClipJSON, FxJSON } from '../state/doc'
import { makeInstrument, makeEffect, setRateBpm, Inst, Fx } from './devices'
import { instSchema, fxSchema, mixSpec, lfoShapeValue, LFO_DIV_TICKS, LFO_PARAMS, midiFxSchema, valueFromSpec } from './schema'
import { applyMidiFx } from './midifx'
import { getSampleBuffer } from './samples'
import { clipAudioBuffer, configureAudioPlayer } from './audioclip'
import { resample, encodeAudio, download, extFor, AudioFormat, Channels, OUT_SR } from './encode'
import { toast } from '../state/store'

// Preferred offline render rate (mirrors the engine's 2x oversampled graph). The
// ACTUAL rate is taken from the live context at render time and falls back to
// 44.1kHz — older Safari/WebKit reject an OfflineAudioContext at non-44.1k
// rates, which silently broke every bounce while live playback (which has its
// own rate fallback) kept working.
const RENDER_SR = 88200

function ticksToSec(ticks: number, bpm: number) {
  return (ticks / PPQ) * (60 / bpm)
}

export type RenderScope =
  | { kind: 'arr' }
  | { kind: 'loop' }
  | { kind: 'scene'; sceneId: string }

export type ExportOptions = {
  format: AudioFormat
  channels: Channels
  kbps?: number
  stems?: boolean
}

// What slice of the project a single render pass should capture.
type RenderTarget =
  | { kind: 'mix' }
  | { kind: 'track'; idx: number }     // one track only, dry (no return/bus tails)
  | { kind: 'return'; idx: number }    // one return bus, fed by every track's sends
  | { kind: 'busstem'; busId: string } // one user bus, fed by every track's sends

type OffFx = { id: string; type: string; dev: OffDevice; out: Tone.Volume }
type OffTrack = {
  json: TrackJSON
  inst: Inst
  fx: OffFx[]
  panner: StereoPannerNode
  vol: Tone.Volume
  sendA: Tone.Gain
  sendB: Tone.Gain
  loopLen: number
  envEntries: [string, { t: number; v: number }[]][] | null
  // live MIDI-fx modulation (LFO/macro → arp/chord/…): the same override + Part
  // regeneration the live engine does, so bounces match playback.
  midiParts: { part: Tone.Part; rebuild: () => void }[]
  midiMod: Map<string, number>       // "fxId|pkey" → live value
  midiQuant: Map<string, number>     // last quantized signature
  midiDirty: boolean
  midiRebuildAt: number
  // free-LFO phase integration so an LFO can modulate another LFO's rate offline
  lfoPhase: Map<string, number>
  lfoRateMod: Map<string, number>    // lfoId → modulated Hz (applied next frame)
  lfoLastTime: number
}

// Tone.PluckSynth (Karplus-Strong) throws AbortError inside an OfflineAudioContext,
// which kills the whole render. Substitute an offline-safe plucky synth so the
// bounce still completes; Record Output captures the real PluckSynth live.
function makeOffInstrument(type: string, params: Record<string, number>, buf?: AudioBuffer, padBuffers?: Map<number, AudioBuffer>): Inst {
  if (type !== 'pluck') return makeInstrument(type, params, buf, padBuffers)
  const out = new Tone.Gain(0.9)
  const synth = new Tone.PolySynth(Tone.Synth).connect(out)
  synth.set({ oscillator: { type: 'triangle' }, envelope: { attack: 0.002, decay: 0.45, sustain: 0, release: 0.4 } })
  const hz = (pp: number) => Tone.Frequency(pp, 'midi').toFrequency()
  return {
    out,
    set: () => {},
    noteOn: (pp, vel) => synth.triggerAttack(hz(pp), undefined, vel),
    noteOff: pp => synth.triggerRelease(hz(pp)),
    trigger: (pp, dur, time, vel) => synth.triggerAttackRelease(hz(pp), Math.min(dur, 0.6), time, vel),
    dispose: () => { synth.dispose(); out.dispose() },
  }
}

// A device with explicit input/output nodes, so reverb (a composite) and normal
// single-node effects share one chain-wiring path.
type OffDevice = { in: Tone.ToneAudioNode; out: Tone.ToneAudioNode; set: (k: string, v: number) => void; tick?: Fx['tick']; trigger?: Fx['trigger'] }

// Tone.Reverb generates its impulse response with a NESTED Tone.Offline render,
// which aborts inside our outer offline render. Build reverb as a Convolver fed a
// synthesized decaying-noise IR + a dry/wet mix instead (no nested rendering).
function makeOffDevice(type: string, params: Record<string, number>): OffDevice {
  if (type === 'reverb') {
    const ctx = Tone.getContext().rawContext as unknown as BaseAudioContext
    const decay = Math.max(0.1, params.size ?? 2.2)
    const len = Math.max(1, Math.floor(decay * ctx.sampleRate))
    const ir = ctx.createBuffer(2, len, ctx.sampleRate)
    for (let c = 0; c < 2; c++) {
      const d = ir.getChannelData(c)
      for (let i = 0; i < len; i++) d[i] = (Math.random() * 2 - 1) * Math.pow(1 - i / len, 2.5)
    }
    const mix = params.mix ?? 0.3
    const input = new Tone.Gain()
    const output = new Tone.Gain()
    const conv = (ctx as unknown as BaseAudioContext).createConvolver()
    conv.normalize = true
    conv.buffer = ir
    const wet = new Tone.Gain(mix)
    const dry = new Tone.Gain(1 - mix)
    input.connect(dry); dry.connect(output)
    Tone.connect(input, conv); Tone.connect(conv, wet); wet.connect(output)
    return { in: input, out: output, set: (k, v) => { if (k === 'mix') { wet.gain.value = v; dry.gain.value = 1 - v } } }
  }
  const fx = makeEffect(type, params)
  return { in: fx.node, out: fx.node, set: fx.set, tick: fx.tick, trigger: fx.trigger }
}

function envValueAt(pts: { t: number; v: number }[], pos: number, loop: number): number {
  if (pts.length === 1) return pts[0].v
  if (pos <= pts[0].t) return pts[0].v
  const last = pts[pts.length - 1]
  if (pos >= last.t) return last.v
  for (let i = 0; i < pts.length - 1; i++) {
    const a = pts[i], b = pts[i + 1]
    if (pos >= a.t && pos <= b.t) return a.v + (b.v - a.v) * ((pos - a.t) / Math.max(1, b.t - a.t))
  }
  return last.v
}

/** Build one track's full signal chain into the current (offline) context. */
function buildOffTrack(t: TrackJSON, master: Tone.Volume, returnInputs: Tone.ToneAudioNode[], toMaster: boolean, legacyAB = true): OffTrack {
  const buf = (t.inst.type === 'sampler' || t.inst.type === 'ksampler' || t.inst.type === 'granular') ? getSampleBuffer(t.inst.sampleId || '') : undefined
  let padBuffers: Map<number, AudioBuffer> | undefined
  if (t.inst.type === 'drum' && t.inst.padSamples) {
    padBuffers = new Map()
    for (const [k, sid] of Object.entries(t.inst.padSamples)) { const b = getSampleBuffer(sid); if (b) padBuffers.set(+k, b) }
  }
  const inst = makeOffInstrument(t.inst.type, t.inst.params, buf, padBuffers)
  const fx: OffFx[] = []
  t.fx.forEach(f => {
    if (!f.on) return
    const dev = makeOffDevice(f.type, f.params)
    fx.push({ id: f.id || f.type, type: f.type, dev, out: new Tone.Volume(f.out ?? 0) })
  })
  const rawCtx = Tone.getContext().rawContext as unknown as BaseAudioContext
  const panner = rawCtx.createStereoPanner()
  panner.pan.value = t.pan
  const vol = new Tone.Volume(t.gain)
  const instOut = new Tone.Volume(t.inst.out ?? 0)
  Tone.connect(inst.out, instOut)
  let prev: Tone.ToneAudioNode = instOut
  fx.forEach(f => {
    Tone.connect(prev, f.dev.in)
    Tone.connect(f.dev.out, f.out)
    prev = f.out
  })
  Tone.connect(prev, panner)
  Tone.connect(panner, vol)
  // Buses route their fader to `output` (master/another bus) in wireBuses, after
  // every track exists; normal tracks feed master directly.
  if (toMaster && t.kind !== 'bus') vol.connect(master)
  // always feed the send gains so the bus-routing block can pick them up; only
  // wire them to the LEGACY return channels when there are no A/B buses (mirrors
  // the live engine's exclusive `sendBusInput() ?? builtReturns` semantics).
  const sendA = new Tone.Gain(t.sendA ?? 0)
  const sendB = new Tone.Gain(t.sendB ?? 0)
  vol.connect(sendA); vol.connect(sendB)
  if (legacyAB && returnInputs.length) {
    if (returnInputs[0]) sendA.connect(returnInputs[0])
    if (returnInputs[1]) sendB.connect(returnInputs[1])
  }
  return {
    json: t, inst, fx, panner, vol, sendA, sendB, loopLen: BAR, envEntries: null,
    midiParts: [], midiMod: new Map(), midiQuant: new Map(), midiDirty: false, midiRebuildAt: 0,
    lfoPhase: new Map(), lfoRateMod: new Map(), lfoLastTime: 0,
  }
}

/** Resolve an LFO/automation target to its spec, base value and live setter. */
function resolveTarget(ot: OffTrack, dest: string, fxId: string, pkey: string) {
  if (dest === 'inst') {
    const spec = instSchema(ot.json.inst.type).params.find(s => s.key === pkey)
    const base = ot.json.inst.params[pkey]
    if (!spec || typeof base !== 'number') return null
    return { spec, base, setter: (v: number) => ot.inst.set(pkey, v) }
  }
  if (dest === 'mix') {
    const spec = mixSpec(pkey)
    const base = pkey === 'gain' ? ot.json.gain : ot.json.pan
    if (!spec || typeof base !== 'number') return null
    return { spec, base, setter: (v: number) => { if (pkey === 'gain') ot.vol.volume.value = v; else ot.panner.pan.value = v } }
  }
  if (dest === 'send') {
    const node = pkey === 'B' ? ot.sendB : ot.sendA
    const base = (pkey === 'B' ? ot.json.sendB : ot.json.sendA) ?? 0
    const spec = { key: 'send', label: 'Send', min: 0, max: 1, def: 0 } as any
    return { spec, base, setter: (v: number) => { node.gain.value = v } }
  }
  if (dest === 'macro') {
    // A macro's value fans out to its mapped params (mirrors the live engine).
    const m = ot.json.macros?.[+pkey]
    if (!m) return null
    const spec = { key: 'macro', label: 'Macro', min: 0, max: 1, def: 0 } as any
    const setter = (v: number) => m.targets?.forEach(tg => {
      const r = resolveTarget(ot, tg.dest, tg.fxId || '', tg.pkey)
      if (r) r.setter(clamp(r.spec.min + (r.spec.max - r.spec.min) * v, r.spec.min, r.spec.max))
    })
    return { spec, base: m.value ?? 0, setter }
  }
  if (dest === 'midi') {
    // A MIDI-fx param (arp/chord/velocity/random): stash the value + mark the
    // track dirty so applyMod regenerates its notes. fxId = midifx id.
    const mfx = ot.json.midifx?.find(d => (d.id || d.type) === fxId)
    if (!mfx) return null
    const spec = midiFxSchema(mfx.type).params.find(s => s.key === pkey)
    const base = mfx.params[pkey]
    if (!spec || typeof base !== 'number') return null
    const key = `${fxId}|${pkey}`
    const quantOf = (v: number) => (spec.int || spec.steps) ? Math.round(v) : Math.round((v - spec.min) / ((spec.max - spec.min) || 1) * 40)
    return {
      spec, base,
      setter: (v: number) => {
        ot.midiMod.set(key, v)
        const q = quantOf(v)
        if (ot.midiQuant.get(key) !== q) { ot.midiQuant.set(key, q); ot.midiDirty = true }
      },
    }
  }
  if (dest === 'lfo') {
    // Another LFO's rate (Hz); free-running only. Setter stashes the effective
    // Hz, read next frame by the phase integrator in applyMod.
    const target = ot.json.lfos?.find(l => l.id === fxId)
    if (!target) return null
    return { spec: LFO_PARAMS[2], base: target.hz ?? 1, setter: (v: number) => ot.lfoRateMod.set(fxId, v) }
  }
  const fxJson = ot.json.fx.find(f => (f.id || f.type) === fxId)
  const bf = ot.fx.find(f => f.id === fxId)
  if (!fxJson || !bf) return null
  const spec = fxSchema(fxJson.type).params.find(s => s.key === pkey)
  const base = fxJson.params[pkey]
  if (!spec || typeof base !== 'number') return null
  return { spec, base, setter: (v: number) => bf.dev.set(pkey, v) }
}

/** Per-frame modulation (sidechain tick + LFOs + scene automation), mirrors the live loop. */
function applyMod(ot: OffTrack, posTicks: number, audioTime: number, bpm: number, withAutomation: boolean) {
  ot.fx.forEach(f => f.dev.tick?.(posTicks, true, bpm))

  const controlled = new Map<string, { dest: string; fxId: string; pkey: string }>()
  const autoNorm = new Map<string, number>()
  const lfoOff = new Map<string, number>()

  if (withAutomation && ot.envEntries) {
    const loop = ot.loopLen || BAR
    const pos = (((posTicks) % loop) + loop) % loop
    for (const [k, pts] of ot.envEntries) {
      if (!pts.length) continue
      autoNorm.set(k, envValueAt(pts, pos, loop))
      const [dest, fxId, pkey] = k.split('|')
      controlled.set(k, { dest, fxId: fxId || '', pkey })
    }
  }

  // per-frame delta for free-LFO phase integration (a synced target keeps its
  // tempo division; a free target integrates its possibly-modulated Hz)
  const dt = Math.min(0.1, Math.max(0, audioTime - (ot.lfoLastTime || audioTime)))
  ot.lfoLastTime = audioTime
  ot.json.lfos?.forEach(lfo => {
    const id = lfo.id || ''
    let raw: number
    if (lfo.sync) {
      raw = lfoShapeValue(lfo.shape | 0, posTicks / (LFO_DIV_TICKS[lfo.rate | 0] || 384) + (lfo.phase ?? 0))
    } else {
      const effHz = ot.lfoRateMod.get(id) ?? (lfo.hz ?? 1)
      const ph = (ot.lfoPhase.get(id) ?? 0) + dt * effHz
      ot.lfoPhase.set(id, ph)
      raw = lfoShapeValue(lfo.shape | 0, ph + (lfo.phase ?? 0))
    }
    if (!lfo.on) return
    // multi-target list; fall back to the legacy single-field mapping
    const tgs = lfo.targets?.length
      ? lfo.targets
      : (lfo.dest && lfo.pkey ? [{ dest: lfo.dest, fxId: lfo.fxId || '', pkey: lfo.pkey }] : [])
    for (const tg of tgs) {
      const k = `${tg.dest}|${tg.fxId || ''}|${tg.pkey}`
      lfoOff.set(k, (lfoOff.get(k) ?? 0) + raw * (lfo.depth ?? 0.5))
      controlled.set(k, { dest: tg.dest, fxId: tg.fxId || '', pkey: tg.pkey })
    }
  })

  for (const [k, tg] of controlled) {
    const r = resolveTarget(ot, tg.dest, tg.fxId, tg.pkey)
    if (!r) continue
    const base = autoNorm.has(k) ? valueFromSpec(r.spec, autoNorm.get(k)!) : r.base
    const off = (lfoOff.get(k) ?? 0) * (r.spec.max - r.spec.min) * 0.5
    r.setter(clamp(base + off, r.spec.min, r.spec.max))
  }

  // a modulated MIDI-fx value crossed a step → regenerate the notes (throttled
  // ≤20 Hz, same as the live engine)
  if (ot.midiDirty && audioTime - ot.midiRebuildAt >= 0.05) {
    ot.midiRebuildAt = audioTime
    ot.midiDirty = false
    ot.midiParts.forEach(mp => mp.rebuild())
  }
}

async function renderBuffer(
  project: ProjectJSON,
  scope: RenderScope,
  target: RenderTarget,
  fromTicks: number,
  lengthTicks: number,
  sceneId?: string,
): Promise<AudioBuffer> {
  const bpm = project.meta.bpm
  const root = project.meta.root ?? 9
  const scaleId = project.meta.scale ?? 'minor'
  const durSec = ticksToSec(lengthTicks, bpm) + 1.5 // reverb/release tail
  // Render at the SAME rate the live engine is actually running (read before
  // Tone.Offline swaps the context) so the bounce matches what's heard; if the
  // browser won't open an OfflineAudioContext at that rate, fall back below.
  const liveSr = Tone.getContext().sampleRate || RENDER_SR

  const build: Parameters<typeof Tone.Offline>[0] = async ({ transport }) => {
    transport.PPQ = PPQ
    transport.bpm.value = bpm
    setRateBpm(bpm)   // synced fx rate knobs derive Hz from this at build time
    transport.swing = project.meta.swing
    transport.swingSubdivision = '16n'

    // The multiband effect (mbcomp) is an AudioWorklet — register its module on
    // THIS offline context so it bounces like it sounds. ONLY when the bounce
    // actually uses it: OfflineAudioContext.audioWorklet.addModule() can stall
    // until startRendering() in some browsers, and Tone.Offline awaits this build
    // callback *before* it renders — so an unconditional await deadlocks every
    // export. We also cap it with a timeout; if the module isn't ready, mbcomp
    // degrades to a clean passthrough (makeEffect handles the missing node).
    const usesMb = project.tracks.some(t => (t.fx ?? []).some(f => f.on && f.type === 'mbcomp'))
      || (project.masterFx ?? []).some(f => f.on && f.type === 'mbcomp')
    if (usesMb) {
      const raw = Tone.getContext().rawContext as any
      if (raw?.audioWorklet) {
        await Promise.race([
          raw.audioWorklet.addModule('/sf-mbdyn.js').catch(() => {}),
          new Promise<void>(res => setTimeout(res, 1500)),
        ])
      }
    }

    // sidechain trigger routing (mirrors the live engine): source track id →
    // duck devices it fires; populated as each duck fx is built below.
    const duckTriggers = new Map<string, { dev: OffDevice; pitch: number }[]>()
    const wireDuck = (f: FxJSON, dev: OffDevice) => {
      if (f.type !== 'duck' || !f.srcTrack) return
      dev.set('srcOn', 1)
      const list = duckTriggers.get(f.srcTrack) ?? []
      list.push({ dev, pitch: f.srcPitch ?? -1 })
      duckTriggers.set(f.srcTrack, list)
    }
    const fireDucks = (tid: string, pitch: number, when: number) => {
      const list = duckTriggers.get(tid)
      if (!list) return
      const pad = ((pitch % 8) + 8) % 8
      for (const d of list) if (d.pitch < 0 || d.pitch === pad) d.dev.trigger?.(when)
    }

    // stereo-preserving master (Tone.Channel would downmix)
    const master = new Tone.Volume(project.meta.masterGain ?? 0)
    const limiter = new Tone.Limiter(-1)
    limiter.connect(Tone.getDestination())
    // master-bus effect chain (master → [fx…] → limiter) — applied to the FULL
    // mix only, mirroring live playback; individual stems stay pre-master.
    const masterOffFx: OffDevice[] = []
    let mtail: Tone.ToneAudioNode = master
    if (target.kind === 'mix') {
      ;(project.masterFx ?? []).forEach(f => {
        if (!f.on) return
        const dev = makeOffDevice(f.type, f.params)
        const out = new Tone.Volume(f.out ?? 0)
        Tone.connect(mtail, dev.in); Tone.connect(dev.out, out)
        masterOffFx.push(dev)
        wireDuck(f, dev)
        mtail = out
      })
    }
    Tone.connect(mtail, limiter)

    // return buses (none for a dry track/bus stem; all for the mix; one for a return stem)
    const returns: { input: Tone.ToneAudioNode }[] = []
    const wantReturns = target.kind === 'mix' || target.kind === 'return'
    if (wantReturns) {
      ;(project.returns ?? []).forEach((r, ri) => {
        if (target.kind === 'return' && ri !== target.idx) { returns.push({ input: new Tone.Gain() }); return }
        const dev = makeOffDevice(r.fxType, r.params)
        const channel = new Tone.Volume(r.gain ?? 0)
        dev.out.connect(channel)
        channel.connect(master)
        returns.push({ input: dev.in })
      })
    }
    const returnInputs = returns.map(r => r.input)
    // legacy A/B return wiring only applies when the project has no A/B send buses
    const legacyAB = !project.tracks.some(t => t.kind === 'bus' && (t.send === 'A' || t.send === 'B'))

    const offTracks: OffTrack[] = []
    project.tracks.forEach((t, idx) => {
      if (target.kind === 'track' && idx !== target.idx) return
      // muted tracks are silent in the full mix and in any bus they feed
      if (t.mute && (target.kind === 'mix' || target.kind === 'busstem')) return
      // tracks feed master only on the mix and their own dry stem; on a return/bus
      // stem we want just the wet send, so they reach master only via the bus chain
      const toMaster = target.kind === 'mix' || target.kind === 'track'
      const ot = buildOffTrack(t, master, returnInputs, toMaster, legacyAB)
      offTracks.push(ot)
      t.fx.forEach(f => { if (f.on) { const bf = ot.fx.find(x => x.id === (f.id || f.type)); if (bf) wireDuck(f, bf.dev) } })

      const isDrum = t.kind === 'drum'
      // keep the id so live MIDI-fx overrides (LFO/macro → arp/chord/…) can be
      // folded in per rebuild; applyMidiFx itself only needs type/on/params
      const midifx = (t.midifx ?? []).map(d => ({ id: d.id || d.type, type: d.type, on: d.on, params: d.params }))
      const tid = t.id ?? t.name

      // The clip's internal pattern always loops (loopEnd = lenTicks). `infinite`
      // controls whether it runs forever (scene preview) or STOPS at start+len
      // (arrangement). Arrangement clips MUST be bounded — mirroring the live
      // buildArrParts — otherwise a clip loops past the bounce window, and its
      // notes overlap into a synth's envelope out of order ("Time must be greater
      // than or equal to the last scheduled time"), aborting the whole render.
      const scheduleClip = (clip: ClipJSON, startTicks: number, lenTicks: number, infinite: boolean) => {
        if (clip.audio) {
          const a = clip.audio
          const raw = getSampleBuffer(a.sampleId || '')
          if (!raw) return
          // identical crop + crossfade + pitch/cents/reverse/fades as live playback
          const buf = clipAudioBuffer(a.sampleId || '', raw, a.offset, a.dur, !!a.loop, a.xfade) as any
          // Session (scene) render loops the scene, so a NON-looping clip re-fires
          // from its head each clip-length — mirroring live session playback. A
          // looping clip streams continuously; arrangement clips play once.
          if (infinite && !a.loop) {
            for (let s = startTicks; s < lengthTicks; s += Math.max(1, lenTicks)) {
              const p = new Tone.Player(buf); configureAudioPlayer(p, a); Tone.connect(p, ot.inst.out)
              try { p.sync().start(`${s}i`) } catch { /* ok */ }
            }
            return
          }
          const player = new Tone.Player(buf)
          configureAudioPlayer(player, a)
          Tone.connect(player, ot.inst.out)
          try { const s = player.sync().start(`${startTicks}i`); if (!infinite) s.stop(`${startTicks + lenTicks}i`) } catch { /* ok */ }
          return
        }
        const rawNotes: Note[] = Object.values(clip.notes).map(n => ({ ...n }))
        // Expand notes through the MIDI-fx chain, folding in any live override
        // (LFO/macro → MIDI-fx param) so a rebuild re-derives the arp/chord/etc.
        const expand = () => {
          const merged = midifx.map(d => {
            let params = d.params
            for (const s of midiFxSchema(d.type).params) {
              const ov = ot.midiMod.get(`${d.id}|${s.key}`)
              if (ov !== undefined) { if (params === d.params) params = { ...d.params }; params[s.key] = ov }
            }
            return { type: d.type, on: d.on, params }
          })
          return applyMidiFx(merged, rawNotes.map(n => ({ ...n })), lenTicks, root, scaleId, isDrum).map(n => ({ time: `${n.s}i`, ...n }))
        }
        const part = new Tone.Part((time, ev: any) => {
          if (ev.pr < 1 && Math.random() > ev.pr) return
          // Two notes on the exact same tick hitting one oscillator-based voice
          // (mono synth, kick/perc pad) restart its Source at a non-increasing time
          // → "Start time must be strictly greater than previous start time", which
          // aborts the bounce. Skip the duplicate (inaudible anyway on a mono voice).
          try { ot.inst.trigger(ev.p, Math.max(0.02, Tone.Ticks(ev.d).toSeconds()), time, ev.v) } catch { /* ok */ }
          fireDucks(tid, ev.p, time)
        }, expand() as any)
        part.loop = true
        part.loopStart = 0
        part.loopEnd = `${lenTicks}i`
        // MIDI-fx-bearing clips register for regeneration; rebuild only actually
        // fires when an override changes (ot.midiDirty), so this is ~free otherwise
        if (midifx.length) ot.midiParts.push({ part, rebuild: () => { part.clear(); expand().forEach(ev => part.add(ev)) } })
        // never let one clip's scheduling abort the whole bounce (matches the
        // live engine's defensive try/catch around clip parts)
        try { part.start(`${startTicks}i`); if (!infinite) part.stop(`${startTicks + lenTicks}i`) } catch { /* ok */ }
      }

      if (scope.kind === 'scene') {
        const clip = project.clips[`${tid}|${sceneId}`]
        if (clip) {
          ot.loopLen = clip.len
          if (clip.env) ot.envEntries = Object.entries(clip.env)
          scheduleClip(clip, 0, clip.len, true)
        }
      } else {
        Object.values(project.arr).forEach(a => {
          if (a.trackId !== tid) return
          scheduleClip(a, a.start, a.len, false)   // bounded: play once across [start, start+len)
        })
      }
    })

    // ---- bus routing (mirrors the live engine's rewireBuses) ----
    // User buses are aux/send buses: every track feeds master AND sends copies to
    // bus inputs (the `sends` map); each bus's fader then routes to its `output`
    // (master or another bus). Wired here, after every track/bus node exists.
    if (target.kind === 'mix' || target.kind === 'busstem') {
      const idOf = (o: OffTrack) => o.json.id ?? o.json.name
      const buses = offTracks.filter(o => o.json.kind === 'bus')
      const busInputOf = (busId: string): Tone.ToneAudioNode | null =>
        buses.find(o => idOf(o) === busId)?.inst.out ?? null
      // 1) bus outputs → master or another bus's input
      buses.forEach(b => {
        const out = b.json.output || 'master'
        let dest: Tone.ToneAudioNode = (out !== 'master' && busInputOf(out)) || master
        if (target.kind === 'busstem') {
          // capture only THIS bus: it goes to master regardless of its real output;
          // other buses stay off master (they reach it only via a bus→bus chain)
          if (idOf(b) === target.busId) dest = master
          else if (dest === master) return
        }
        Tone.connect(b.vol, dest)
      })
      // 2) per-track (and per-bus) sends → bus inputs
      offTracks.forEach(ot => {
        const sends = ot.json.sends
        if (!sends) return
        for (const [bid, level] of Object.entries(sends)) {
          if (!level) continue
          if (bid === idOf(ot)) {
            // Feedback bus loops into itself through a short delay (mirrors the
            // live engine); other self-sends are skipped as before.
            if (ot.json.send === 'F') {
              const d = new Tone.Delay({ delayTime: 0.09, maxDelay: 1 })
              const g = new Tone.Gain(level)
              Tone.connect(ot.vol, d); Tone.connect(d, g); Tone.connect(g, ot.inst.out)
            }
            continue
          }
          const inp = busInputOf(bid)
          if (!inp) continue
          const g = new Tone.Gain(level)
          Tone.connect(ot.vol, g)
          Tone.connect(g, inp)
        }
      })
      // 3) built-in A/B sends → the A/B buses (tagged json.send); gain already set
      const sendBus = (which: 'A' | 'B') => buses.find(o => o.json.send === which)?.inst.out ?? null
      const aIn = sendBus('A'), bIn = sendBus('B')
      offTracks.forEach(ot => {
        if (ot.json.kind === 'bus') return
        if (aIn) Tone.connect(ot.sendA, aIn)
        if (bIn) Tone.connect(ot.sendB, bIn)
      })
    }

    // per-frame modulation: sidechain ducking, LFOs, and (scene) automation
    const withAutomation = scope.kind === 'scene'
    transport.scheduleRepeat(time => {
      const posTicks = transport.getTicksAtTime(time)
      for (const ot of offTracks) applyMod(ot, posTicks, time, bpm, withAutomation)
      masterOffFx.forEach(d => d.tick?.(posTicks, true, bpm))
    }, '32n', 0)

    transport.start(0.02, `${fromTicks}i`)
  }

  let rendered: Tone.ToneAudioBuffer
  try {
    rendered = await Tone.Offline(build, durSec, 2, liveSr)
  } catch (e) {
    // e.g. older Safari rejecting a non-44.1k OfflineAudioContext. Retry once at
    // the universally-supported rate so the export still completes (resample()
    // already normalises everything to 44.1kHz before encoding).
    if (liveSr === OUT_SR) throw e
    console.warn(`Offline render at ${liveSr}Hz failed (${(e as Error)?.message || e}); retrying at ${OUT_SR}Hz`)
    rendered = await Tone.Offline(build, durSec, 2, OUT_SR)
  }

  return rendered.get() as AudioBuffer
}

function fileTitle(project: ProjectJSON) {
  return project.meta.title.replace(/[^\w\- ]+/g, '') || 'synthtagram'
}

function scopeBounds(project: ProjectJSON, scope: RenderScope): { from: number; ticks: number; sceneId?: string } | null {
  const loopStart = docMeta.get('loopStart') ?? 0
  const loopEnd = docMeta.get('loopEnd') ?? BAR * 4
  if (scope.kind === 'loop') return { from: loopStart, ticks: Math.max(BAR, loopEnd - loopStart) }
  if (scope.kind === 'scene') {
    let longest = BAR
    for (const [key, c] of Object.entries(project.clips)) if (key.endsWith(`|${scope.sceneId}`)) longest = Math.max(longest, c.len)
    return { from: 0, ticks: longest * 2, sceneId: scope.sceneId }
  }
  const ticks = Math.max(BAR, arrEndTicks())
  if (ticks <= BAR && Object.keys(project.arr).length === 0) return null
  return { from: 0, ticks }
}

/** Render + encode + download in the chosen format / channels, optionally as stems. */
export async function exportAudio(scope: RenderScope, opts: ExportOptions) {
  const project = exportProject()
  const title = fileTitle(project)
  const bounds = scopeBounds(project, scope)
  if (!bounds) { toast('Arrangement is empty — drag some clips in first'); return }
  const { from, ticks, sceneId } = bounds
  const ext = extFor(opts.format)
  const enc = (buf: AudioBuffer) => encodeAudio(buf, opts.format, opts.channels, opts.kbps)

  toast(opts.stems ? 'Rendering stems…' : `Rendering ${opts.format.toUpperCase()}…`)
  try {
    if (opts.stems) {
      for (let i = 0; i < project.tracks.length; i++) {
        // buses carry no clips of their own and are rendered as their own stems below
        if (project.tracks[i].kind === 'bus') continue
        const buf = await renderBuffer(project, scope, { kind: 'track', idx: i }, from, ticks, sceneId)
        download(enc(await resample(buf)), `${title} - ${project.tracks[i].name}.${ext}`)
        await new Promise(r => setTimeout(r, 250))
      }
      // return + user buses as their own stems so a stem set fully reconstructs the
      // mix (sends are additive, so dry tracks + return stems + bus stems sum back)
      for (let r = 0; r < (project.returns?.length ?? 0); r++) {
        const buf = await renderBuffer(project, scope, { kind: 'return', idx: r }, from, ticks, sceneId)
        download(enc(await resample(buf)), `${title} - ${project.returns![r].name || `Return ${r + 1}`}.${ext}`)
        await new Promise(r => setTimeout(r, 250))
      }
      for (const b of project.tracks.filter(t => t.kind === 'bus')) {
        const buf = await renderBuffer(project, scope, { kind: 'busstem', busId: b.id ?? b.name }, from, ticks, sceneId)
        download(enc(await resample(buf)), `${title} - ${b.name}.${ext}`)
        await new Promise(r => setTimeout(r, 250))
      }
      toast(`Exported stems ✓`)
    } else {
      const buf = await renderBuffer(project, scope, { kind: 'mix' }, from, ticks, sceneId)
      download(enc(await resample(buf)), `${title}.${ext}`)
      toast(`Exported ${opts.format.toUpperCase()} ✓`)
    }
  } catch (e) {
    console.error(e)
    const msg = (e as Error)?.message || String(e)
    toast(`Export failed: ${msg.slice(0, 120)}`)
  }
}

export function exportProjectFile() {
  const project = exportProject()
  const blob = new Blob([JSON.stringify(project, null, 2)], { type: 'application/json' })
  download(blob, `${project.meta.title.replace(/[^\w\- ]+/g, '') || 'project'}.synthtagram.json`)
  toast('Project file saved')
}
