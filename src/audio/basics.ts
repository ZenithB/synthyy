// The "Basics" bank: test tones every project can lean on — sweeps, a
// calibration sine, the four noise colors and a set of impulse pings.
// Generated locally on first run (pure math, no shipped audio files) and
// stored in the sample bank, so they show up in every session.

import { addGeneratedSample, libReady, listSamples } from './samples'

const SR = 44100
const FOLDER = 'Basics'
const FLAG = 'sf-basics-v1'

// ---- tiny mono 16-bit WAV writer ----
function wavBytes(samples: Float32Array): ArrayBuffer {
  const n = samples.length
  const ab = new ArrayBuffer(44 + n * 2)
  const dv = new DataView(ab)
  const ws = (o: number, s: string) => { for (let i = 0; i < s.length; i++) dv.setUint8(o + i, s.charCodeAt(i)) }
  ws(0, 'RIFF'); dv.setUint32(4, 36 + n * 2, true); ws(8, 'WAVE')
  ws(12, 'fmt '); dv.setUint32(16, 16, true); dv.setUint16(20, 1, true)
  dv.setUint16(22, 1, true); dv.setUint32(24, SR, true)
  dv.setUint32(28, SR * 2, true); dv.setUint16(32, 2, true); dv.setUint16(34, 16, true)
  ws(36, 'data'); dv.setUint32(40, n * 2, true)
  for (let i = 0, o = 44; i < n; i++, o += 2) {
    const s = Math.max(-1, Math.min(1, samples[i]))
    dv.setInt16(o, s < 0 ? s * 0x8000 : s * 0x7fff, true)
  }
  return ab
}

// ---- generators ----
const fade = (a: Float32Array, ms = 8) => {
  const n = Math.min(a.length >> 1, Math.round((ms / 1000) * SR))
  for (let i = 0; i < n; i++) { const g = i / n; a[i] *= g; a[a.length - 1 - i] *= g }
  return a
}
const normalize = (a: Float32Array, peak = 0.85) => {
  let m = 0
  for (let i = 0; i < a.length; i++) m = Math.max(m, Math.abs(a[i]))
  if (m > 0) { const g = peak / m; for (let i = 0; i < a.length; i++) a[i] *= g }
  return a
}

/** Exponential (log-frequency) sine sweep f0 → f1 over `dur` seconds. */
function sweep(f0: number, f1: number, dur: number): Float32Array {
  const n = Math.round(SR * dur)
  const out = new Float32Array(n)
  const k = Math.pow(f1 / f0, 1 / dur)
  const c = (2 * Math.PI * f0) / Math.log(k)   // phase(t) = c·(k^t − 1)
  for (let i = 0; i < n; i++) out[i] = 0.8 * Math.sin(c * (Math.pow(k, i / SR) - 1))
  return fade(out)
}

function sine(f: number, dur: number): Float32Array {
  const n = Math.round(SR * dur)
  const out = new Float32Array(n)
  for (let i = 0; i < n; i++) out[i] = 0.8 * Math.sin((2 * Math.PI * f * i) / SR)
  return fade(out)
}

function white(dur: number): Float32Array {
  const out = new Float32Array(Math.round(SR * dur))
  for (let i = 0; i < out.length; i++) out[i] = Math.random() * 2 - 1
  return fade(normalize(out))
}

/** Pink (1/f) noise — Paul Kellet's economy filter. */
function pink(dur: number): Float32Array {
  const out = new Float32Array(Math.round(SR * dur))
  let b0 = 0, b1 = 0, b2 = 0
  for (let i = 0; i < out.length; i++) {
    const w = Math.random() * 2 - 1
    b0 = 0.99765 * b0 + w * 0.099046
    b1 = 0.963 * b1 + w * 0.2965164
    b2 = 0.57 * b2 + w * 1.0526913
    out[i] = b0 + b1 + b2 + w * 0.1848
  }
  return fade(normalize(out))
}

/** Brown (1/f²) noise — leaky-integrated white. */
function brown(dur: number): Float32Array {
  const out = new Float32Array(Math.round(SR * dur))
  let acc = 0
  for (let i = 0; i < out.length; i++) {
    acc = acc * 0.997 + (Math.random() * 2 - 1) * 0.05
    out[i] = acc
  }
  return fade(normalize(out))
}

/** Green noise — the mid band of white noise (bandpass around 1kHz). */
function green(dur: number): Float32Array {
  const out = new Float32Array(Math.round(SR * dur))
  // RBJ constant-skirt bandpass, fc 1kHz, Q 0.6
  const w0 = (2 * Math.PI * 1000) / SR
  const alpha = Math.sin(w0) / (2 * 0.6)
  const a0 = 1 + alpha
  const b0 = alpha / a0, b2 = -alpha / a0
  const a1 = (-2 * Math.cos(w0)) / a0, a2 = (1 - alpha) / a0
  let x1 = 0, x2 = 0, y1 = 0, y2 = 0
  for (let i = 0; i < out.length; i++) {
    const x = Math.random() * 2 - 1
    const y = b0 * x + b2 * x2 - a1 * y1 - a2 * y2
    x2 = x1; x1 = x; y2 = y1; y1 = y
    out[i] = y
  }
  return fade(normalize(out))
}

/** A decaying sine ping at `f` — for exciting resonances, reverbs, tuning. */
function ping(f: number, tau = 0.15): Float32Array {
  const dur = Math.min(1, tau * 5.5)
  const n = Math.round(SR * dur)
  const out = new Float32Array(n)
  for (let i = 0; i < n; i++) {
    const t = i / SR
    out[i] = 0.9 * Math.sin(2 * Math.PI * f * t) * Math.exp(-t / tau)
  }
  return out
}

/** A short decaying white-noise burst. */
function noiseBurst(): Float32Array {
  const n = Math.round(SR * 0.12)
  const out = new Float32Array(n)
  for (let i = 0; i < n; i++) out[i] = (Math.random() * 2 - 1) * Math.exp(-(i / SR) / 0.02)
  return normalize(out, 0.9)
}

const SOUNDS: [string, () => Float32Array][] = [
  ['Sweep 20Hz–1k', () => sweep(20, 1000, 3)],
  ['Sweep 100Hz–10k', () => sweep(100, 10000, 3)],
  ['Sine 1kHz', () => sine(1000, 1)],
  ['White noise', () => white(2)],
  ['Pink noise', () => pink(2)],
  ['Brown noise', () => brown(2)],
  ['Green noise', () => green(2)],
  ['Impulse 50Hz', () => ping(50, 0.2)],
  ['Impulse 100Hz', () => ping(100, 0.18)],
  ['Impulse 500Hz', () => ping(500, 0.15)],
  ['Impulse 1kHz', () => ping(1000, 0.12)],
  ['Impulse 2.5kHz', () => ping(2500, 0.1)],
  ['Impulse 5kHz', () => ping(5000, 0.08)],
  ['Impulse 10kHz', () => ping(10000, 0.06)],
  ['Impulse white noise', noiseBurst],
]

/** Generate the Basics bank once per browser (skips if it already exists). */
export async function ensureBasicSamples() {
  try {
    if (localStorage.getItem(FLAG)) return
    await libReady
    if (listSamples().some(s => s.folder === FOLDER || s.folder.startsWith(FOLDER + '/'))) {
      localStorage.setItem(FLAG, '1')
      return
    }
    for (const [name, gen] of SOUNDS) await addGeneratedSample(name, FOLDER, wavBytes(gen()))
    localStorage.setItem(FLAG, '1')
  } catch { /* best-effort — retried next launch */ }
}
