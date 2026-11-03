// Destroyer — "distortion plus": a multiband-capable diode-clipper distortion
// in one AudioWorklet. Architecture modelled on the classic three-filter
// distortion units (dynamics flattener → clipper with pre/post/side filters);
// the DSP here is original.
//
//   in ─► Flatten ─┬─► [Pre filters] ─► Drive ─► Clipper ─► DC block ─► [Post filters] ─┐
//                  └─► [Side filters, latency-aligned] ───────────────────────────────────┴─► Tone ─► Mix ─► Out ─► Limiter
//
// Flatten   one-knob leveller: stereo-linked peak follower, gain =
//           (ref / env)^amount clamped to −24..+12 dB, so quiet and loud parts
//           hit the clipper at similar levels (the "flattened" sustain).
// Filters   F1, F2: 12 dB/oct TPT state-variable (Off/LP/BP/HP/BR), cutoff,
//           resonance, volume. `route` places each one Pre (in series before
//           the clipper), Post (in series after it) or Side (in PARALLEL with
//           it, fed the flattened clean signal) — 3 × 3 = 9 routings. A Side
//           filter is how you keep, e.g., a clean low end under distorted mids.
//           An Off filter is a bypass in series and silent on the side.
//           F3 ("Tone") always sits after the sum.
// Clipper   drive into one of six curves with `bias` (asymmetry → even
//           harmonics; the static offset is removed so bias never jumps the
//           output), run at 1×/2×/4× with windowed-sinc half-band FIRs, then
//           a 5 Hz DC blocker. Dry and Side paths are delayed by the FIR
//           latency (0 / 15 / 23 samples) so every path stays phase-aligned.
// Output    dry/wet mix, output gain, optional stereo-linked peak limiter
//           (instant attack, 50 ms release, ceiling 0.97).
//
// All maths is double precision, in an order mirrored exactly by the native
// port (crates/sf-dsp/src/fx/destroyer.rs, verified bit-identical). This file
// is the canonical DSP: change both together (CLAUDE.md render-path parity).

const DB = v => Math.pow(10, v / 20)
const FLAT_REF = 0.25          // flattener target level (≈ −12 dBFS)
const FLAT_MIN = 1 / 16        // −24 dB
const FLAT_MAX = 4             // +12 dB
const LIM_CEIL = 0.97
const UP_TAPS_A = 31           // 1× ↔ 2× half-band (latency 15 base samples)
const UP_TAPS_B = 33           // 2× ↔ 4× half-band (latency +8 base samples)
// route → [F1 role, F2 role]; 0 = Pre, 1 = Post, 2 = Side
const ROUTES = [[0, 0], [0, 1], [0, 2], [1, 0], [1, 1], [1, 2], [2, 0], [2, 1], [2, 2]]

// Half-band low-pass FIR taps: windowed sinc at a quarter of the (oversampled)
// rate, Blackman window, normalised to unity DC gain.
function halfband(n) {
  const h = new Float64Array(n)
  const c = (n - 1) / 2
  let sum = 0
  for (let i = 0; i < n; i++) {
    const x = (i - c) * 0.5
    const sinc = x === 0 ? 1 : Math.sin(Math.PI * x) / (Math.PI * x)
    const w = 0.42 - 0.5 * Math.cos((2 * Math.PI * i) / (n - 1)) + 0.08 * Math.cos((4 * Math.PI * i) / (n - 1))
    h[i] = 0.5 * sinc * w
    sum += h[i]
  }
  for (let i = 0; i < n; i++) h[i] /= sum
  return h
}

// FIR history ring; `out()` sums taps newest-first (order mirrored natively).
class Fir {
  constructor(h) { this.h = h; this.n = h.length; this.buf = new Float64Array(h.length); this.pos = 0 }
  reset() { this.buf.fill(0); this.pos = 0 }
  push(x) { this.pos = this.pos + 1 === this.n ? 0 : this.pos + 1; this.buf[this.pos] = x }
  out() {
    const h = this.h, b = this.buf, n = this.n
    let acc = 0, j = this.pos
    for (let i = 0; i < n; i++) {
      acc += h[i] * b[j]
      j = j === 0 ? n - 1 : j - 1
    }
    return acc
  }
}

