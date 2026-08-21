// The BROWSER engine (Tone.js / Web Audio) — reference implementation of the
// EngineApi facade (engine-api.ts). UI code imports the facade from engine.ts,
// never this file; only web-only internals (recorder.ts's captureTap) bind to
// this concrete class.
//
// The conductor: owns the Tone.js context/transport, builds each track's
// instrument→fx→channel chain from the shared doc, and reacts to remote edits
// live (a friend's knob tweak retunes your audio within one frame).
//
// Playback is intentionally LOCAL per user (like every collaborative DAW):
// the *project* is shared in real time, the *transport* is yours.

import * as Tone from 'tone'
import * as Y from 'yjs'
import { BAR, STEP16, Note, clamp } from '../types'
import {
  meta, tracks, clips, arr, clipKey, notesOf, addNote, updateNotes,
  createClip, trackById, returns, ensureReturns, midifxOf, envKeys, envPoints, followOf, scenes, sceneIndex,
  isAudioClip, masterFx, masterAuto, barTicks, beatTicks, timeSig,
} from '../state/doc'
import { makeInstrument, makeEffect, setRateBpm, fxNeutral, Inst, Fx } from './devices'
import { instSchema, fxSchema, lfoShapeValue, LFO_DIV_TICKS, LFO_PARAMS, mixSpec, midiFxSchema, valueFromSpec, ParamSpec } from './schema'
import { getSampleBuffer, onSampleReady } from './samples'
import { clipAudioBuffer, configureAudioPlayer, audioFieldsFromMap, audioRate } from './audioclip'
import { applyMidiFx as applyMidiFxData } from './midifx'
import { renderFreezeTrack, FrozenTrackAudio, FrozenClip } from './render'
import { getAudioPrefs } from './prefs'
import { setUI, toast, ui } from '../state/store'
import type { EngineApi, EngineMode } from './engine-api'

const STOP = '__stop__'

// `bypassed`: the device's params make it audibly transparent (fxNeutral) and
// nothing (LFO/macro/automation) targets it, so the chain is wired AROUND it.
// The node stays built (disconnected nodes render ~free, and their internal
// state survives), so re-engaging it never resets the other devices' tails.
type BuiltFx = { id: string; type: string; fx: Fx; out: Tone.Volume; meter: Tone.Meter; bypassed: boolean }
type BuiltTrack = {
  id: string
  kind: string
  inst: Inst
  instOut: Tone.Volume
  instMeter: Tone.Meter
  fx: BuiltFx[]
  panner: StereoPannerNode
  vol: Tone.Volume
  meter: Tone.Meter
  sendA: Tone.Gain
  sendB: Tone.Gain
  muted: boolean                       // engine-tracked mute state (panner→vol cut idles the chain)
  outNode: Tone.ToneAudioNode | null   // node this track's fader currently outputs to (null for buses until wired)
  busSends: Map<string, Tone.Gain>     // per-bus send gains: busTrackId → gain (vol → gain → bus input)
  part: Tone.Part | null
  player: Tone.Player | null
  clipRepeat: number | null            // transport repeat id re-firing a non-looping session audio clip each scene loop
  partKey: string | null
  partStartTicks: number
  partLoopTicks: number
  queuedKey: string | null
  queuedPart: Tone.Part | null
  unobserve: (() => void) | null
  // Feedback bus only: its post-fader signal loops back into its own input
  // through a short delay (the delay makes the Web-Audio cycle valid + audible);
  // fbSend is the loop level (the "30%" send).
  fbDelay: Tone.Delay | null
  fbSend: Tone.Gain | null
}

type BuiltReturn = { id: string; fx: Fx; channel: Tone.Volume }

// Plain-JS snapshot of an LFO's config — read every frame by the modulation
// loop, so we deserialize the Y.Map once per change instead of per frame.
// `targets` is the (multi-)mapping list; legacy single dest/fxId/pkey fields
// from older docs are folded into it at read time.
type LfoTarget = { dest: string; fxId: string; pkey: string }
type LfoCfg = {
  id: string; on: boolean; shape: number; sync: boolean; rate: number
  hz: number; depth: number; phase: number; targets: LfoTarget[]
}
type EnvPts = { t: number; v: number }[]

class Engine implements EngineApi {
  started = false
  mode: EngineMode = 'session'
  built = new Map<string, BuiltTrack>()
  master!: Tone.Volume
  limiter!: Tone.Limiter
  captureTap!: Tone.Gain // post-limiter tap for "Record Output" (exactly what's heard)
  masterMeter!: Tone.Meter
  arrParts: Array<Tone.Part | Tone.Player> = []
  arrSeekTicks = 0
  sampleRate = 0
  // Audio render-thread load (Chromium `renderCapacity`), 0..1. This is the real
  // "about to glitch" signal — how full the audio thread's per-callback budget is.
  audioLoad = { avg: 0, peak: 0, underrun: 0, supported: false }
  // Cross-browser render-thread health, fed by the sf-load AudioWorklet probe.
  // rtf = audio produced ÷ real time (1 = keeping up, <1 = underrunning = the
  // crackle). minRtf is the worst recent window (the dip you actually hear);
  // glitches counts windows that dipped audibly. Works where renderCapacity
  // doesn't (Safari/Firefox) and catches sub-second stalls the 1s renderCapacity
  // average smooths away. See public/sf-load-worklet.js.
  audioProbe = { supported: false, hasClock: false, rtf: 1, minRtf: 1, glitches: 0, maxGapMs: 0 }
  private metro!: Tone.Synth
  private metroRepeatId: number | null = null
  private rebuildTimers = new Map<string, ReturnType<typeof setTimeout>>()
  private fxRebuildTimers = new Map<string, ReturnType<typeof setTimeout>>()
  private audioWatchdog: ReturnType<typeof setInterval> | null = null
  private partTimers = new Map<string, ReturnType<typeof setTimeout>>()
  private arrTimer: ReturnType<typeof setTimeout> | null = null
  private pendingRec = new Map<number, { clipMap: Y.Map<any>; startInClip: number; vel: number }>()

  // ---- LFO / automation modulation (local per client; runs at frame rate) ----
  private modRAF = 0
  private lfoVals = new Map<string, number>()  // lfoId -> current raw value [-1,1]
  private activeMod = new Map<string, { tid: string; dest: string; fxId: string; pkey: string }>()
  // Free-running LFOs integrate their own phase (so a modulated rate glides
  // instead of jumping), keyed by LFO id. lfoRateMod holds this frame's
  // effective rate (Hz) for any LFO whose rate is being modulated/automated —
  // by another LFO (dest 'lfo') or an automation lane — else the LFO uses its
  // stored Hz. Entries not refreshed each frame are dropped (snap back to base).
  private lfoPhase = new Map<string, number>()
  private lfoLastTime = 0
  private lfoRateMod = new Map<string, number>()
  private lfoRatedThisFrame = new Set<string>()

  // ---- live MIDI-fx modulation (LFO/macro → arp/chord/velocity/random params) ----
  // MIDI fx transform notes at scheduling time, not in the audio graph, so a
  // modulated value is stashed here (read by applyMidiFx) and the playing Part
  // is regenerated when the value crosses a meaningful (quantized) step.
  private midiFxMod = new Map<string, number>()      // "tid|fxId|pkey" → live value
  private midiFxQuant = new Map<string, number>()     // last quantized signature per key
  private midiDirty = new Set<string>()               // trackIds needing a Part rebuild
  private midiRebuildAt = new Map<string, number>()   // throttle: last rebuild time per track

  // ---- per-frame lookup caches ----
  // The modulation loop runs every animation frame; everything it needs from
  // the Yjs doc (track list, LFO configs, envelope/automation breakpoints,
  // resolved param targets, the arrangement clip index) changes only on edits,
  // so it's deserialized once and invalidated from the existing observers /
  // recompute points instead of being re-derived 60× a second.
  private tracksCache: Y.Map<any>[] | null = null
  private lfoCfgCache = new Map<string, LfoCfg[]>()
  private resolveCache = new Map<string, { spec: ParamSpec; base: number; setter: (v: number) => void } | null>()
  private clipEnvCache = new Map<Y.Map<any>, [string, EnvPts][]>()
  private trackAutoCache = new Map<string, [string, EnvPts][]>()
  private arrIndexCache: { cm: Y.Map<any>; trackId: string; start: number; len: number }[] | null = null
  private masterAutoCache: [string, EnvPts][] | null = null

  private allTracks(): Y.Map<any>[] {
    if (!this.tracksCache) this.tracksCache = tracks.toArray()
    return this.tracksCache
  }
  private lfoCfgsOf(tid: string, t: Y.Map<any>): LfoCfg[] {
    let cfgs = this.lfoCfgCache.get(tid)
    if (!cfgs) {
      const list: LfoCfg[] = []
      ;(t.get('lfos') as Y.Array<Y.Map<any>> | undefined)?.forEach(l => {
        const targets: LfoTarget[] = []
        const seen = new Set<string>()
        const push = (dest: string, fxId: string, pkey: string) => {
          if (!dest || !pkey) return
          const k = `${dest}|${fxId}|${pkey}`
          if (seen.has(k)) return
          seen.add(k)
          targets.push({ dest, fxId, pkey })
        }
        ;(l.get('targets') as Y.Array<Y.Map<any>> | undefined)?.forEach(tg =>
          push(tg.get('dest') || '', tg.get('fxId') || '', tg.get('pkey') || ''))
        // legacy single-field mapping from older docs
        push((l.get('dest') as string) || '', (l.get('fxId') as string) || '', (l.get('pkey') as string) || '')
        list.push({
          id: l.get('id'), on: !!l.get('on'), shape: l.get('shape') | 0, sync: !!l.get('sync'),
          rate: l.get('rate') | 0, hz: l.get('hz') ?? 1, depth: l.get('depth') ?? 0.5, phase: l.get('phase') ?? 0,
          targets,
        })
      })
      cfgs = list
      this.lfoCfgCache.set(tid, cfgs)
    }
    return cfgs
  }
  private envEntriesOf(cm: Y.Map<any>): [string, EnvPts][] {
    let e = this.clipEnvCache.get(cm)
    if (!e) {
      e = envKeys(cm).map(k => [k, envPoints(cm, k)] as [string, EnvPts])
      this.clipEnvCache.set(cm, e)
    }
    return e
  }
  private autoEntriesOf(tid: string, t: Y.Map<any>): [string, EnvPts][] {
    let e = this.trackAutoCache.get(tid)
    if (!e) {
      const list: [string, EnvPts][] = []
      ;(t.get('auto') as Y.Map<any> | undefined)?.forEach((a, k) => list.push([k, (a as Y.Array<any>).toArray()]))
      e = list
      this.trackAutoCache.set(tid, e)
    }
    return e
  }
  private arrIndex() {
    if (!this.arrIndexCache) {
      const idx: { cm: Y.Map<any>; trackId: string; start: number; len: number }[] = []
      arr.forEach((cm: Y.Map<any>) => idx.push({ cm, trackId: cm.get('trackId'), start: cm.get('start') ?? 0, len: cm.get('len') ?? BAR }))
      this.arrIndexCache = idx
    }
    return this.arrIndexCache
  }
  private masterAutoEntries(): [string, EnvPts][] {
    if (!this.masterAutoCache) {
      const list: [string, EnvPts][] = []
      masterAuto.forEach((a: Y.Array<any>, k: string) => list.push([k, a.toArray()]))
      this.masterAutoCache = list
    }
    return this.masterAutoCache
  }
  private resolveCached(t: Y.Map<any>, rec: BuiltTrack, tid: string, k: string, dest: string, fxId: string, pkey: string) {
    let r = this.resolveCache.get(tid + '|' + k)
    if (r === undefined) {
      r = this.resolveTarget(t, rec, dest, fxId, pkey)
      this.resolveCache.set(tid + '|' + k, r)
    }
    return r
  }
  private resolveMasterCached(k: string) {
    let r = this.resolveCache.get('master|' + k)
    if (r === undefined) {
      r = this.resolveMasterTarget(k)
      this.resolveCache.set('master|' + k, r)
    }
    return r
  }

  // ---- track freeze (LOCAL per client, like playback) ----
  // A frozen track's device processing (instrument → fx chain) is bounced to
  // audio by render.ts and played back as plain buffers; the mixer half of the
  // strip (pan/fader/sends) and any Sidechain devices stay LIVE. Freeze state
  // lives here (not in the shared doc): each collaborator freezes for their own
  // CPU. Any edit that would change the baked sound auto-unfreezes.
  private frozen = new Map<string, FrozenTrackAudio>()
  private freezingIds = new Set<string>()
  isFrozen(trackId: string) { return this.frozen.has(trackId) }
  isFreezing(trackId: string) { return this.freezingIds.has(trackId) }

  // ---- send/return buses + master analysers ----
  private builtReturns: BuiltReturn[] = []
  private masterFFT!: Tone.Analyser
  private masterWave!: Tone.Analyser
  private followTimers = new Map<string, number>()
  // ---- master-bus effect chain (master → [fx…] → limiter) ----
  private builtMasterFx: BuiltFx[] = []
  private masterChainOut!: Tone.ToneAudioNode   // tail of the master fx chain (meters/limiter tap here)
  private masterFxTimer: ReturnType<typeof setTimeout> | null = null

  // ---- tiny emitter so React can follow launch-state changes ----
  version = 0
  private listeners = new Set<() => void>()
  subscribe = (fn: () => void) => {
    this.listeners.add(fn)
    return () => { this.listeners.delete(fn) }
  }
  private emit() {
    this.version++
    // launch/stop transitions change which clips' envelopes can drive sends —
    // refresh the idle-bus cut on every engine-state change (cheap, event-driven)
    this.recomputeBusIdle()
    this.listeners.forEach(l => l())
  }

  get transport() {
    return Tone.getTransport()
  }

  private startPromise: Promise<void> | null = null
  private noOversample = false
  // ---- render-thread health probe (sf-load worklet) ----
  private loadProbeReady: Promise<void> | null = null
  private loadProbeNode: AudioWorkletNode | null = null
  private loadProbeSink: GainNode | null = null
  private probeWinHist: number[] = []   // recent rtf windows → rolling min (the dip)
  private probeStartAt = 0              // ignore boot-transient glitches before this+grace
  private glitchWarned = false          // one nudge to Audio settings per session

  ensureStarted(): Promise<void> {
    if (this.started) return Promise.resolve()
    // Memoize: the first-gesture pre-warm (main.tsx) and a Play click must not
    // double-init. Crucially, a *failed* init must not wedge the engine — we
    // clear the promise and leave `started` false so the next click retries,
    // instead of latching `started = true` up front (the old silent-wedge bug).
    if (!this.startPromise) {
      this.startPromise = this.doStart().catch(e => {
        console.error('audio init failed', e)
        // Persistent, visible failure (not just a transient toast) — a dead audio
        // engine is the kind of thing a user must actually see, with a way to act.
        setUI({ audioError: 'Audio engine couldn’t start. Click Retry — if it keeps failing, your browser may not be supported (use the latest Chrome, Edge, Firefox, or Safari 15+).' })
        this.startPromise = null
      })
    }
    return this.startPromise
  }

  /**
   * Resume the AudioContext whenever it leaves the 'running' state while we
   * intend to be playing — covers OS device changes, Bluetooth handoff, focus
   * loss and power events that browsers respond to by suspending/interrupting
   * the context. Listens to statechange + window focus/visibility, plus a slow
   * periodic safety net. Only acts once the engine has actually started, so it
   * never fights the initial autoplay gesture.
   */
  private setupAudioWatchdog() {
    const ctx = Tone.getContext().rawContext as AudioContext
    const resume = () => { if (this.started && ctx.state !== 'running') ctx.resume().catch(() => { /* ok */ }) }
    try { ctx.addEventListener('statechange', resume) } catch { /* ok */ }
    document.addEventListener('visibilitychange', () => { if (!document.hidden) resume() })
    window.addEventListener('focus', resume)
    if (this.audioWatchdog) clearInterval(this.audioWatchdog)
    this.audioWatchdog = setInterval(resume, 2000)
  }

