// Audio render-thread HEALTH PROBE. Runs on the audio thread every render
// quantum and reports how well that thread is keeping up with real time — the
// signal that actually reveals crackles/dropouts, on EVERY browser (unlike
// Chromium's renderCapacity). Produces no sound; a 0-gain sink keeps it pulled
// by the graph. Registered by src/audio/engine.ts (setupLoadProbe).
//
// Why "keep-up": the output device drains the buffer at exactly `sampleRate`.
// If the thread can't refill that fast (too many voices/effects, oversampling,
// a blocked callback) the buffer underruns — the audible crackle — and over a
// window we produce LESS than real-time worth of audio. So:
//     keepUp = audioProduced ÷ wallElapsed         (1.0 = perfect, <1 = glitching)
// Crucially the wall clock is read with performance.now() ON the audio thread,
// so a janky MAIN thread can't pollute the number (that's the whole point — our
// old FPS metric measured the wrong thread). If performance.now() isn't exposed
// to the worklet scope (rare/old browsers) we fall back to counting quanta and
// let the engine time them against its own clock.
class SFLoad extends AudioWorkletProcessor {
  constructor(options) {
    super()
    const o = (options && options.processorOptions) || {}
    this.reportMs = o.reportMs || 250
    this.haveClock = typeof performance !== 'undefined' && typeof performance.now === 'function'
    this.frames = 0
    this.lastT = -1
    this.maxGap = 0
    this.winT = this.haveClock ? performance.now() : 0
  }

  process(inputs, outputs) {
    // count FRAMES, not callbacks — the render quantum is 128 by default but
    // renderSizeHint (Stable buffer tier) makes it bigger, and hard-coding 128
    // would then over/under-report how much audio was actually produced.
    const out = outputs && outputs[0] && outputs[0][0]
    this.frames += out ? out.length : 128
    if (this.haveClock) {
      const t = performance.now()
      if (this.lastT >= 0) {
        const gap = t - this.lastT
        if (gap > this.maxGap) this.maxGap = gap
      }
      this.lastT = t
      const elapsed = t - this.winT
      if (elapsed >= this.reportMs) {
        this.port.postMessage({ clock: 1, frames: this.frames, winMs: elapsed, maxGapMs: this.maxGap })
        this.frames = 0
        this.maxGap = 0
        this.winT = t
      }
    } else {
      // No wall clock on this thread — emit a frame count the engine can time
      // against its own clock (coarser; couples slightly to main-thread jank).
      if (this.frames >= 16384) {
        this.port.postMessage({ clock: 0, frames: this.frames })
        this.frames = 0
      }
    }
    return true
  }
}

registerProcessor('sf-load', SFLoad)
