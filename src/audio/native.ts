// Desktop (Tauri) IPC helpers — the Phase 0 native-audio surface
// (RUST_REWRITE_PLAN.md §5): device enumeration, test-tone stream at a chosen
// buffer size, live callback stats. The full engine facade (engine-native.ts)
// replaces the per-command style in Phase 1; this file stays the low-level
// invoke wrapper.
//
// Uses the global injected by `app.withGlobalTauri` so the web bundle needs no
// @tauri-apps/api dependency — in a plain browser isDesktop() is false and
// nothing here is called.

export type NativeDevice = { name: string; is_default: boolean }
/** A driver type compiled into the desktop build: CoreAudio (macOS),
 *  WASAPI (Windows), ASIO (Windows builds with the `asio` feature). */
export type NativeHost = { name: string; is_default: boolean }
export type NativeStreamInfo = {
  device: string
  sample_rate: number
  /** null = the driver manages it (always on WASAPI) — see the stats'
   *  callback frame range for what is actually happening */
  buffer_frames: number | null
  host?: string
  channels?: number
  sample_format?: string
}
export type NativeStats = {
  callbacks: number
  xruns: number
  over_budget: number
  last_callback_us: number
  max_callback_us: number
  budget_us: number
  peak_load: number
  /** callback sizes actually seen (0 = none yet) */
  min_callback_frames?: number
  max_callback_frames?: number
  /** 'host' = driver-managed priority; 'mmcss' / 'mmcss-failed' on WASAPI */
  rt_priority?: 'host' | 'mmcss' | 'mmcss-failed'
  device_lost?: boolean
}

export function isDesktop(): boolean {
  return typeof (window as any).__TAURI__ !== 'undefined'
}

function invoke<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  return (window as any).__TAURI__.core.invoke(cmd, args)
}

export const nativeAudio = {
  hosts: () => invoke<NativeHost[]>('audio_hosts'),
  /** host null = the driver type currently in use */
  devices: (host: string | null = null) => invoke<NativeDevice[]>('audio_devices', { host }),
  startTone: (device: string | null, bufferFrames: number | null, host: string | null = null) =>
    invoke<NativeStreamInfo>('audio_start_tone', { host, device, bufferFrames }),
  stop: () => invoke<void>('audio_stop'),
  stats: () => invoke<NativeStats>('audio_stats'),
}

// ---- desktop output choice (remembered per machine, never in the doc) ----
// Driver type + output device + buffer size the ENGINE opens. null = platform
// default driver / system default device / buffer from the latency preset.
// Device names are per-machine, so this lives in localStorage like the
// per-track input choice (state/inputs.ts).

export type NativeOutputPrefs = { host: string | null; device: string | null; buffer: number | null }
const OUT_KEY = 'sf-native-output'

export function nativeOutputPrefs(): NativeOutputPrefs {
  try {
    const p = JSON.parse(localStorage.getItem(OUT_KEY) || '{}')
    return {
      host: typeof p.host === 'string' ? p.host : null,
      device: typeof p.device === 'string' ? p.device : null,
      buffer: typeof p.buffer === 'number' ? p.buffer : null,
    }
  } catch {
    return { host: null, device: null, buffer: null }
  }
}

export function setNativeOutputPrefs(p: NativeOutputPrefs) {
  try { localStorage.setItem(OUT_KEY, JSON.stringify(p)) } catch { /* storage blocked — session only */ }
}

// ---- Phase 6: native recording (input takes, master capture, latency) ----
// Takes arrive as base64 f32le planar PCM at the input device's rate, already
// shifted left by the native latency compensation; `playhead_ticks` from
// record_take_start is the transport tick the take's sample 0 lines up with.

export type NativePcm = {
  sample_rate: number
  channels: number
  frames: number
  l_b64: string
  r_b64: string | null
  compensation_frames: number
}
export type NativeTakeStart = { take_id: number; playhead_ticks: number; playing: boolean }
export type NativeLatency = {
  compensation_ms: number
  measured_roundtrip_ms: number | null
  override_ms: number | null
  estimate_ms: number
}

export const nativeRecord = {
  /** host null = the driver type currently in use (inputs follow the output's driver) */
  inputDevices: (host: string | null = null) => invoke<NativeDevice[]>('input_devices', { host }),
  inputStart: (device: string | null, bufferFrames: number | null) =>
    invoke<NativeStreamInfo>('input_start', { device, bufferFrames }),
  inputStop: () => invoke<void>('input_stop'),
  inputInfo: () => invoke<NativeStreamInfo | null>('input_info'),
  takeStart: () => invoke<NativeTakeStart>('record_take_start'),
  takeStop: () => invoke<NativePcm>('record_take_stop'),
  masterStart: () => invoke<void>('master_capture_start'),
  masterStop: () => invoke<NativePcm>('master_capture_stop'),
  latencyMeasure: () => invoke<number>('latency_measure'),
  latencyGet: () => invoke<NativeLatency>('latency_get'),
  latencySet: (ms: number | null) => invoke<NativeLatency>('latency_set', { ms }),
}

/** base64 f32le → Float32Array (inverse of engine-native's f32ToB64). */
export function b64ToF32(b64: string): Float32Array {
  const bin = atob(b64)
  const bytes = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
  return new Float32Array(bytes.buffer, 0, bytes.length >> 2)
}

/** Native PCM payload → an AudioBuffer at the capture rate (built through an
 *  OfflineAudioContext so no audio hardware is touched — storage only). */
export function pcmToAudioBuffer(p: NativePcm): AudioBuffer {
  const l = b64ToF32(p.l_b64)
  const r = p.r_b64 ? b64ToF32(p.r_b64) : null
  const len = Math.max(1, l.length)
  const ctx = new OfflineAudioContext(r ? 2 : 1, len, p.sample_rate)
  const buf = ctx.createBuffer(r ? 2 : 1, len, p.sample_rate)
  buf.copyToChannel(l as any, 0)
  if (r) buf.copyToChannel(r as any, 1)
  return buf
}
