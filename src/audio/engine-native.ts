// The DESKTOP engine — EngineApi facade over the Rust core in the Tauri
// shell (docs/RUST_ARCH.md §IPC). The audio graph, transport and scheduler
// live in-process in Rust (sf-engine hosted by sf-io); this file is a thin
// client: it pushes the Yjs doc over IPC, forwards transport/clip/param
// intents as commands, and mirrors just enough state (launched-clip keys,
// launch boundaries, the latest engine snapshot) for the UI's synchronous
// reads (clipState / clipProgress / positionTicks / meters).
//
// Sync model: pure param edits ride a `param_set` fast path (no graph
// rebuild); bpm rides `set_tempo`; EVERYTHING else re-pushes the full
// ProjectJSON (debounced 150 ms) — cheap enough at project scale, diffs
// later. A re-push while playing resumes transport/clips at the position.
//
// Honest no-ops in this v1 (the UI degrades gracefully on each):
//   • audition / auditionPitches / auditionSample / preloadSample — resolve
//     without sound (library previews need a native preview lane later).
//   • freezeTrack — toasts; freezing exists to save webaudio CPU and is not
//     needed on the native engine. isFrozen/isFreezing are always false.
//   • getSpectrum / getWaveform → empty arrays; getPitchHz → 0 (no master
//     analyser taps over IPC yet); deviceAnalyser → null. Device meters
//     (deviceMeterDb), per-fx GR (deviceReductionDb) and the multiband's
//     per-band GR (deviceBandReduction) are all LIVE from the engine snapshot.
//   • lfoValue → 0 (LFO/macro/automation modulation isn't ported yet — the
//     Rust engine plays the doc's stored values).
//   • perfStats.audioLoad.supported=false; the probe fields carry REAL
//     native callback stats (xruns/over-budget/peak load) instead.
// This module must stay import-safe in a plain browser: no top-level
// __TAURI__ access — everything goes through invoke() at call time.

import * as Y from 'yjs'
import { BAR, STEP16, clamp } from '../types'
import {
  meta, tracks, clips, arr, clipKey, addNote, masterFx, barTicks, exportProject,
} from '../state/doc'
import { getAudioPrefs } from './prefs'
import { getSampleBuffer, onSampleReady } from './samples'
import { setUI, toast, ui } from '../state/store'
import { nativeIo, nativeOutputPrefs } from './native'
import type { EngineApi, EngineMode, PerfStats } from './engine-api'

type NativeSnapshot = {
  playing: boolean
  mode: string
  playhead_ticks: number
  master: { peak: number; rms: number }
  tracks: { id: string; peak: number; rms: number; inst: number }[]
  gr: number[]
  /** per-fx output level (linear, decaying peak-hold), same order as `gr` */
  fx_level: number[]
  /** per-fx, per-band gain reduction in dB (≤ 0), same order as `gr` */
  fx_bands: [number, number, number][]
  xruns: number
  callbacks: number
  over_budget: number
  peak_load: number
  max_callback_us: number
  budget_us: number
  /** automatic device-change notice (stream restarted by the Rust
   *  supervisor after an unplug / driver reset / default change) */
  notice_seq?: number
  notice?: string | null
  /** input stream delivered only exact digital silence for 2 s */
  input_silent?: boolean
}

type StreamStart = { device: string; sample_rate: number; buffer_frames: number | null }

type ProjectLoadResult = {
  tracks: string[]
  gr: { track_id: string; fx_id: string }[]
}

function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return (window as any).__TAURI__.core.invoke(cmd, args)
}

/** latencyHint tier → requested output buffer (frames), mirroring the web
 *  engine's interactive/balanced/playback intent at typical device rates. */
function bufferForPrefs(): number {
  const l = getAudioPrefs().latency
  return l === 'playback' ? 512 : l === 'balanced' ? 256 : 128
}

const linToDb = (lin: number) => (lin > 1e-5 ? 20 * Math.log10(lin) : -100)
const posMod = (a: number, m: number) => ((a % m) + m) % m