  /**
   * Cross-browser render-thread health probe. Loads the sf-load AudioWorklet
   * (mirrors recorder.ts: static file in public/, 0-gain sink keeps it pulled)
   * and turns its per-window quantum counts into a real-time-factor (rtf): how
   * much audio the thread produced vs how much wall time passed. <1 means the
   * thread fell behind and the output buffer underran — exactly the popping the
   * user hears, and the thing FPS/renderCapacity miss (FPS is the wrong thread;
   * renderCapacity is Chromium-only and 1s-averaged). Best-effort: any failure
   * is swallowed so it can never block audio from starting.
   */
  private async setupLoadProbe() {
    try {
      const ctx = Tone.getContext().rawContext as AudioContext
      if (!ctx.audioWorklet) return
      if (!this.loadProbeReady) this.loadProbeReady = ctx.audioWorklet.addModule('/sf-load-worklet.js')
      await this.loadProbeReady
      if (this.loadProbeNode) return // already running (e.g. a re-entrant start)
      const node = new AudioWorkletNode(ctx, 'sf-load', { numberOfInputs: 0, numberOfOutputs: 1, processorOptions: { reportMs: 250 } })
      const sink = ctx.createGain()
      sink.gain.value = 0
      node.connect(sink)
      sink.connect(ctx.destination)
      let lastMsgT = 0
      node.port.onmessage = (e: MessageEvent) => {
        const d = e.data as { clock: number; frames?: number; quanta?: number; winMs?: number; maxGapMs?: number }
        let winSec = 0
        if (d.clock) {
          winSec = (d.winMs ?? 0) / 1000
          this.audioProbe.hasClock = true
          this.audioProbe.maxGapMs = Math.round(d.maxGapMs ?? 0)
        } else {
          const now = typeof performance !== 'undefined' ? performance.now() : 0
          if (lastMsgT) winSec = (now - lastMsgT) / 1000
          lastMsgT = now
          this.audioProbe.hasClock = false
        }
        if (winSec <= 0.02) return
        // frames (block-size agnostic); legacy quanta·128 kept for safety
        const produced = (d.frames ?? (d.quanta ?? 0) * 128) / (this.sampleRate || 44100)
        const rtf = produced / winSec
        this.audioProbe.supported = true
        this.audioProbe.rtf = rtf
        this.probeWinHist.push(rtf)
        if (this.probeWinHist.length > 8) this.probeWinHist.shift() // ~2s rolling min
        this.audioProbe.minRtf = Math.min(...this.probeWinHist)
        // Below this fraction of real-time in a window the thread dropped audio
        // (≈3.75ms lost per 250ms) = an audible glitch. With the audio-thread
        // clock (hasClock) the number is jank-immune so we use a tight 0.985;
        // the main-thread-timed fallback couples to UI jank, so only count
        // clearly-deeper dips there to avoid false positives.
        const glitchThresh = this.audioProbe.hasClock ? 0.985 : 0.95
        // Skip the first ~2.5s after start — the boot transient (graph wiring,
        // first GC, sample decode, React mount) under-runs harmlessly.
        if (rtf < glitchThresh && Tone.now() - this.probeStartAt > 2.5) {
          this.audioProbe.glitches++
          if (!this.glitchWarned && this.audioProbe.glitches >= 3) {
            this.glitchWarned = true
            toast('Audio glitches detected — open Audio settings to add buffer headroom')
          }
        }
      }
      this.loadProbeNode = node
      this.loadProbeSink = sink
      this.probeStartAt = Tone.now()
    } catch { /* probe is best-effort; never let it break audio */ }
  }

  // Worklet-backed effects (sf-mbdyn multiband; sf-fx chorus/phaser/shifter/
  // autotune) are single AudioWorklet nodes, so their modules must be registered
  // before any track that uses them is built. Loaded once at start; each device
  // falls back cleanly (passthrough / Tone composite) if registration fails.
  private mbWorkletReady: Promise<void> | null = null
  private fxWorkletReady: Promise<void> | null = null
  private async ensureMbWorklet() {
    try {
      const ctx = Tone.getContext().rawContext as AudioContext
      if (!ctx.audioWorklet) return
      if (!this.mbWorkletReady) this.mbWorkletReady = ctx.audioWorklet.addModule('/sf-mbdyn.js')
      if (!this.fxWorkletReady) this.fxWorkletReady = ctx.audioWorklet.addModule('/sf-fx.js')
      await this.mbWorkletReady
      await this.fxWorkletReady
    } catch { /* best-effort; devices degrade to passthrough/Tone if unavailable */ }
  }

  private async doStart() {
    // Audio settings (user-tunable in the Audio Settings dialog, read here at boot):
    //  • oversample → run the graph at 88.2kHz (2x) for alias-free FM/distortion,
    //    or native rate to roughly HALVE CPU (more tracks/fx before glitching).
    //  • latency (latencyHint) → output buffer size; bigger = fewer dropouts when
    //    many tracks play, at the cost of a little input latency.
    //  • sampleRate → native context rate (only when oversample is off).
    // Custom sample rates are unsupported on older Safari → fall back cleanly.
    const prefs = getAudioPrefs()
    const latencyHint = prefs.latency
    // The "Stable" buffer tier additionally asks for a hardware-sized render
    // quantum (renderSizeHint, Chromium 2024+). Bigger blocks amortize per-node
    // callback overhead across the whole graph — a real throughput win with
    // hundreds of nodes. Unknown dictionary members are ignored by WebIDL, so
    // this is a harmless no-op on browsers without support.
    const sizeHint = prefs.latency === 'playback' ? { renderSizeHint: 'hardware' } : {}
    const nativeOpts = (): AudioContextOptions =>
      ({ latencyHint, ...sizeHint, ...(prefs.sampleRate === 'auto' ? {} : { sampleRate: prefs.sampleRate }) }) as AudioContextOptions

    if (prefs.oversample && !this.noOversample) {
      try {
        Tone.setContext(new AudioContext({ sampleRate: 88200, latencyHint, ...sizeHint } as AudioContextOptions))
      } catch {
        this.noOversample = true
      }
    }
    if (!prefs.oversample || this.noOversample) {
      try { Tone.setContext(new AudioContext(nativeOpts())) }
      catch { Tone.setContext(new AudioContext({ latencyHint })) }
    }
    await Tone.start()
    // Safari can leave the context 'suspended' even after start() — the Play
    // button would then silently do nothing. Force-resume and verify; if a
    // custom-rate context refuses to wake, drop oversampling and retry clean.
    let raw = Tone.getContext().rawContext as AudioContext
    if (raw.state !== 'running') { try { await raw.resume() } catch {} }
    if (raw.state !== 'running' && prefs.oversample && !this.noOversample) {
      this.noOversample = true
      try { await raw.close() } catch {}
      try { Tone.setContext(new AudioContext(nativeOpts())) }
      catch { Tone.setContext(new AudioContext({ latencyHint })) }
      await Tone.start()
      raw = Tone.getContext().rawContext as AudioContext
      if (raw.state !== 'running') { try { await raw.resume() } catch {} }
    }
    // If the context STILL won't run after every fallback, this is a real failure
    // (unsupported browser, or autoplay lock). Throw so ensureStarted surfaces the
    // banner and leaves `started` false — a later Retry/gesture re-attempts cleanly
    // instead of the engine silently building a graph onto a dead context.
    if (raw.state !== 'running') throw new Error(`AudioContext stuck in "${raw.state}"`)
    this.sampleRate = Tone.getContext().sampleRate

    // ---- playback reliability ----
    // Widen the scheduling safety margin. Clip notes are scheduled this far ahead
    // on Tone's clock; if the MAIN thread stalls (a heavy React render, a Yjs
    // sync, a P2P burst, GC) longer than the window, the audio runs out of
    // scheduled events and you hear a gap — while audio-thread load reads LOW
    // (there's nothing to render during the gap). A bigger lookAhead absorbs
    // those stalls. Live notes trigger via immediate() so they're unaffected, and
    // note timing itself is unchanged — only the safety margin grows.
    Tone.getContext().lookAhead = 0.2
    // Keep the context alive: browsers suspend/interrupt it on audio-device
    // changes, Bluetooth handoff, focus loss or power events, which silently kills
    // playback with nothing to recover it. Auto-resume on every such transition.
    this.setupAudioWatchdog()

    // Audio render-thread load monitor (Chromium only). Directly measures how
    // close the audio thread is to underrunning — exactly what causes glitches.
    const rc = (raw as any).renderCapacity
    if (rc?.start) {
      try {
        this.audioLoad.supported = true
        rc.onupdate = (e: any) => {
          this.audioLoad = {
            avg: e.averageLoad ?? 0, peak: e.peakLoad ?? 0,
            underrun: e.underrunRatio ?? 0, supported: true,
          }
        }
        rc.start({ updateInterval: 1 })
      } catch { this.audioLoad.supported = false }
    }
    // Cross-browser glitch detection (Safari/Firefox have no renderCapacity, and
    // even on Chromium this catches the sub-second stalls the 1s average hides).
    void this.setupLoadProbe()

    const t = this.transport
    t.PPQ = 96
    t.bpm.value = meta.get('bpm') ?? 120
    setRateBpm(t.bpm.value)   // tempo-synced fx rate knobs
    t.swing = meta.get('swing') ?? 0
    t.swingSubdivision = (meta.get('swingSubdivision') as any) ?? '16n'

    // Tone.Channel downmixes stereo input to mono (its PanVol has a 1-channel
    // input); use a plain stereo-preserving Volume for the master & returns.
    this.master = new Tone.Volume(meta.get('masterGain') ?? 0)
    this.limiter = new Tone.Limiter(-1)
    this.masterMeter = new Tone.Meter({ smoothing: 0.85 })
    this.masterFFT = new Tone.Analyser('fft', 1024)
    this.masterWave = new Tone.Analyser('waveform', 1024)
    // The limiter → destination link (and the post-limiter capture tap) are
    // permanent; buildMasterFx() wires master → [fx…] → limiter and taps the
    // analysers off the chain tail, so the meters/spectrum reflect master fx.
    this.limiter.connect(Tone.getDestination())
    this.captureTap = new Tone.Gain(1)
    this.limiter.connect(this.captureTap)
    // The mbcomp worklet module MUST be registered before ANY chain builds: an
    // mbcomp constructed pre-registration silently degrades to a passthrough
    // Gain, and the master chain (unlike tracks, rebuilt on every edit) would
    // stay a passthrough until some bypass toggle forced a rebuild — the
    // "master effects don't work until I bypass something" session-start bug.
    await this.ensureMbWorklet()
    this.masterChainOut = this.master
    this.buildMasterFx()
    masterFx.observeDeep(this.onMasterFxDeep)

    ensureReturns()
    this.buildReturns()
    returns.observeDeep(this.onReturnsDeep)
    onSampleReady(id => {
      // a sample finished loading/decoding → rebuild any sampler OR drum pad using it
      tracks.forEach(t => {
        const it = t.get('inst') as Y.Map<any> | undefined
        if (!it) return
        const ity = it.get('type')
        if ((ity === 'sampler' || ity === 'ksampler' || ity === 'granular') && it.get('sampleId') === id) {
          this.scheduleRebuildTrack(t.get('id'))
        } else if (it.get('type') === 'drum') {
          const ps = it.get('padSamples') as Y.Map<string> | undefined
          let used = false
          ps?.forEach(v => { if (v === id) used = true })
          if (used) this.scheduleRebuildTrack(t.get('id'))
        }
      })
    })

    this.metro = new Tone.Synth({
      oscillator: { type: 'sine' },
      envelope: { attack: 0.001, decay: 0.05, sustain: 0, release: 0.01 },
    }).connect(this.master)
    this.scheduleMetronome()
    t.timeSignature = [timeSig().top, timeSig().bottom]

    await this.ensureMbWorklet()
    this.buildAll()
    tracks.observeDeep(this.onTracksDeep)
    tracks.observe(() => { this.tracksCache = null })          // membership → drop cached list
    masterAuto.observeDeep(() => { this.masterAutoCache = null; this.scheduleMasterFxRewire() })
    clips.observe(this.onClipsShallow)
    clips.observeDeep(this.onClipsDeepFreeze)
    arr.observeDeep(this.onArrDeep)
    meta.observe(this.onMeta)
    this.startModLoop()
    this.started = true
    setUI({ audioReady: true, audioError: null })
    this.emit()
  }

  // ---------------- LFO modulation loop ----------------
  // Each client locally oscillates mapped parameters around their doc base
  // value. The LFO *config* is shared (Yjs), the per-frame modulation is local
  // — same model as playback. Restores the base value when a mapping is removed.

  private startModLoop() {
    let frame = 0
    const tick = () => {
      this.modRAF = requestAnimationFrame(tick)
      try {
        this.applyModulation()
        if ((++frame & 127) === 0) this.sweepMeters()   // ~every 2s
      } catch { /* never kill the loop */ }
    }
    this.modRAF = requestAnimationFrame(tick)
  }

  /** Free-running tick position so synced LFOs animate even when stopped. */
  private freeTicks() {
    return Tone.now() * (this.transport.bpm.value / 60) * 96
  }

  private findFxMap(t: Y.Map<any>, fxId: string): Y.Map<any> | undefined {
    const a = t.get('fx') as Y.Array<Y.Map<any>>
    for (let i = 0; i < a.length; i++) if (a.get(i).get('id') === fxId) return a.get(i)
    return undefined
  }

  /** Resolve a modulation/automation target to its spec, base value and live setter. */
  private resolveTarget(t: Y.Map<any>, rec: BuiltTrack, dest: string, fxId: string, pkey: string) {
    if (dest === 'inst') {
      const spec = instSchema(t.get('inst').get('type')).params.find(s => s.key === pkey)
      const base = (t.get('inst').get('params') as Y.Map<number>).get(pkey)
      if (!spec || typeof base !== 'number') return null
      return { spec, base, setter: (v: number) => rec.inst.set(pkey, v) }
    }
    if (dest === 'mix') {
      const spec = mixSpec(pkey)
      const base = t.get(pkey)
      if (!spec || typeof base !== 'number') return null
      const setter = (v: number) => { if (pkey === 'gain') rec.vol.volume.rampTo(v, 0.02); else rec.panner.pan.setTargetAtTime(v, Tone.now(), 0.02) }
      return { spec, base, setter }
    }
    if (dest === 'send') {
      if (pkey === 'F') {
        // Feedback bus self-send (the ↺ loop level) — only exists on the F bus
        const fb = rec.fbSend
        if (!fb) return null
        const base = (t.get('sends') as Y.Map<number> | undefined)?.get(t.get('id')) ?? 0.3
        const spec = { key: 'send', label: 'Feedback send', min: 0, max: 0.95, def: 0.3 } as any
        return { spec, base, setter: (v: number) => fb.gain.rampTo(v, 0.02) }
      }
      const node = pkey === 'B' ? rec.sendB : rec.sendA
      const base = (t.get(pkey === 'B' ? 'sendB' : 'sendA') as number) ?? 0
      const spec = { key: 'send', label: 'Send', min: 0, max: 1, def: 0 } as any
      return { spec, base, setter: (v: number) => node.gain.rampTo(v, 0.02) }
    }
    if (dest === 'macro') {
      // Target a MACRO's value (pkey = macro index). The setter fans the value
      // out to every parameter the macro is mapped to — audio-side only, the
      // shared doc (and the knob) keeps the user's stored value. Macro targets
      // are only inst/mix/send/fx, so the recursion can't loop back here.
      const macros = t.get('macros') as Y.Array<Y.Map<any>> | undefined
      const m = macros?.get(+pkey)
      if (!m) return null
      const spec = { key: 'macro', label: 'Macro', min: 0, max: 1, def: 0 } as any
      const setter = (v: number) => {
        const tgs = m.get('targets') as Y.Array<Y.Map<any>> | undefined
        tgs?.forEach(tg => {
          const r = this.resolveTarget(t, rec, tg.get('dest'), tg.get('fxId') || '', tg.get('pkey'))
          if (r) r.setter(clamp(r.spec.min + (r.spec.max - r.spec.min) * v, r.spec.min, r.spec.max))
        })
      }
      return { spec, base: (m.get('value') as number) ?? 0, setter }
    }
    if (dest === 'midi') {
      // Target a MIDI-fx param (arp/chord/velocity/random). fxId = midifx id.
      // The setter stashes the live value; when it crosses a quantized step the
      // track is marked for a Part rebuild (applyModulation regenerates notes).
      const tid = t.get('id') as string
      const chain = midifxOf(t)
      let fxMap: Y.Map<any> | undefined
      if (chain) for (let i = 0; i < chain.length; i++) if (chain.get(i).get('id') === fxId) { fxMap = chain.get(i); break }
      if (!fxMap) return null
      const spec = midiFxSchema(fxMap.get('type')).params.find(s => s.key === pkey)
      const base = (fxMap.get('params') as Y.Map<number>).get(pkey)
      if (!spec || typeof base !== 'number') return null
      const key = `${tid}|${fxId}|${pkey}`
      // int/stepped params rebuild only on integer crossings; continuous params
      // are quantized to ~40 steps of their range so a slow LFO doesn't rebuild
      // the Part every single frame.
      const quantOf = (v: number) => (spec.int || spec.steps) ? Math.round(v) : Math.round((v - spec.min) / ((spec.max - spec.min) || 1) * 40)
      return {
        spec, base,
        setter: (v: number) => {
          this.midiFxMod.set(key, v)
          const q = quantOf(v)
          if (this.midiFxQuant.get(key) !== q) { this.midiFxQuant.set(key, q); this.midiDirty.add(tid) }
        },
      }
    }
    if (dest === 'lfo') {
      // Target another LFO's RATE (Hz). fxId holds the target LFO's id. Only
      // free-running (non-sync) LFOs actually track Hz — a synced target keeps
      // its tempo division. The setter stashes the effective Hz for this frame;
      // the LFO loop reads it (see applyModulation).
      const lfos = t.get('lfos') as Y.Array<Y.Map<any>> | undefined
      let base: number | undefined
      if (lfos) for (let i = 0; i < lfos.length; i++) {
        const l = lfos.get(i)
        if (l.get('id') === fxId) { base = l.get('hz') ?? 1; break }
      }
      if (typeof base !== 'number') return null
      return { spec: LFO_PARAMS[2], base, setter: (v: number) => this.lfoRateMod.set(fxId, v) }
    }
    const fxMap = this.findFxMap(t, fxId)
    const bf = rec.fx.find(f => f.id === fxId)
    if (!fxMap || !bf) return null
    const spec = fxSchema(fxMap.get('type')).params.find(s => s.key === pkey)
    const base = (fxMap.get('params') as Y.Map<number>).get(pkey)
    if (!spec || typeof base !== 'number') return null
    return { spec, base, setter: (v: number) => bf.fx.set(pkey, v) }
  }

