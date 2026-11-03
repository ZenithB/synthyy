// Advanced gate / downward expander — one AudioWorklet so the whole device is
// a single in=out node (Web Audio has no gate primitive, and the dual-threshold
// state machine below needs per-sample state that no graph of native nodes can
// express).
//
// Chain, per sample:
//   key select → key filter (HP→LP) → detector (peak | RMS) → dB
//   → hysteresis state machine (separate open / close thresholds)
//   → expander curve (ratio, soft knee, clamped by range)
//   → attack / hold / release ramp (linear in dB)
//   → applied to the LOOKAHEAD-delayed audio → mix → output gain
//
// The two thresholds are what make this more than a gate. Open fires when the
// key rises past `thresh + up`, close when it falls below `thresh + dn`:
//   dn < up   classic hysteresis — the window between them is the chatter guard
//   dn == up  a bare comparator (and, with up = dn = 0, a plain expander)
//   dn > up   the thresholds CROSS: the gate now closes on the way DOWN at a
//             level it would not yet have opened at on the way UP, so it clips
//             the sustain of every note and re-opens on the next transient —
//             a percussive chop rather than a noise gate
// `inverse` mirrors the key level about `thresh` (x = −(lvl − thresh)), so the
// same offsets keep their meaning and the gate passes what is BELOW it.
//
// This file is the canonical DSP: crates/sf-dsp/src/fx/gate.rs is a port of it,
// and any change here must land there too (CLAUDE.md: render-path parity).

const LOG2DB = 8.685889638065035 // 20/ln(10)
const DB_FLOOR = -160            // dB floor for log of near-silence
const MIN_LIN = 1e-8             // -160 dBFS
const RMS_WIN_SEC = 0.01         // RMS detector integration window
const PEAK_REL_SEC = 0.005       // peak detector release (instant attack)
const MAX_LOOK_MS = 10           // schema `look` max — sizes the delay line

const dbOf = v => (v > MIN_LIN ? Math.log(v) * LOG2DB : DB_FLOOR)

// One-pole coefficient for a time constant in seconds (0 time ⇒ instant).
const poleOf = (sec, sr) => (sec > 0 ? Math.exp(-1 / (sec * sr)) : 0)

// Direct-Form-I biquad (one per channel per filter slot) — same as sf-mbdyn.
class Biquad {
  constructor() { this.x1 = 0; this.x2 = 0; this.y1 = 0; this.y2 = 0; this.c = [1, 0, 0, 0, 0] }
  set(c) { this.c = c }
  reset() { this.x1 = this.x2 = this.y1 = this.y2 = 0 }
  proc(x) {
    const c = this.c
    const y = c[0] * x + c[1] * this.x1 + c[2] * this.x2 - c[3] * this.y1 - c[4] * this.y2
    this.x2 = this.x1; this.x1 = x; this.y2 = this.y1; this.y1 = y
    return y
  }
}

// RBJ cookbook, Butterworth Q, normalized by a0.
function lpCoeffs(f, sr) {
  const w0 = (2 * Math.PI * Math.min(f, sr * 0.49)) / sr
  const cw = Math.cos(w0), alpha = Math.sin(w0) / (2 * Math.SQRT1_2)
  const a0 = 1 + alpha
  return [((1 - cw) / 2) / a0, (1 - cw) / a0, ((1 - cw) / 2) / a0, (-2 * cw) / a0, (1 - alpha) / a0]
}
function hpCoeffs(f, sr) {
  const w0 = (2 * Math.PI * Math.min(f, sr * 0.49)) / sr
  const cw = Math.cos(w0), alpha = Math.sin(w0) / (2 * Math.SQRT1_2)
  const a0 = 1 + alpha
  return [((1 + cw) / 2) / a0, (-(1 + cw)) / a0, ((1 + cw) / 2) / a0, (-2 * cw) / a0, (1 - alpha) / a0]
}