type PartMirror = { key: string; startTicks: number; loopTicks: number }
type PendingSlot = { action: 'launch' | 'stop'; key?: string; atTicks: number; loopTicks: number }

const EMPTY_F32 = new Float32Array(0)

class NativeEngine implements EngineApi {
  started = false
  mode: EngineMode = 'session'
  sampleRate = 0
  arrSeekTicks = 0
  meteredTrackId: string | null = null

  private bufferFrames = 0
  private playFlag = false
  private startPromise: Promise<void> | null = null
  private observersAttached = false

  // ---- last engine snapshot (30 Hz poll) + interpolation anchor ----
  private snap: NativeSnapshot | null = null
  private snapAt = 0
  private pollTimer: ReturnType<typeof setInterval> | null = null
  private pollInFlight = false
  private trackDb = new Map<string, number>()
  /** instrument output level per track, dB — the device rack's inst meter */
  private instDb = new Map<string, number>()

  // ---- id maps from the last project_load ----
  private trackIds: string[] = []
  private grIndex = new Map<string, number>() // `${trackId}|${fxId}` → flat gr idx
  private effectCount = 0

  // ---- session clip mirrors (drive clipState/clipProgress/record) ----
  private parts = new Map<string, PartMirror>()
  private pending = new Map<string, PendingSlot>()
  private pendingRec = new Map<number, { clipMap: Y.Map<any>; startInClip: number; vel: number }>()

  // ---- perf probe bookkeeping ----
  private glitchBase = 0
  private minRtf = 1

  private repushTimer: ReturnType<typeof setTimeout> | null = null

  // ---- tiny emitter (same contract as engine-tone) ----
  version = 0
  private listeners = new Set<() => void>()
  subscribe = (fn: () => void) => {
    this.listeners.add(fn)
    return () => { this.listeners.delete(fn) }
  }
  private emit() {
    this.version++
    this.listeners.forEach(l => l())
  }

  get playing() {
    return this.started && this.playFlag
  }

  // ---------------- lifecycle ----------------

  ensureStarted(): Promise<void> {
    if (this.started) return Promise.resolve()
    if (!this.startPromise) {
      this.startPromise = this.doStart().catch(e => {
        console.error('native audio init failed', e)
        // Surface the REAL reason — a generic banner sent us hunting blind.
        const why = String((e as Error)?.message ?? e).slice(0, 200)
        setUI({ audioError: `The native audio engine couldn’t start: ${why}` })
        this.startPromise = null
      })
    }
    return this.startPromise
  }

  /** Open the engine stream on the remembered driver/device/buffer
   *  (Audio settings → Native output). If that device is gone (USB
   *  interface unplugged since last session) fall back to the system default
   *  on the platform driver rather than starting silent. */
  private async openStream(): Promise<StreamStart> {
    const p = nativeOutputPrefs()
    const bufferFrames = p.buffer ?? bufferForPrefs()
    try {
      return await invoke<StreamStart>('engine_start', { host: p.host, device: p.device, bufferFrames, outFirst: p.outFirst ?? 0 })
    } catch (e) {
      if (!p.host && !p.device) throw e
      // name the platform default driver explicitly: host null would mean
      // "keep the driver in use", which may be the one that just failed
      const hosts = await invoke<{ name: string; is_default: boolean }[]>('audio_hosts').catch(() => [])
      const host = hosts.find(h => h.is_default)?.name ?? null
      const info = await invoke<StreamStart>('engine_start', { host, device: null, bufferFrames: bufferForPrefs(), outFirst: 0 })
      const want = p.device ?? `the ${p.host} driver`
      toast(`Couldn’t open ${want} — using ${info.device}`)
      return info
    }
  }

  private async doStart() {
    // the platform default driver is only the fallback — openStream sets the
    // remembered one (host null there = keep whatever is in use)
    const info = await this.openStream()
    this.sampleRate = info.sample_rate
    this.bufferFrames = info.buffer_frames ?? bufferForPrefs()
    // late IndexedDB decodes land through the ready callback (engine-tone's
    // callback never registers on desktop — the facade owns the single slot)
    onSampleReady(() => void this.pushSamples())
    await this.pushProject()
    this.attachObservers()
    this.startPolling()
    this.started = true
    setUI({ audioReady: true, audioError: null })
    this.emit()
  }

