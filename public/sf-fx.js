// Pitch-shifter + auto-tune AudioWorklets: each is a single node replacing a
// Tone.js composite of many native nodes, and both run identically inside an
// OfflineAudioContext, so exports and track freezes reproduce them exactly.
//
//  · shifter — dual-tap "tape splice" delay: two half-window-offset taps whose
//              delay ramps at (1−ratio), equal-power crossfaded (Tone.PitchShift
//              topology, but with the mathematically exact ramp rate — Tone's
//              1.2/windowSize fudge made it audibly off-pitch).
//  · autotune— the same shifter core driven by an in-worklet YIN detector (the
//              engine used to run YIN on the MAIN thread per animation frame).
//
// Chorus and phaser worklet ports were tried here and REVERTED: even faithful
// reimplementations audibly shifted the sound of existing projects, so those
// devices stay Tone composites (devices.ts). Auto-Tune kept the worklet because
// its change was a correctness fix, not a character change.
//
// All processors are render-quantum-size agnostic (renderSizeHint safe).
// Params arrive as port messages: { init: {…} } once, then { k, v } updates.

'use strict'

// equal-power dry/wet (Tone.CrossFade): dry·cos(m·π/2) + wet·sin(m·π/2)
function mixGains(mix) {
  const m = Math.max(0, Math.min(1, mix))
  return [Math.cos(m * Math.PI * 0.5), Math.sin(m * Math.PI * 0.5)]
}

// Interpolating ring-buffer delay line (one channel).
class DelayLine {
  constructor(maxSamples) {
    this.size = Math.max(64, maxSamples | 0)
    this.buf = new Float32Array(this.size)
    this.w = 0
  }
  write(x) {
    this.buf[this.w] = x
    this.w = (this.w + 1) % this.size
  }
  /** Read `d` samples back from the most recently written sample (d ≥ 0). */
  read(d) {
    if (d < 0) d = 0
    if (d > this.size - 2) d = this.size - 2
    const di = d | 0
    const frac = d - di
    let i0 = this.w - 1 - di
    if (i0 < 0) i0 += this.size
    let i1 = i0 - 1
    if (i1 < 0) i1 += this.size
    return this.buf[i0] + (this.buf[i1] - this.buf[i0]) * frac
  }
}

// ---------------- pitch-shifter core (shared by sf-shifter & sf-autotune) ----------------

// Dual-tap granular ("tape splice") shifter: two taps half a window apart whose
// delay ramps 0→window at rate (1−ratio); each tap fades out (sin/cos equal-power)
// exactly when its delay wraps, so the splice is inaudible.
class ShiftCore {
  constructor(windowSec) {
    this.win = Math.max(0.03, Math.min(0.25, windowSec || 0.1))
    this.ratio = 1        // current (smoothed) playback ratio
    this.target = 1
    this.ph = 0           // tap phase 0..1 (delay = ph·window)
    const max = Math.ceil(sampleRate * 0.6)
    this.dl = [new DelayLine(max), new DelayLine(max)]
  }
  setWindow(sec) { this.win = Math.max(0.03, Math.min(0.25, sec)) }
  setPitch(semi) { this.target = Math.pow(2, semi / 12) }
  /** Process one stereo block through the shifter, writing the WET signal. */
  block(inL, inR, wetL, wetR, n) {
    const winS = this.win * sampleRate
    let ph = this.ph
    const dlL = this.dl[0], dlR = this.dl[1]
    for (let i = 0; i < n; i++) {
      // glide the ratio (~2ms time constant) so pitch changes don't zipper
      this.ratio += (this.target - this.ratio) * 0.01
      const inc = (1 - this.ratio) / winS
      dlL.write(inL ? inL[i] : 0)
      dlR.write(inR ? inR[i] : 0)
      ph += inc
      ph -= Math.floor(ph)             // wrap to [0,1)
      const phB = (ph + 0.5) % 1
      const dA = ph * winS
      const dB = phB * winS
      const gA = Math.sin(Math.PI * ph)      // 0 at the wrap point of tap A
      const gB = Math.sin(Math.PI * phB)     // 0 at the wrap point of tap B
      wetL[i] = dlL.read(dA) * gA + dlL.read(dB) * gB
      wetR[i] = dlR.read(dA) * gA + dlR.read(dB) * gB
    }
    this.ph = ph
  }
}