class Gate extends AudioWorkletProcessor {
  constructor(options) {
    super()
    // Schema defaults (schema.ts "gate") — overwritten by the init message.
    this.p = {
      thresh: -40, up: 0, dn: -6, range: -60, ratio: 20, knee: 3,
      attack: 0.002, hold: 0.05, release: 0.15, look: 0,
      hpf: 20, lpf: 20000, key: 0, det: 0, listen: 0, inverse: 0,
      mix: 1, gain: 0,
    }
    // Two key channels (Stereo keys off both; the mono modes use [0] only).
    this.hp = [new Biquad(), new Biquad()]
    this.lp = [new Biquad(), new Biquad()]
    this.peakEnv = [0, 0]
    this.msEnv = [0, 0]     // mean-square accumulator for RMS detection
    this.open = false
    this.holdLeft = 0       // samples of hold remaining
    this.gainDb = 0         // current ramp position, dB (≤ 0)
    this.grMax = 0          // peak reduction since the last UI report

    const maxDelay = Math.ceil((MAX_LOOK_MS / 1000) * sampleRate) + 2
    this.dl = new Float32Array(maxDelay)
    this.dr = new Float32Array(maxDelay)
    this.dlen = maxDelay
    this.widx = 0

    this.rep = 0
    this.repEvery = Math.max(256, Math.floor(sampleRate * 0.03))
    // Initial params arrive through processorOptions, NOT the port: a port
    // message is delivered on the control-message queue, and an
    // OfflineAudioContext can render the whole bounce before that queue is
    // drained — the device then renders with its DEFAULTS. processorOptions is
    // structured-cloned into this constructor at node creation, so it is always
    // in place for sample 0. The port stays for live param changes.
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

  recompute() {
    this.dirty = false
    const p = this.p, sr = sampleRate
    // Keep the key band ordered — a crossed HP/LP would key off silence.
    const hpF = Math.min(p.hpf, p.lpf * 0.98)
    const lpF = Math.max(p.lpf, p.hpf * 1.02)
    const hc = hpCoeffs(hpF, sr), lc = lpCoeffs(lpF, sr)
    for (let c = 0; c < 2; c++) { this.hp[c].set(hc); this.lp[c].set(lc) }

    this.peakRel = poleOf(PEAK_REL_SEC, sr)
    this.rmsPole = poleOf(RMS_WIN_SEC, sr)
    // Ramps are linear in dB and span the full `range`, so "attack" is literally
    // the time for the gate to open from fully closed (and release to close).
    const span = Math.max(6, Math.abs(p.range))
    this.attStep = span / (Math.max(0.0001, p.attack) * sr)
    this.relStep = span / (Math.max(0.0005, p.release) * sr)
    this.holdSamples = Math.round(Math.max(0, p.hold) * sr)
    this.delay = Math.min(this.dlen - 1, Math.round((p.look / 1000) * sr))
    this.mixWet = Math.min(1, Math.max(0, p.mix))
    this.outLin = Math.pow(10, p.gain / 20)
    this.rangeDb = Math.min(0, p.range)
    this.ratio = Math.max(1, p.ratio)
    this.infinite = this.ratio >= 19.95
    this.knee = Math.max(0, p.knee)
    this.sign = p.inverse >= 0.5 ? -1 : 1
  }

  /** Reduction (dB ≥ 0) the expander asks for at `x` dB past the threshold. */
  curveRed(x) {
    if (this.infinite) return -this.rangeDb
    const k = this.knee, slope = this.ratio - 1
    let red
    if (x >= k * 0.5) red = 0
    else if (x <= -k * 0.5) red = slope * -x
    else { const d = x - k * 0.5; red = (slope * d * d) / (2 * k) }
    return Math.min(red, -this.rangeDb)
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
    const p = this.p, sr = sampleRate
    const mode = p.key | 0, rms = p.det >= 0.5, listen = p.listen >= 0.5
    const stereoKey = mode === 0
    const openAt = p.thresh + p.up
    const closeAt = p.thresh + p.dn
    const dry = 1 - this.mixWet

    for (let i = 0; i < n; i++) {
      const xl = inL[i], xr = inR[i]

      // ---- key signal (pre-filter) ----
      let k0, k1 = 0
      if (stereoKey) { k0 = xl; k1 = xr }
      else if (mode === 1) k0 = xl
      else if (mode === 2) k0 = xr
      else if (mode === 3) k0 = (xl + xr) * 0.5
      else k0 = (xl - xr) * 0.5

      // ---- key filter: HP then LP ----
      k0 = this.lp[0].proc(this.hp[0].proc(k0))
      if (stereoKey) k1 = this.lp[1].proc(this.hp[1].proc(k1))

      // ---- detector ----
      let lvl
      if (rms) {
        this.msEnv[0] += (k0 * k0 - this.msEnv[0]) * (1 - this.rmsPole)
        let ms = this.msEnv[0]
        if (stereoKey) {
          this.msEnv[1] += (k1 * k1 - this.msEnv[1]) * (1 - this.rmsPole)
          if (this.msEnv[1] > ms) ms = this.msEnv[1]
        }
        lvl = Math.sqrt(Math.max(0, ms))
      } else {
        const a0 = k0 < 0 ? -k0 : k0
        this.peakEnv[0] = a0 > this.peakEnv[0] ? a0 : a0 + (this.peakEnv[0] - a0) * this.peakRel
        lvl = this.peakEnv[0]
        if (stereoKey) {
          const a1 = k1 < 0 ? -k1 : k1
          this.peakEnv[1] = a1 > this.peakEnv[1] ? a1 : a1 + (this.peakEnv[1] - a1) * this.peakRel
          if (this.peakEnv[1] > lvl) lvl = this.peakEnv[1]
        }
      }
      const lvlDb = dbOf(lvl)

      // ---- hysteresis state machine ----
      // `sign` mirrors the comparison for Inverse, so up/dn keep their meaning.
      const sg = this.sign
      if (this.open) {
        if (sg * lvlDb < sg * closeAt) {
          // Hold keeps the gate wide open for its full time before releasing,
          // so short dips inside a note don't chop it up.
          if (this.holdLeft > 0) this.holdLeft--
          else this.open = false
        } else {
          this.holdLeft = this.holdSamples
        }
      } else if (sg * lvlDb > sg * openAt) {
        this.open = true
        this.holdLeft = this.holdSamples
      }

      // ---- target gain ----
      // Open ⇒ unity. Closed ⇒ the expander curve at the key's distance past
      // the threshold, so a low ratio expands gently and 20:1 ("Gate") slams
      // straight to `range`.
      const targetDb = this.open ? 0 : -this.curveRed(sg * (lvlDb - p.thresh))

      // ---- attack / release ramp (linear in dB) ----
      if (targetDb > this.gainDb) this.gainDb = Math.min(targetDb, this.gainDb + this.attStep)
      else if (targetDb < this.gainDb) this.gainDb = Math.max(targetDb, this.gainDb - this.relStep)
      const g = Math.pow(10, this.gainDb * 0.05)
      if (-this.gainDb > this.grMax) this.grMax = -this.gainDb

      // ---- lookahead delay (audio only; the detector ran early) ----
      this.dl[this.widx] = xl
      this.dr[this.widx] = xr
      let ridx = this.widx - this.delay
      if (ridx < 0) ridx += this.dlen
      const dL = this.dl[ridx], dR = this.dr[ridx]
      this.widx = this.widx + 1 === this.dlen ? 0 : this.widx + 1

      if (listen) {
        // Audition what the detector hears — key band, ungated, so the HP/LP
        // can be dialled by ear. Ignores mix on purpose.
        const m = stereoKey ? (k0 + k1) * 0.5 : k0
        outL[i] = m * this.outLin
        if (outR !== outL) outR[i] = m * this.outLin
      } else {
        outL[i] = (dL * g * this.mixWet + dL * dry) * this.outLin
        if (outR !== outL) outR[i] = (dR * g * this.mixWet + dR * dry) * this.outLin
      }
    }

    this.rep += n
    if (this.rep >= this.repEvery) {
      this.rep = 0
      this.port.postMessage({ gr: this.grMax, open: this.open })
      this.grMax = 0
    }
    return true
  }
}

registerProcessor('sf-gate', Gate)
