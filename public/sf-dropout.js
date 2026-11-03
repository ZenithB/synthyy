// Dropout — the sound of audio falling apart over a bad stream or call. One
// AudioWorklet; canonical DSP for the native port
// crates/sf-dsp/src/fx/dropout.rs (verified to match — change both together).
//
// Real codecs (AAC/Opus-style) code audio in overlapping MDCT frames. This
// does the same — 512-sample sine-windowed frames, hop M = 256, perfect
// reconstruction when nothing is degraded — and degrades the spectrum the
// way a starved codec does:
//   Bitrate    per-band quantiser (16 log bands, step = band RMS × (24/kbps)^1.5):
//              quiet bins round to zero → sparse "birdie" artefacts; each band
//              is then rescaled to its original energy (as CELT/Opus do), so
//              low bitrates change the texture, not the level
//   Warble     random dither on that rounding, so WHICH bins survive changes
//              every frame → the swirly underwater sound
//   Bandwidth  bins above the cut-off are dropped
//   Loss/Burst packets of 4 frames (~21 ms) are lost with a two-state
//              (Gilbert–Elliott) model: Burst raises the chance a loss follows
//              a loss
//   Conceal    what fills a lost packet: Silence, Repeat (last good frame,
//              decaying — the classic stuck-buffer stutter) or Noise (last
//              good magnitudes, random signs — robotic)
// Latency 2M = 512 samples; the dry path is delayed to match so Mix is
// phase-aligned. Random choices: xorshift32 from a fixed seed.

const M = 256
const N = 2 * M
const BANDS = 16
const PACKET = 4
const SEED = 0x2545f491

