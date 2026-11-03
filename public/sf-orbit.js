// Orbit — moves a sound around the listener's head in 3D (binaural; best on
// headphones). One AudioWorklet; canonical DSP for the native port
// crates/sf-dsp/src/fx/orbit.rs (verified to match — change both together).
//
// The source (mono sum of the input) travels a tempo-synced path around the
// head. Per sample:
//   path phase → position (x right, y front, z up, metres)
//   → distance r, lateral angle, front/back
//   → per-ear delay = Doppler propagation delay (r/c × amount)
//                     + interaural time difference (Woodworth: a/c·(θ+sinθ))
//   → per-ear head shadow (Brown–Duda one-pole/one-zero spherical-head filter)
//   → per-ear low-pass: air absorption with distance + darkening behind the head
//   → 1/r distance gain (clamped)
// Delays are read from a mono ring with linear interpolation, so a moving
// source pitch-shifts naturally (Doppler). Delay targets are smoothed (20 ms)
// so path or parameter changes never click.
//
// Transport: the device posts {pos, t, bpm, playing}; the processor keeps its
// own sample-accurate position and re-syncs only when it drifts > 2 ticks.

const PPQ = 96
const C = 343                 // speed of sound, m/s
const HEAD = 0.0875           // head radius, m
const W0 = C / HEAD           // Brown–Duda corner, rad/s
const ALPHA_MIN = 0.1
const THETA_MIN = (150 * Math.PI) / 180
const RATE_TICKS = [3072, 1536, 768, 384, 192, 96, 48]   // 8 bars … 1/8 (4/4)

class Orbit extends AudioWorkletProcessor {
  constructor(options) {
    super()
    this.p = { path: 0, rate: 3, dist: 2, height: 0, width: 1, doppler: 0.5, mix: 1, gain: 0 }
    this.sr = sampleRate
    this.len = Math.ceil(0.1 * sampleRate) + 4      // ≥ 24.5 m × doppler + ITD
    this.ring = new Float64Array(this.len)
    this.w = 0
    this.dL = 0; this.dR = 0                        // smoothed delays, samples
    this.hsL = [0, 0]; this.hsR = [0, 0]            // head-shadow x1, y1
    this.lpL = 0; this.lpR = 0                      // one-pole low-pass states
    this.first = true
    this.smooth = 1 - Math.exp(-1 / (0.02 * sampleRate))
    // transport
    this.pos = 0; this.bpm = 120; this.playing = false; this.pending = null
    const init = (options && options.processorOptions && options.processorOptions.init) || null
    if (init) for (const k in init) this.setParam(k, init[k])
    this.port.onmessage = e => {
      const d = e.data
      if (d.tr) this.pending = d.tr
      else if (d.init) for (const k in d.init) this.setParam(k, d.init[k])
      else this.setParam(d.k, d.v)
    }
  }

  setParam(k, v) { if (k in this.p && Number.isFinite(v)) this.p[k] = v }

  /** Transport anchor → sample-accurate position (shared logic, see header). */
  transport(target, playing, bpm) {
    this.bpm = bpm > 0 ? bpm : 120
    if (!playing) { this.playing = false; return }
    if (!this.playing || Math.abs(target - this.pos) > 2) this.pos = target
    this.playing = true
  }

  /** Path position for phase f ∈ [0,1) → this.px, this.py (unit space).
   *  Results go to fields, not arrays: no allocation on the audio thread. */
  pathXY(path, f) {
    const T = 2 * Math.PI
    switch (path) {
      case 0: this.px = Math.sin(T * f); this.py = Math.cos(T * f); break          // Circle
      case 1: this.px = Math.sin(T * f); this.py = Math.sin(2 * T * f); break      // Figure 8
      case 2: this.px = 2 * (1 - 4 * Math.abs(f - 0.5)); this.py = 0.35; break    // Fly-by
      case 3: this.px = Math.sin(T * f); this.py = 1; break                        // Pendulum
      default:                                                                     // Drift
        this.px = 0.6 * Math.sin(T * f) + 0.4 * Math.sin(T * 2.31 * f + 1)
        this.py = 0.6 * Math.cos(T * 1.37 * f) + 0.4 * Math.cos(T * 0.73 * f + 2)
    }
  }

  /** Brown–Duda head shadow, bilinear-transformed → this.b0, b1, a1. */
  shadow(thetaInc) {
    const a = 1 + ALPHA_MIN / 2 + (1 - ALPHA_MIN / 2) * Math.cos((thetaInc / THETA_MIN) * Math.PI)
    const K = 2 * this.sr
    const den = 2 * W0 + K
    this.b0 = (2 * W0 + a * K) / den
    this.b1 = (2 * W0 - a * K) / den
    this.a1 = (2 * W0 - K) / den
  }