  private applyModulation() {
    const playing = this.transport.state === 'started'
    // Idle the whole scan when stopped and nothing is free-running. The
    // activeMod check lets one final frame run after the last mapping is removed,
    // so a just-unmapped param still snaps back to its base before we go quiet.
    if (!playing && !this.modActive && this.activeMod.size === 0) return
    const newActive = new Map<string, { tid: string; dest: string; fxId: string; pkey: string }>()
    const syncedPos = playing ? this.transport.ticks : this.freeTicks()
    const audioTime = Tone.now()
    const bpm = this.transport.bpm.value
    const tick = this.transport.ticks
    const isArr = playing && this.mode === 'arr'
    // Per-frame time delta for free-LFO phase integration (clamped so a long
    // idle gap can't fling the phase). Which LFO rates got modulated this frame
    // is tracked so stale rate overrides snap back to the LFO's stored Hz.
    const dt = Math.min(0.1, Math.max(0, audioTime - (this.lfoLastTime || audioTime)))
    this.lfoLastTime = audioTime
    this.lfoRatedThisFrame.clear()

    // ARRANGEMENT: one pass over the cached clip index finds the clips under
    // the playhead per track — the old code re-walked EVERY arr clip once per
    // track per frame (O(tracks × clips) Yjs reads at 60Hz).
    let activeArr: Map<string, { cm: Y.Map<any>; pos: number; loop: number }[]> | null = null
    if (isArr) {
      activeArr = new Map()
      for (const c of this.arrIndex()) {
        if (tick < c.start || tick >= c.start + c.len) continue
        const loop = c.len || BAR
        const entry = { cm: c.cm, pos: (((tick - c.start) % loop) + loop) % loop, loop }
        const list = activeArr.get(c.trackId)
        if (list) list.push(entry)
        else activeArr.set(c.trackId, [entry])
      }
    }

    for (const t of this.allTracks()) {
      const tid = t.get('id') as string
      const rec = this.built.get(tid)
      if (!rec) continue

      // tempo-synced effect ticks (sidechain ducker, etc.) — bypassed devices
      // are out of the graph, so don't burn frames driving them
      for (const bf of rec.fx) if (!bf.bypassed) bf.fx.tick?.(syncedPos, playing, bpm)

      const lfos = this.lfoCfgsOf(tid, t)

      const controlled = new Map<string, { dest: string; fxId: string; pkey: string }>()
      const autoNorm = new Map<string, number>()
      const lfoOff = new Map<string, number>()

      // ---- automation ----
      if (playing && this.mode === 'session' && rec.partKey) {
        // SESSION: the launched clip's looping envelopes.
        const clipMap = clips.get(rec.partKey) as Y.Map<any> | undefined
        if (clipMap) {
          const loop = rec.partLoopTicks || BAR
          const pos = (((tick - rec.partStartTicks) % loop) + loop) % loop
          for (const [k, pts] of this.envEntriesOf(clipMap)) {
            if (!pts.length) continue
            autoNorm.set(k, this.envValueAt(pts, pos, loop))
            const [dest, fxId, pkey] = k.split('|')
            controlled.set(k, { dest, fxId: fxId || '', pkey })
          }
        }
      } else if (isArr) {
        // 1) ARRANGEMENT (track-timeline) automation — absolute song time (Option B).
        for (const [k, pts] of this.autoEntriesOf(tid, t)) {
          if (!pts.length) continue
          autoNorm.set(k, this.envValueAt(pts, tick, 0))
          const [dest, fxId, pkey] = k.split('|')
          controlled.set(k, { dest, fxId: fxId || '', pkey })
        }
        // 2) the arrangement clip under the playhead — its CLIP envelopes (these
        //    override track automation for the same param while the clip plays).
        const under = activeArr!.get(tid)
        if (under) for (const u of under) {
          for (const [k, pts] of this.envEntriesOf(u.cm)) {
            if (!pts.length) continue
            autoNorm.set(k, this.envValueAt(pts, u.pos, u.loop))
            const [dest, fxId, pkey] = k.split('|')
            controlled.set(k, { dest, fxId: fxId || '', pkey })
          }
        }
      }

      // ---- LFOs: add a bipolar offset on top ----
      for (const lfo of lfos) {
        let raw: number
        if (lfo.sync) {
          // tempo-locked: cycle length is a fixed tick division (rate not Hz)
          raw = lfoShapeValue(lfo.shape, syncedPos / (LFO_DIV_TICKS[lfo.rate] || 384) + lfo.phase)
        } else {
          // free-run: integrate phase from the (possibly modulated) rate so a
          // changing Hz glides smoothly instead of jumping
          const effHz = this.lfoRateMod.get(lfo.id) ?? lfo.hz
          let ph = (this.lfoPhase.get(lfo.id) ?? 0) + dt * effHz
          if (ph > 1e6) ph -= 1e6
          this.lfoPhase.set(lfo.id, ph)
          raw = lfoShapeValue(lfo.shape, ph + lfo.phase)
        }
        this.lfoVals.set(lfo.id, raw)
        if (!lfo.on || !lfo.targets.length) continue
        for (const tg of lfo.targets) {
          const k = `${tg.dest}|${tg.fxId}|${tg.pkey}`
          lfoOff.set(k, (lfoOff.get(k) ?? 0) + raw * lfo.depth)
          controlled.set(k, tg)
        }
      }

      // ---- combine automation base + LFO offset, apply once per param ----
      for (const [k, tg] of controlled) {
        const r = this.resolveCached(t, rec, tid, k, tg.dest, tg.fxId, tg.pkey)
        if (!r) continue
        // Automation points are stored normalized [0,1]; map back along the
        // spec curve so frequency/exp params track logarithmically (matching the
        // lane's vertical scale and the knob). LFO offset stays a linear ± swing.
        const base = autoNorm.has(k) ? valueFromSpec(r.spec, autoNorm.get(k)!) : r.base
        const off = (lfoOff.get(k) ?? 0) * (r.spec.max - r.spec.min) * 0.5
        r.setter(clamp(base + off, r.spec.min, r.spec.max))
        if (tg.dest === 'lfo') this.lfoRatedThisFrame.add(tg.fxId)   // its rate override is live this frame
        newActive.set(`${tid}|${k}`, { tid, dest: tg.dest, fxId: tg.fxId, pkey: tg.pkey })
      }

      // A modulated MIDI-fx value crossed a step → regenerate the playing part's
      // notes with the new value. Throttled to ≤20 Hz/track (rebuilds re-expand
      // the arp/chord); the dirty flag persists until a rebuild actually fires.
      if (this.midiDirty.has(tid) && rec.part && rec.partKey) {
        if (audioTime - (this.midiRebuildAt.get(tid) ?? 0) >= 0.05) {
          this.midiRebuildAt.set(tid, audioTime)
          this.midiDirty.delete(tid)
          this.refreshPart(rec)
        }
      }
    }

    // ---- master-bus arrangement automation (its own auto store, not a track) ----
    if (isArr) {
      for (const [k, pts] of this.masterAutoEntries()) {
        if (!pts.length) continue
        const r = this.resolveMasterCached(k)
        if (!r) continue
        r.setter(clamp(valueFromSpec(r.spec, this.envValueAt(pts, tick, 0)), r.spec.min, r.spec.max))
        const [dest, fxId, pkey] = k.split('|')
        newActive.set(`master|${k}`, { tid: 'master', dest, fxId: fxId || '', pkey })
      }
    }

    // master-bus effects that need a transport-synced tick (e.g. a ducker on the mix)
    if (this.builtMasterFx.length) {
      const bpm = this.transport.bpm.value
      this.builtMasterFx.forEach(bf => { if (!bf.bypassed) bf.fx.tick?.(syncedPos, playing, bpm) })
    }

    // params modulated last frame but not now → snap back to their base value
    for (const [k, m] of this.activeMod) {
      if (!newActive.has(k)) this.restoreParam(m)
    }
    this.activeMod = newActive
    // drop rate overrides no longer driven this frame → those LFOs revert to
    // their stored Hz on the next frame
    if (this.lfoRateMod.size) {
      for (const id of this.lfoRateMod.keys()) if (!this.lfoRatedThisFrame.has(id)) this.lfoRateMod.delete(id)
    }
  }

  private envValueAt(pts: { t: number; v: number }[], pos: number, loop: number): number {
    if (pts.length === 1) return pts[0].v
    if (pos <= pts[0].t) return pts[0].v
    const last = pts[pts.length - 1]
    if (pos >= last.t) return last.v
    for (let i = 0; i < pts.length - 1; i++) {
      const a = pts[i], b = pts[i + 1]
      if (pos >= a.t && pos <= b.t) {
        const f = (pos - a.t) / Math.max(1, b.t - a.t)
        return a.v + (b.v - a.v) * f
      }
    }
    return last.v
  }

  private restoreParam(m: { tid: string; dest: string; fxId: string; pkey: string }) {
    if (m.tid === 'master') {
      const r = this.resolveMasterTarget(`${m.dest}|${m.fxId}|${m.pkey}`)
      if (r) r.setter(r.base)
      return
    }
    // MIDI-fx: drop the live override so the stored param takes over again, then
    // regenerate the playing part with the un-modulated value.
    if (m.dest === 'midi') {
      const key = `${m.tid}|${m.fxId}|${m.pkey}`
      this.midiFxMod.delete(key); this.midiFxQuant.delete(key)
      const rec = this.built.get(m.tid)
      if (rec?.part) this.refreshPart(rec)
      return
    }
    const t = trackById(m.tid)
    const rec = this.built.get(m.tid)
    if (!t || !rec) return
    const r = this.resolveTarget(t, rec, m.dest, m.fxId, m.pkey)
    if (r) r.setter(r.base)
  }

  /** Resolve a master-bus automation key to its spec, base value and live setter. */
  private resolveMasterTarget(key: string): { spec: ParamSpec; base: number; setter: (v: number) => void } | null {
    const [dest, fxId, pkey] = key.split('|')
    if (dest === 'mix' && pkey === 'gain') {
      const spec = mixSpec('gain'); if (!spec) return null
      return { spec, base: (meta.get('masterGain') as number) ?? 0, setter: v => this.master.volume.rampTo(v, 0.02) }
    }
    if (dest === 'fx') {
      const fxMap = masterFx.toArray().find(f => f.get('id') === fxId)
      const bf = this.builtMasterFx.find(f => f.id === fxId)
      if (!fxMap || !bf) return null
      const spec = fxSchema(fxMap.get('type')).params.find(s => s.key === pkey)
      const base = (fxMap.get('params') as Y.Map<number>).get(pkey)
      if (!spec || typeof base !== 'number') return null
      return { spec, base, setter: v => bf.fx.set(pkey, v) }
    }
    return null
  }

  /** Live LFO output [-1,1] for the UI indicator. */
  lfoValue(lfoId: string) {
    return this.lfoVals.get(lfoId) ?? 0
  }

  // ---------------- master analysers ----------------
  getSpectrum(): Float32Array {
    return (this.masterFFT?.getValue() as Float32Array) ?? new Float32Array(0)
  }
  getWaveform(): Float32Array {
    return (this.masterWave?.getValue() as Float32Array) ?? new Float32Array(0)
  }
  /** Rough fundamental-frequency estimate (autocorrelation) for the tuner.
   *  Coarse pass strides both the lag and the samples by 2 (~¼ the multiplies
   *  of the old full O(n²) scan — that was ~262k mults per frame), then a full-
   *  precision refine around the winner keeps the reported lag exact. */
  getPitchHz(): number {
    const buf = this.getWaveform()
    if (!buf.length) return 0
    const sr = this.sampleRate || 44100
    let rms = 0
    for (let i = 0; i < buf.length; i++) rms += buf[i] * buf[i]
    if (Math.sqrt(rms / buf.length) < 0.01) return 0
    const half = buf.length / 2
    let bestLag = -1, bestCorr = 0
    for (let lag = 8; lag < half; lag += 2) {
      let c = 0
      for (let i = 0; i < half; i += 2) c += buf[i] * buf[i + lag]
      if (c > bestCorr) { bestCorr = c; bestLag = lag }
    }
    if (bestLag < 0) return 0
    let fineLag = bestLag, fineCorr = -Infinity
    for (let lag = Math.max(8, bestLag - 2); lag <= Math.min(half - 1, bestLag + 2); lag++) {
      let c = 0
      for (let i = 0; i < half; i++) c += buf[i] * buf[i + lag]
      if (c > fineCorr) { fineCorr = c; fineLag = lag }
    }
    return sr / fineLag
  }

  // ---------------- graph building ----------------

  /** ids of effects on track `tid` that some LFO / macro / automation lane /
   *  clip envelope targets — those must stay wired even when currently neutral,
   *  so the modulation can open them up mid-performance. */
  private fxModTargets(t: Y.Map<any>, tid: string): Set<string> {
    const s = new Set<string>()
    ;(t.get('lfos') as Y.Array<Y.Map<any>> | undefined)?.forEach(l => {
      ;(l.get('targets') as Y.Array<Y.Map<any>> | undefined)?.forEach(tg => { if (tg.get('dest') === 'fx') s.add(tg.get('fxId') || '') })
      if (l.get('dest') === 'fx') s.add((l.get('fxId') as string) || '')   // legacy single-field mapping
    })
    ;(t.get('macros') as Y.Array<Y.Map<any>> | undefined)?.forEach(m => {
      ;(m.get('targets') as Y.Array<Y.Map<any>> | undefined)?.forEach(tg => { if (tg.get('dest') === 'fx') s.add(tg.get('fxId') || '') })
    })
    ;(t.get('auto') as Y.Map<any> | undefined)?.forEach((_a, k) => {
      const [dest, fxId] = (k as string).split('|')
      if (dest === 'fx') s.add(fxId || '')
    })
    // clip envelopes: any session clip on this track + any arrangement clip
    clips.forEach((cm: Y.Map<any>, key: string) => {
      if (!key.startsWith(tid + '|')) return
      for (const k of envKeys(cm)) { const [dest, fxId] = k.split('|'); if (dest === 'fx') s.add(fxId || '') }
    })
    arr.forEach((cm: Y.Map<any>) => {
      if (cm.get('trackId') !== tid) return
      for (const k of envKeys(cm)) { const [dest, fxId] = k.split('|'); if (dest === 'fx') s.add(fxId || '') }
    })
    return s
  }

  /** Build the BuiltFx list for a track's current fx array (enabled devices
   *  only), marking neutral non-targeted ones bypassed so wireFx skips them.
   *  `duckOnly` (frozen tracks): everything except Sidechain is baked audio. */
  private buildFxList(t: Y.Map<any>, tid: string, duckOnly = false): BuiltFx[] {
    const targets = this.fxModTargets(t, tid)
    const fx: BuiltFx[] = []
    ;(t.get('fx') as Y.Array<Y.Map<any>>).forEach(f => {
      if (!f.get('on')) return
      if (duckOnly && f.get('type') !== 'duck') return
      const type = f.get('type')
      const params = (f.get('params') as Y.Map<number>).toJSON()
      fx.push({
        id: f.get('id'), type,
        fx: makeEffect(type, params),
        // per-device output level (pre-meter) + its level meter (Tone.Volume
        // preserves stereo; Tone.Channel/Panner would downmix)
        out: new Tone.Volume(f.get('out') ?? 0),
        meter: new Tone.Meter({ smoothing: 0.8 }),
        bypassed: fxNeutral(type, params) && !targets.has(f.get('id')),
      })
    })
    return fx
  }