class SFShifter extends AudioWorkletProcessor {
  constructor(options) {
    super()
    const o = (options && options.processorOptions) || {}
    this.core = new ShiftCore(o.window || 0.1)
    this.mix = 1
    this.tmpL = null
    this.tmpR = null
    this.port.onmessage = e => {
      const d = e.data
      if (d.init) for (const k in d.init) this.set(k, d.init[k])
      else this.set(d.k, d.v)
    }
  }
  set(k, v) {
    if (k === 'pitch') this.core.setPitch(v)
    else if (k === 'window') this.core.setWindow(v)
    else if (k === 'mix') this.mix = v
  }
  process(inputs, outputs) {
    const output = outputs[0]
    if (!output) return true
    const outL = output[0], outR = output[1] || output[0]
    const n = outL.length
    const input = inputs[0]
    const inL = input && input[0], inR = (input && (input[1] || input[0]))
    if (!this.tmpL || this.tmpL.length < n) { this.tmpL = new Float32Array(n); this.tmpR = new Float32Array(n) }
    this.core.block(inL, inR, this.tmpL, this.tmpR, n)
    const [dry, wet] = mixGains(this.mix)
    for (let i = 0; i < n; i++) {
      outL[i] = (inL ? inL[i] : 0) * dry + this.tmpL[i] * wet
      if (outR !== outL) outR[i] = (inR ? inR[i] : 0) * dry + this.tmpR[i] * wet
    }
    return true
  }
}

// ---------------- auto-tune ----------------

// YIN pitch detection (cumulative-mean-normalised difference) — the same
// algorithm/constants the engine used to run per animation frame on the main
// thread, now on the audio thread over a 2×-decimated window, with an early
// exit once the first sub-threshold dip is walked to its local minimum.
function yinDetect(buf, n, sr) {
  const maxTau = Math.min(n - 256, Math.floor(sr / 70))    // down to ~70 Hz
  const minTau = Math.max(2, Math.floor(sr / 1100))         // up to ~1100 Hz
  if (maxTau <= minTau + 2) return 0
  const lim = n - maxTau
  let running = 0, best = 1, bt = -1, prev = 1, found = -1
  for (let tau = minTau; tau < maxTau; tau++) {
    let sum = 0
    for (let i = 0; i < lim; i++) { const d = buf[i] - buf[i + tau]; sum += d * d }
    running += sum
    const c = running > 0 ? (sum * (tau - minTau + 1)) / running : 1
    if (found >= 0) {
      if (c < prev) { found = tau; prev = c; continue }   // still descending
      break                                               // local minimum found
    }
    if (c < 0.15) { found = tau; prev = c; continue }
    if (c < best) { best = c; bt = tau }
    prev = c
  }
  let tau = found
  if (tau < 0) {
    if (best > 0.4) return 0
    tau = bt
  }
  return tau > 0 ? sr / tau : 0
}

const DETECT_N = 1024        // analysis window (decimated samples ≈ 46ms @ 44.1k)
const DETECT_HOP = 512       // input samples between detections (~12ms @ 44.1k)