  /** Linear-interpolated read `d` samples behind the write head. */
  readRing(d) {
    const len = this.len
    let pos = this.w - d
    while (pos < 0) pos += len
    let i0 = Math.floor(pos)
    const fr = pos - i0
    // a tiny negative offset + len can round to exactly len (one past the end)
    if (i0 >= len) i0 -= len
    const i1 = i0 + 1 === len ? 0 : i0 + 1
    return this.ring[i0] + (this.ring[i1] - this.ring[i0]) * fr
  }

  process(inputs, outputs) {
    const input = inputs[0], output = outputs[0]
    if (!output) return true
    const outL = output[0], outR = output[1] || output[0]
    const n = outL.length
    if (this.pending) {
      const a = this.pending; this.pending = null
      const target = a.pos + (currentTime - a.t) * (a.bpm / 60) * PPQ
      this.transport(target, !!a.playing, a.bpm)
    }
    if (!input || input.length === 0) { outL.fill(0); if (outR !== outL) outR.fill(0); return true }
    const inL = input[0], inR = input[1] || input[0]
    const p = this.p, sr = this.sr
    const path = Math.min(4, Math.max(0, p.path | 0))
    const cyc = RATE_TICKS[Math.min(6, Math.max(0, p.rate | 0))]
    const dist = Math.min(10, Math.max(0.5, p.dist))
    const width = Math.min(1, Math.max(0, p.width))
    const height = Math.min(1, Math.max(-1, p.height))
    const dop = Math.min(1, Math.max(0, p.doppler))
    const mix = Math.min(1, Math.max(0, p.mix))
    const outLin = Math.pow(10, p.gain / 20)
    const tps = (this.bpm / 60) * PPQ / sr
    for (let i = 0; i < n; i++) {
      const xl = inL[i], xr = inR[i]
      const src = (xl + xr) * 0.5
      this.ring[this.w] = src

      // ---- position ----
      let f = (this.pos / cyc) % 1
      if (f < 0) f += 1
      this.pos += tps
      this.pathXY(path, f)
      const X = this.px * width * dist, Y = this.py * dist, Z = height * dist
      const r = Math.max(0.3, Math.sqrt(X * X + Y * Y + Z * Z))
      const lat = Math.asin(Math.max(-1, Math.min(1, X / r)))   // lateral angle, + = right
      const front = Y / r                                        // +1 front … −1 behind

      // ---- per-ear delays (samples) ----
      const prop = (dop * r / C) * sr
      const itd = (HEAD / C) * (Math.abs(lat) + Math.sin(Math.abs(lat))) * sr
      const tL = prop + (lat > 0 ? itd : 0)     // source right → left ear later
      const tR = prop + (lat < 0 ? itd : 0)
      if (this.first) { this.dL = tL; this.dR = tR; this.first = false }
      this.dL += (tL - this.dL) * this.smooth
      this.dR += (tR - this.dR) * this.smooth

      // ---- fractional delay reads (linear) ----
      let sL = this.readRing(this.dL), sR = this.readRing(this.dR)
      this.w = this.w + 1 === this.len ? 0 : this.w + 1

      // ---- head shadow per ear ----
      this.shadow(Math.PI / 2 + lat)   // incidence measured from the left ear axis
      let y = this.b0 * sL + this.b1 * this.hsL[0] - this.a1 * this.hsL[1]
      this.hsL[0] = sL; this.hsL[1] = y; sL = y
      this.shadow(Math.PI / 2 - lat)
      y = this.b0 * sR + this.b1 * this.hsR[0] - this.a1 * this.hsR[1]
      this.hsR[0] = sR; this.hsR[1] = y; sR = y

      // ---- air absorption + behind-the-head darkening (one-pole LP) ----
      const fcAir = 20000 / (1 + r * 0.15)
      const fcBack = 6000 + 14000 * (0.5 + 0.5 * front)
      const fc = Math.min(fcAir, fcBack, sr * 0.45)
      const g = 1 - Math.exp((-2 * Math.PI * fc) / sr)
      this.lpL += (sL - this.lpL) * g
      this.lpR += (sR - this.lpR) * g

      // ---- distance gain ----
      const dg = Math.min(2, 1 / r)
      const wl = this.lpL * dg, wr = this.lpR * dg
      const ol = (wl * mix + xl * (1 - mix)) * outLin
      const orr = (wr * mix + xr * (1 - mix)) * outLin
      outL[i] = ol
      if (outR !== outL) outR[i] = orr
    }
    return true
  }
}

registerProcessor('sf-orbit', Orbit)