  /** (Re)wire instOut → [audible fx…] → panner, disconnecting everything first.
   *  Wiring only — device nodes and their internal state are left untouched, so
   *  a rewire never drops a delay/reverb tail. Bypassed devices are skipped
   *  (a node with no path to the destination costs ~no audio-thread CPU). */
  private wireFx(tid: string, instOut: Tone.Volume, instMeter: Tone.Meter, fx: BuiltFx[], panner: StereoPannerNode) {
    const metered = tid === this.meteredTrackId
    try { instOut.disconnect() } catch { /* ok */ }
    // Per-instrument & per-effect meters only feed an AnalyserNode while THIS
    // track's device rack is open (engine.meteredTrackId). Off-panel tracks skip
    // the meter taps entirely — N fewer always-on analysers on the audio thread.
    if (metered) instOut.connect(instMeter) // .connect (not Tone.connect) so the meter reads POST-gain
    for (const bf of fx) {
      try { (bf.fx.node as any).disconnect() } catch { /* ok */ }
      try { bf.out.disconnect() } catch { /* ok */ }
      if (metered) bf.out.connect(bf.meter)
    }
    // Per device: node → outGain → meter(tap); series continues from outGain so
    // the output knob is genuinely PRE-meter.
    let prev: any = instOut
    for (const bf of fx) {
      if (bf.bypassed) continue
      Tone.connect(prev, bf.fx.node)
      if (bf.fx.detect) Tone.connect(prev, bf.fx.detect) // pre-effect tap (Auto-Tune fallback / EQ7 RTA)
      Tone.connect(bf.fx.node, bf.out)
      prev = bf.out
    }
    Tone.connect(prev, panner)
  }

  /** Re-derive each device's bypassed flag; rewire only when something flipped.
   *  Called (debounced) after param edits cross a neutral boundary and after
   *  modulation-target topology changes (LFO/macro/automation/envelope edits). */
  private refreshFxWiring(tid: string) {
    const rec = this.built.get(tid)
    const t = trackById(tid)
    if (!rec || !t || !rec.fx.length) return
    const targets = this.fxModTargets(t, tid)
    let changed = false
    for (const bf of rec.fx) {
      const fxMap = this.findFxMap(t, bf.id)
      const want = fxMap
        ? fxNeutral(bf.type, (fxMap.get('params') as Y.Map<number>).toJSON()) && !targets.has(bf.id)
        : bf.bypassed
      if (want !== bf.bypassed) { bf.bypassed = want; changed = true }
    }
    if (changed) {
      this.wireFx(tid, rec.instOut, rec.instMeter, rec.fx, rec.panner)
      this.rebuildDuckTriggers()   // a bypassed duck must not fire
      this.emit()
    }
  }

  private fxRewireTimers = new Map<string, ReturnType<typeof setTimeout>>()
  private scheduleFxRewire(tid: string) {
    const prev = this.fxRewireTimers.get(tid)
    if (prev) clearTimeout(prev)
    this.fxRewireTimers.set(tid, setTimeout(() => {
      this.fxRewireTimers.delete(tid)
      this.refreshFxWiring(tid)
    }, 50))
  }

  // Modulation-target topology changed somewhere (LFO mapping, macro mapping,
  // automation lane, clip envelope) → re-check every chain's bypass flags.
  private wiringRefreshTimer: ReturnType<typeof setTimeout> | null = null
  private scheduleWiringRefresh() {
    if (!this.started) return
    if (this.wiringRefreshTimer) clearTimeout(this.wiringRefreshTimer)
    this.wiringRefreshTimer = setTimeout(() => {
      this.wiringRefreshTimer = null
      for (const rec of this.built.values()) this.refreshFxWiring(rec.id)
      this.scheduleMasterFxRewire()
    }, 150)
  }

  // ---------------- track freeze ----------------

  /** Bounce a track's device chain offline, then rebuild it as a buffer player.
   *  Sends/pan/fader stay live; ducks stay live; everything else is baked
   *  (including LFOs/automation that target inst/fx params). */
  async freezeTrack(trackId: string) {
    if (this.frozen.has(trackId) || this.freezingIds.has(trackId)) return
    const t = trackById(trackId)
    if (!t || t.get('kind') === 'bus') { toast('Buses can’t be frozen — freeze the tracks feeding them'); return }
    await this.ensureStarted()
    this.freezingIds.add(trackId)
    this.emit()
    toast(`Freezing “${t.get('name')}”…`)
    try {
      const data = await renderFreezeTrack(trackId)
      if (!data || (data.clips.size === 0 && !data.arr)) {
        toast('Nothing to freeze — the track has no clips')
        return
      }
      this.frozen.set(trackId, data)
      this.rebuildTrack(trackId)
      this.refreshArrIfPlaying()
      toast(`“${t.get('name')}” frozen — device processing bounced ✓`)
    } catch (e) {
      console.error('freeze failed', e)
      toast(`Freeze failed: ${String((e as Error)?.message || e).slice(0, 80)}`)
    } finally {
      this.freezingIds.delete(trackId)
      this.emit()
    }
  }

  unfreezeTrack(trackId: string) {
    if (!this.frozen.delete(trackId)) return
    this.rebuildTrack(trackId)
    this.refreshArrIfPlaying()
    toast('Track unfrozen — devices live again')
    this.emit()
  }

  /** An edit invalidated a frozen bounce → drop it and go live again. */
  private lastUnfreezeToast = 0
  private autoUnfreeze(trackId: string, what: string) {
    if (!this.frozen.delete(trackId)) return
    this.scheduleRebuildTrack(trackId)
    if (this.mode === 'arr') setTimeout(() => this.refreshArrIfPlaying(), 120)
    const now = performance.now()
    if (now - this.lastUnfreezeToast > 1500) {
      this.lastUnfreezeToast = now
      toast(`Track unfrozen — ${what}`)
    }
    this.emit()
  }

  /** Arrangement parts hold node references — regenerate them after a frozen
   *  track's graph was swapped mid-play. */
  private refreshArrIfPlaying() {
    if (this.mode === 'arr' && this.transport.state === 'started') {
      this.clearArrParts()
      this.buildArrParts()
    }
  }

  /** Loop a frozen clip buffer in sync with the transport. The player is
   *  re-fired at every loop boundary by a transport repeat (sample-accurate,
   *  proven click-free by the audio-clip one-shot path; a time-looped Player
   *  would accumulate sub-sample rounding drift against the tick grid). */
  private startFrozenLoop(rec: BuiltTrack, entry: FrozenClip, anchorTicks: number) {
    const player = new Tone.Player(entry.buf as any)
    player.connect(rec.inst.out as any)
    const len = Math.max(1, entry.lenTicks)
    const now = this.transport.state === 'started' ? this.transport.ticks : 0
    let firstAt = anchorTicks
    if (firstAt < now) {
      const into = (((now - anchorTicks) % len) + len) % len
      firstAt = into === 0 ? now : now + (len - into)
      // enter mid-loop immediately; the boundary re-fire takes over from there
      if (into > 0) { try { player.start(undefined, Tone.Ticks(into).toSeconds()) } catch { /* ok */ } }
    }
    rec.clipRepeat = this.transport.scheduleRepeat(time => {
      try { player.stop(time); player.start(time) } catch { /* ok */ }
    }, `${len}i`, `${firstAt}i`)
    rec.player = player
  }

  /** Session-launch a frozen clip (mirrors launchAudioClip's boundary logic). */
  private launchFrozenClip(trackId: string, sceneId: string, key: string) {
    const rec = this.built.get(trackId)
    const entry = this.frozen.get(trackId)?.clips.get(key)
    if (!rec || !entry) return
    const started = this.transport.state === 'started'
    const atTicks = started ? this.nextBoundaryTicks() : 0
    this.cancelQueued(rec)
    if (rec.part) { try { rec.part.stop(); rec.part.dispose() } catch { /* ok */ } rec.part = null }
    rec.unobserve?.(); rec.unobserve = null
    const oldPlayer = rec.player
    this.clearClipRepeat(rec)
    if (!started) { this.transport.ticks = 0 as any; this.transport.start('+0.05'); this.resumeArmed(trackId) }
    this.startFrozenLoop(rec, entry, atTicks)
    if (oldPlayer) { try { oldPlayer.stop(`${atTicks}i`) } catch { /* ok */ } setTimeout(() => { try { oldPlayer.unsync(); oldPlayer.dispose() } catch { /* ok */ } }, 900) }
    rec.partKey = key
    rec.partStartTicks = atTicks
    rec.partLoopTicks = entry.lenTicks
    this.armFollow(trackId, sceneId, atTicks)
    this.emit()
  }

  /** Clip CONTENT edits (notes, envelopes, audio fields) invalidate a frozen
   *  bounce — playback observers only cover the playing clip, so a dedicated
   *  deep observer guards every clip of every frozen track. */
  private onClipsDeepFreeze = (events: Y.YEvent<any>[]) => {
    if (!this.frozen.size) return
    for (const ev of events) {
      if (ev.path.length > 0) {
        const tid = String(ev.path[0]).split('|')[0]
        if (this.frozen.has(tid)) this.autoUnfreeze(tid, 'a clip was edited')
      } else {
        ;(ev as Y.YMapEvent<any>).changes.keys.forEach((_c, key) => {
          const tid = key.split('|')[0]
          if (this.frozen.has(tid)) this.autoUnfreeze(tid, 'a clip was added or removed')
        })
      }
    }
  }

  private buildTrack(t: Y.Map<any>) {
    const tid = t.get('id') as string
    const it = t.get('inst') as Y.Map<any>
    const instType = it.get('type')
    // FROZEN: the instrument and (non-duck) fx are already bounced into audio
    // buffers — build only a passthrough input (frozen players connect into it)
    // plus any Sidechain devices, then the normal pan/fader/sends strip. The
    // heavy half of the chain simply doesn't exist while frozen.
    const isFrozenTrack = this.frozen.has(tid) && t.get('kind') !== 'bus'
    const buf = (!isFrozenTrack && (instType === 'sampler' || instType === 'ksampler' || instType === 'granular')) ? getSampleBuffer(it.get('sampleId') || '') : undefined
    // drum: resolve any per-pad sample overrides to buffers (missing ones stay synth)
    let padBuffers: Map<number, AudioBuffer> | undefined
    if (!isFrozenTrack && it.get('type') === 'drum') {
      const ps = it.get('padSamples') as Y.Map<string> | undefined
      if (ps && ps.size) {
        padBuffers = new Map()
        ps.forEach((sid, k) => { const b = getSampleBuffer(sid); if (b) padBuffers!.set(+k, b) })
      }
    }
    const inst = isFrozenTrack
      ? makeInstrument('audiobus', {})
      : makeInstrument(it.get('type'), (it.get('params') as Y.Map<number>).toJSON(), buf, padBuffers)
    const fx = this.buildFxList(t, tid, isFrozenTrack)
    // Stereo-preserving strip: pan → volume. We use a RAW StereoPannerNode
    // because Tone.Panner (like Tone.Channel) downmixes stereo input to mono —
    // which would flatten stereo samples and stereo effects.
    const ctx = Tone.getContext().rawContext as AudioContext
    const panner = ctx.createStereoPanner()
    panner.pan.value = t.get('pan') ?? 0
    const vol = new Tone.Volume(t.get('gain') ?? 0)
    const meter = new Tone.Meter({ smoothing: 0.8 })
    // frozen: the instrument's out gain is baked into the bounce → neutral here
    const instOut = new Tone.Volume(isFrozenTrack ? 0 : (it.get('out') ?? 0))
    const instMeter = new Tone.Meter({ smoothing: 0.8 })
    Tone.connect(inst.out, instOut)
    this.wireFx(tid, instOut, instMeter, fx, panner)
    Tone.connect(panner, vol)
    const isBus = t.get('kind') === 'bus'
    // Buses route to their `output` target (master or another bus) in rewireBuses,
    // after every track exists; normal tracks go straight to master.
    if (!isBus) vol.connect(this.master)
    // NOTE: the track meter is NOT tapped here — meterDb() connects it lazily
    // on first poll and sweepMeters() detaches it when nothing reads it, so
    // off-screen tracks pay no analyser cost.
    // post-fader sends feed the return buses
    const sendA = new Tone.Gain(t.get('sendA') ?? 0)
    const sendB = new Tone.Gain(t.get('sendB') ?? 0)
    vol.connect(sendA)
    vol.connect(sendB)
    const rec: BuiltTrack = {
      id: tid, kind: t.get('kind'), inst, instOut, instMeter, fx, panner, vol, meter, sendA, sendB,
      muted: false,
      outNode: isBus ? null : this.master, busSends: new Map(),
      part: null, player: null, clipRepeat: null, partKey: null, partStartTicks: 0, partLoopTicks: BAR,
      queuedKey: null, queuedPart: null, unobserve: null,
      fbDelay: null, fbSend: null,
    }
    // Feedback bus: post-fader → short delay → loop gain → back into the bus input
    // (inst.out). The delay makes the cycle a legal Web-Audio graph and gives the
    // regeneration an audible spacing; the loop gain is the "send level".
    if (t.get('send') === 'F') {
      rec.fbDelay = new Tone.Delay({ delayTime: 0.09, maxDelay: 1 })
      rec.fbSend = new Tone.Gain((t.get('sends') as Y.Map<number> | undefined)?.get(tid) ?? 0.3)
      vol.connect(rec.fbDelay); rec.fbDelay.connect(rec.fbSend); rec.fbSend.connect(inst.out)
    }
    this.built.set(tid, rec)
    this.wireSends(rec)
    this.applyMuteSolo()
    return rec
  }

  /** A bus's input node (where sends + upstream-bus outputs land) = its passthrough. */
  private busInputOf(busId: string): Tone.ToneAudioNode | null {
    const r = this.built.get(busId)
    return r && r.kind === 'bus' ? (r.inst.out as Tone.ToneAudioNode) : null
  }

  /**
   * (Re)connect every bus's output to its target and every track's per-bus send.
   * Idempotent + cheap; run after graph builds and on any output/send change.
   * A delay-less Web-Audio cycle is dropped by the engine, so a user-confirmed
   * feedback route is harmless here.
   */
  private rewireBuses() {
    // 1) bus outputs → master or another bus's input
    this.built.forEach(rec => {
      if (rec.kind !== 'bus') return
      const out = (trackById(rec.id)?.get('output') as string) || 'master'
      const target = (out !== 'master' && this.busInputOf(out)) || this.master
      if (rec.outNode !== target) {
        if (rec.outNode) { try { rec.vol.disconnect(rec.outNode) } catch { /* ok */ } }
        try { rec.vol.connect(target) } catch { /* ok */ }
        rec.outNode = target
      }
    })
    // 2) per-track bus sends (vol → gain → bus input). Lazily create a gain the
    //    first time a send is non-zero; thereafter just ramp it.
    const busIds = [...this.built.values()].filter(r => r.kind === 'bus').map(r => r.id)
    this.built.forEach(rec => {
      const sm = trackById(rec.id)?.get('sends') as Y.Map<number> | undefined
      // drop sends to buses that no longer exist
      rec.busSends.forEach((g, bid) => {
        if (!busIds.includes(bid)) { try { g.disconnect() } catch { /* ok */ } try { rec.vol.disconnect(g) } catch { /* ok */ } g.dispose(); rec.busSends.delete(bid) }
      })
      for (const bid of busIds) {
        if (bid === rec.id) {
          // the Feedback bus loops into itself through its dedicated delay path
          if (rec.fbSend) rec.fbSend.gain.rampTo((sm?.get(bid) as number) ?? 0, 0.02)
          continue
        }
        const level = (sm?.get(bid) as number) ?? 0
        const g = rec.busSends.get(bid)
        if (g) g.gain.rampTo(level, 0.02)
        else if (level > 0) {
          const inp = this.busInputOf(bid); if (!inp) continue
          const ng = new Tone.Gain(level); rec.vol.connect(ng); ng.connect(inp); rec.busSends.set(bid, ng)
        }
      }
    })
    // 3) the built-in A/B sends feed busA/busB — (re)wire now that buses exist.
    this.built.forEach(rec => this.wireSends(rec))
  }

  /** Mute = own mute OR (some track soloed AND this one isn't). Engine-coordinated.
   *  Besides muting the fader, we CUT panner→vol on muted tracks: with no path to
   *  the destination, Web Audio stops rendering the whole upstream chain
   *  (instrument + effects + their always-on LFOs), so a muted track costs ~no
   *  audio-thread CPU. Sends/bus/meter wiring on the fader is left intact — the
   *  fader just receives silence — so routing logic (rewireBuses) is unaffected. */
  private applyMuteSolo() {
    const list = this.allTracks()
    const soloAny = list.some(t => t.get('solo'))
    for (const t of list) {
      const rec = this.built.get(t.get('id'))
      if (!rec) continue
      const muted = !!t.get('mute') || (soloAny && !t.get('solo'))
      // an idle bus (nothing routes into it) is cut exactly like a muted track
      const cut = muted || this.idleBusIds.has(rec.id)
      rec.vol.mute = muted
      if (cut !== rec.muted) {
        rec.muted = cut
        if (cut) { try { rec.panner.disconnect() } catch { /* ok */ } }
        else { try { Tone.connect(rec.panner, rec.vol) } catch { /* ok */ } }
      }
    }
  }

