// The engine facade contract — the seam between the UI and whichever audio
// engine is running. Two implementations:
//   • engine-tone.ts   — the browser engine (Tone.js / Web Audio). Reference
//                        behavior during the Rust migration.
//   • engine-native.ts — the desktop engine (IPC client to the Rust core in
//                        the Tauri shell). Phase 1+ of docs/RUST_REWRITE_PLAN.md.
// `engine.ts` selects the implementation; UI code imports ONLY from there and
// must never depend on Tone types. Everything here is plain data + DOM types.
//
// Web-only internals (e.g. the post-limiter captureTap used by recorder.ts)
// are deliberately NOT part of this contract — web-only modules import the
// concrete Tone engine directly.

export type EngineMode = 'session' | 'arr'

/** Launch-state of one session clip slot (drives the slot's play/queue UI). */
export type ClipState = {
  playing: boolean
  queued: boolean
  stopQueued: boolean
}

/** Audio render-thread load (Chromium `renderCapacity`), all 0..1. */
export type AudioLoad = {
  avg: number
  peak: number
  underrun: number
  supported: boolean
}

/** Cross-engine render-thread health probe (rtf = audio produced ÷ real time;
 *  <1 means the thread fell behind = the audible glitch). The native engine
 *  reports its callback load/xruns through the same shape. */
export type AudioProbe = {
  supported: boolean
  hasClock: boolean
  rtf: number
  minRtf: number
  glitches: number
  maxGapMs: number
}

/** Snapshot for the performance monitor. */
export type PerfStats = {
  started: boolean
  tracks: number
  effects: number
  returns: number
  sampleRate: number
  oversampling: boolean
  latencyMs: number
  audioLoad: AudioLoad
  probe: AudioProbe
}

export interface EngineApi {
  // ---- lifecycle / state ----
  readonly started: boolean
  readonly playing: boolean
  readonly mode: EngineMode
  readonly sampleRate: number
  /** Arrangement playhead position while stopped (ticks). */
  readonly arrSeekTicks: number
  /** Bumps on every engine-state change; pair with subscribe for useSyncExternalStore. */
  readonly version: number
  subscribe(fn: () => void): () => void
  /** Idempotent init (first user gesture / Play). Resolves when audio is live. */
  ensureStarted(): Promise<void>

  // ---- transport ----
  togglePlay(): Promise<void>
  playArrangement(fromTicks?: number): Promise<void>
  seekArr(ticks: number): void
  stopAll(): void
  positionTicks(): number

  // ---- session clips ----
  launchClip(trackId: string, sceneId: string): Promise<void>
  launchScene(sceneId: string): Promise<void>
  stopClip(trackId: string): Promise<void>
  stopAllClips(): void
  clipState(trackId: string, sceneId: string): ClipState
  /** 0..1 phase of the playing clip on a track, or null when none. */
  clipProgress(trackId: string): number | null

  // ---- live input ----
  previewOn(trackId: string | null, pitch: number, vel: number): void
  previewOff(trackId: string | null, pitch: number): void
  /** True if the note was captured into the playing clip (record path). */
  recordNoteOn(trackId: string, pitch: number, vel: number): boolean
  recordNoteOff(trackId: string, pitch: number): void

  // ---- auditioning (browser/library previews; never touches tracks) ----
  audition(type: string, params: Record<string, number>): Promise<void>
  auditionPitches(type: string, params: Record<string, number>, chords: number[][], chordDur?: number): Promise<void>
  auditionSample(sampleId: string): Promise<void>
  stopAudition(): void
  preloadSample(sampleId: string): void

  // ---- meters / analysis (polled at rAF rate by the UI) ----
  meterDb(trackId: string): number
  masterDb(): number
  deviceMeterDb(trackId: string, deviceId: string): number
  deviceReductionDb(trackId: string, fxId: string): number
  deviceBandReduction(trackId: string, fxId: string): number[]
  /** Pre-effect analyser tap (Auto-Tune detector / EQ7 RTA), when available. */
  deviceAnalyser(trackId: string, fxId: string): AnalyserNode | null
  readonly meteredTrackId: string | null
  /** Scope per-device meters to the open rack's track ('master' | trackId | null). */
  setMeteredTrack(tid: string | null): void
  getSpectrum(): Float32Array
  getWaveform(): Float32Array
  getPitchHz(): number
  /** Live LFO output [-1,1] for the UI indicator. */
  lfoValue(lfoId: string): number

  // ---- track freeze ----
  freezeTrack(trackId: string): Promise<void>
  unfreezeTrack(trackId: string): void
  isFrozen(trackId: string): boolean
  isFreezing(trackId: string): boolean

  // ---- diagnostics ----
  perfStats(): PerfStats
  resetGlitchCount(): void
  outputLatencyMs(): number
}
