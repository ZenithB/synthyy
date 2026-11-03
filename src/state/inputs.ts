// Audio input device selection — strictly LOCAL, never in the shared Yjs doc:
// a MediaDevices deviceId is meaningful only to the browser that enumerated it,
// so syncing it to collaborators would be nonsense. Per-track choice persists to
// localStorage; the device list is refreshed on demand (labels need mic
// permission, so we request it once when the user first opens a picker).

import { isDesktop, nativeRecord } from '../audio/native'

// On desktop the "deviceId" is the native (cpal) device NAME — stable across
// sessions, and what `input_start` wants.
export type InputDevice = { deviceId: string; label: string; channels?: number }

let devices: InputDevice[] = []
const listeners = new Set<() => void>()
let version = 0
const emit = () => { version++; listeners.forEach(l => l()) }

export function subscribeInputs(fn: () => void) { listeners.add(fn); return () => { listeners.delete(fn) } }
export function inputsVersion() { return version }
export function inputDevices(): InputDevice[] { return devices }

/** Enumerate audio inputs. Labels are blank until mic permission is granted, so
 *  the caller can pass `prompt` to request a one-shot stream first. */
export async function refreshInputDevices(prompt = false): Promise<InputDevice[]> {
  if (isDesktop()) {
    try {
      devices = (await nativeRecord.inputDevices()).map(d => ({ deviceId: d.name, label: d.is_default ? `${d.name} ★` : d.name, channels: d.channels }))
      emit()
    } catch { /* native enumeration failed — keep the old list */ }
    return devices
  }
  try {
    if (prompt && navigator.mediaDevices?.getUserMedia) {
      try { const s = await navigator.mediaDevices.getUserMedia({ audio: true }); s.getTracks().forEach(t => t.stop()) } catch { /* denied — labels stay blank */ }
    }
    const all = await navigator.mediaDevices.enumerateDevices()
    devices = all.filter(d => d.kind === 'audioinput').map((d, i) => ({ deviceId: d.deviceId, label: d.label || `Input ${i + 1}` }))
    emit()
  } catch { /* enumerateDevices unsupported/blocked */ }
  return devices
}

// keep the list fresh if the user plugs/unplugs an interface mid-session
if (typeof navigator !== 'undefined' && navigator.mediaDevices) {
  navigator.mediaDevices.addEventListener?.('devicechange', () => { void refreshInputDevices(false) })
}

// ---- per-track input choice (localStorage) ----
const KEY = 'sf-track-inputs'
function load(): Record<string, string> {
  try { return JSON.parse(localStorage.getItem(KEY) || '{}') } catch { return {} }
}
let chosen = load()

export function trackInput(trackId: string): string { return chosen[trackId] ?? '' }
export function setTrackInput(trackId: string, deviceId: string) {
  chosen = { ...chosen, [trackId]: deviceId }
  localStorage.setItem(KEY, JSON.stringify(chosen))
  emit()
}

// ---- per-track input CHANNELS (desktop) ----
// Which of the device's inputs a track records: e.g. a mic on input 2 of a
// USB interface records as a mono take of just that input. Stored per track
// like the device choice; absent = inputs 1–2 (the previous behaviour).
export type InputChannels = { first: number; count: 1 | 2 }
const CH_KEY = 'sf-track-input-ch'
function loadCh(): Record<string, InputChannels> {
  try { return JSON.parse(localStorage.getItem(CH_KEY) || '{}') } catch { return {} }
}
let chosenCh = loadCh()

export function trackInputChannels(trackId: string): InputChannels | null { return chosenCh[trackId] ?? null }
export function setTrackInputChannels(trackId: string, ch: InputChannels | null) {
  const next = { ...chosenCh }
  if (ch) next[trackId] = ch; else delete next[trackId]
  chosenCh = next
  try { localStorage.setItem(CH_KEY, JSON.stringify(chosenCh)) } catch { /* storage blocked */ }
  emit()
}

/** Picker options for a device with `n` inputs: stereo pairs, then each
 *  input alone. Value encoding "first:count" (0-based first). */
export function inputChannelOptions(n: number): { value: string; label: string }[] {
  const opts: { value: string; label: string }[] = []
  for (let i = 0; i + 1 < n; i += 2) opts.push({ value: `${i}:2`, label: `In ${i + 1}+${i + 2}` })
  for (let i = 0; i < n; i++) opts.push({ value: `${i}:1`, label: `In ${i + 1}` })
  return opts
}