  // The per-track A/B sends feed the two built-in bus tracks (busA/busB). Old
  // projects without those buses fall back to the legacy return channels. Bus
  // tracks themselves are skipped so a bus can't send into itself (feedback).
  /** Input node of the built-in A or B send bus (tagged track.send), if present. */
  private sendBusInput(which: 'A' | 'B'): Tone.ToneAudioNode | null {
    for (const t of tracks.toArray()) {
      if (t.get('kind') === 'bus' && t.get('send') === which) return this.busInputOf(t.get('id'))
    }
    return null
  }
  private wireSends(rec: BuiltTrack) {
    try { rec.sendA.disconnect() } catch { /* ok */ }
    try { rec.sendB.disconnect() } catch { /* ok */ }
    if (rec.kind === 'bus') return
    const aIn = this.sendBusInput('A') ?? this.builtReturns[0]?.fx.node
    const bIn = this.sendBusInput('B') ?? this.builtReturns[1]?.fx.node
    if (aIn) rec.sendA.connect(aIn)
    if (bIn) rec.sendB.connect(bIn)
  }

  // ---------------- return buses ----------------
  private buildReturns() {
    this.builtReturns.forEach(r => { try { r.fx.dispose(); r.channel.dispose() } catch { /* ok */ } })
    this.builtReturns = []
    returns.forEach(r => {
      const fx = makeEffect(r.get('fxType'), (r.get('params') as Y.Map<number>).toJSON())
      const channel = new Tone.Volume(r.get('gain') ?? 0)
      fx.node.connect(channel)
      channel.connect(this.master)
      this.builtReturns.push({ id: r.get('id'), fx, channel })
    })
    this.built.forEach(rec => this.wireSends(rec))
  }

  private onReturnsDeep = (events: Y.YEvent<any>[]) => {
    if (!this.started) return
    let structural = false
    for (const ev of events) {
      const path = ev.path
      if (path.length === 0) { structural = true; continue }
      const idx = path[0] as number
      const br = this.builtReturns[idx]
      const rm = returns.get(idx)
      if (!br || !rm) { structural = true; continue }
      if (path.length === 1) {
        ev.changes.keys.forEach((_c, key) => {
          if (key === 'gain') br.channel.volume.rampTo(rm.get('gain') ?? 0, 0.05)
          else if (key === 'fxType') structural = true
        })
      } else if (path[path.length - 1] === 'params' || path[1] === 'params') {
        ev.changes.keys.forEach((_c, key) => {
          const v = (ev.target as Y.Map<number>).get(key)
          if (typeof v === 'number') br.fx.set(key, v)
        })
      }
    }
    if (structural) this.buildReturns()
  }

  // ---------------- master-bus effect chain ----------------
  // Everything sums at `this.master`; these effects sit between it and the
  // limiter, so the whole mix (every track, bus, return, audio clip) passes
  // through them — live and, mirrored in render.ts, in exports. Built with the
  // same makeEffect()/BuiltFx machinery as track effects, so the device rack
  // (target id 'master') drives it unchanged.
  /** Master-fx ids targeted by a master automation lane (must stay wired). */
  private masterFxTargets(): Set<string> {
    const s = new Set<string>()
    masterAuto.forEach((_a: Y.Array<any>, k: string) => {
      const [dest, fxId] = k.split('|')
      if (dest === 'fx') s.add(fxId || '')
    })
    return s
  }

  /** Re-derive master bypass flags; returns true when any flipped. */
  private applyMasterBypassFlags(): boolean {
    const targets = this.masterFxTargets()
    let changed = false
    for (const bf of this.builtMasterFx) {
      const fxMap = masterFx.toArray().find(f => f.get('id') === bf.id)
      const want = fxMap
        ? fxNeutral(bf.type, (fxMap.get('params') as Y.Map<number>).toJSON()) && !targets.has(bf.id)
        : bf.bypassed
      if (want !== bf.bypassed) { bf.bypassed = want; changed = true }
    }
    return changed
  }

  /** (Re)wire master → [audible fx…] → limiter + analyser taps. Wiring only —
   *  device state survives, mirroring wireFx. */
  private wireMasterFx() {
    try { this.master.disconnect() } catch { /* ok */ }   // detach old chain + meter taps
    const metered = this.meteredTrackId === 'master'
    for (const bf of this.builtMasterFx) {
      try { (bf.fx.node as any).disconnect() } catch { /* ok */ }
      try { bf.out.disconnect() } catch { /* ok */ }
      if (metered) bf.out.connect(bf.meter)
    }
    let prev: Tone.ToneAudioNode = this.master
    for (const bf of this.builtMasterFx) {
      if (bf.bypassed) continue
      Tone.connect(prev, bf.fx.node)
      if (bf.fx.detect) Tone.connect(prev, bf.fx.detect)
      Tone.connect(bf.fx.node, bf.out)
      prev = bf.out
    }
    this.masterChainOut = prev
    Tone.connect(prev, this.limiter)
    // analysers/meter tap the chain tail so the master meter + spectrum reflect fx
    ;(prev as any).connect(this.masterMeter)
    ;(prev as any).connect(this.masterFFT)
    ;(prev as any).connect(this.masterWave)
  }

  private buildMasterFx() {
    this.builtMasterFx.forEach(bf => { try { bf.fx.dispose() } catch { /* ok */ } try { bf.out.dispose() } catch { /* ok */ } try { bf.meter.dispose() } catch { /* ok */ } })
    this.builtMasterFx = []
    masterFx.forEach(f => {
      if (!f.get('on')) return
      this.builtMasterFx.push({
        id: f.get('id'), type: f.get('type'),
        fx: makeEffect(f.get('type'), (f.get('params') as Y.Map<number>).toJSON()),
        out: new Tone.Volume(f.get('out') ?? 0),
        meter: new Tone.Meter({ smoothing: 0.8 }),
        bypassed: false,
      })
    })
    this.applyMasterBypassFlags()
    this.wireMasterFx()
    this.recomputeModActive()   // a master duck/autotune contributes a tick
  }

  private scheduleMasterFxRebuild() {
    if (this.masterFxTimer) clearTimeout(this.masterFxTimer)
    this.masterFxTimer = setTimeout(() => { this.masterFxTimer = null; this.buildMasterFx() }, 50)
  }

  private masterRewireTimer: ReturnType<typeof setTimeout> | null = null
  private scheduleMasterFxRewire() {
    if (this.masterRewireTimer) clearTimeout(this.masterRewireTimer)
    this.masterRewireTimer = setTimeout(() => {
      this.masterRewireTimer = null
      if (this.applyMasterBypassFlags()) { this.wireMasterFx(); this.rebuildDuckTriggers(); this.emit() }
    }, 50)
  }

  private onMasterFxDeep = (events: Y.YEvent<any>[]) => {
    if (!this.started) return
    this.resolveCache.clear()   // master-fx param bases may have changed
    let structural = false
    for (const ev of events) {
      const path = ev.path
      if (path.length === 0) { structural = true; continue }   // add / remove / reorder
      const idx = path[0] as number
      const fxMap = masterFx.get(idx)
      const bf = fxMap && this.builtMasterFx.find(f => f.id === fxMap.get('id'))
      if (path.length >= 2 && (path[path.length - 1] === 'params' || path[1] === 'params')) {
        if (bf) {
          ev.changes.keys.forEach((_c, key) => { const v = (ev.target as Y.Map<number>).get(key); if (typeof v === 'number') bf.fx.set(key, v) })
          if (fxMap && fxNeutral(bf.type, (fxMap.get('params') as Y.Map<number>).toJSON()) !== bf.bypassed) this.scheduleMasterFxRewire()
        } else structural = true
      } else if (path.length === 2) {
        // direct keys on an fx map: 'out' is a live ramp; 'on'/'type' rebuild
        ev.changes.keys.forEach((_c, key) => {
          if (key === 'out' && bf) bf.out.volume.rampTo(fxMap!.get('out') ?? 0, 0.03)
          else structural = true
        })
      } else structural = true
    }
    if (structural) this.scheduleMasterFxRebuild()
  }

  private disposeTrack(rec: BuiltTrack) {
    this.stopTrackNow(rec)
    try {
      rec.inst.dispose()
      rec.instOut.dispose()
      rec.instMeter.dispose()
      rec.fx.forEach(f => { f.fx.dispose(); f.out.dispose(); f.meter.dispose() })
      rec.panner.disconnect()
      rec.vol.dispose()
      rec.meter.dispose()
      rec.sendA.dispose()
      rec.sendB.dispose()
      rec.busSends.forEach(g => { try { g.disconnect() } catch { /* ok */ } g.dispose() })
      rec.busSends.clear()
      if (rec.fbSend) { try { rec.fbSend.disconnect() } catch { /* ok */ } rec.fbSend.dispose() }
      if (rec.fbDelay) { try { rec.fbDelay.disconnect() } catch { /* ok */ } rec.fbDelay.dispose() }
    } catch { /* dispose races are harmless */ }
    this.built.delete(rec.id)
    this.meterConnected.delete(rec.id)
    this.meterLastPoll.delete(rec.id)
  }

  buildAll() {
    const ids = new Set<string>()
    tracks.forEach(t => ids.add(t.get('id')))
    for (const tid of [...this.frozen.keys()]) if (!ids.has(tid)) this.frozen.delete(tid)
    ;[...this.built.values()].filter(r => !ids.has(r.id)).forEach(r => this.disposeTrack(r))
    tracks.forEach(t => {
      if (!this.built.has(t.get('id'))) this.buildTrack(t)
    })
    this.rewireBuses()
    this.recomputeModActive()
    this.emit()
  }

  private rebuildTrack(tid: string) {
    if (!this.started) return
    const t = trackById(tid)
    const old = this.built.get(tid)
    const wasPlaying = old?.partKey ?? null
    const wasAnchor = old?.partStartTicks ?? 0
    if (old) this.disposeTrack(old)
    if (!t) { this.emit(); return }
    this.buildTrack(t)
    // resume the session clip that was playing, preserving its exact phase
    if (wasPlaying && this.transport.state === 'started' && this.mode === 'session') {
      this.startPartNow(tid, wasPlaying, wasAnchor)
    }
    // a rebuilt bus has a fresh input node — drop any sends pointing at the old
    // one so rewireBuses recreates them against the new input.
    if (t?.get('kind') === 'bus') {
      this.built.forEach(r => {
        const g = r.id !== tid && r.busSends.get(tid)
        if (g) { try { g.disconnect() } catch { /* ok */ } try { r.vol.disconnect(g) } catch { /* ok */ } g.dispose(); r.busSends.delete(tid) }
      })
    }
    this.rewireBuses()
    this.recomputeModActive()
    this.emit()
  }

  private scheduleRebuildTrack(tid: string) {
    const prev = this.rebuildTimers.get(tid)
    if (prev) clearTimeout(prev)
    this.rebuildTimers.set(tid, setTimeout(() => {
      this.rebuildTimers.delete(tid)
      this.rebuildTrack(tid)
    }, 60))
  }

  private scheduleFxRebuild(tid: string) {
    const prev = this.fxRebuildTimers.get(tid)
    if (prev) clearTimeout(prev)
    this.fxRebuildTimers.set(tid, setTimeout(() => {
      this.fxRebuildTimers.delete(tid)
      this.rebuildFxChain(tid)
    }, 50))
  }

  /**
   * Rebuild ONLY a track's effect chain, in place — the instrument, panner,
   * fader, sends and the *playing clip* are all left running untouched. Adding,
   * removing, reordering, bypassing or swapping an effect re-splices the fx nodes
   * between instOut and the panner without restarting the part, so playback never
   * skips or drifts out of time (the old behaviour rebuilt the whole track).
   */
  private rebuildFxChain(tid: string) {
    const rec = this.built.get(tid)
    const t = trackById(tid)
    if (!rec || !t) return
    // tear down the old fx nodes
    rec.fx.forEach(bf => { try { bf.fx.dispose() } catch { /* ok */ } try { bf.out.dispose() } catch { /* ok */ } try { bf.meter.dispose() } catch { /* ok */ } })
    // build the current chain and splice instOut → [fx…] → panner
    rec.fx = this.buildFxList(t, tid)
    this.wireFx(tid, rec.instOut, rec.instMeter, rec.fx, rec.panner)
    this.recomputeModActive()   // fx tick set may have changed (duck/autotune)
    this.emit()
  }

  // ---------------- doc observers ----------------

  private onTracksDeep = (events: Y.YEvent<any>[]) => {
    if (!this.started) return
    let membership = false
    const structural = new Set<string>()   // full track rebuild (instrument swap)
    const fxDirty = new Set<string>()       // fx-chain-only rebuild (no part restart)
    for (const ev of events) {
      const path = ev.path
      if (path.length === 0) { membership = true; continue }
      const t = tracks.get(path[0] as number)
      if (!t) { membership = true; continue }
      const tid = t.get('id') as string
      const rec = this.built.get(tid)
      if (!rec) { structural.add(tid); continue }
      // any edit that touches what the freeze baked → back to live processing
      if (this.frozen.has(tid)) {
        const BAKED = ['inst', 'fx', 'midifx', 'lfos', 'macros', 'auto']
        const sub = path.length > 1 ? String(path[1]) : ''
        let hit = BAKED.includes(sub)
        if (!hit && path.length === 1) ev.changes.keys.forEach((_c, key) => { if (BAKED.includes(key)) hit = true })
        if (hit) this.autoUnfreeze(tid, 'its devices were edited')
      }
      if (path.length === 1) {
        ev.changes.keys.forEach((_c, key) => {
          if (key === 'gain') rec.vol.volume.rampTo(t.get('gain'), 0.05)
          else if (key === 'pan') rec.panner.pan.setTargetAtTime(t.get('pan'), Tone.now(), 0.03)
          else if (key === 'mute') this.applyMuteSolo()
          else if (key === 'solo') this.applyMuteSolo()
          else if (key === 'sendA') rec.sendA.gain.rampTo(t.get('sendA') ?? 0, 0.03)
          else if (key === 'sendB') rec.sendB.gain.rampTo(t.get('sendB') ?? 0, 0.03)
          else if (key === 'output' || key === 'sends') this.rewireBuses()
          else if (key === 'inst') structural.add(tid)
          else if (key === 'fx') fxDirty.add(tid)
          else if (key === 'midifx') this.refreshPart(rec)
          else if (key === 'lfos' || key === 'macros' || key === 'auto') this.scheduleWiringRefresh()
        })
      } else if (path[1] === 'sends') {
        this.rewireBuses()
      } else if (path[1] === 'midifx') {
        this.refreshPart(rec)
      } else if (path[1] === 'inst') {
        if (path[path.length - 1] === 'params') {
          ev.changes.keys.forEach((_c, key) => {
            const v = (ev.target as Y.Map<number>).get(key)
            if (typeof v === 'number') rec.inst.set(key, v)
          })
        } else if (path.length === 2) {
          // direct keys on the instrument map: 'out' is a live gain, everything
          // else (type / sampleId / params swap) needs a rebuild
          let rebuild = false
          ev.changes.keys.forEach((_c, key) => {
            if (key === 'out') rec.instOut.volume.rampTo((t.get('inst') as Y.Map<any>).get('out') ?? 0, 0.03)
            else rebuild = true
          })
          if (rebuild) structural.add(tid)
        } else structural.add(tid)
      } else if (path[1] === 'fx') {
        if (path.length >= 4 && path[3] === 'params') {
          const fxMap = (t.get('fx') as Y.Array<Y.Map<any>>).get(path[2] as number)
          const bf = fxMap && rec.fx.find(f => f.id === fxMap.get('id'))
          if (bf) {
            ev.changes.keys.forEach((_c, key) => {
              const v = (ev.target as Y.Map<number>).get(key)
              if (typeof v === 'number') bf.fx.set(key, v)
            })
            // the edit may have crossed the neutral boundary (e.g. wet → 0, or
            // back up) → re-derive the device's bypass wiring (debounced)
            if (fxNeutral(bf.type, (fxMap!.get('params') as Y.Map<number>).toJSON()) !== bf.bypassed) this.scheduleFxRewire(tid)
          } else fxDirty.add(tid)
        } else if (path.length === 3) {
          // direct keys on an fx map: 'out' is a live gain; 'on'/'type' re-splice
          const fxMap = (t.get('fx') as Y.Array<Y.Map<any>>).get(path[2] as number)
          const bf = fxMap && rec.fx.find(f => f.id === fxMap.get('id'))
          let rebuild = false
          ev.changes.keys.forEach((_c, key) => {
            if (key === 'out' && bf) bf.out.volume.rampTo(fxMap!.get('out') ?? 0, 0.03)
            else rebuild = true
          })
          if (rebuild || !bf) fxDirty.add(tid)
        } else fxDirty.add(tid)   // add / remove / reorder an effect
      } else if (path[1] === 'lfos' || path[1] === 'macros' || path[1] === 'auto') {
        // modulation-target topology changed → bypassed devices may need to wake
        this.scheduleWiringRefresh()
      }
    }
    if (membership) this.buildAll()
    structural.forEach(tid => this.scheduleRebuildTrack(tid))
    // fx-chain changes re-splice in place (no part restart); skip if a full
    // rebuild is already queued for that track.
    fxDirty.forEach(tid => { if (!structural.has(tid)) this.scheduleFxRebuild(tid) })
    // LFO add/remove doesn't rebuild the graph but changes whether the
    // modulation loop must keep running — refresh the idle flag.
    this.recomputeModActive()
  }