class Dropout extends AudioWorkletProcessor {
  constructor(options) {
    super()
    this.p = { bitrate: 48, warble: 0.3, bw: 16000, loss: 0, burst: 0.3, conceal: 1, mix: 1, gain: 0 }
    // sine window + cosine table T[k*N + n] = cos(π/M (n + ½ + M/2)(k + ½))
    this.win = new Float64Array(N)
    for (let n = 0; n < N; n++) this.win[n] = Math.sin((Math.PI * (n + 0.5)) / N)
    this.T = new Float64Array(M * N)
    for (let k = 0; k < M; k++) for (let n = 0; n < N; n++) {
      this.T[k * N + n] = Math.cos((Math.PI / M) * (n + 0.5 + M / 2) * (k + 0.5))
    }
    // band edges (bins), log-spaced
    this.edges = new Int32Array(BANDS + 1)
    for (let b = 0; b <= BANDS; b++) this.edges[b] = Math.floor((M * (Math.pow(2, (8 * b) / BANDS) - 1)) / 255)
    this.prevIn = [new Float64Array(M), new Float64Array(M)]
    this.newIn = [new Float64Array(M), new Float64Array(M)]
    this.outQ = [new Float64Array(M), new Float64Array(M)]
    this.tail = [new Float64Array(M), new Float64Array(M)]
    this.last = [new Float64Array(M), new Float64Array(M)]   // last good spectrum
    this.X = new Float64Array(M)
    this.wb = new Float64Array(N)
    this.y = new Float64Array(N)
    this.k = 0
    this.frame = 0
    this.lost = false
    this.lostRun = 0
    this.rng = SEED
    // dry alignment delay (2M)
    this.dry = [new Float64Array(N), new Float64Array(N)]
    this.dw = 0
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

  /** One hop: MDCT both channels, degrade, IMDCT, overlap-add into outQ. */
  runFrame() {
    const p = this.p
    const bitrate = Math.min(320, Math.max(8, p.bitrate))
    // quantiser step relative to band RMS: ≈ −48 dB error at 320 kbps,
    // ≈ −20 dB at 48, heavily broken at 8
    const qf = Math.pow(24 / bitrate, 1.5)
    const warble = Math.min(1, Math.max(0, p.warble))
    const cutBin = Math.min(M, Math.max(0, Math.floor((Math.min(20000, Math.max(1000, p.bw)) * N) / sampleRate - 0.5) + 1))
    const loss = Math.min(1, Math.max(0, p.loss))
    const burst = Math.min(1, Math.max(0, p.burst))
    const conceal = Math.min(2, Math.max(0, p.conceal | 0))

    if (this.frame % PACKET === 0) {
      const pl = this.lost ? loss + burst * (1 - loss) : loss
      this.lost = this.rand() < pl
    }
    this.frame++
    if (this.lost) this.lostRun++
    else this.lostRun = 0
    const decay = Math.pow(0.85, this.lostRun)

    for (let ch = 0; ch < 2; ch++) {
      const prev = this.prevIn[ch], cur = this.newIn[ch]
      const wb = this.wb, X = this.X, T = this.T, win = this.win
      for (let n = 0; n < M; n++) { wb[n] = win[n] * prev[n]; wb[M + n] = win[M + n] * cur[n] }
      for (let k = 0; k < M; k++) {
        let acc = 0
        const o = k * N
        for (let n = 0; n < N; n++) acc += wb[n] * T[o + n]
        X[k] = acc
      }
      const last = this.last[ch]
      if (!this.lost) {
        // ---- bandwidth + per-band quantisation ----
        for (let b = 0; b < BANDS; b++) {
          const lo = this.edges[b], hi = this.edges[b + 1]
          if (hi <= lo) continue
          // bins above the cut-off are dropped first, so the band's energy
          // (and the rescale below) covers only what is actually kept
          const top = Math.min(hi, cutBin)
          let e = 0
          for (let k = lo; k < top; k++) e += X[k] * X[k]
          for (let k = top > lo ? top : lo; k < hi; k++) X[k] = 0
          if (top <= lo) continue
          const step = Math.sqrt(e / (top - lo)) * qf
          let eq = 0
          for (let k = lo; k < top; k++) {
            if (step <= 0) { X[k] = 0; continue }
            let u = X[k] / step
            if (warble > 0) u += (this.rand() - 0.5) * warble
            X[k] = Math.floor(u + 0.5) * step
            eq += X[k] * X[k]
          }
          // keep each band's energy (as CELT/Opus do): coarse rounding then
          // changes the TEXTURE, not the level
          if (eq > 0) {
            const g = Math.sqrt(e / eq)
            for (let k = lo; k < top; k++) X[k] *= g
          }
        }
        for (let k = this.edges[BANDS]; k < M; k++) X[k] = 0
        for (let k = 0; k < M; k++) last[k] = X[k]
      } else if (conceal === 0) {
        for (let k = 0; k < M; k++) X[k] = 0
      } else if (conceal === 1) {
        for (let k = 0; k < M; k++) X[k] = last[k] * decay
      } else {
        for (let k = 0; k < M; k++) {
          const m = last[k] < 0 ? -last[k] : last[k]
          X[k] = (this.rand() < 0.5 ? -m : m) * decay
        }
      }
      // ---- IMDCT + overlap-add ----
      const y = this.y
      for (let n = 0; n < N; n++) {
        let acc = 0
        for (let k = 0; k < M; k++) acc += X[k] * T[k * N + n]
        y[n] = win[n] * (2 / M) * acc
      }
      const q = this.outQ[ch], tail = this.tail[ch]
      for (let n = 0; n < M; n++) { q[n] = tail[n] + y[n]; tail[n] = y[M + n] }
      prev.set(cur)
    }
  }

  process(inputs, outputs) {
    const input = inputs[0], output = outputs[0]
    if (!output) return true
    const outL = output[0], outR = output[1] || output[0]
    const n = outL.length
    if (!input || input.length === 0) { outL.fill(0); if (outR !== outL) outR.fill(0); return true }
    const inL = input[0], inR = input[1] || input[0]
    const p = this.p
    const mix = Math.min(1, Math.max(0, p.mix))
    const outLin = Math.pow(10, p.gain / 20)
    for (let i = 0; i < n; i++) {
      const k = this.k
      const xl = inL[i], xr = inR[i]
      // dry, delayed by 2M to line up with the codec path
      const dl = this.dry[0][this.dw], dr = this.dry[1][this.dw]
      this.dry[0][this.dw] = xl; this.dry[1][this.dw] = xr
      this.dw = this.dw + 1 === N ? 0 : this.dw + 1
      const wl = this.outQ[0][k], wr = this.outQ[1][k]
      this.newIn[0][k] = xl; this.newIn[1][k] = xr
      if (k + 1 === M) { this.runFrame(); this.k = 0 } else this.k = k + 1
      const ol = (wl * mix + dl * (1 - mix)) * outLin
      const orr = (wr * mix + dr * (1 - mix)) * outLin
      outL[i] = ol
      if (outR !== outL) outR[i] = orr
    }
    return true
  }
}

registerProcessor('sf-dropout', Dropout)
