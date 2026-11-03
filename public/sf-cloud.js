// Cloud — granular delay. One AudioWorklet; canonical DSP for the native port
// crates/sf-dsp/src/fx/cloud.rs (verified to match — change both together).
//
// A 4 s stereo buffer records the input (+ feedback of the cloud itself,
// soft-clipped with tanh) unless Freeze holds it. Grains spawn at `density`
// per second (interval jittered ±50 %); each grain:
//   position  `delay` + spray × random × (4 × size) behind the write head
//             (pitched-up grains start further back so they never overrun it)
//   pitch     2^((pitch ± scatter·random)/12), linear-interpolated reads
//   reverse   with probability `reverse`
//   window    Hann over `size`
//   pan       constant-power, ± spread·random
// Up to 32 grains (preallocated — no allocation on the audio thread); the sum
// is normalised by the expected overlap (1/√(density × size)) so Density
// changes texture, not loudness. Random choices: xorshift32, fixed seed.

const MAX_GRAINS = 32
const SEED = 0x6d2b79f5

class Cloud extends AudioWorkletProcessor {
  constructor(options) {
    super()
    this.p = { size: 80, density: 20, delay: 250, spray: 0.3, pitch: 0, scatter: 0, reverse: 0, spread: 0.6, feedback: 0.3, freeze: 0, mix: 0.5, gain: 0 }
    this.sr = sampleRate
    this.len = Math.ceil(4 * sampleRate)
    this.bufL = new Float64Array(this.len)
    this.bufR = new Float64Array(this.len)
    this.w = 0
    // grain pool: active, pos (read position, buffer samples), inc, age, dur, gl, gr
    this.gAct = new Uint8Array(MAX_GRAINS)
    this.gPos = new Float64Array(MAX_GRAINS)
    this.gInc = new Float64Array(MAX_GRAINS)
    this.gAge = new Float64Array(MAX_GRAINS)
    this.gDur = new Float64Array(MAX_GRAINS)
    this.gL = new Float64Array(MAX_GRAINS)
    this.gR = new Float64Array(MAX_GRAINS)
    this.untilNext = 0
    this.rng = SEED
    this.fbL = 0; this.fbR = 0
    const init = (options && options.processorOptions && options.processorOptions.init) || null
    if (init) for (const k in init) this.setParam(k, init[k])
    this.port.onmessage = e => {
      const d = e.data
      if (d.init) for (const k in d.init) this.setParam(k, d.init[k])
      else if ('k' in d) this.setParam(d.k, d.v)
    }
  }

  setParam(k, v) { if (k in this.p && Number.isFinite(v)) this.p[k] = v }

  rand() {
    let x = this.rng
    x ^= x << 13; x >>>= 0
    x ^= x >>> 17
    x ^= x << 5; x >>>= 0
    this.rng = x
    return x / 4294967296
  }

  read(buf, pos) {
    const len = this.len
    while (pos < 0) pos += len
    while (pos >= len) pos -= len
    let i0 = Math.floor(pos)
    const fr = pos - i0
    // a tiny negative offset + len can round to exactly len (one past the end)
    if (i0 >= len) i0 -= len
    const i1 = i0 + 1 === len ? 0 : i0 + 1
    return buf[i0] + (buf[i1] - buf[i0]) * fr
  }

  spawn(sizeS, delayS, spray, pitch, scatter, reverse, spread) {
    let g = -1
    for (let i = 0; i < MAX_GRAINS; i++) if (!this.gAct[i]) { g = i; break }
    const rSpray = this.rand(), rPitch = this.rand(), rRev = this.rand(), rPan = this.rand()
    if (g < 0) return                       // pool full: skip (draws still consumed)
    const sr = this.sr
    const dur = Math.max(16, sizeS * sr)
    const inc = Math.pow(2, (pitch + scatter * (2 * rPitch - 1)) / 12)
    const rev = rRev < reverse
    // how far behind the write head the grain's audio sits
    let back = delayS * sr + spray * rSpray * 4 * sizeS * sr
    if (!rev && inc > 1) back += dur * (inc - 1)   // forward + fast: start further back
    if (rev) back += dur * inc                      // reverse reads end → start
    back = Math.min(this.len - 2, Math.max(1, back))
    this.gPos[g] = this.w - back + (rev ? dur * inc : 0)
    this.gInc[g] = rev ? -inc : inc
    this.gAge[g] = 0
    this.gDur[g] = dur
    const pan = Math.min(1, Math.max(-1, spread * (2 * rPan - 1)))
    const a = (pan + 1) * 0.25 * Math.PI
    this.gL[g] = Math.cos(a)
    this.gR[g] = Math.sin(a)
    this.gAct[g] = 1
  }

  process(inputs, outputs) {
    const input = inputs[0], output = outputs[0]
    if (!output) return true
    const outL = output[0], outR = output[1] || output[0]
    const n = outL.length
    if (!input || input.length === 0) { outL.fill(0); if (outR !== outL) outR.fill(0); return true }
    const inL = input[0], inR = input[1] || input[0]
    const p = this.p, sr = this.sr, len = this.len
    const sizeS = Math.min(0.5, Math.max(0.01, p.size / 1000))
    const density = Math.min(100, Math.max(1, p.density))
    const delayS = Math.min(2, Math.max(0, p.delay / 1000))
    const spray = Math.min(1, Math.max(0, p.spray))
    const scatter = Math.min(12, Math.max(0, p.scatter))
    const reverse = Math.min(1, Math.max(0, p.reverse))
    const spread = Math.min(1, Math.max(0, p.spread))
    const fb = Math.min(0.95, Math.max(0, p.feedback))
    const freeze = p.freeze >= 0.5
    const mix = Math.min(1, Math.max(0, p.mix))
    const outLin = Math.pow(10, p.gain / 20)
    const norm = 1 / Math.sqrt(Math.max(1, density * sizeS))
    const T = 2 * Math.PI

    for (let i = 0; i < n; i++) {
      const xl = inL[i], xr = inR[i]
      if (!freeze) {
        this.bufL[this.w] = xl + Math.tanh(this.fbL * fb)
        this.bufR[this.w] = xr + Math.tanh(this.fbR * fb)
      }

      // ---- scheduler ----
      this.untilNext -= 1
      if (this.untilNext <= 0) {
        this.spawn(sizeS, delayS, spray, p.pitch, scatter, reverse, spread)
        this.untilNext += (sr / density) * (0.5 + this.rand())
      }

      // ---- grains ----
      let wl = 0, wr = 0
      for (let g = 0; g < MAX_GRAINS; g++) {
        if (!this.gAct[g]) continue
        const ph = this.gAge[g] / this.gDur[g]
        const env = 0.5 - 0.5 * Math.cos(T * ph)
        const s = (this.read(this.bufL, this.gPos[g]) + this.read(this.bufR, this.gPos[g])) * 0.5 * env
        wl += s * this.gL[g]
        wr += s * this.gR[g]
        this.gPos[g] += this.gInc[g]
        this.gAge[g] += 1
        if (this.gAge[g] >= this.gDur[g]) this.gAct[g] = 0
      }
      wl *= norm; wr *= norm
      this.fbL = wl; this.fbR = wr
      if (!freeze) this.w = this.w + 1 === len ? 0 : this.w + 1

      const ol = (wl * mix + xl * (1 - mix)) * outLin
      const orr = (wr * mix + xr * (1 - mix)) * outLin
      outL[i] = ol
      if (outR !== outL) outR[i] = orr
    }
    return true
  }
}

registerProcessor('sf-cloud', Cloud)