  private onClipsShallow = (ev: Y.YMapEvent<any>) => {
    ev.changes.keys.forEach((change, key) => {
      if (change.action === 'delete') {
        this.clipEnvCache.clear()
        for (const rec of this.built.values()) {
          if (rec.partKey === key) this.stopTrackNow(rec)
          if (rec.queuedKey === key) {
            this.cancelQueued(rec)
            rec.queuedKey = null
          }
        }
        this.emit()
      }
    })
  }

  private onArrDeep = (events?: Y.YEvent<any>[]) => {
    // any arrangement change invalidates the clip index + cached clip envelopes
    this.arrIndexCache = null
    this.clipEnvCache.clear()
    this.scheduleWiringRefresh()   // arr clip envelopes are fx-bypass guards too
    // a frozen track's LANE bounce is invalidated by arrangement edits: clip
    // moves/edits name their track; membership changes (add/remove) can't, so
    // those conservatively unfreeze every track that carries a lane bounce
    if (this.frozen.size && events) {
      for (const ev of events) {
        if (ev.path.length > 0) {
          const tid = (arr.get(String(ev.path[0])) as Y.Map<any> | undefined)?.get('trackId')
          if (tid && this.frozen.has(tid)) this.autoUnfreeze(tid, 'the arrangement changed')
        } else {
          for (const [tid, d] of [...this.frozen]) if (d.arr) this.autoUnfreeze(tid, 'the arrangement changed')
        }
      }
    }
    if (!this.started || this.mode !== 'arr') return
    if (this.arrTimer) clearTimeout(this.arrTimer)
    this.arrTimer = setTimeout(() => {
      this.arrTimer = null
      if (this.mode === 'arr' && this.transport.state === 'started') {
        this.clearArrParts()
        this.buildArrParts()
      }
      this.recomputeBusIdle()   // a send envelope may have been added/removed
    }, 150)
  }

  private onMeta = (ev: Y.YMapEvent<any>) => {
    if (!this.started) return
    const t = this.transport
    ev.changes.keys.forEach((_c, key) => {
      // frozen bounces bake tempo/meter/key/humanize — any of these changing
      // means the baked audio no longer matches, so every frozen track goes live
      if (this.frozen.size && ['bpm', 'swing', 'swingSubdivision', 'tsTop', 'tsBottom', 'root', 'scale', 'humanize'].includes(key)) {
        for (const tid of [...this.frozen.keys()]) this.autoUnfreeze(tid, 'project settings changed')
      }
      if (key === 'bpm') {
        t.bpm.rampTo(meta.get('bpm'), 0.1)
        setRateBpm(meta.get('bpm') ?? 120)   // retune tempo-synced fx rates
        // tempo-synced delays follow the new tempo
        setTimeout(() => this.refreshDelays(), 150)
      } else if (key === 'swing') t.swing = meta.get('swing') ?? 0
      else if (key === 'swingSubdivision') t.swingSubdivision = (meta.get('swingSubdivision') as any) ?? '16n'
      else if (key === 'masterGain') {
        this.master.volume.rampTo(meta.get('masterGain') ?? 0, 0.05)
        this.resolveCache.delete('master|mix||gain')   // automation base changed
      }
      else if (key === 'loopOn' || key === 'loopStart' || key === 'loopEnd') this.applyLoopRegion()
      else if (key === 'tsTop' || key === 'tsBottom') {
        // meter change: re-accent the metronome and tell Tone's transport
        t.timeSignature = [timeSig().top, timeSig().bottom]
        this.scheduleMetronome()
      }
    })
  }

  private refreshDelays() {
    for (const t of tracks.toArray()) {
      const rec = this.built.get(t.get('id'))
      if (!rec) continue
      ;(t.get('fx') as Y.Array<Y.Map<any>>).forEach(f => {
        if (f.get('type') === 'delay') {
          const bf = rec.fx.find(x => x.id === f.get('id'))
          bf?.fx.set('time', (f.get('params') as Y.Map<number>).get('time') ?? 2)
        }
      })
    }
    // master-bus delays re-sync too
    masterFx.forEach(f => {
      if (f.get('type') === 'delay') {
        const bf = this.builtMasterFx.find(x => x.id === f.get('id'))
        bf?.fx.set('time', (f.get('params') as Y.Map<number>).get('time') ?? 2)
      }
    })
  }

  private applyLoopRegion() {
    const t = this.transport
    if (this.mode === 'arr' && meta.get('loopOn')) {
      t.setLoopPoints(`${meta.get('loopStart') ?? 0}i`, `${Math.max((meta.get('loopStart') ?? 0) + BAR, meta.get('loopEnd') ?? BAR * 4)}i`)
      t.loop = true
    } else {
      t.loop = false
    }
  }

  // ---------------- session playback ----------------

  /** Build the scheduled note events for a clip, applying the track's live MIDI fx. */
  private buildEvents(t: Y.Map<any> | undefined, clipMap: Y.Map<any>) {
    let notes = notesOf(clipMap).map(([, n]) => ({ ...n }))
    if (t) notes = this.applyMidiFx(t, notes, clipMap.get('len') ?? BAR)
    return notes.map(n => ({ time: `${n.s}i`, ...n }))
  }

  /** Transform a clip's note list through the track's MIDI-fx chain (offline expansion). */
  private applyMidiFx(t: Y.Map<any>, notes: Note[], loopLen: number): Note[] {
    const chain = midifxOf(t)
    if (!chain || chain.length === 0) return notes
    const tid = t.get('id') as string
    const data = chain.map(d => {
      const fxId = d.get('id') as string
      const params = (d.get('params') as Y.Map<number>).toJSON() as Record<string, number>
      // fold in any live LFO/macro override before expanding the notes
      if (this.midiFxMod.size) for (const s of midiFxSchema(d.get('type')).params) {
        const ov = this.midiFxMod.get(`${tid}|${fxId}|${s.key}`)
        if (ov !== undefined) params[s.key] = ov
      }
      return { type: d.get('type') as string, on: !!d.get('on'), params }
    })
    return applyMidiFxData(data, notes, loopLen, (meta.get('root') ?? 9) as number, (meta.get('scale') ?? 'minor') as string, t.get('kind') === 'drum')
  }

  private makePart(rec: BuiltTrack, clipMap: Y.Map<any>): Tone.Part {
    const events = this.buildEvents(trackById(rec.id), clipMap)
    const part = new Tone.Part((time, ev: any) => {
      if (ev.pr < 1 && Math.random() > ev.pr) return
      // Humanize: random per-note micro-variation, re-rolled every time the note
      // plays. 0 = dead on the grid; at 100% up to ±10% of a beat of timing
      // jitter and ±10% of velocity (scaled linearly by the knob). Read live so
      // the knob takes effect on the next note without rebuilding parts. Live
      // played notes don't pass through here, so the performer is never jittered.
      const h = (meta.get('humanize') as number) ?? 0
      let when = time
      let vel = ev.v
      if (h > 0) {
        const jitterTicks = (Math.random() * 2 - 1) * 0.1 * (BAR / 4) * h  // ±10% of a beat
        when = Math.max(Tone.immediate(), time + Tone.Ticks(jitterTicks).toSeconds())
        vel = clamp(ev.v + (Math.random() * 2 - 1) * 0.1 * h, 0.02, 1)
      }
      // skip a note that can't schedule (e.g. two on the same tick restarting one
      // oscillator-based voice) rather than killing the part's callback
      try { rec.inst.trigger(ev.p, Math.max(0.02, Tone.Ticks(ev.d).toSeconds()), when, vel) } catch { /* ok */ }
      this.fireDucks(rec.id, ev.p, when)
    }, events as any)
    part.loop = true
    part.loopStart = 0
    part.loopEnd = `${clipMap.get('len') ?? BAR}i`
    return part
  }

  /** Re-derive a playing part's events (after a MIDI-fx change). */
  private refreshPart(rec: BuiltTrack) {
    if (!rec.part || !rec.partKey) return
    const clipMap = clips.get(rec.partKey) as Y.Map<any> | undefined
    if (!clipMap) return
    rec.part.clear()
    this.buildEvents(trackById(rec.id), clipMap).forEach(ev => rec.part!.add(ev as any))
  }

  private observeClipForPart(rec: BuiltTrack, key: string, clipMap: Y.Map<any>) {
    const h = () => {
      this.clipEnvCache.delete(clipMap)   // envelope edits re-read next frame
      const prev = this.partTimers.get(rec.id)
      if (prev) clearTimeout(prev)
      this.partTimers.set(rec.id, setTimeout(() => {
        this.partTimers.delete(rec.id)
        this.recomputeBusIdle()   // a send envelope may have been added/removed
        this.scheduleWiringRefresh()   // an fx envelope may need a bypassed device awake
        if (rec.partKey !== key || !rec.part) return
        rec.part.clear()
        this.buildEvents(trackById(rec.id), clipMap).forEach(ev => rec.part!.add(ev as any))
        rec.part.loopEnd = `${clipMap.get('len') ?? BAR}i`
        rec.partLoopTicks = clipMap.get('len') ?? BAR
      }, 120))
    }
    clipMap.observeDeep(h)
    rec.unobserve = () => clipMap.unobserveDeep(h)
  }

  private stopTrackNow(rec: BuiltTrack) {
    if (rec.part) {
      try { rec.part.stop(); rec.part.dispose() } catch { /* ok */ }
    }
    this.clearClipRepeat(rec)
    if (rec.player) {
      try { rec.player.stop(); rec.player.unsync(); rec.player.dispose() } catch { /* ok */ }
      rec.player = null
    }
    this.cancelQueued(rec)
    this.clearFollow(rec.id)
    rec.unobserve?.()
    rec.unobserve = null
    rec.part = null
    rec.partKey = null
    rec.queuedKey = null
  }

  // ---------------- audio-clip playback ----------------
  private makePlayer(rec: BuiltTrack, clipMap: Y.Map<any>): Tone.Player {
    const sid = clipMap.get('sampleId') || ''
    const raw = getSampleBuffer(sid)
    const c = audioFieldsFromMap(clipMap)
    // crop + loop crossfade are baked into the played buffer; pitch/cents/reverse/
    // fades/gain are set live — same path the offline export uses.
    const buf = raw ? clipAudioBuffer(sid, raw, c.offset, c.dur, !!c.loop, c.xfade) : undefined
    const player = new Tone.Player(buf as any)
    configureAudioPlayer(player, c)
    player.connect(rec.inst.out)
    return player
  }

  /** Create + start a session audio-clip player, anchored at `atTicks`.
   *  loop enabled  → synced, loops the sample continuously for the whole run.
   *  loop disabled → a one-shot re-fired at every scene-clip-length boundary,
   *  so the clip re-triggers each time its parent scene loops around (rather
   *  than playing once and going silent). Sets rec.player (+ rec.clipRepeat). */
  private startSessionPlayer(rec: BuiltTrack, clipMap: Y.Map<any>, atTicks: number): Tone.Player {
    const player = this.makePlayer(rec, clipMap)
    if (clipMap.get('loop')) {
      try { player.sync().start(`${atTicks}i`) } catch { try { player.start() } catch { /* ok */ } }
    } else {
      const len = clipMap.get('len') ?? BAR
      // unsynced: the transport repeat is the clock — stop+start re-fires the
      // one-shot from its head at each boundary (proven click-free; cuts off a
      // still-playing take if the sample is longer than the scene length).
      rec.clipRepeat = this.transport.scheduleRepeat((time) => {
        try { player.stop(time); player.start(time) } catch { /* ok */ }
      }, `${len}i`, `${atTicks}i`)
    }
    rec.player = player
    return player
  }

  private clearClipRepeat(rec: BuiltTrack) {
    if (rec.clipRepeat !== null) { try { this.transport.clear(rec.clipRepeat) } catch { /* ok */ } rec.clipRepeat = null }
  }

  private observeAudioClip(rec: BuiltTrack, key: string, clipMap: Y.Map<any>) {
    const h = () => {
      this.clipEnvCache.delete(clipMap)
      if (rec.partKey !== key || !rec.player) return
      // Toggling Loop flips between "continuous synced loop" and "re-fire each
      // scene loop" — a structural change in how the player is driven, so
      // re-arm the clip rather than morphing a running player between modes.
      // Deferred out of the Yjs observer (re-check still-active + still-changed)
      // so we never re-enter the audio graph mid-transaction.
      const wantLoop = !!clipMap.get('loop')
      const isOneShot = rec.clipRepeat !== null
      if (wantLoop === isOneShot) {
        const tid = rec.id, sid = key.split('|')[1]
        setTimeout(() => {
          const r = this.built.get(tid)
          const c = clips.get(key) as Y.Map<any> | undefined
          if (!r || r.partKey !== key || !r.player || !c) return
          if ((!!c.get('loop')) === (r.clipRepeat !== null)) this.launchAudioClip(tid, sid, key)
        }, 60)
        return
      }
      const p = rec.player
      try {
        p.volume.rampTo(clipMap.get('gainDb') ?? 0, 0.03)
        p.playbackRate = audioRate(clipMap.get('pitch') ?? 0, clipMap.get('cents') ?? 0)
      } catch { /* ok */ }
    }
    clipMap.observeDeep(h)
    rec.unobserve = () => clipMap.unobserveDeep(h)
  }

  private launchAudioClip(trackId: string, sceneId: string, key: string) {
    const rec = this.built.get(trackId)
    const clipMap = clips.get(key) as Y.Map<any> | undefined
    if (!rec || !clipMap) return
    this.clipEnvCache.delete(clipMap)   // may have been edited while stopped
    const started = this.transport.state === 'started'
    const atTicks = started ? this.nextBoundaryTicks() : 0
    const atT = `${atTicks}i`
    this.cancelQueued(rec)
    if (rec.part) { try { rec.part.stop(); rec.part.dispose() } catch { /* ok */ } rec.part = null }
    rec.unobserve?.(); rec.unobserve = null
    const oldPlayer = rec.player
    this.clearClipRepeat(rec)   // stop the previous clip's re-fire clock (if any)
    if (!started) { this.transport.ticks = 0 as any; this.transport.start('+0.05'); this.resumeArmed(trackId) }
    this.startSessionPlayer(rec, clipMap, atTicks)
    if (oldPlayer) { try { oldPlayer.stop(atT) } catch { /* ok */ } setTimeout(() => { try { oldPlayer.unsync(); oldPlayer.dispose() } catch { /* ok */ } }, 900) }
    rec.partKey = key
    rec.partStartTicks = atTicks
    rec.partLoopTicks = clipMap.get('len') ?? BAR
    this.observeAudioClip(rec, key, clipMap)
    this.armFollow(trackId, sceneId, atTicks)
    this.emit()
  }

  private clearFollow(trackId: string) {
    const ft = this.followTimers.get(trackId)
    if (ft !== undefined) { this.transport.clear(ft); this.followTimers.delete(trackId) }
  }

  /** Arm a clip's follow action: after N bars, jump/stop per its config. */
  private armFollow(trackId: string, sceneId: string, startTicks: number) {
    this.clearFollow(trackId)
    const clipMap = clips.get(clipKey(trackId, sceneId)) as Y.Map<any> | undefined
    if (!clipMap) return
    const f = followOf(clipMap)
    if (!f || !f.on) return
    const at = startTicks + Math.max(1, f.bars) * BAR
    const id = this.transport.scheduleOnce(() => {
      this.followTimers.delete(trackId)
      const rec = this.built.get(trackId)
      if (!rec || rec.partKey !== clipKey(trackId, sceneId)) return
      if (f.chance < 1 && Math.random() > f.chance) { this.armFollow(trackId, sceneId, this.transport.ticks); return }
      this.performFollow(trackId, sceneId, f.action)
    }, `${at}i`)
    this.followTimers.set(trackId, id)
  }

