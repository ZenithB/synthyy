import { useEffect, useReducer, useRef } from 'react'
import * as Y from 'yjs'

/** Edge auto-scroll velocity for a marquee drag along one axis (px/frame).
 *  `pos` is the pointer coordinate; [lo,hi] the visible viewport bounds. Returns
 *  0 while the pointer is well inside, then a signed velocity that ramps from a
 *  slow crawl at the activation margin up to a cap as the pointer pushes past
 *  the edge. Content-awareness (stopping at the scroll limits) is the caller's
 *  job — clamp the applied scroll so it never runs into empty space. */
export function edgeScrollVelocity(pos: number, lo: number, hi: number, margin = 30): number {
  const MIN = 1.5, MAX = 30, RAMP = margin * 4
  if (pos > hi - margin) return MIN + Math.min(1, (pos - (hi - margin)) / RAMP) * (MAX - MIN)
  if (pos < lo + margin) return -(MIN + Math.min(1, ((lo + margin) - pos) / RAMP) * (MAX - MIN))
  return 0
}

/** Re-render whenever a Y type (deeply) changes. */
export function useY(target: Y.AbstractType<any> | null | undefined) {
  const [, bump] = useReducer((x: number) => x + 1, 0)
  useEffect(() => {
    if (!target) return
    const h = () => bump()
    target.observeDeep(h)
    return () => target.unobserveDeep(h)
  }, [target])
}

/** Re-render only on DIRECT changes to a Y type (its own keys/entries — not
 *  nested content). Use for containers whose children subscribe themselves:
 *  the session grid observes `clips` shallowly so a note edit inside one clip
 *  doesn't re-render every slot. */
export function useYShallow(target: Y.AbstractType<any> | null | undefined) {
  const [, bump] = useReducer((x: number) => x + 1, 0)
  useEffect(() => {
    if (!target) return
    const h = () => bump()
    target.observe(h)
    return () => target.unobserve(h)
  }, [target])
}

// ---- shared frame ticker ----
// One requestAnimationFrame chain drives every per-frame subscriber (meters,
// playheads, scopes, the piano-roll painter…). Dozens of independent rAF loops
// each pay scheduling overhead every frame; a single pump runs them all in one
// callback and stops entirely when nothing is subscribed.
type FrameCb = () => void
const frameCbs = new Set<FrameCb>()
let framePump = 0

function pump() {
  framePump = requestAnimationFrame(pump)
  frameCbs.forEach(cb => {
    try { cb() } catch { /* one bad subscriber must not starve the rest */ }
  })
}

/** Subscribe a callback to the shared frame ticker. Returns an unsubscribe. */
export function subscribeFrame(cb: FrameCb): () => void {
  frameCbs.add(cb)
  if (frameCbs.size === 1) framePump = requestAnimationFrame(pump)
  return () => {
    frameCbs.delete(cb)
    if (frameCbs.size === 0) { cancelAnimationFrame(framePump); framePump = 0 }
  }
}

/** Run a callback every animation frame while `active`. `every` runs it on
 *  every Nth frame only — meters are smoothed and read fine at 30Hz. */
export function useRaf(cb: () => void, active = true, every = 1) {
  const ref = useRef(cb)
  ref.current = cb
  useEffect(() => {
    if (!active) return
    let n = 0
    return subscribeFrame(() => {
      if (every > 1 && (n = (n + 1) % every) !== 0) return
      ref.current()
    })
  }, [active, every])
}

/** Latest-value ref helper for stable event handlers. */
export function useLatest<T>(v: T) {
  const r = useRef(v)
  r.current = v
  return r
}