  /** Re-open the engine's output stream and re-push the project.
   *
   *  sf-io hosts ONE stream: `audio_start_tone` (the Audio-settings test tone)
   *  REPLACES the engine stream, and `audio_stop` drops it entirely. Without
   *  this, playing the test tone — or just opening and closing Audio settings —
   *  left the app permanently silent, because ensureStarted() is memoized and
   *  never re-runs. Called by the settings panel when it releases the device.
   */
  async restartStream() {
    if (!this.started) return
    const info = await this.openStream()
    this.sampleRate = info.sample_rate
    this.bufferFrames = info.buffer_frames ?? bufferForPrefs()
    await this.pushProject()
    this.emit()
  }

  private async pushProject() {
    const res = await invoke<ProjectLoadResult>('project_load', { project: exportProject() })
    this.trackIds = res.tracks
    this.grIndex.clear()
    res.gr.forEach((g, i) => this.grIndex.set(`${g.track_id}|${g.fx_id}`, i))
    this.effectCount = res.gr.length
    // ship every referenced-but-unpushed sample to the native store (async;
    // the Rust side re-pushes the graph itself as each sample lands)
    void this.pushSamples()
  }

  // ---------------- sample push (webview PCM → native SampleBank) ----------------

  private pushedSamples = new Set<string>()
  private pushingSamples = false
  private samplesDirty = false

  /** Every sampleId the doc references: sampler/ksampler/granular inst
   *  samples, drum padSamples, and session/arrangement audio clips. */
  private collectSampleIds(): string[] {
    const p = exportProject() as any
    const ids = new Set<string>()
    for (const t of p.tracks ?? []) {
      if (t.inst?.sampleId) ids.add(t.inst.sampleId)
      const ps = t.inst?.padSamples
      if (ps) for (const sid of Object.values(ps)) { if (sid) ids.add(sid as string) }
    }
    const scanClip = (c: any) => { if (c?.audio?.sampleId) ids.add(c.audio.sampleId) }
    for (const c of Object.values(p.clips ?? {})) scanClip(c)
    for (const a of Object.values(p.arr ?? {})) scanClip(a) // arr flattens clip fields
    return [...ids]
  }

  /** Push referenced samples the native store doesn't have yet. Decoded
   *  buffers come from the shared sample store; ids still decoding from
   *  IndexedDB are skipped here and re-attempted from onSampleReady (mirrors
   *  engine-tone's late-decode handling). Channels go as one raw f32le IPC
   *  body (no base64 — see native.ts nativeIo); extremely long samples
   *  (hundreds of MB) would still need chunking, a known v1 limit. */
  private async pushSamples() {
    if (this.pushingSamples) { this.samplesDirty = true; return }
    this.pushingSamples = true
    try {
      do {
        this.samplesDirty = false
        for (const id of this.collectSampleIds()) {
          if (this.pushedSamples.has(id)) continue
          const buf = getSampleBuffer(id) // miss ⇒ kicks off the IDB restore
          if (!buf) continue // onSampleReady re-enters when the decode lands
          const l = buf.getChannelData(0)
          const r = buf.numberOfChannels > 1 ? buf.getChannelData(1) : null
          await nativeIo.samplePut(id, buf.sampleRate, l, r)
          this.pushedSamples.add(id)
        }
      } while (this.samplesDirty)
    } catch (e) {
      console.error('sample push failed', e)
    }
    this.pushingSamples = false
  }

  // ---------------- doc sync ----------------

  private scheduleRepush() {
    if (!this.started) return
    if (this.repushTimer) clearTimeout(this.repushTimer)
    this.repushTimer = setTimeout(() => {
      this.repushTimer = null
      void this.repushNow()
    }, 150)
  }

