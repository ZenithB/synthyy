// Stutter — tempo-synced beat repeat. One AudioWorklet; canonical DSP for the
// native port crates/sf-dsp/src/fx/stutter.rs (verified to match — change
// both together).
//
// The input is recorded continuously into a ring. On every `interval`
// boundary of the transport a seeded die (`chance`) decides whether to
// stutter. A stutter lasts `gate` × interval and repeats a slice of `grid`
// length captured AT the trigger: the first pass is the live audio itself
// (zero latency), later passes replay it. Each repeat n can:
//   · shrink: slice × (1 − decay/2)^n (the classic build-up roll; ≥ 1/128)
//   · pitch:  play at 2^(pitch·n/12) (tape-style: duration follows pitch)
//   · reverse with probability `reverse` (from the 2nd pass on)
//   · fade:   gain × (1 − fade·0.3)^n
// Every repeat boundary and the stutter's start/end use 2 ms crossfades.
// Stopped transport ⇒ dry passthrough. Random choices come from a xorshift32
// reseeded on every transport start, so live playback and exports agree.

const PPQ = 96
const IVL_TICKS = [96, 192, 384, 768]              // 1/4, 1/2, 1 bar, 2 bars
const GRID_TICKS = [96, 48, 24, 12, 6, 32]         // 1/4 1/8 1/16 1/32 1/64 1/8T
const MIN_SLICE_TICKS = 3                          // 1/128
const SEED = 0x9e3779b9

class Stutter extends AudioWorkletProcessor {
  constructor(options) {
    super()
    this.p = { interval: 2, chance: 1, gate: 0.5, grid: 2, decay: 0, pitch: 0, reverse: 0, fade: 0, mix: 1, gain: 0 }
    this.sr = sampleRate
    this.len = Math.ceil(4 * sampleRate)
    this.bufL = new Float64Array(this.len)
    this.bufR = new Float64Array(this.len)
    this.w = 0
    this.fadeLen = Math.max(1, Math.round(0.002 * sampleRate))
    this.rng = SEED
    this.active = false
    this.env = 0               // 0 = dry … 1 = stutter, ramps over fadeLen
    this.left = 0              // samples of stutter remaining
    this.start = 0             // ring index where the slice begins
    this.rep = 0               // repeat index
    this.rp = 0                // read position within the repeat (slice samples)
    this.repLen = 0            // current repeat's slice length (samples)
    this.rate = 1
    this.rev = false
    this.repGain = 1
    this.baseSlice = 0
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

  rand() {
    let x = this.rng
    x ^= x << 13; x >>>= 0
    x ^= x >>> 17
    x ^= x << 5; x >>>= 0
    this.rng = x
    return x / 4294967296
  }

  transport(target, playing, bpm) {
    this.bpm = bpm > 0 ? bpm : 120
    if (!playing) { this.playing = false; this.active = false; return }
    if (!this.playing) { this.rng = SEED; this.active = false; this.env = 0 }
    if (!this.playing || Math.abs(target - this.pos) > 2) this.pos = target
    this.playing = true
  }

  /** Configure repeat n of the current stutter. */
  beginRepeat() {
    const p = this.p
    const n = this.rep
    const shrink = Math.pow(1 - Math.min(1, Math.max(0, p.decay)) * 0.5, n)
    const minLen = (MIN_SLICE_TICKS * 60 / (this.bpm * PPQ)) * this.sr
    this.repLen = Math.max(minLen, this.baseSlice * shrink)
    this.rate = Math.pow(2, (p.pitch * n) / 12)
    this.rev = n > 0 && this.rand() < p.reverse
    this.repGain = Math.pow(1 - Math.min(1, Math.max(0, p.fade)) * 0.3, n)
    this.rp = 0
  }

  /** Slice sample at fractional position q (linear interpolation). */
  readSlice(buf, q) {
    const len = this.len
    let pos = this.start + q
    while (pos >= len) pos -= len
    let i0 = Math.floor(pos)
    if (i0 >= len) i0 -= len
    const fr = pos - i0
    const i1 = i0 + 1 === len ? 0 : i0 + 1
    return buf[i0] + (buf[i1] - buf[i0]) * fr
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
    const p = this.p, sr = this.sr, len = this.len, fl = this.fadeLen
    const ivl = IVL_TICKS[Math.min(3, Math.max(0, p.interval | 0))]
    const grid = GRID_TICKS[Math.min(5, Math.max(0, p.grid | 0))]
    const mix = Math.min(1, Math.max(0, p.mix))
    const outLin = Math.pow(10, p.gain / 20)
    const tps = (this.bpm / 60) * PPQ / sr
    const spt = sr * 60 / (this.bpm * PPQ)           // samples per tick

    for (let i = 0; i < n; i++) {
      const xl = inL[i], xr = inR[i]
      this.bufL[this.w] = xl
      this.bufR[this.w] = xr

      // ---- trigger on interval boundaries ----
      if (this.playing) {
        const prev = this.pos
        this.pos += tps
        if (Math.floor(this.pos / ivl) !== Math.floor(prev / ivl) && this.rand() < p.chance) {
          this.active = true
          this.start = this.w
          this.left = Math.max(1, Math.round(Math.min(1, Math.max(0, p.gate)) * ivl * spt))
          this.baseSlice = grid * spt
          this.rep = 0
          this.beginRepeat()
        }
      }

      // ---- stutter voice ----
      // the voice keeps playing through the 2 ms release so ending a stutter
      // crossfades back to dry instead of dipping
      let sl = 0, sr2 = 0
      if (this.active || this.env > 0) {
        if (this.rp >= this.repLen) { this.rep++; this.beginRepeat() }
        const q = this.rev ? this.repLen - this.rp : this.rp
        const edge = Math.min(1, this.rp / fl, (this.repLen - this.rp) / fl)
        const g = this.repGain * (this.rep === 0 ? 1 : Math.max(0, edge))
        sl = this.readSlice(this.bufL, Math.max(0, Math.min(q, this.repLen))) * g
        sr2 = this.readSlice(this.bufR, Math.max(0, Math.min(q, this.repLen))) * g
        this.rp += this.rep === 0 ? 1 : this.rate
        if (this.active) {
          this.left--
          if (this.left <= 0) this.active = false
        }
      }
      const target = this.active ? 1 : 0
      this.env += target > this.env ? Math.min(1 / fl, target - this.env) : -Math.min(1 / fl, this.env - target)
      const e = this.env * mix
      this.w = this.w + 1 === len ? 0 : this.w + 1

      const ol = (sl * e + xl * (1 - e)) * outLin
      const orr = (sr2 * e + xr * (1 - e)) * outLin
      outL[i] = ol
      if (outR !== outL) outR[i] = orr
    }
    return true
  }
}

registerProcessor('sf-stutter', Stutter)