// One clipper channel: up-FIRs, shaper, down-FIRs and the DC blocker.
class ClipChan {
  constructor(hA, hB) {
    this.upA = new Fir(hA); this.dnA = new Fir(hA)
    this.upB = new Fir(hB); this.dnB = new Fir(hB)
    this.dcx = 0; this.dcy = 0
  }
  reset() { this.upA.reset(); this.dnA.reset(); this.upB.reset(); this.dnB.reset(); this.dcx = 0; this.dcy = 0 }
}

// 12 dB/oct TPT state-variable filter state (coefficients live on the slot).
class Svf {
  constructor() { this.ic1 = 0; this.ic2 = 0 }
  reset() { this.ic1 = 0; this.ic2 = 0 }
}

// Clipper curves. Slope 1 at 0 for every curve, so drive means the same thing.
function shape(curve, u) {
  switch (curve) {
    case 0: return Math.tanh(u)                                               // Soft
    case 1: return u > 1 ? 1 : u < -1 ? -1 : u                                // Hard
    case 2: return u >= 0 ? 1 - Math.exp(-u) : -0.7 * (1 - Math.exp(u / 0.7)) // Diode (asymmetric)
    case 3: return u >= 0 ? u / (1 + u) : u / (1 - 1.5 * u)                   // Tube (asymmetric)
    case 4: return Math.sin(u)                                                // Fold
    default: {                                                                // Rectify (octave fuzz)
      const t = Math.tanh(u)
      return 0.5 * t + 0.5 * Math.abs(t)
    }
  }
}

class Destroyer extends AudioWorkletProcessor {
  constructor(options) {
    super()
    // Schema defaults (schema.ts "destroyer") — overwritten by the init params.
    this.p = {
      drive: 18, flat: 0.3, curve: 2, bias: 0, thresh: 0, route: 0,
      f1type: 0, f1freq: 800, f1res: 0.2, f1vol: 0,
      f2type: 0, f2freq: 3000, f2res: 0.2, f2vol: 0,
      f3type: 0, f3freq: 8000, f3res: 0.1, f3vol: 0,
      os: 1, limit: 1, mix: 1, gain: 0,
    }
    const hA = halfband(UP_TAPS_A), hB = halfband(UP_TAPS_B)
    this.clip = [new ClipChan(hA, hB), new ClipChan(hA, hB)]
    this.svf = [[new Svf(), new Svf()], [new Svf(), new Svf()], [new Svf(), new Svf()]]
    this.fc = [{}, {}, {}]   // per filter slot: { type, a1, a2, a3, k, vol }
    this.env = 0             // flattener follower
    this.lenv = 0            // limiter follower
    this.dlen = 32           // ≥ max latency (23) + 1
    this.dDry = [new Float64Array(this.dlen), new Float64Array(this.dlen)]
    this.dFlat = [new Float64Array(this.dlen), new Float64Array(this.dlen)]
    this.widx = 0
    this.osNow = -1
    const init = (options && options.processorOptions && options.processorOptions.init) || null
    if (init) for (const k in init) this.setParam(k, init[k])
    this.dirty = true
    this.port.onmessage = e => {
      const d = e.data
      if (d.init) for (const k in d.init) this.setParam(k, d.init[k])
      else this.setParam(d.k, d.v)
      this.dirty = true
    }
  }

  setParam(k, v) {
    if (k in this.p && Number.isFinite(v)) this.p[k] = v
  }