  /** Full-doc re-push. A graph swap resets the Rust schedulers, so when the
   *  transport is running we re-launch the mirrored session clips (anchored
   *  at 0 while stopped) and resume at the current position.
   *
   *  Two things this MUST get right, both of which caused "playback stops
   *  after editing a device; mute/unmute brings it back":
   *   • A launch that hasn't reached its quantize boundary lives ONLY in the
   *     Rust scheduler (mirrored here as `pending`). The swap wipes it, so it
   *     has to be re-launched too — otherwise resolvePending later promotes it
   *     into `parts` and the UI shows a clip playing that the engine dropped.
   *   • Re-pushes must not overlap: every step here is a separate await, so
   *     two in flight can interleave stop/launch/play and end up stopped. */
  private repushing = false
  private repushAgain = false
  private async repushNow() {
    if (this.repushing) { this.repushAgain = true; return }
    this.repushing = true
    try {
      do {
        this.repushAgain = false
        await this.repushOnce()
      } while (this.repushAgain)
    } finally {
      this.repushing = false
    }
  }

  private async repushOnce() {
    const wasPlaying = this.playFlag
    const pos = this.positionTicks()
    // snapshot before any await: the poll mutates both maps
    const relaunch = new Map<string, string>() // trackId → sceneId
    for (const [tid, p] of this.parts) relaunch.set(tid, p.key.split('|')[1])
    for (const [tid, p] of this.pending) {
      if (p.action === 'launch' && p.key) relaunch.set(tid, p.key.split('|')[1])
    }
    const restop = [...this.pending].filter(([, p]) => p.action === 'stop').map(([tid]) => tid)
    try {
      await this.pushProject()
      if (!wasPlaying) return
      if (this.mode === 'arr') {
        await invoke('transport_play', { mode: 'arr', fromTicks: pos })
      } else {
        await invoke('transport_stop')
        for (const [tid, sceneId] of relaunch) {
          await invoke('clip_launch', { trackId: tid, sceneId })
        }
        await invoke('transport_play', { mode: 'session', fromTicks: pos })
        // a stop queued for the next boundary was dropped with the old graph
        for (const tid of restop) invoke('clip_stop', { trackId: tid }).catch(() => {})
      }
    } catch (e) {
      console.error('project re-push failed', e)
    }
  }

  private paramSet(scope: 'track' | 'master', trackId: string, deviceId: string, key: string, value: number) {
    invoke<boolean>('param_set', { scope, trackId, deviceId, key, value })
      .then(ok => { if (!ok) this.scheduleRepush() })  // stale address → resync
      .catch(() => {})
  }