  private performFollow(trackId: string, sceneId: string, action: number) {
    if (action === 5) { this.stopClip(trackId); return } // Stop
    const withClip: { sid: string }[] = []
    for (let i = 0; i < scenes.length; i++) {
      const sid = scenes.get(i).get('id')
      if (clips.get(clipKey(trackId, sid))) withClip.push({ sid })
    }
    if (!withClip.length) return
    const pos = withClip.findIndex(w => w.sid === sceneId)
    let target: { sid: string } | undefined
    if (action === 0) target = withClip[(pos + 1) % withClip.length]                       // Next
    else if (action === 1) target = withClip[(pos - 1 + withClip.length) % withClip.length] // Prev
    else if (action === 2) target = withClip[0]                                              // First
    else if (action === 3) target = withClip[Math.floor(Math.random() * withClip.length)]   // Any
    else if (action === 4) {                                                                 // Random (other)
      const others = withClip.filter(w => w.sid !== sceneId)
      const pool = others.length ? others : withClip
      target = pool[Math.floor(Math.random() * pool.length)]
    }
    if (target) this.launchClip(trackId, target.sid)
  }

  private cancelQueued(rec: BuiltTrack) {
    if (rec.queuedPart) {
      try { rec.queuedPart.stop(); rec.queuedPart.dispose() } catch { /* ok */ }
      rec.queuedPart = null
    }
  }

  private startPartNow(tid: string, key: string, anchorTicks?: number) {
    const rec = this.built.get(tid)
    const clipMap = clips.get(key) as Y.Map<any> | undefined
    if (!rec || !clipMap) return
    // frozen: resume as a synced buffer loop, preserving the anchor phase
    const fentry = this.frozen.get(tid)?.clips.get(key)
    if (fentry) {
      this.stopTrackNow(rec)
      const loopLen = Math.max(1, fentry.lenTicks)
      const nowTicks = this.transport.ticks
      const anchor = anchorTicks != null ? anchorTicks : nowTicks - (((nowTicks % loopLen) + loopLen) % loopLen)
      this.startFrozenLoop(rec, fentry, anchor)
      rec.partKey = key
      rec.partStartTicks = anchor
      rec.partLoopTicks = loopLen
      return
    }
    this.stopTrackNow(rec)
    // the clip may have been edited while stopped (no observer attached then)
    this.clipEnvCache.delete(clipMap)
    const part = this.makePart(rec, clipMap)
    const loopLen = clipMap.get('len') ?? BAR
    const nowTicks = this.transport.ticks
    // Anchor the loop so the clip keeps its phase. Resuming after a rebuild reuses
    // the previous anchor; otherwise snap to the loop boundary at/under now. The
    // part starts AT the anchor with NO offset, so its position at `now` resolves
    // to (now - anchor) mod loopLen — grid-aligned. (The old code passed both a
    // 0 start time AND an offset, double-counting the phase → playback drifted.)
    const anchor = anchorTicks != null ? anchorTicks : nowTicks - (((nowTicks % loopLen) + loopLen) % loopLen)
    part.start(`${anchor}i`)
    rec.part = part
    rec.partKey = key
    rec.partStartTicks = anchor
    rec.partLoopTicks = loopLen
    this.observeClipForPart(rec, key, clipMap)
  }

  /**
   * The metronome ticks at the meter's counted note value (the bottom numeral)
   * and accents beat 1 of every bar — re-scheduled whenever the time signature
   * changes so 7/8 accents every 7 eighths, 3/4 every 3 quarters, etc.
   */
  private scheduleMetronome() {
    const t = this.transport
    if (this.metroRepeatId !== null) { try { t.clear(this.metroRepeatId) } catch { /* ok */ } }
    const bt = beatTicks()
    const bar = barTicks()
    this.metroRepeatId = t.scheduleRepeat(time => {
      if (!ui.metronome) return
      // getTicksAtTime returns fractional ticks; round to the nearest beat so
      // the bar-start accent reliably lands instead of drifting off the tick.
      const beat = Math.round(t.getTicksAtTime(time) / bt)
      const accent = ((beat * bt) % bar + bar) % bar === 0
      this.metro.triggerAttackRelease(accent ? 1760 : 1175, 0.03, time, accent ? 0.5 : 0.25)
    }, `${bt}i`)
  }

  /**
   * Next launch boundary as a TRANSPORT TICK position. Everything is scheduled
   * in ticks ("Ni" notation) — never wall-clock seconds — because the audio
   * context clock and the transport timeline drift apart over a session, and
   * mixing the two delays launches by exactly that drift.
   */
  private nextBoundaryTicks(): number {
    const q = meta.get('launchQ') ?? 1
    const cur = Math.round(this.transport.ticks)
    if (q === 0) return cur + 4 // tiny lookahead ≈ immediate
    const qTicks = q * barTicks()   // quantize follows the current meter's bar
    return Math.ceil((cur + 1) / qTicks) * qTicks
  }

  ensureSessionMode() {
    if (this.mode === 'arr') {
      this.clearArrParts()
      this.mode = 'session'
      this.transport.loop = false
    }
  }

  async launchClip(trackId: string, sceneId: string) {
    await this.ensureStarted()
    this.ensureSessionMode()
    const key = clipKey(trackId, sceneId)
    const rec = this.built.get(trackId)
    const clipMap = clips.get(key) as Y.Map<any> | undefined
    if (!rec || !clipMap) return
    const fz = this.frozen.get(trackId)
    if (fz) {
      if (fz.clips.has(key)) { this.launchFrozenClip(trackId, sceneId, key); return }
      // no bounce for this clip (shouldn't happen — edits auto-unfreeze) → go live
      this.autoUnfreeze(trackId, 'this clip wasn’t in the bounce')
    }
    if (isAudioClip(clipMap)) { this.launchAudioClip(trackId, sceneId, key); return }
    if (rec.partKey === key && !rec.queuedKey && !rec.queuedPart) return

    if (this.transport.state !== 'started') {
      this.transport.ticks = 0 as any
      this.transport.start('+0.05')
      // if we're resuming from a session pause, bring the other armed clips back
      this.resumeArmed(trackId)
      this.startPartNow(trackId, key)
      this.armFollow(trackId, sceneId, 0)
      this.emit()
      return
    }

    // replace any pending launch on this track
    this.cancelQueued(rec)
    const atTicks = this.nextBoundaryTicks()
    const atT = `${atTicks}i`
    const newPart = this.makePart(rec, clipMap)
    newPart.start(atT)
    rec.queuedPart = newPart
    rec.queuedKey = key
    const old = rec.part
    if (old) old.stop(atT)
    this.transport.scheduleOnce(() => {
      if (rec.queuedKey !== key || rec.queuedPart !== newPart) return
      rec.unobserve?.()
      rec.unobserve = null
      rec.part = newPart
      rec.queuedPart = null
      rec.partKey = key
      rec.queuedKey = null
      rec.partStartTicks = atTicks
      rec.partLoopTicks = clipMap.get('len') ?? BAR
      this.observeClipForPart(rec, key, clipMap)
      this.armFollow(trackId, sceneId, atTicks)
      if (old) setTimeout(() => { try { old.dispose() } catch { /* ok */ } }, 500)
      this.emit()
    }, atT)
    this.emit()
  }

  async stopClip(trackId: string) {
    const rec = this.built.get(trackId)
    // `partKey` is included so a clip armed during a session pause (no live part
    // yet) can still be un-armed by a stop.
    if (!rec || (!rec.part && !rec.player && !rec.queuedKey && !rec.queuedPart && !rec.partKey)) return
    if (this.transport.state !== 'started') {
      this.sessionArmed = this.sessionArmed.filter(a => a.trackId !== trackId)
      this.stopTrackNow(rec)
      this.emit()
      return
    }
    // audio clip: stop the player at the next boundary
    if (rec.player && !rec.part) {
      const atT = `${this.nextBoundaryTicks()}i`
      const p = rec.player
      this.clearClipRepeat(rec)   // stop the one-shot re-fire clock
      rec.player = null; rec.partKey = null
      rec.unobserve?.(); rec.unobserve = null
      this.clearFollow(trackId)
      try { p.stop(atT) } catch { try { p.stop() } catch { /* ok */ } }
      setTimeout(() => { try { p.unsync(); p.dispose() } catch { /* ok */ } }, 900)
      this.emit()
      return
    }
    this.cancelQueued(rec)
    const atT = `${this.nextBoundaryTicks()}i`
    rec.queuedKey = STOP
    const old = rec.part
    if (old) old.stop(atT)
    this.transport.scheduleOnce(() => {
      if (rec.queuedKey !== STOP) return
      rec.unobserve?.()
      rec.unobserve = null
      rec.part = null
      rec.partKey = null
      rec.queuedKey = null
      if (old) setTimeout(() => { try { old.dispose() } catch { /* ok */ } }, 500)
      this.emit()
    }, atT)
    this.emit()
  }

  async launchScene(sceneId: string) {
    await this.ensureStarted()
    for (const t of tracks.toArray()) {
      const tid = t.get('id')
      if (clips.get(clipKey(tid, sceneId))) this.launchClip(tid, sceneId)
      else this.stopClip(tid)
    }
  }

  stopAllClips() {
    tracks.forEach(t => this.stopClip(t.get('id')))
  }

  // ---------------- arrangement playback ----------------

  private clearArrParts() {
    this.arrParts.forEach(p => { try { p.stop(); p.dispose() } catch { /* ok */ } })
    this.arrParts = []
  }

  private buildArrParts() {
    const frozenStarted = new Set<string>()
    arr.forEach(clipMap => {
      const rec = this.built.get(clipMap.get('trackId'))
      if (!rec) return
      // frozen: ONE synced player carries the whole lane bounce (starts at
      // tick 0, so transport seeks resolve to the right offset automatically)
      const fz = this.frozen.get(rec.id)
      if (fz?.arr) {
        if (frozenStarted.has(rec.id)) return
        frozenStarted.add(rec.id)
        const player = new Tone.Player(fz.arr as any)
        player.connect(rec.inst.out as any)
        try { player.sync().start('0i') } catch { /* ok */ }
        this.arrParts.push(player)
        return
      }
      const start = clipMap.get('start') ?? 0
      const len = clipMap.get('len') ?? BAR
      if (isAudioClip(clipMap)) {
        if (!getSampleBuffer(clipMap.get('sampleId') || '')) return
        const player = this.makePlayer(rec, clipMap)
        try { player.sync().start(`${start}i`).stop(`${start + len}i`) } catch { /* ok */ }
        this.arrParts.push(player)
        return
      }
      const part = this.makePart(rec, clipMap)
      part.start(`${start}i`)
      part.stop(`${start + len}i`)
      this.arrParts.push(part)
    })
  }

  async playArrangement(fromTicks?: number) {
    await this.ensureStarted()
    const t = this.transport
    this.sessionArmed = []   // leaving session for the timeline drops any armed clips
    // stop session clips immediately — the timeline takes over
    this.built.forEach(rec => this.stopTrackNow(rec))
    if (t.state === 'started') t.stop()
    this.mode = 'arr'
    this.clearArrParts()
    this.buildArrParts()
    this.applyLoopRegion()
    const from = fromTicks ?? this.arrSeekTicks
    this.arrSeekTicks = from
    t.start('+0.05', `${from}i`)
    this.emit()
  }

  seekArr(ticks: number) {
    this.arrSeekTicks = Math.max(0, ticks)
    if (this.mode === 'arr' && this.transport.state === 'started') {
      this.transport.ticks = this.arrSeekTicks as any
    }
    this.emit()
  }

  async togglePlay() {
    await this.ensureStarted()
    if (this.transport.state === 'started') {
      // In the Session view, stopping is a PAUSE: the transport resets to the
      // top but each launched clip stays "armed" (its slot stays lit), so a
      // later Play restarts global playback and every armed clip is heard again
      // from bar 1. The arrangement timeline has no such notion — it hard-stops.
      if (this.mode === 'session') this.pauseSession()
      else this.stopAll()
      return
    }
    if (ui.view === 'arr') { this.playArrangement(); return }
    this.ensureSessionMode()
    this.transport.ticks = 0 as any
    this.transport.start('+0.05')
    this.resumeArmed()
    this.emit()
  }

  // Clips that were playing when the user hit Stop in the Session view. Kept so
  // the next Play resumes them (and so their slots stay visually active).
  private sessionArmed: { trackId: string; key: string }[] = []

  /** Session "stop" = pause: silence + rewind, but remember & re-mark active clips. */
  private pauseSession() {
    // A queued STOP means the user asked that track to stop, so don't re-arm it.
    this.sessionArmed = []
    this.built.forEach(rec => {
      if (rec.partKey && rec.partKey !== STOP && rec.queuedKey !== STOP) {
        this.sessionArmed.push({ trackId: rec.id, key: rec.partKey })
      }
    })
    this.built.forEach(rec => this.stopTrackNow(rec))
    const t = this.transport
    t.stop()
    t.loop = false
    t.ticks = 0 as any
    this.pendingRec.clear()
    // re-mark armed clips active (partKey set, no live part) so slots stay lit
    for (const a of this.sessionArmed) {
      const rec = this.built.get(a.trackId)
      if (!rec) continue
      rec.partKey = a.key
      rec.partStartTicks = 0
      rec.partLoopTicks = (clips.get(a.key) as Y.Map<any> | undefined)?.get('len') ?? BAR
    }
    this.emit()
  }

  /** Re-launch every clip armed by a session pause, anchored to tick 0. */
  private resumeArmed(exceptTrackId?: string) {
    if (!this.sessionArmed.length) return
    const armed = this.sessionArmed.filter(a => a.trackId !== exceptTrackId)
    this.sessionArmed = []
    for (const a of armed) this.resumeClipAtZero(a.trackId, a.key)
  }

  /** Start a single armed clip's part/player at transport tick 0 (transport must
   *  already be running). Used only by the session pause/resume path. */
  private resumeClipAtZero(trackId: string, key: string) {
    const rec = this.built.get(trackId)
    const clipMap = clips.get(key) as Y.Map<any> | undefined
    if (!rec || !clipMap) return
    const sceneId = key.split('|')[1]
    this.stopTrackNow(rec)   // clear the paused marker + any stale nodes
    this.clipEnvCache.delete(clipMap)
    const fentry = this.frozen.get(trackId)?.clips.get(key)
    if (fentry) {
      this.startFrozenLoop(rec, fentry, 0)
      rec.partKey = key
      rec.partStartTicks = 0
      rec.partLoopTicks = fentry.lenTicks
      this.armFollow(trackId, sceneId, 0)
      return
    }
    if (isAudioClip(clipMap)) {
      this.startSessionPlayer(rec, clipMap, 0)
      rec.partKey = key
      rec.partStartTicks = 0
      rec.partLoopTicks = clipMap.get('len') ?? BAR
      this.observeAudioClip(rec, key, clipMap)
    } else {
      this.startPartNow(trackId, key, 0)
    }
    this.armFollow(trackId, sceneId, 0)
  }

  stopAll() {
    this.sessionArmed = []
    this.built.forEach(rec => this.stopTrackNow(rec))
    this.clearArrParts()
    const t = this.transport
    t.stop()
    t.loop = false
    t.ticks = 0 as any
    this.pendingRec.clear()
    this.emit()
  }

  get playing() {
    return this.started && this.transport.state === 'started'
  }

  positionTicks() {
    if (!this.started) return 0
    return this.mode === 'arr' || this.transport.state === 'started' ? this.transport.ticks : 0
  }

  clipState(trackId: string, sceneId: string) {
    const rec = this.built.get(trackId)
    const key = clipKey(trackId, sceneId)
    return {
      playing: rec?.partKey === key,
      queued: rec?.queuedKey === key,
      stopQueued: rec?.partKey === key && rec?.queuedKey === STOP,
    }
  }

  clipProgress(trackId: string): number | null {
    const rec = this.built.get(trackId)
    if (!rec || !rec.partKey || this.transport.state !== 'started') return null
    const t = this.transport.ticks
    return ((t - rec.partStartTicks) % rec.partLoopTicks + rec.partLoopTicks) % rec.partLoopTicks / rec.partLoopTicks
  }

  // Track meters connect lazily: the analyser tap only exists while something
  // is actually polling it (the session-view meter bars), and a periodic sweep
  // detaches it again — e.g. in Arrangement view no track analysers run at all.
  private meterConnected = new Set<string>()
  private meterLastPoll = new Map<string, number>()
  meterDb(trackId: string): number {
    const rec = this.built.get(trackId)
    if (!rec) return -100
    this.meterLastPoll.set(trackId, performance.now())
    if (!this.meterConnected.has(trackId)) {
      try { rec.vol.connect(rec.meter) } catch { /* ok */ }
      this.meterConnected.add(trackId)
    }
    const v = rec.meter.getValue()
    return typeof v === 'number' ? v : Math.max(...(v as number[]))
  }
  private sweepMeters() {
    if (!this.meterConnected.size) return
    const cutoff = performance.now() - 1000
    for (const tid of [...this.meterConnected]) {
      if ((this.meterLastPoll.get(tid) ?? 0) >= cutoff) continue
      const rec = this.built.get(tid)
      if (rec) { try { rec.vol.disconnect(rec.meter) } catch { /* ok */ } }
      this.meterConnected.delete(tid)
      this.meterLastPoll.delete(tid)
    }
  }