class SFAutotune extends AudioWorkletProcessor {
  constructor() {
    super()
    this.core = new ShiftCore(0.06)
    this.amount = 1
    this.speed = 20            // ms glide
    this.mix = 1
    this.mode = 0              // 0 = snap to key/scale, 1 = chromatic
    this.root = 9
    this.mask = 0x5AD          // minor-scale pitch-class bitmask (root-relative)
    this.cur = 0               // current correction (semitones)
    this.desired = 0
    // 2×-decimated mono ring for detection
    this.ring = new Float32Array(4096)
    this.rw = 0
    this.decim = 0             // pending sample for pairwise averaging
    this.decimHold = 0
    this.sinceHop = 0
    this.ana = new Float32Array(DETECT_N)
    this.tmpL = null
    this.tmpR = null
    this.port.onmessage = e => {
      const d = e.data
      if (d.init) for (const k in d.init) this.set(k, d.init[k])
      else this.set(d.k, d.v)
    }
  }
  set(k, v) {
    if (k === 'amount') this.amount = v
    else if (k === 'speed') this.speed = v
    else if (k === 'mix') this.mix = v
    else if (k === 'mode') this.mode = v | 0
    else if (k === 'root') this.root = v | 0
    else if (k === 'mask') this.mask = v | 0
  }
  inMask(pitch) {
    return (this.mask >> ((((pitch - this.root) % 12) + 12) % 12)) & 1
  }
  snap(pitch) {
    if (this.mode === 1) return pitch
    if (this.inMask(pitch)) return pitch
    for (let off = 1; off <= 6; off++) {
      if (this.inMask(pitch - off)) return pitch - off
      if (this.inMask(pitch + off)) return pitch + off
    }
    return pitch
  }
  detect() {
    // copy the latest DETECT_N decimated samples out of the ring, oldest first
    const ring = this.ring, N = ring.length
    let start = this.rw - DETECT_N
    if (start < 0) start += N
    for (let i = 0; i < DETECT_N; i++) this.ana[i] = ring[(start + i) % N]
    let rms = 0
    for (let i = 0; i < DETECT_N; i++) rms += this.ana[i] * this.ana[i]
    rms = Math.sqrt(rms / DETECT_N)
    if (rms <= 0.004) return                       // silence: hold the correction
    const f = yinDetect(this.ana, DETECT_N, sampleRate / 2)
    if (f <= 0) return
    const midi = 69 + 12 * Math.log2(f / 440)
    const target = this.snap(Math.round(midi))
    this.desired = Math.max(-12, Math.min(12, (target - midi) * this.amount))
  }
  process(inputs, outputs) {
    const output = outputs[0]
    if (!output) return true
    const outL = output[0], outR = output[1] || output[0]
    const n = outL.length
    const input = inputs[0]
    const inL = input && input[0], inR = (input && (input[1] || input[0]))
    // feed the decimated detection ring (mono mix, average adjacent pairs)
    for (let i = 0; i < n; i++) {
      const x = ((inL ? inL[i] : 0) + (inR ? inR[i] : 0)) * 0.5
      if (this.decim === 0) { this.decimHold = x; this.decim = 1 }
      else {
        this.ring[this.rw] = (this.decimHold + x) * 0.5
        this.rw = (this.rw + 1) % this.ring.length
        this.decim = 0
      }
    }
    this.sinceHop += n
    if (this.sinceHop >= DETECT_HOP) {
      this.sinceHop = 0
      this.detect()
    }
    // glide toward the desired correction (time constant = speed ms)
    const coeff = 1 - Math.exp(-(n / sampleRate) / Math.max(0.001, this.speed / 1000))
    this.cur += (this.desired - this.cur) * coeff
    this.core.setPitch(this.cur)
    if (!this.tmpL || this.tmpL.length < n) { this.tmpL = new Float32Array(n); this.tmpR = new Float32Array(n) }
    this.core.block(inL, inR, this.tmpL, this.tmpR, n)
    const [dry, wet] = mixGains(this.mix)
    for (let i = 0; i < n; i++) {
      outL[i] = (inL ? inL[i] : 0) * dry + this.tmpL[i] * wet
      if (outR !== outL) outR[i] = (inR ? inR[i] : 0) * dry + this.tmpR[i] * wet
    }
    return true
  }
}

registerProcessor('sf-shifter', SFShifter)
registerProcessor('sf-autotune', SFAutotune)