  private attachObservers() {
    if (this.observersAttached) return
    this.observersAttached = true

    tracks.observeDeep(events => {
      for (const ev of events) {
        const path = ev.path
        if (path.length === 0) { this.scheduleRepush(); continue }
        const t = tracks.get(path[0] as number)
        if (!t) { this.scheduleRepush(); continue }
        const tid = t.get('id') as string
        if (path.length === 1) {
          let repush = false
          ev.changes.keys.forEach((_c, k) => {
            if (k === 'gain' || k === 'pan' || k === 'sendA' || k === 'sendB') {
              const v = t.get(k)
              if (typeof v === 'number') this.paramSet('track', tid, 'mixer', k, v)
            } else repush = true // mute/solo/inst/fx/output/… → rebuild
          })
          if (repush) this.scheduleRepush()
        } else if (path[1] === 'inst' && path[path.length - 1] === 'params') {
          ev.changes.keys.forEach((_c, k) => {
            const v = (ev.target as Y.Map<number>).get(k)
            if (typeof v === 'number') this.paramSet('track', tid, 'inst', k, v)
          })
        } else if (path[1] === 'fx' && path.length >= 4 && path[3] === 'params') {
          const fxMap = (t.get('fx') as Y.Array<Y.Map<any>>).get(path[2] as number)
          const fxId = fxMap?.get('id') as string | undefined
          if (!fxId) { this.scheduleRepush(); continue }
          ev.changes.keys.forEach((_c, k) => {
            const v = (ev.target as Y.Map<number>).get(k)
            if (typeof v === 'number') this.paramSet('track', tid, fxId, k, v)
          })
        } else if (path[1] === 'fx' && path.length === 3) {
          const fxMap = (t.get('fx') as Y.Array<Y.Map<any>>).get(path[2] as number)
          let repush = false
          ev.changes.keys.forEach((_c, k) => {
            if (k === 'out' && fxMap) this.paramSet('track', tid, fxMap.get('id'), 'out', fxMap.get('out') ?? 0)
            else repush = true // on/type/srcTrack… → rebuild
          })
          if (repush || !fxMap) this.scheduleRepush()
        } else if (path[1] === 'sends' && path.length === 2) {
          const sm = t.get('sends') as Y.Map<number> | undefined
          ev.changes.keys.forEach((_c, busId) => {
            const v = sm?.get(busId)
            if (typeof v === 'number') this.paramSet('track', tid, 'send', busId, v)
            else this.scheduleRepush()
          })
        } else {
          this.scheduleRepush() // midifx/lfos/macros/auto/inst-structure/…
        }
      }
    })

    masterFx.observeDeep(events => {
      for (const ev of events) {
        const path = ev.path
        if (path.length >= 2 && path[path.length - 1] === 'params') {
          const fxMap = masterFx.get(path[0] as number)
          const fxId = fxMap?.get('id') as string | undefined
          if (!fxId) { this.scheduleRepush(); continue }
          ev.changes.keys.forEach((_c, k) => {
            const v = (ev.target as Y.Map<number>).get(k)
            if (typeof v === 'number') this.paramSet('master', '', fxId, k, v)
          })
        } else if (path.length === 1) {
          const fxMap = masterFx.get(path[0] as number)
          let repush = false
          ev.changes.keys.forEach((_c, k) => {
            if (k === 'out' && fxMap) this.paramSet('master', '', fxMap.get('id'), 'out', fxMap.get('out') ?? 0)
            else repush = true
          })
          if (repush || !fxMap) this.scheduleRepush()
        } else {
          this.scheduleRepush()
        }
      }
    })

    meta.observe(ev => {
      ev.changes.keys.forEach((_c, k) => {
        if (k === 'bpm') invoke('set_tempo', { bpm: meta.get('bpm') ?? 120 }).catch(() => {})
        else if (k === 'masterGain') this.paramSet('master', '', 'mixer', 'gain', meta.get('masterGain') ?? 0)
        else if (k === 'title' || k === 'masterAutoOpen' || k === 'masterAutoParam' || k === 'launchQ') { /* no graph impact */ }
        else this.scheduleRepush() // swing/humanize/loop*/ts*/root/scale
      })
    })

    clips.observeDeep(() => this.scheduleRepush())
    arr.observeDeep(() => this.scheduleRepush())
  }

  // ---------------- snapshot polling ----------------

  private startPolling() {
    if (this.pollTimer) return
    this.pollTimer = setInterval(() => void this.poll(), 33)
  }

  private async poll() {
    if (this.pollInFlight) return
    this.pollInFlight = true
    try {
      const s = await invoke<NativeSnapshot>('engine_snapshot')
      const playingChanged = this.playFlag !== s.playing
      this.showNotices(s)
      this.snap = s
      this.snapAt = performance.now()
      this.playFlag = s.playing
      this.trackDb.clear()
      this.instDb.clear()
      for (const t of s.tracks) {
        this.trackDb.set(t.id, linToDb(t.peak))
        this.instDb.set(t.id, linToDb(t.inst))
      }
      // rolling worst "keeping up" factor for the perf monitor
      const rtf = s.peak_load > 1 ? 1 / s.peak_load : 1
      this.minRtf = Math.min(this.minRtf, rtf)
      this.resolvePending(s.playhead_ticks)
      if (playingChanged) this.emit()
    } catch { /* engine restarting — keep the last snapshot */ }
    this.pollInFlight = false
  }

  private noticeSeq = 0
  private silentWarned = false