  /** A built effect from either a track or the master bus (trackId === 'master'). */
  private findBuiltFx(trackId: string, fxId: string): BuiltFx | undefined {
    if (trackId === 'master') return this.builtMasterFx.find(f => f.id === fxId)
    return this.built.get(trackId)?.fx.find(f => f.id === fxId)
  }

  /** Per-device output level in dB. deviceId = 'inst' or an fx id. */
  deviceMeterDb(trackId: string, deviceId: string): number {
    const meter = deviceId === 'inst' ? this.built.get(trackId)?.instMeter : this.findBuiltFx(trackId, deviceId)?.meter
    if (!meter) return -100
    const v = meter.getValue()
    return typeof v === 'number' ? v : Math.max(...(v as number[]))
  }

  /** An effect's input-tap AnalyserNode (Auto-Tune detector, EQ Seven RTA). */
  deviceAnalyser(trackId: string, fxId: string): AnalyserNode | null {
    return this.findBuiltFx(trackId, fxId)?.fx.detect ?? null
  }

  /** Live gain reduction (dB, ≤0) for a compressor-type fx — drives the GR meter. */
  deviceReductionDb(trackId: string, fxId: string): number {
    return this.findBuiltFx(trackId, fxId)?.fx.gr?.() ?? 0
  }

  /** Per-band gain reduction (dB, ≤0) for the multiband device. */
  deviceBandReduction(trackId: string, fxId: string): number[] {
    return this.findBuiltFx(trackId, fxId)?.fx.grBands?.() ?? []
  }

  // ---- metered-track scoping ----
  // Per-instrument & per-effect meters are only useful in the open device rack.
  // The UI calls setMeteredTrack(selTrackId) while the rack is shown and null on
  // close, so only that one track's device meters feed analysers; every other
  // track skips them, cutting always-on audio-thread work.
  meteredTrackId: string | null = null
  setMeteredTrack(tid: string | null) {
    if (tid === this.meteredTrackId) return
    this.setMetersFor(this.meteredTrackId, false)
    this.meteredTrackId = tid
    this.setMetersFor(tid, true)
  }
  private setMetersFor(tid: string | null, on: boolean) {
    if (!tid) return
    if (tid === 'master') { this.builtMasterFx.forEach(f => { try { on ? f.out.connect(f.meter) : f.out.disconnect(f.meter) } catch { /* ok */ } }); return }
    const rec = this.built.get(tid)
    if (rec) this.setRecMeters(rec, on)
  }
  private setRecMeters(rec: BuiltTrack, on: boolean) {
    try {
      if (on) { rec.instOut.connect(rec.instMeter); rec.fx.forEach(f => f.out.connect(f.meter)) }
      else { try { rec.instOut.disconnect(rec.instMeter) } catch { /* ok */ } rec.fx.forEach(f => { try { f.out.disconnect(f.meter) } catch { /* ok */ } }) }
    } catch { /* ok */ }
  }

  // ---- modulation-loop activity ----
  // The per-frame modulation scan only needs to run while playing, or (when
  // stopped) if anything free-running exists: an LFO, or an fx with a tick
  // (sidechain ducker live-pumps, Auto-Tune corrects live input). Recomputed on
  // graph changes so applyModulation can early-out to a true idle otherwise.
  private modActive = false
  private recomputeModActive() {
    // Track/fx/LFO topology or values changed — drop the per-frame caches so
    // the modulation loop re-derives them once. (This is called from every
    // graph rebuild and from every onTracksDeep batch, so it doubles as the
    // central invalidation point.)
    this.lfoCfgCache.clear()
    this.resolveCache.clear()
    this.trackAutoCache.clear()
    let active = this.builtMasterFx.some(f => !!f.fx.tick)   // a master ducker/autotune ticks
    for (const r of this.built.values()) {
      if (active) break
      if (r.fx.some(f => !!f.fx.tick)) { active = true; break }
      const t = trackById(r.id)
      if (((t?.get('lfos') as Y.Array<any> | undefined)?.length ?? 0) > 0) { active = true; break }
    }
    this.modActive = active
    this.rebuildDuckTriggers()
    this.recomputeBusIdle()
  }

  // ---- sidechain trigger routing ----
  // Duck fx with a srcTrack field fire from that track's note events instead
  // of the beat grid: srcTrack id → the built duck devices it should trigger
  // (pitch −1 = any note; 0–7 = one drum pad). Rebuilt with the mod caches.
  private duckTriggers = new Map<string, { fx: Fx; pitch: number }[]>()
  private rebuildDuckTriggers() {
    this.duckTriggers.clear()
    const wire = (fxMap: Y.Map<any>, dev: Fx | undefined) => {
      if (fxMap.get('type') !== 'duck' || !dev) return
      const src = (fxMap.get('srcTrack') as string) || ''
      dev.set('srcOn', src && fxMap.get('on') ? 1 : 0)
      if (!src || !fxMap.get('on')) return
      const entry = { fx: dev, pitch: (fxMap.get('srcPitch') as number) ?? -1 }
      const list = this.duckTriggers.get(src)
      if (list) list.push(entry)
      else this.duckTriggers.set(src, [entry])
    }
    for (const rec of this.built.values()) {
      const t = trackById(rec.id)
      if (!t) continue
      for (const bf of rec.fx) { if (!bf.bypassed) wire(this.findFxMap(t, bf.id)!, bf.fx) }
    }
    masterFx.forEach(m => {
      const bf = this.builtMasterFx.find(f => f.id === m.get('id'))
      if (bf && !bf.bypassed) wire(m, bf.fx)
    })
  }
  /** A note fired on `tid` — kick any duck listening to that track (or pad). */
  private fireDucks(tid: string, pitch: number, when: number) {
    const list = this.duckTriggers.get(tid)
    if (!list) return
    const pad = ((pitch % 8) + 8) % 8
    for (const d of list) if (d.pitch < 0 || d.pitch === pad) d.fx.trigger?.(when)
  }

  // ---- idle send-bus cut ----
  // A bus that nothing sends into still renders its whole fx chain — the
  // built-in A bus carries an always-on convolution reverb, one of the most
  // expensive nodes in the graph. When every route into a bus is statically
  // zero AND nothing can be modulating a send (no LFO on a send, no send
  // envelope on a playing/arrangement clip or track automation), cut it with
  // the same panner-disconnect trick as mute so it costs ~no audio-thread CPU.
  private idleBusIds = new Set<string>()
  private recomputeBusIdle() {
    if (!this.started) return
    const buses = [...this.built.values()].filter(r => r.kind === 'bus')
    if (!buses.length) {
      if (this.idleBusIds.size) { this.idleBusIds.clear(); this.applyMuteSolo() }
      return
    }
    const list = this.allTracks()
    let sendMod = false
    for (const t of list) {
      // direct LFO→send, or LFO→macro where that macro fans out to a send
      const macros = t.get('macros') as Y.Array<Y.Map<any>> | undefined
      const macroHasSend = (pkey: string) => {
        const tgs = macros?.get(+pkey)?.get('targets') as Y.Array<Y.Map<any>> | undefined
        let has = false
        tgs?.forEach(x => { if (x.get('dest') === 'send') has = true })
        return has
      }
      if (this.lfoCfgsOf(t.get('id') as string, t).some(l => l.on && l.targets.some(tg =>
        tg.dest === 'send' || (tg.dest === 'macro' && macroHasSend(tg.pkey))))) { sendMod = true; break }
    }
    if (!sendMod && this.mode === 'session') {
      for (const rec of this.built.values()) {
        if (!rec.partKey) continue
        const cm = clips.get(rec.partKey) as Y.Map<any> | undefined
        if (cm && this.envEntriesOf(cm).some(([k]) => k.startsWith('send|'))) { sendMod = true; break }
      }
    } else if (!sendMod && this.mode === 'arr') {
      sendMod = this.arrIndex().some(c => this.envEntriesOf(c.cm).some(([k]) => k.startsWith('send|')))
        || list.some(t => this.autoEntriesOf(t.get('id') as string, t).some(([k]) => k.startsWith('send|')))
    }
    const active = new Set<string>()
    for (const b of buses) {
      const bt = trackById(b.id)
      const send = bt?.get('send') as string | undefined   // built-in send marker (A/B/F)
      const ab = send === 'A' || send === 'B'               // only A/B carry per-track sendA/sendB
      if (ab && sendMod) { active.add(b.id); continue }
      for (const t of list) {
        if (t.get('id') === b.id) continue                  // self-send doesn't count as external input
        if (ab && (((t.get(send === 'B' ? 'sendB' : 'sendA') as number) ?? 0) > 0.0001)) { active.add(b.id); break }
        const sm = t.get('sends') as Y.Map<number> | undefined
        if (sm && (((sm.get(b.id) as number) ?? 0) > 0.0001)) { active.add(b.id); break }
      }
    }
    // activity flows along bus→bus output routing (an active bus wakes its target)
    let grew = true
    while (grew) {
      grew = false
      for (const b of buses) {
        if (!active.has(b.id)) continue
        const out = (trackById(b.id)?.get('output') as string) || 'master'
        if (out !== 'master' && this.built.get(out)?.kind === 'bus' && !active.has(out)) { active.add(out); grew = true }
      }
    }
    const idle = new Set<string>()
    for (const b of buses) if (!active.has(b.id)) idle.add(b.id)
    let changed = idle.size !== this.idleBusIds.size
    if (!changed) for (const id of idle) if (!this.idleBusIds.has(id)) { changed = true; break }
    if (changed) { this.idleBusIds = idle; this.applyMuteSolo() }
  }

  masterDb(): number {
    if (!this.masterMeter) return -100
    const v = this.masterMeter.getValue()
    return typeof v === 'number' ? v : Math.max(...(v as number[]))
  }

  // ---------------- live input: monitor / record / audition ----------------

  previewOn(trackId: string | null, p: number, vel: number) {
    if (!this.started || !trackId) return
    this.built.get(trackId)?.inst.noteOn(p, vel)
    this.fireDucks(trackId, p, Tone.now())
  }

  previewOff(trackId: string | null, p: number) {
    if (!this.started || !trackId) return
    this.built.get(trackId)?.inst.noteOff(p)
  }

  /**
   * Rough output latency in ms: the audio render buffer (`baseLatency`) plus the
   * device/OS output buffer (`outputLatency`). This is the floor a live note
   * can't beat — what's left after we removed Tone's lookahead. `outputLatency`
   * reads 0 until audio is actually flowing, so poll it after playing a note.
   */
  outputLatencyMs(): number {
    if (!this.started) return 0
    const ctx = Tone.getContext().rawContext as AudioContext
    return Math.round(((ctx.baseLatency || 0) + (ctx.outputLatency || 0)) * 1000)
  }

  /** Snapshot of engine load for the performance monitor. */
  perfStats() {
    let effects = 0
    this.built.forEach(t => { effects += t.fx.length })
    return {
      started: this.started,
      tracks: this.built.size,
      effects,
      returns: this.builtReturns.length,
      sampleRate: this.sampleRate,
      oversampling: this.sampleRate >= 88000,
      latencyMs: this.outputLatencyMs(),
      audioLoad: this.audioLoad,
      probe: this.audioProbe,
    }
  }

  /** Clear the accumulated glitch count (after the user acts on a warning). */
  resetGlitchCount() {
    this.audioProbe.glitches = 0
    this.glitchWarned = false
    this.probeWinHist = []
    this.audioProbe.minRtf = this.audioProbe.rtf
    this.probeStartAt = Tone.now()
  }

  /** Returns true if the note was captured into a clip (recording path). */
  recordNoteOn(trackId: string, p: number, vel: number): boolean {
    if (!ui.recording || !this.started) return false
    const rec = this.built.get(trackId)
    if (!rec) return false
    if (!rec.partKey || this.transport.state !== 'started') return false
    const clipMap = clips.get(rec.partKey) as Y.Map<any> | undefined
    if (!clipMap) return false
    let pos = ((this.transport.ticks - rec.partStartTicks) % rec.partLoopTicks + rec.partLoopTicks) % rec.partLoopTicks
    if (ui.recQuantize) pos = Math.round(pos / ui.gridTicks) * ui.gridTicks % rec.partLoopTicks
    this.pendingRec.set(p, { clipMap, startInClip: pos, vel })
    return true
  }

  recordNoteOff(trackId: string, p: number) {
    const pending = this.pendingRec.get(p)
    if (!pending) return
    this.pendingRec.delete(p)
    const rec = this.built.get(trackId)
    let dur = STEP16
    if (rec && rec.partKey) {
      const pos = ((this.transport.ticks - rec.partStartTicks) % rec.partLoopTicks + rec.partLoopTicks) % rec.partLoopTicks
      dur = pos - pending.startInClip
      if (dur <= 0) dur += rec.partLoopTicks
      dur = clamp(Math.round(dur), 6, rec.partLoopTicks)
      if (ui.recQuantize) dur = Math.max(STEP16 / 2, Math.round(dur / (ui.gridTicks / 2)) * (ui.gridTicks / 2))
    }
    addNote(pending.clipMap, { p, s: pending.startInClip, d: dur, v: pending.vel, pr: 1 }, 'Record note')
  }

  /** Audition arbitrary pitches (e.g. a chord progression preview). */
  async auditionPitches(type: string, params: Record<string, number>, chords: number[][], chordDur = 0.55) {
    await this.ensureStarted()
    const inst = makeInstrument(type, params)
    inst.out.connect(this.master)
    const now = Tone.now()
    chords.forEach((chord, i) => {
      chord.forEach(p => inst.trigger(p, chordDur * 0.92, now + i * chordDur, 0.75))
    })
    setTimeout(() => { try { inst.dispose() } catch { /* ok */ } }, (chords.length * chordDur + 2) * 1000)
  }

  /** Warm a sample's decoded buffer (hover preload) so the audition click is instant. */
  preloadSample(sampleId: string) {
    getSampleBuffer(sampleId)   // synchronous cache hit or kicks off the IDB restore + decode
  }

  private auditionPlayer: Tone.Player | null = null
  private auditionId: string | null = null
  /** Stop the current sample audition (if any). */
  stopAudition() {
    const p = this.auditionPlayer
    this.auditionPlayer = null
    this.auditionId = null
    if (p) { try { p.stop(); p.dispose() } catch { /* ok */ } }
  }

  /** Play a sample once (browser audition / preview, capped at 10s).
   *  Clicking the sample that's already auditioning stops it instead. */
  async auditionSample(sampleId: string) {
    if (this.auditionId === sampleId && this.auditionPlayer) { this.stopAudition(); return }
    await this.ensureStarted()
    // the hover preload usually decoded it already; otherwise wait briefly for
    // the background restore getSampleBuffer() just kicked off
    let buf = getSampleBuffer(sampleId)
    for (let i = 0; !buf && i < 30; i++) { await new Promise(r => setTimeout(r, 100)); buf = getSampleBuffer(sampleId) }
    if (!buf) return
    this.stopAudition()   // one audition at a time
    const dur = Math.min(buf.duration, 10)
    const p = new Tone.Player(buf).connect(this.master)
    this.auditionPlayer = p
    this.auditionId = sampleId
    p.start(undefined, 0, dur)
    setTimeout(() => {
      if (this.auditionPlayer === p) { this.auditionPlayer = null; this.auditionId = null }
      try { p.dispose() } catch { /* ok */ }
    }, (dur + 0.4) * 1000)
  }

  /** Quick preset audition from the browser, without touching any track. */
  async audition(type: string, params: Record<string, number>) {
    await this.ensureStarted()
    const inst = makeInstrument(type, params)
    inst.out.connect(this.master)
    const now = Tone.now()
    if (type === 'drum') {
      inst.trigger(0, 0.3, now, 0.9)
      inst.trigger(3, 0.2, now + 0.18, 0.7)
      inst.trigger(1, 0.25, now + 0.36, 0.85)
      inst.trigger(3, 0.2, now + 0.54, 0.7)
    } else {
      inst.trigger(60, 0.45, now, 0.85)
      inst.trigger(67, 0.4, now + 0.22, 0.8)
      inst.trigger(72, 0.6, now + 0.44, 0.85)
    }
    setTimeout(() => { try { inst.dispose() } catch { /* ok */ } }, 2500)
  }
}

// The concrete Tone engine. Web-only modules that need Tone internals
// (recorder.ts → captureTap) import THIS; everything else goes through the
// facade in engine.ts.
export const engine = new Engine()
;(window as any).__engine = engine
