// Audio input device selection — strictly LOCAL, never in the shared Yjs doc:
// a MediaDevices deviceId is meaningful only to the browser that enumerated it,
// so syncing it to collaborators would be nonsense. Per-track choice persists to
// localStorage; the device list is refreshed on demand (labels need mic
// permission, so we request it once when the user first opens a picker).

export type InputDevice = { deviceId: string; label: string }

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