  /** Toast what the native side did on its own (device unplugged → stream
   *  restarted elsewhere) and the no-input-signal hint. */
  private showNotices(s: NativeSnapshot) {
    const seq = s.notice_seq ?? 0
    if (seq > this.noticeSeq) {
      this.noticeSeq = seq
      if (s.notice) toast(s.notice)
    }
    if (s.input_silent && !this.silentWarned) {
      this.silentWarned = true
      const win = /Windows/i.test(navigator.userAgent)
      toast(win
        ? 'No signal from the recording input. Check Windows Settings > Privacy & security > Microphone > “Let desktop apps access your microphone”.'
        : 'No signal from the recording input — check the input device and its gain.')
    } else if (!s.input_silent) {
      this.silentWarned = false
    }
  }

  private resolvePending(nowTicks: number) {
    if (!this.pending.size) return
    let changed = false
    for (const [tid, p] of [...this.pending]) {
      if (this.playFlag && nowTicks + 1 < p.atTicks) continue
      this.pending.delete(tid)
      if (p.action === 'launch' && p.key) {
        this.parts.set(tid, { key: p.key, startTicks: p.atTicks, loopTicks: p.loopTicks })
      } else if (p.action === 'stop') {
        this.parts.delete(tid)
      }
      changed = true
    }
    if (changed) this.emit()
  }

  // ---------------- transport ----------------

  positionTicks(): number {
    if (!this.started) return 0
    const s = this.snap
    if (!s || !this.playFlag) return this.mode === 'arr' ? this.arrSeekTicks : 0
    const bpm = (meta.get('bpm') as number) ?? 120
    const dt = (performance.now() - this.snapAt) / 1000
    return s.playhead_ticks + dt * (bpm / 60) * 96
  }

  async togglePlay() {
    await this.ensureStarted()
    if (this.playFlag) {
      if (this.mode === 'session') {
        // session "stop" = pause: the Rust schedulers keep their launched
        // clips, so the next Play resumes every armed slot from bar 1
        this.playFlag = false
        await invoke('transport_stop')
      } else {
        this.stopAll()
      }
      this.emit()
      return
    }
    if (ui.view === 'arr') { void this.playArrangement(); return }
    this.mode = 'session'
    this.playFlag = true
    await invoke('transport_play', { mode: 'session', fromTicks: 0 })
    for (const p of this.parts.values()) p.startTicks = 0
    this.emit()
  }

  async playArrangement(fromTicks?: number) {
    await this.ensureStarted()
    // the timeline takes over: drop session clip mirrors + stop their scheds
    for (const tid of this.parts.keys()) invoke('clip_stop', { trackId: tid }).catch(() => {})
    this.parts.clear()
    this.pending.clear()
    this.mode = 'arr'
    const from = fromTicks ?? this.arrSeekTicks
    this.arrSeekTicks = from
    this.playFlag = true
    await invoke('transport_play', { mode: 'arr', fromTicks: from })
    this.emit()
  }

  seekArr(ticks: number) {
    this.arrSeekTicks = Math.max(0, ticks)
    if (this.mode === 'arr' && this.playFlag) {
      invoke('transport_play', { mode: 'arr', fromTicks: this.arrSeekTicks }).catch(() => {})
    }
    this.emit()
  }

  stopAll() {
    this.playFlag = false
    invoke('transport_stop').catch(() => {})
    for (const tid of this.parts.keys()) invoke('clip_stop', { trackId: tid }).catch(() => {})
    this.parts.clear()
    this.pending.clear()
    this.pendingRec.clear()
    this.emit()
  }

  // ---------------- session clips ----------------

  /** Next launch-quantize boundary in ticks (mirrors the Rust engine's). */
  private nextBoundaryTicks(): number {
    const q = (meta.get('launchQ') as number) ?? 1
    const cur = Math.round(this.positionTicks())
    if (q === 0) return cur + 4
    const qTicks = q * barTicks()
    return Math.ceil((cur + 1) / qTicks) * qTicks
  }