  setFilter(slot, type, freq, res) {
    const sr = sampleRate
    const g = Math.tan((Math.PI * Math.min(Math.max(freq, 10), sr * 0.49)) / sr)
    const k = 2 - 1.96 * Math.min(Math.max(res, 0), 1)
    const a1 = 1 / (1 + g * (g + k))
    const a2 = g * a1
    const a3 = g * a2
    const f = this.fc[slot]
    f.type = type | 0; f.k = k; f.a1 = a1; f.a2 = a2; f.a3 = a3
  }

  recompute() {
    this.dirty = false
    const p = this.p, sr = sampleRate
    this.setFilter(0, p.f1type, p.f1freq, p.f1res)
    this.setFilter(1, p.f2type, p.f2freq, p.f2res)
    this.setFilter(2, p.f3type, p.f3freq, p.f3res)
    this.fc[0].vol = DB(p.f1vol); this.fc[1].vol = DB(p.f2vol); this.fc[2].vol = DB(p.f3vol)
    const r = ROUTES[Math.min(8, Math.max(0, p.route | 0))]
    this.role1 = r[0]; this.role2 = r[1]
    this.curve = Math.min(5, Math.max(0, p.curve | 0))
    this.bias = Math.min(1, Math.max(-1, p.bias))
    this.biasOut = shape(this.curve, this.bias)
    this.thrLin = DB(Math.min(0, p.thresh))
    this.driveGain = DB(p.drive) / this.thrLin
    this.flat = Math.min(1, Math.max(0, p.flat))
    this.flatAtt = 1 - Math.exp(-1 / (0.001 * sr))
    this.flatRel = 1 - Math.exp(-1 / (0.08 * sr))
    this.limRel = Math.exp(-1 / (0.05 * sr))
    this.dcR = 1 - (2 * Math.PI * 5) / sr
    this.mix = Math.min(1, Math.max(0, p.mix))
    this.outLin = DB(p.gain)
    this.limit = p.limit >= 0.5
    const os = Math.min(2, Math.max(0, p.os | 0))
    if (os !== this.osNow) {
      // new oversampling factor: the FIR histories and the alignment delay
      // belong to the old one
      this.osNow = os
      for (const c of this.clip) c.reset()
      for (let ch = 0; ch < 2; ch++) { this.dDry[ch].fill(0); this.dFlat[ch].fill(0) }
      this.widx = 0
    }
    this.latency = os === 0 ? 0 : os === 1 ? 15 : 23
  }

  /** One filter slot on one channel; Off returns the input (series bypass). */
  filt(slot, ch, x) {
    const f = this.fc[slot]
    if (f.type === 0) return x
    const s = this.svf[slot][ch]
    const v3 = x - s.ic2
    const v1 = f.a1 * s.ic1 + f.a2 * v3
    const v2 = s.ic2 + f.a2 * s.ic1 + f.a3 * v3
    s.ic1 = 2 * v1 - s.ic1
    s.ic2 = 2 * v2 - s.ic2
    let y
    if (f.type === 1) y = v2                          // LP
    else if (f.type === 2) y = f.k * v1               // BP (unity peak)
    else if (f.type === 3) y = x - f.k * v1 - v2      // HP
    else y = x - f.k * v1                             // BR
    return y * f.vol
  }

  /** Oversampled clipper for one channel's base-rate sample. */
  clipOne(c, x) {
    const v = x * this.driveGain
    const cv = this.curve, b = this.bias, bo = this.biasOut
    let y
    if (this.osNow === 0) {
      y = shape(cv, v + b) - bo
    } else if (this.osNow === 1) {
      c.upA.push(v * 2); const u0 = c.upA.out()
      c.upA.push(0); const u1 = c.upA.out()
      c.dnA.push(shape(cv, u0 + b) - bo)
      c.dnA.push(shape(cv, u1 + b) - bo)
      y = c.dnA.out()
    } else {
      c.upA.push(v * 2); const a0 = c.upA.out()
      c.upA.push(0); const a1 = c.upA.out()
      c.upB.push(a0 * 2); const q0 = c.upB.out()
      c.upB.push(0); const q1 = c.upB.out()
      c.upB.push(a1 * 2); const q2 = c.upB.out()
      c.upB.push(0); const q3 = c.upB.out()
      c.dnB.push(shape(cv, q0 + b) - bo)
      c.dnB.push(shape(cv, q1 + b) - bo)
      const d0 = c.dnB.out()
      c.dnB.push(shape(cv, q2 + b) - bo)
      c.dnB.push(shape(cv, q3 + b) - bo)
      const d1 = c.dnB.out()
      c.dnA.push(d0)
      c.dnA.push(d1)
      y = c.dnA.out()
    }
    y *= this.thrLin
    // DC blocker (bias and Rectify leave a moving offset)
    const o = y - c.dcx + this.dcR * c.dcy
    c.dcx = y; c.dcy = o
    return o
  }

  process(inputs, outputs) {
    const input = inputs[0], output = outputs[0]
    if (!output) return true
    const outL = output[0], outR = output[1] || output[0]
    const n = outL.length
    if (!input || input.length === 0) {
      outL.fill(0); if (outR !== outL) outR.fill(0)
      return true
    }
    const inL = input[0], inR = input[1] || input[0]
    if (this.dirty) this.recompute()
    const r1 = this.role1, r2 = this.role2, dlen = this.dlen, lat = this.latency
    const wet = this.mix, dry = 1 - this.mix

    for (let i = 0; i < n; i++) {
      const xl = inL[i], xr = inR[i]

      // ---- flattener (stereo-linked) ----
      const a = Math.max(Math.abs(xl), Math.abs(xr))
      this.env += (a - this.env) * (a > this.env ? this.flatAtt : this.flatRel)
      let fg = 1
      if (this.flat > 0) {
        fg = Math.pow(FLAT_REF / Math.max(this.env, 1e-5), this.flat)
        fg = fg < FLAT_MIN ? FLAT_MIN : fg > FLAT_MAX ? FLAT_MAX : fg
      }

      // ---- latency-alignment delay for the dry and Side paths ----
      const w = this.widx
      let ridx = w - lat
      if (ridx < 0) ridx += dlen
      this.widx = w + 1 === dlen ? 0 : w + 1

      let ol = 0, orr = 0
      for (let ch = 0; ch < 2; ch++) {
        const x = ch === 0 ? xl : xr
        const fx = x * fg
        this.dDry[ch][w] = x
        this.dFlat[ch][w] = fx
        const dDry = this.dDry[ch][ridx]
        const dFlat = this.dFlat[ch][ridx]

        // pre filters (series, F1 then F2)
        let s = fx
        if (r1 === 0) s = this.filt(0, ch, s)
        if (r2 === 0) s = this.filt(1, ch, s)
        // clipper
        let y = this.clipOne(this.clip[ch], s)
        // post filters (series)
        if (r1 === 1) y = this.filt(0, ch, y)
        if (r2 === 1) y = this.filt(1, ch, y)
        // side filters (parallel, clean, aligned); Off = silent here
        if (r1 === 2 && this.fc[0].type !== 0) y += this.filt(0, ch, dFlat)
        if (r2 === 2 && this.fc[1].type !== 0) y += this.filt(1, ch, dFlat)
        // tone
        y = this.filt(2, ch, y)
        const o = (y * wet + dDry * dry) * this.outLin
        if (ch === 0) ol = o; else orr = o
      }

      // ---- limiter (stereo-linked, instant attack) ----
      if (this.limit) {
        const m = Math.max(Math.abs(ol), Math.abs(orr))
        this.lenv = m > this.lenv ? m : m + (this.lenv - m) * this.limRel
        if (this.lenv > LIM_CEIL) {
          const g = LIM_CEIL / this.lenv
          ol *= g; orr *= g
        }
      }
      outL[i] = ol
      if (outR !== outL) outR[i] = orr
    }
    return true
  }
}

registerProcessor('sf-destroyer', Destroyer)