  async launchClip(trackId: string, sceneId: string) {
    await this.ensureStarted()
    const key = clipKey(trackId, sceneId)
    const clipMap = clips.get(key) as Y.Map<any> | undefined
    if (!clipMap) return
    this.mode = 'session'
    const loopTicks = (clipMap.get('len') as number) ?? BAR
    const wasPlaying = this.playFlag
    await invoke('clip_launch', { trackId, sceneId })
    if (!wasPlaying) {
      // Rust side starts the transport (session, from 0) on the first launch
      this.playFlag = true
      this.parts.set(trackId, { key, startTicks: 0, loopTicks })
      this.pending.delete(trackId)
    } else {
      this.pending.set(trackId, { action: 'launch', key, atTicks: this.nextBoundaryTicks(), loopTicks })
    }
    this.emit()
  }

  async stopClip(trackId: string) {
    if (!this.parts.has(trackId) && !this.pending.has(trackId)) return
    invoke('clip_stop', { trackId }).catch(() => {})
    if (this.playFlag) {
      const cur = this.parts.get(trackId)
      if (cur) {
        this.pending.set(trackId, { action: 'stop', atTicks: this.nextBoundaryTicks(), loopTicks: cur.loopTicks })
      } else {
        this.pending.delete(trackId) // un-queue a pending launch
      }
    } else {
      this.parts.delete(trackId)
      this.pending.delete(trackId)
    }
    this.emit()
  }

  async launchScene(sceneId: string) {
    await this.ensureStarted()
    for (const t of tracks.toArray()) {
      const tid = t.get('id') as string
      if (clips.get(clipKey(tid, sceneId))) void this.launchClip(tid, sceneId)
      else void this.stopClip(tid)
    }
  }

  stopAllClips() {
    tracks.forEach(t => void this.stopClip(t.get('id')))
  }

  clipState(trackId: string, sceneId: string) {
    const key = clipKey(trackId, sceneId)
    const part = this.parts.get(trackId)
    const pend = this.pending.get(trackId)
    return {
      playing: part?.key === key,
      queued: pend?.action === 'launch' && pend.key === key,
      stopQueued: part?.key === key && pend?.action === 'stop',
    }
  }

  clipProgress(trackId: string): number | null {
    const part = this.parts.get(trackId)
    if (!part || !this.playFlag) return null
    return posMod(this.positionTicks() - part.startTicks, part.loopTicks) / part.loopTicks
  }

  // ---------------- live input ----------------

  previewOn(trackId: string | null, pitch: number, vel: number) {
    if (!this.started || !trackId) return
    invoke('note_preview', { trackId, pitch, vel, on: true }).catch(() => {})
  }

  previewOff(trackId: string | null, pitch: number) {
    if (!this.started || !trackId) return
    invoke('note_preview', { trackId, pitch, vel: 0, on: false }).catch(() => {})
  }

  /** Record path — same doc-write logic as engine-tone (no Tone dependency),
   *  driven by the mirrored part anchors + interpolated position. */
  recordNoteOn(trackId: string, p: number, vel: number): boolean {
    if (!ui.recording || !this.started) return false
    const part = this.parts.get(trackId)
    if (!part || !this.playFlag) return false
    const clipMap = clips.get(part.key) as Y.Map<any> | undefined
    if (!clipMap) return false
    let pos = posMod(this.positionTicks() - part.startTicks, part.loopTicks)
    if (ui.recQuantize) pos = Math.round(pos / ui.gridTicks) * ui.gridTicks % part.loopTicks
    this.pendingRec.set(p, { clipMap, startInClip: pos, vel })
    return true
  }

  recordNoteOff(trackId: string, p: number) {
    const pending = this.pendingRec.get(p)
    if (!pending) return
    this.pendingRec.delete(p)
    const part = this.parts.get(trackId)
    let dur = STEP16
    if (part) {
      const pos = posMod(this.positionTicks() - part.startTicks, part.loopTicks)
      dur = pos - pending.startInClip
      if (dur <= 0) dur += part.loopTicks
      dur = clamp(Math.round(dur), 6, part.loopTicks)
      if (ui.recQuantize) dur = Math.max(STEP16 / 2, Math.round(dur / (ui.gridTicks / 2)) * (ui.gridTicks / 2))
    }
    addNote(pending.clipMap, { p, s: pending.startInClip, d: dur, v: pending.vel, pr: 1 }, 'Record note')
  }

  // ---------------- auditioning (no-op on desktop, see header) ----------------

  async audition(_type: string, _params: Record<string, number>) { /* no-op */ }
  async auditionPitches(_type: string, _params: Record<string, number>, _chords: number[][], _chordDur?: number) { /* no-op */ }
  async auditionSample(_sampleId: string) { /* no-op */ }
  stopAudition() { /* no-op */ }
  preloadSample(_sampleId: string) { /* no-op */ }

  // ---------------- meters / analysis ----------------

  meterDb(trackId: string): number {
    return this.trackDb.get(trackId) ?? -100
  }

  masterDb(): number {
    return this.snap ? linToDb(this.snap.master.peak) : -100
  }

  /** Per-device output level in dB — 'inst' or an fx id (trackId 'master'
   *  for the master chain). The engine already applies meter ballistics. */
  deviceMeterDb(trackId: string, deviceId: string): number {
    if (deviceId === 'inst') return this.instDb.get(trackId) ?? -100
    const i = this.grIndex.get(`${trackId}|${deviceId}`)
    const lin = i === undefined ? undefined : this.snap?.fx_level?.[i]
    return lin === undefined ? -100 : linToDb(lin)
  }

  deviceReductionDb(trackId: string, fxId: string): number {
    const i = this.grIndex.get(`${trackId}|${fxId}`)
    if (i === undefined) return 0
    return this.snap?.gr[i] ?? 0
  }

  /** Per-band gain reduction in dB (≤ 0) — only the multiband reports it. */
  deviceBandReduction(trackId: string, fxId: string): number[] {
    const i = this.grIndex.get(`${trackId}|${fxId}`)
    return (i === undefined ? undefined : this.snap?.fx_bands?.[i]) ?? []
  }

  deviceAnalyser(_trackId: string, _fxId: string): AnalyserNode | null {
    return null
  }

  setMeteredTrack(tid: string | null) {
    this.meteredTrackId = tid
  }

  getSpectrum(): Float32Array { return EMPTY_F32 }
  getWaveform(): Float32Array { return EMPTY_F32 }
  getPitchHz(): number { return 0 }
  lfoValue(_lfoId: string): number { return 0 }

  // ---------------- track freeze ----------------

  async freezeTrack(_trackId: string) {
    toast('Freeze isn’t needed on desktop — the native engine has the headroom')
  }
  unfreezeTrack(_trackId: string) { /* no-op */ }
  isFrozen(_trackId: string) { return false }
  isFreezing(_trackId: string) { return false }

  // ---------------- diagnostics ----------------

  outputLatencyMs(): number {
    if (!this.bufferFrames || !this.sampleRate) return 0
    return Math.round((this.bufferFrames / this.sampleRate) * 1000)
  }

  perfStats(): PerfStats {
    const s = this.snap
    const peakLoad = s?.peak_load ?? 0
    const rtf = peakLoad > 1 ? 1 / peakLoad : 1
    const glitchesNow = (s?.xruns ?? 0) + (s?.over_budget ?? 0)
    return {
      started: this.started,
      tracks: this.trackIds.length,
      effects: this.effectCount,
      returns: 0,
      sampleRate: this.sampleRate,
      oversampling: false,
      latencyMs: this.outputLatencyMs(),
      audioLoad: { avg: 0, peak: 0, underrun: 0, supported: false },
      probe: {
        supported: this.started,
        hasClock: true,
        rtf,
        minRtf: this.minRtf,
        glitches: Math.max(0, glitchesNow - this.glitchBase),
        maxGapMs: Math.round((s?.max_callback_us ?? 0) / 1000),
      },
    }
  }

  resetGlitchCount() {
    const s = this.snap
    this.glitchBase = (s?.xruns ?? 0) + (s?.over_budget ?? 0)
    this.minRtf = 1
  }
}

export const nativeEngine = new NativeEngine()
