// Canvas piano-roll editor. Works on session AND arrangement clips (same data
// shape). Draw mode (B), marquee select, drag/resize/copy, velocity + chance
// lanes, scale highlighting & snap, fold-to-scale, and a Tools menu with
// chordify / arp / strum / humanize / quantize / legato / reverse / double.

import React, { useEffect, useRef, useState } from 'react'
import * as Y from 'yjs'
import { BAR, GRID_OPTIONS, DRUM_PADS, STEP16, Note, clamp, CLIP_COLORS } from '../types'
import {
  getClipMap, notesOf, addNote, addNotes, updateNotes, deleteNotes, setClipField, meta, trackById,
  envPoints, setEnvPoints, followOf, setFollow, barTicks, beatTicks,
} from '../state/doc'
import { setUI, ui, useUI, toast } from '../state/store'
import { useY, subscribeFrame, edgeScrollVelocity } from './hooks'
import { openMenu, ColorRow, beginVDrag } from './widgets'
import { inScale, snapToScale, midiName, getScale } from '../theory'
import { instSchema, fxSchema, mixSpec, FOLLOW_ACTIONS } from '../audio/schema'
import * as tools from '../noteTools'
import { engine } from '../audio/engine'
import { clips, clipKey } from '../state/doc'
import { Icon } from './icons'

// Automation targets available for a track (continuous params only).
import { autoTargets } from '../audio/params'

const KEY_W = 48
const LANE_H = 64
const MAX_P = 108
const MIN_P = 21

// Note clipboard, shared across clip switches (copy here, paste there).
let noteClip: Note[] = []

// 16-step drum grid — the fastest way to build a beat. Writes to clip notes.
// Playback position [0,1) within the selected clip, or null when it isn't the
// clip currently sounding. Mirrors the canvas piano-roll's playhead logic so
// the two views agree frame-for-frame.
function clipPlayFraction(selClip: any, clipMap: Y.Map<any>): number | null {
  if (!engine.playing || !selClip) return null
  const len = clipMap.get('len') ?? BAR
  if (selClip.kind === 'session' && engine.mode === 'session') {
    const st = engine.clipState(selClip.trackId, selClip.sceneId)
    if (st.playing) { const prog = engine.clipProgress(selClip.trackId); if (prog !== null) return prog }
  } else if (selClip.kind === 'arr' && engine.mode === 'arr') {
    const start = clipMap.get('start') ?? 0
    const t = engine.positionTicks()
    if (t >= start && t < start + len) return (((t - start) % len) + len) % len / len
  }
  return null
}

function StepGrid({ clipMap, trackId, selClip }: { clipMap: Y.Map<any>; trackId: string | null; selClip: any }) {
  const len = clipMap.get('len') ?? BAR
  // Show the WHOLE loop — up to 16 bars (256 steps in 4/4; hard cap keeps odd
  // meters from exploding the DOM). Long grids scroll horizontally; the pad
  // names stay pinned (sticky) so you always know which row you're editing.
  const nSteps = Math.min(512, Math.max(4, Math.round(len / STEP16)))
  // beat/bar shading follows the meter (16th-note steps per beat / per bar)
  const stepsPerBeat = Math.max(1, Math.round(beatTicks() / STEP16))
  const stepsPerBar = Math.max(1, Math.round(barTicks() / STEP16))
  const notes = notesOf(clipMap)
  const at = (pad: number, step: number) =>
    notes.find(([, n]) => n.p === pad && Math.round(n.s / STEP16) === step)
  const toggle = (pad: number, step: number) => {
    const hit = at(pad, step)
    if (hit) deleteNotes(clipMap, [hit[0]], 'Clear step')
    else { addNote(clipMap, { p: pad, s: step * STEP16, d: STEP16, v: ui.velo, pr: ui.drawProb }, 'Add step'); engine.previewOn(trackId, pad, ui.velo) }
  }

  // Sweeping playhead: a vertical line moved each frame across the cells region,
  // aligned to the first row's .step-cells box (which spans all rows).
  const gridRef = useRef<HTMLDivElement>(null)
  const phRef = useRef<HTMLDivElement>(null)
  useEffect(() => {
    const draw = () => {
      const line = phRef.current, grid = gridRef.current
      if (!line || !grid) return
      const frac = clipPlayFraction(selClip, clipMap)
      if (frac === null) { if (line.style.opacity !== '0') line.style.opacity = '0'; return }
      const cells = grid.querySelector('.step-cells') as HTMLElement | null
      if (!cells) return
      const gRect = grid.getBoundingClientRect(), cRect = cells.getBoundingClientRect()
      const x = (cRect.left - gRect.left) + grid.scrollLeft + frac * cRect.width
      line.style.transform = `translateX(${x}px)`
      line.style.opacity = '1'
    }
    return subscribeFrame(draw)
  }, [selClip, clipMap])

  return (
    <div className="step-grid" ref={gridRef}>
      <div className="step-playhead" ref={phRef} style={{ opacity: 0 }} />
      {DRUM_PADS.map((name, pad) => (
        <div key={pad} className="step-row">
          <button className="step-pad" onClick={() => engine.previewOn(trackId, pad, 0.9)}>{name}</button>
          <div className="step-cells">
            {Array.from({ length: nSteps }, (_, step) => {
              const hit = at(pad, step)
              return (
                <button key={step}
                  className={`step-cell ${hit ? 'on' : ''} ${step % stepsPerBeat === 0 ? 'beat' : ''} ${step % stepsPerBar === 0 ? 'bar' : ''}`}
                  style={hit ? { opacity: 0.4 + 0.6 * hit[1].v } : undefined}
                  onClick={() => toggle(pad, step)} />
              )
            })}
          </div>
        </div>
      ))}
    </div>
  )
}

type Drag =
  | { type: 'move'; ids: string[]; orig: Map<string, Note>; startTick: number; startPitch: number; dTick: number; dPitch: number; copy: boolean; moved: boolean }
  | { type: 'resize'; ids: string[]; orig: Map<string, Note>; startTick: number; dTick: number }
  | { type: 'marquee'; x0: number; y0: number; x1: number; y1: number; add: boolean }
  | { type: 'paint'; lastCell: string }
  | { type: 'lane'; }
  | null

export function PianoRoll() {
  const selClip = useUI(s => s.selClip)
  const gridTicks = useUI(s => s.gridTicks)
  const drawLen = useUI(s => s.drawLen)
  const drawMode = useUI(s => s.drawMode)
  const snapScale = useUI(s => s.snapScale)
  const notePreview = useUI(s => s.notePreview)
  const lane = useUI(s => s.lane)
  const theme = useUI(s => s.theme)
  const [fold, setFold] = useState(false)
  const [autoTarget, setAutoTarget] = useState<string | null>(null)
  const [stepView, setStepView] = useState(true)
  const [, force] = useState(0)
  const bump = () => force(x => x + 1)

  const clipMap = getClipMap(selClip)
  useY(clipMap ?? undefined)
  useY(meta)

  const canvasRef = useRef<HTMLCanvasElement>(null)
  const boxRef = useRef<HTMLDivElement>(null)
  // pxPerBar = horizontal zoom, rowZoom = vertical zoom (both driven by dragging
  // the key/pad column). Read live by the draw loop, so zooming needs no React
  // re-render mid-drag — just needsDraw.
  const view = useRef({ scrollX: 0, scrollY: 0, pxPerBar: 150, rowZoom: 1 })
  const sel = useRef<Set<string>>(new Set())
  const dragRef = useRef<Drag>(null)
  const hoverRef = useRef<{ tick: number; pitch: number } | null>(null)
  const cursorRef = useRef('default')
  const autoPaint = useRef<Map<number, number> | null>(null)
  const autoMode = !!autoTarget

  // Audition helper for the "preview on select/move" toggle: gated by ui.notePreview
  // so the button truly silences it. Used for click-select, vertical drag, and
  // arrow-key transpose — never for drawing new notes or key-column clicks,
  // which already have their own always-on preview.
  const previewNote = (p: number, vel: number, dur = 160) => {
    if (!ui.notePreview) return
    engine.previewOn(trackId, p, vel)
    setTimeout(() => engine.previewOff(trackId, p), dur)
  }
  // Tracks the anchor note's original pitch/velocity for the duration of a move
  // drag, and the last dPitch we previewed at, so a vertical drag re-triggers
  // the note only when it actually crosses into a new pitch — not every pixel.
  const dragPreviewAnchor = useRef<{ p: number; v: number } | null>(null)
  const lastPreviewDPitch = useRef(0)

  // Dirty flag for the canvas painter: the rAF loop only repaints when
  // something could have changed. Every React render marks it (useY re-renders
  // on clip edits, bump() on selection changes); scroll/zoom/pointer handlers
  // and a ResizeObserver mark it directly since they bypass React.
  const needsDraw = useRef(true)
  needsDraw.current = true

  // reset selection when switching clips
  const clipIdRef = useRef<string | null>(null)
  const clipId = clipMap?.get('id') ?? null
  if (clipId !== clipIdRef.current) {
    clipIdRef.current = clipId
    sel.current = new Set()
    dragRef.current = null
    autoPaint.current = null
  }

  const trackId: string | null = selClip
    ? (selClip.kind === 'session' ? selClip.trackId : clipMap?.get('trackId') ?? null)
    : null
  const isDrum = trackId ? trackById(trackId)?.get('kind') === 'drum' : false
  const trackColor = trackId ? CLIP_COLORS[trackById(trackId)?.get('color') ?? 0] : '#888'
  const root = meta.get('root') ?? 9
  const scaleId = meta.get('scale') ?? 'minor'
  const targetsList = autoTargets(trackId)
  const autoTargetLabel = autoTarget ? (targetsList.find(t => t.key === autoTarget)?.label ?? '') : ''

  // ---- visible pitch rows (fold support) ----
  // rowZoom scales row height (vertical zoom). Read from the ref so it tracks
  // live zoom drags; a bump() after the drag re-renders so interaction handlers
  // pick up the new height.
  const rowH = Math.max(4, Math.round((isDrum ? 30 : 13) * view.current.rowZoom))
  const pitches: number[] = []
  if (isDrum) {
    for (let p = DRUM_PADS.length - 1; p >= 0; p--) pitches.push(p)
  } else if (fold) {
    for (let p = MAX_P; p >= MIN_P; p--) if (inScale(p, root, scaleId)) pitches.push(p)
  } else {
    for (let p = MAX_P; p >= MIN_P; p--) pitches.push(p)
  }
  const rowOf = (p: number) => pitches.indexOf(p)
  const pitchAtRow = (r: number) => pitches[clamp(r, 0, pitches.length - 1)]

  // Edge auto-scroll while marquee-selecting: pan the view when the selection
  // box's moving end nears a grid edge (speed ramps with overshoot), clamped to
  // the notes/pitch content so it never scrolls into empty space. The anchor
  // (x0/y0, canvas px) is pinned to its content point as we scroll, so the box
  // resolves to the right notes on release.
  const marqueeRaf = useRef<number | null>(null)
  const stopMarqueeScroll = () => { if (marqueeRaf.current != null) { cancelAnimationFrame(marqueeRaf.current); marqueeRaf.current = null } }
  const runMarqueeScroll = () => {
    const d = dragRef.current, box = boxRef.current
    if (d && d.type === 'marquee' && box && clipMap) {
      const W = box.clientWidth, gridH = box.clientHeight - LANE_H
      const rh = Math.max(4, Math.round((isDrum ? 30 : 13) * view.current.rowZoom))
      const pxPerTick = view.current.pxPerBar / BAR
      const vx = edgeScrollVelocity(d.x1, KEY_W, W)
      const vy = edgeScrollVelocity(d.y1, 0, gridH)
      if (vx || vy) {
        let contentTicks = clipMap.get('len') ?? BAR
        for (const [, n] of notesOf(clipMap)) contentTicks = Math.max(contentTicks, n.s + n.d)
        const maxSX = Math.max(0, contentTicks * pxPerTick - (W - KEY_W))
        const maxSY = Math.max(0, pitches.length * rh - gridH)
        if (vx) { const ns = clamp(view.current.scrollX + vx, 0, maxSX); d.x0 -= ns - view.current.scrollX; view.current.scrollX = ns }
        if (vy) { const ns = clamp(view.current.scrollY + vy, 0, maxSY); d.y0 -= ns - view.current.scrollY; view.current.scrollY = ns }
        needsDraw.current = true
      }
    }
    marqueeRaf.current = requestAnimationFrame(runMarqueeScroll)
  }
  useEffect(() => stopMarqueeScroll, [])

  // first-open: center on content or C4
  const centeredRef = useRef<string | null>(null)
  useEffect(() => {
    if (!clipMap || centeredRef.current === clipId || isDrum) return
    centeredRef.current = clipId
    const ns = notesOf(clipMap)
    const target = ns.length ? Math.max(...ns.map(([, n]) => n.p)) : 64
    const r = rowOf(clamp(target + 4, MIN_P, MAX_P))
    view.current.scrollY = Math.max(0, r * rowH - 60)
    view.current.scrollX = 0
  })

  // ---------------- drawing ----------------
  useEffect(() => {
    // Resolve the theme palette ONCE per effect run (theme is a dep), not per
    // frame — the old per-frame getComputedStyle + getPropertyValue per row/
    // grid-line/note was hundreds of style-engine calls every frame.
    const css = getComputedStyle(document.documentElement)
    const C = (v: string) => css.getPropertyValue(v).trim()
    const pal = {
      bg0: C('--bg0'), bg1: C('--bg1'), bg2: C('--bg2'),
      rowRoot: C('--rowRoot'), rowIn: C('--rowIn'), rowBlack: C('--rowBlack'), rowOut: C('--rowOut'),
      gridBar: C('--gridBar'), gridBeat: C('--gridBeat'), gridSub: C('--gridSub'),
      text: C('--text'), dim: C('--dim'), accent: C('--accent'), accent2: C('--accent2'),
      playhead: C('--playhead'), keyBlack: C('--keyBlack'), keyWhite: C('--keyWhite'), keyText: C('--keyText'),
    }
    let wasPlaying = false
    const draw = () => {
      const canvas = canvasRef.current
      const box = boxRef.current
      if (!canvas || !box || !clipMap) return
      // Repaint only when something can have changed: data/view edits mark the
      // dirty flag, dragging and playback animate continuously, and one extra
      // frame after playback stops erases the playhead.
      if (!needsDraw.current && !dragRef.current && !engine.playing && !wasPlaying) return
      wasPlaying = engine.playing
      needsDraw.current = false
      const dpr = window.devicePixelRatio || 1
      const W = box.clientWidth
      const H = box.clientHeight
      if (canvas.width !== W * dpr || canvas.height !== H * dpr) {
        canvas.width = W * dpr
        canvas.height = H * dpr
        canvas.style.width = `${W}px`
        canvas.style.height = `${H}px`
      }
      const ctx = canvas.getContext('2d')!
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
      const len = clipMap.get('len') ?? BAR
      const pxPerTick = view.current.pxPerBar / BAR
      // live row height (vertical zoom) — shadows the outer const so a zoom drag
      // repaints at the new height without waiting for a React render
      const rowH = Math.max(4, Math.round((isDrum ? 30 : 13) * view.current.rowZoom))
      const { scrollX, scrollY } = view.current
      const gridH = H - LANE_H

      ctx.fillStyle = pal.bg0
      ctx.fillRect(0, 0, W, H)

      // rows
      for (let r = 0; r < pitches.length; r++) {
        const y = r * rowH - scrollY
        if (y + rowH < 0 || y > gridH) continue
        const p = pitches[r]
        const inSc = isDrum || inScale(p, root, scaleId)
        const isRoot = !isDrum && ((p - root) % 12 + 12) % 12 === 0
        const black = !isDrum && [1, 3, 6, 8, 10].includes(p % 12)
        ctx.fillStyle = isRoot ? pal.rowRoot : inSc ? pal.rowIn : black ? pal.rowBlack : pal.rowOut
        ctx.fillRect(KEY_W, y, W - KEY_W, rowH - 0.5)
      }

      // vertical grid — three passes so bar/beat shading follows the METER even
      // when the grid resolution doesn't divide the bar (e.g. 1/4 grid in 7/8):
      // sub-grid at the grid setting, then beat lines, then bar lines on top.
      const barT = barTicks()
      const beatT = beatTicks()
      const maxTick = Math.max(len, (W - KEY_W) / pxPerTick + scrollX / pxPerTick)
      const vline = (t: number, style: string) => {
        const x = KEY_W + t * pxPerTick - scrollX
        if (x < KEY_W - 1 || x > W) return
        ctx.strokeStyle = style
        ctx.beginPath()
        ctx.moveTo(x, 0)
        ctx.lineTo(x, gridH)
        ctx.stroke()
      }
      for (let t = 0; t <= maxTick; t += gridTicks) {
        if (t % beatT !== 0) vline(t, pal.gridSub)
      }
      for (let t = 0; t <= maxTick; t += beatT) {
        if (t % barT !== 0) vline(t, pal.gridBeat)
      }
      for (let t = 0; t <= maxTick; t += barT) vline(t, pal.gridBar)
      // beyond-loop shade
      const loopX = KEY_W + len * pxPerTick - scrollX
      if (loopX < W) {
        ctx.fillStyle = 'rgba(0,0,0,0.35)'
        ctx.fillRect(loopX, 0, W - loopX, gridH)
      }

      // notes
      const entries = notesOf(clipMap)
      const d = dragRef.current
      const noteRect = (n: Note, id: string): [number, number, number, number] => {
        let { p, s, d: dur } = n
        if (d && d.type === 'move' && sel.current.has(id) && !d.copy) {
          s = Math.max(0, n.s + d.dTick)
          p = clamp(n.p + d.dPitch, 0, 127)
        }
        if (d && d.type === 'resize' && sel.current.has(id)) {
          dur = Math.max(6, n.d + d.dTick)
        }
        const r = rowOf(isDrum ? clamp(p, 0, 7) : clamp(p, MIN_P, MAX_P))
        return [KEY_W + s * pxPerTick - scrollX, r * rowH - scrollY, Math.max(3, dur * pxPerTick - 1), rowH - 1.5]
      }
      for (const [id, n] of entries) {
        const r = rowOf(isDrum ? clamp(n.p, 0, 7) : n.p)
        if (r < 0) continue
        const [x, y, w, h] = noteRect(n, id)
        if (x + w < KEY_W || x > W || y + h < 0 || y > gridH) continue
        ctx.globalAlpha = 0.45 + 0.55 * n.v
        ctx.fillStyle = trackColor
        ctx.fillRect(x, y, w, h)
        ctx.globalAlpha = 1
        if (n.pr < 1) {
          ctx.strokeStyle = 'rgba(255,255,255,0.75)'
          ctx.setLineDash([3, 3])
          ctx.strokeRect(x + 0.5, y + 0.5, w - 1, h - 1)
          ctx.setLineDash([])
        }
        if (sel.current.has(id)) {
          ctx.strokeStyle = pal.text
          ctx.lineWidth = 1.5
          ctx.strokeRect(x + 0.75, y + 0.75, w - 1.5, h - 1.5)
          ctx.lineWidth = 1
        }
        // copy-drag ghost
        if (d && d.type === 'move' && d.copy && sel.current.has(id)) {
          const gx = KEY_W + Math.max(0, n.s + d.dTick) * pxPerTick - scrollX
          const gr = rowOf(clamp(n.p + d.dPitch, isDrum ? 0 : MIN_P, isDrum ? 7 : MAX_P))
          ctx.globalAlpha = 0.4
          ctx.fillRect(gx, gr * rowH - scrollY, w, h)
          ctx.globalAlpha = 1
        }
      }

      // marquee
      if (d && d.type === 'marquee') {
        ctx.strokeStyle = pal.accent
        ctx.fillStyle = 'rgba(255,176,46,0.12)'
        const x = Math.min(d.x0, d.x1)
        const y = Math.min(d.y0, d.y1)
        ctx.fillRect(x, y, Math.abs(d.x1 - d.x0), Math.abs(d.y1 - d.y0))
        ctx.strokeRect(x, y, Math.abs(d.x1 - d.x0), Math.abs(d.y1 - d.y0))
      }

      // playhead
      let phTicks: number | null = null
      if (engine.playing && selClip) {
        if (selClip.kind === 'session' && engine.mode === 'session') {
          const st = engine.clipState(selClip.trackId, selClip.sceneId)
          if (st.playing) {
            const prog = engine.clipProgress(selClip.trackId)
            if (prog !== null) phTicks = prog * (clipMap.get('len') ?? BAR)
          }
        } else if (selClip.kind === 'arr' && engine.mode === 'arr') {
          const start = clipMap.get('start') ?? 0
          const t = engine.positionTicks()
          if (t >= start && t < start + len) phTicks = (t - start) % len
        }
      }
      if (phTicks !== null) {
        const x = KEY_W + phTicks * pxPerTick - scrollX
        ctx.strokeStyle = pal.playhead
        ctx.lineWidth = 2
        ctx.beginPath()
        ctx.moveTo(x, 0)
        ctx.lineTo(x, gridH)
        ctx.stroke()
        ctx.lineWidth = 1
      }

      // velocity / chance lane
      ctx.fillStyle = pal.bg1
      ctx.fillRect(0, gridH, W, LANE_H)
      ctx.strokeStyle = pal.gridBar
      ctx.beginPath()
      ctx.moveTo(0, gridH + 0.5)
      ctx.lineTo(W, gridH + 0.5)
      ctx.stroke()
      ctx.fillStyle = pal.dim
      ctx.font = '9px sans-serif'
      const laneY = (v: number) => gridH + (LANE_H - 4) - v * (LANE_H - 14)
      if (autoMode) {
        ctx.fillStyle = pal.accent2
        ctx.fillText(`AUTO · ${autoTargetLabel}`, 6, gridH + 12)
        // build the curve from the live paint map (if painting) or the stored env
        const src = autoPaint.current
          ? [...autoPaint.current.entries()].map(([t, v]) => ({ t, v })).sort((a, b) => a.t - b.t)
          : envPoints(clipMap, autoTarget!)
        if (src.length) {
          ctx.strokeStyle = pal.accent2
          ctx.fillStyle = `color-mix(in srgb, ${pal.accent2} 18%, transparent)`
          ctx.lineWidth = 1.5
          ctx.beginPath()
          src.forEach((p, i) => {
            const x = KEY_W + p.t * pxPerTick - scrollX
            const y = laneY(p.v)
            i === 0 ? ctx.moveTo(x, y) : ctx.lineTo(x, y)
          })
          ctx.stroke()
          ctx.lineWidth = 1
          for (const p of src) {
            const x = KEY_W + p.t * pxPerTick - scrollX
            if (x < KEY_W - 4 || x > W) continue
            ctx.beginPath(); ctx.arc(x, laneY(p.v), 2.5, 0, Math.PI * 2); ctx.fill()
          }
        }
      } else {
        ctx.fillText(lane === 'vel' ? 'VELOCITY' : 'CHANCE', 6, gridH + 12)
        for (const [id, n] of entries) {
          const x = KEY_W + n.s * pxPerTick - scrollX
          if (x < KEY_W - 4 || x > W) continue
          const val = lane === 'vel' ? n.v : n.pr
          const bh = val * (LANE_H - 14)
          ctx.fillStyle = trackColor
          ctx.globalAlpha = sel.current.has(id) ? 1 : 0.55
          ctx.fillRect(x - 1.5, gridH + (LANE_H - bh) - 4, 3, bh)
          ctx.beginPath()
          ctx.arc(x, gridH + (LANE_H - bh) - 4, 3, 0, Math.PI * 2)
          ctx.fill()
          ctx.globalAlpha = 1
        }
      }

      // piano keys column (over rows)
      ctx.fillStyle = pal.bg1
      ctx.fillRect(0, 0, KEY_W, gridH)
      for (let r = 0; r < pitches.length; r++) {
        const y = r * rowH - scrollY
        if (y + rowH < 0 || y > gridH) continue
        const p = pitches[r]
        if (isDrum) {
          ctx.fillStyle = pal.bg2
          ctx.fillRect(0, y, KEY_W - 1, rowH - 1)
          ctx.fillStyle = pal.text
          ctx.font = '9px sans-serif'
          ctx.fillText(DRUM_PADS[p], 4, y + rowH / 2 + 3)
        } else {
          const black = [1, 3, 6, 8, 10].includes(p % 12)
          ctx.fillStyle = black ? pal.keyBlack : pal.keyWhite
          ctx.fillRect(0, y, KEY_W - 1, rowH - 0.5)
          if (p % 12 === 0) {
            ctx.fillStyle = black ? '#fff' : pal.keyText
            ctx.font = '9px sans-serif'
            ctx.fillText(midiName(p), 4, y + rowH - 3)
          }
        }
      }
      ctx.strokeStyle = pal.gridBar
      ctx.beginPath()
      ctx.moveTo(KEY_W - 0.5, 0)
      ctx.lineTo(KEY_W - 0.5, gridH)
      ctx.stroke()
    }
    needsDraw.current = true
    const unsub = subscribeFrame(draw)
    // panel resizes (detail-height drag, window resize) bypass React — repaint
    const ro = new ResizeObserver(() => { needsDraw.current = true })
    if (boxRef.current) ro.observe(boxRef.current)
    return () => { unsub(); ro.disconnect() }
  }, [clipMap, gridTicks, lane, fold, isDrum, trackColor, root, scaleId, theme, selClip, autoTarget, autoTargetLabel])

  // ---------------- interactions ----------------

  // Only the pointer coordinates are needed — lets throttled handlers stash a
  // tiny {clientX, clientY} instead of retaining the whole event.
  type Pt = { clientX: number; clientY: number }
  const locate = (e: Pt) => {
    const rect = canvasRef.current!.getBoundingClientRect()
    // getBoundingClientRect/clientX are in the zoomed (visual) space; the canvas
    // draws in layout px. Divide by the UI zoom to map a click to the grid.
    const z = ui.uiZoom || 1
    const x = (e.clientX - rect.left) / z
    const y = (e.clientY - rect.top) / z
    const pxPerTick = view.current.pxPerBar / BAR
    const tick = (x - KEY_W + view.current.scrollX) / pxPerTick
    const row = Math.floor((y + view.current.scrollY) / rowH)
    return { x, y, tick, pitch: pitchAtRow(row), inLane: y > (boxRef.current!.clientHeight - LANE_H), inKeys: x < KEY_W }
  }

  const hitNote = (tick: number, pitch: number): [string, Note] | null => {
    if (!clipMap) return null
    for (const [id, n] of notesOf(clipMap)) {
      const np = isDrum ? clamp(n.p, 0, 7) : n.p
      if (np === pitch && tick >= n.s && tick < n.s + n.d) return [id, n]
    }
    return null
  }

  const snapT = (t: number) => Math.max(0, Math.round(t / gridTicks) * gridTicks)
  const floorT = (t: number) => Math.max(0, Math.floor(t / gridTicks) * gridTicks)

  const addAt = (tick: number, pitch: number) => {
    if (!clipMap) return
    let p = pitch
    if (!isDrum && snapScale) p = snapToScale(p, root, scaleId)
    const s = floorT(tick)
    const id = addNote(clipMap, { p, s, d: ui.drawLen, v: ui.velo, pr: ui.drawProb })
    sel.current = new Set([id])
    engine.previewOn(trackId, p, ui.velo)
    setTimeout(() => engine.previewOff(trackId, p), 180)
    return id
  }

  const onPointerDown = (e: React.PointerEvent) => {
    if (!clipMap || e.button === 2) return
    needsDraw.current = true
    const loc = locate(e)
    try { (e.target as HTMLElement).setPointerCapture(e.pointerId) } catch { /* synthetic */ }

    if (loc.inKeys) {
      // audition the clicked key…
      engine.previewOn(trackId, loc.pitch, 0.85)
      setTimeout(() => engine.previewOff(trackId, loc.pitch), 250)
      // …and let a horizontal drag from the key/pad column zoom the roll on both
      // axes together: drag left = zoom in, drag right = zoom out.
      const startX = e.clientX
      const z0 = ui.uiZoom || 1
      const startPx = view.current.pxPerBar
      const startRz = view.current.rowZoom
      beginVDrag(
        ev => {
          const dx = (ev.clientX - startX) / z0
          const f = clamp(Math.pow(1.006, -dx), 0.15, 8)
          view.current.pxPerBar = clamp(startPx * f, 40, 600)
          view.current.rowZoom = clamp(startRz * f, 0.5, 3.5)
          needsDraw.current = true
        },
        () => {
          const rh = Math.max(4, Math.round((isDrum ? 30 : 13) * view.current.rowZoom))
          view.current.scrollY = clamp(view.current.scrollY, 0, Math.max(0, pitches.length * rh - 100))
          needsDraw.current = true
          bump()
        },
      )
      return
    }
    if (loc.inLane) {
      dragRef.current = { type: 'lane' }
      if (autoMode) {
        const m = new Map<number, number>()
        envPoints(clipMap, autoTarget!).forEach(p => m.set(p.t, p.v))
        autoPaint.current = m
        paintAuto(e)
      } else laneDrag(e)
      return
    }

    const hit = hitNote(loc.tick, loc.pitch)
    if (hit) {
      const [id, n] = hit
      const pxPerTick = view.current.pxPerBar / BAR
      const edgePx = (n.s + n.d) * pxPerTick - (loc.tick * pxPerTick)
      if (drawMode && edgePx > 5) {
        deleteNotes(clipMap, [id], 'Erase note')
        sel.current.delete(id)
        return
      }
      if (!sel.current.has(id)) {
        sel.current = e.shiftKey ? new Set([...sel.current, id]) : new Set([id])
      } else if (e.shiftKey) {
        sel.current.delete(id)
        bump()
        return
      }
      bump()
      const orig = new Map<string, Note>()
      notesOf(clipMap).forEach(([nid, nn]) => { if (sel.current.has(nid)) orig.set(nid, { ...nn }) })
      if (edgePx <= 5) {
        dragRef.current = { type: 'resize', ids: [...sel.current], orig, startTick: loc.tick, dTick: 0 }
      } else {
        dragRef.current = { type: 'move', ids: [...sel.current], orig, startTick: loc.tick, startPitch: loc.pitch, dTick: 0, dPitch: 0, copy: e.altKey, moved: false }
        // the clicked note plays once here for "selected", then again during the
        // drag itself each time it crosses into a new pitch (see onPointerMove)
        dragPreviewAnchor.current = { p: n.p, v: n.v }
        lastPreviewDPitch.current = 0
        previewNote(n.p, n.v)
      }
    } else {
      if (drawMode) {
        addAt(loc.tick, loc.pitch)
        dragRef.current = { type: 'paint', lastCell: `${floorT(loc.tick)}:${loc.pitch}` }
      } else {
        if (!e.shiftKey) { sel.current = new Set(); bump() }
        dragRef.current = { type: 'marquee', x0: loc.x, y0: loc.y, x1: loc.x, y1: loc.y, add: e.shiftKey }
        stopMarqueeScroll()
        marqueeRaf.current = requestAnimationFrame(runMarqueeScroll)
      }
    }
  }

  const paintAuto = (e: { clientX: number; clientY: number }) => {
    if (!clipMap || !autoPaint.current) return
    const loc = locate(e)
    const H = boxRef.current!.clientHeight
    const val = clamp(1 - (loc.y - (H - LANE_H) - 4) / (LANE_H - 14), 0, 1)
    const snap = Math.max(6, Math.round(gridTicks / 2))
    const t = Math.max(0, Math.round(loc.tick / snap) * snap)
    autoPaint.current.set(t, Math.round(val * 100) / 100)
    bump()
  }

  const laneDrag = (e: { clientX: number; clientY: number }) => {
    if (!clipMap) return
    if (autoMode && autoPaint.current) { paintAuto(e); return }
    const loc = locate(e)
    const H = boxRef.current!.clientHeight
    const val = clamp(1 - (loc.y - (H - LANE_H) - 4) / (LANE_H - 14), 0.02, 1)
    // nearest note start within half a grid step
    let best: [string, Note] | null = null
    let bestDist = gridTicks / 2
    for (const [id, n] of notesOf(clipMap)) {
      const dist = Math.abs(n.s - loc.tick)
      if (dist < bestDist) { best = [id, n]; bestDist = dist }
    }
    if (!best) return
    const targets = sel.current.has(best[0]) ? [...sel.current] : [best[0]]
    const patch: Partial<Note> = lane === 'vel' ? { v: val } : { pr: Math.round(val * 20) / 20 }
    updateNotes(clipMap, targets.map(id => [id, patch]), lane === 'vel' ? 'Edit velocity' : 'Edit chance')
  }

  // Lane drags write to the doc (a labeled Yjs transaction that fans out to
  // undo, engine observers and the P2P mesh) — coalesce pointer events to one
  // write per frame, flushing the final position on release.
  const laneRafId = useRef(0)
  const lanePending = useRef<{ clientX: number; clientY: number } | null>(null)
  const laneDragThrottled = (e: React.PointerEvent) => {
    lanePending.current = { clientX: e.clientX, clientY: e.clientY }
    if (laneRafId.current) return
    laneRafId.current = requestAnimationFrame(() => {
      laneRafId.current = 0
      const p = lanePending.current
      lanePending.current = null
      if (p && canvasRef.current) laneDrag(p)
    })
  }
  const flushLaneDrag = () => {
    if (laneRafId.current) { cancelAnimationFrame(laneRafId.current); laneRafId.current = 0 }
    const p = lanePending.current
    lanePending.current = null
    if (p && canvasRef.current) laneDrag(p)
  }

  const onPointerMove = (e: React.PointerEvent) => {
    const d = dragRef.current
    if (!d || !clipMap) {
      const loc = clipMap ? locate(e) : null
      // hover: a resize cursor when over a note's right edge (same hit-test the
      // resize-drag uses), default everywhere else.
      let cur = 'default'
      if (loc && !loc.inLane && !loc.inKeys) {
        hoverRef.current = { tick: loc.tick, pitch: loc.pitch }
        const hit = hitNote(loc.tick, loc.pitch)
        if (hit && (hit[1].s + hit[1].d - loc.tick) * (view.current.pxPerBar / BAR) <= 5) cur = 'ew-resize'
      }
      if (cursorRef.current !== cur && canvasRef.current) { canvasRef.current.style.cursor = cur; cursorRef.current = cur }
      return
    }
    const loc = locate(e)
    if (d.type === 'move') {
      d.dTick = snapT(loc.tick - d.startTick + 10000 * gridTicks) - 10000 * gridTicks // snapped delta (allow negative)
      d.dTick = Math.round((loc.tick - d.startTick) / gridTicks) * gridTicks
      d.dPitch = (rowOf(loc.pitch) >= 0 && rowOf(d.startPitch) >= 0) ? pitchAtRow(rowOf(loc.pitch)) - d.startPitch : 0
      if (fold && !isDrum) {
        // moving across folded rows = move within scale
        d.dPitch = loc.pitch - d.startPitch
      }
      d.copy = d.copy || e.altKey
      d.moved = d.moved || Math.abs(d.dTick) > 0 || Math.abs(d.dPitch) > 0
      // "moved up/down" — re-audition the anchor note each time the drag crosses
      // into a new pitch step, so dragging vertically helps find the right note.
      if (d.dPitch !== lastPreviewDPitch.current) {
        lastPreviewDPitch.current = d.dPitch
        const anchor = dragPreviewAnchor.current
        if (anchor) previewNote(clamp(anchor.p + d.dPitch, 0, 127), anchor.v, 140)
      }
    } else if (d.type === 'resize') {
      d.dTick = Math.round((loc.tick - d.startTick) / (e.shiftKey ? 6 : gridTicks)) * (e.shiftKey ? 6 : gridTicks)
    } else if (d.type === 'marquee') {
      d.x1 = loc.x
      d.y1 = loc.y
    } else if (d.type === 'paint') {
      const cell = `${floorT(loc.tick)}:${loc.pitch}`
      if (cell !== d.lastCell && !hitNote(loc.tick, loc.pitch) && loc.tick >= 0) {
        d.lastCell = cell
        addAt(loc.tick, loc.pitch)
      }
    } else if (d.type === 'lane') {
      laneDragThrottled(e)
    }
  }

  const onPointerUp = (e: React.PointerEvent) => {
    stopMarqueeScroll()
    const d = dragRef.current
    dragRef.current = null
    needsDraw.current = true
    if (d?.type === 'lane') flushLaneDrag()
    if (!d || !clipMap) return
    if (d.type === 'lane' && autoMode && autoPaint.current) {
      const pts = [...autoPaint.current.entries()].map(([t, v]) => ({ t, v }))
      setEnvPoints(clipMap, autoTarget!, pts)
      autoPaint.current = null
      bump()
      return
    }
    if (d.type === 'move' && d.moved) {
      if (d.copy) {
        const copies: Note[] = []
        d.orig.forEach(n => {
          copies.push({ ...n, s: Math.max(0, n.s + d.dTick), p: clamp(n.p + d.dPitch, 0, 127) })
        })
        const ids = addNotes(clipMap, copies, 'Copy notes')
        sel.current = new Set(ids)
      } else {
        updateNotes(clipMap, [...d.orig.entries()].map(([id, n]) => [id, {
          s: Math.max(0, n.s + d.dTick),
          p: clamp(n.p + d.dPitch, 0, 127),
        }]), 'Move notes')
      }
    } else if (d.type === 'resize') {
      updateNotes(clipMap, [...d.orig.entries()].map(([id, n]) => [id, { d: Math.max(6, n.d + d.dTick) }]), 'Resize notes')
    } else if (d.type === 'marquee') {
      const pxPerTick = view.current.pxPerBar / BAR
      const t0 = (Math.min(d.x0, d.x1) - KEY_W + view.current.scrollX) / pxPerTick
      const t1 = (Math.max(d.x0, d.x1) - KEY_W + view.current.scrollX) / pxPerTick
      const r0 = Math.floor((Math.min(d.y0, d.y1) + view.current.scrollY) / rowH)
      const r1 = Math.floor((Math.max(d.y0, d.y1) + view.current.scrollY) / rowH)
      const next = d.add ? new Set(sel.current) : new Set<string>()
      notesOf(clipMap).forEach(([id, n]) => {
        const r = rowOf(isDrum ? clamp(n.p, 0, 7) : n.p)
        if (r >= r0 && r <= r1 && n.s + n.d > t0 && n.s < t1) next.add(id)
      })
      sel.current = next
    }
    bump()
  }

  const onDoubleClick = (e: React.MouseEvent) => {
    if (!clipMap || drawMode) return
    const loc = locate(e)
    if (loc.inKeys || loc.inLane) return
    if (!hitNote(loc.tick, loc.pitch)) addAt(loc.tick, loc.pitch)
  }

  const onContextMenu = (e: React.MouseEvent) => {
    e.preventDefault()
    if (!clipMap) return
    const loc = locate(e)
    if (loc.inKeys) return
    const clipLen = clipMap.get('len') ?? BAR
    const atTick = loc.tick

    // ---- shared note ops ----
    const copyIds = (ids: string[]) => {
      const a = notesOf(clipMap).filter(([id]) => ids.includes(id)).map(([, n]) => ({ ...n }))
      if (a.length) noteClip = a
    }
    const pasteAt = (tick: number) => {
      if (!noteClip.length) return
      const minS = Math.min(...noteClip.map(n => n.s))
      const off = floorT(tick) - minS
      const ids = addNotes(clipMap, noteClip.map(n => ({ ...n, s: Math.max(0, n.s + off) })), 'Paste notes')
      sel.current = new Set(ids); bump()
    }
    const duplicateIds = (ids: string[]) => {
      const entries = notesOf(clipMap).filter(([id]) => ids.includes(id))
      if (!entries.length) return
      const minS = Math.min(...entries.map(([, n]) => n.s))
      const maxE = Math.max(...entries.map(([, n]) => n.s + n.d))
      const span = Math.max(gridTicks, Math.ceil((maxE - minS) / gridTicks) * gridTicks)
      const newIds = addNotes(clipMap, entries.map(([, n]) => ({ ...n, s: n.s + span })), 'Duplicate notes')
      sel.current = new Set(newIds); bump()
    }
    const selEntries = (ids: string[]) => notesOf(clipMap).filter(([id]) => ids.includes(id))
    const replaceSel = (ids: string[], notesObj: Record<string, Note>, label: string) => {
      deleteNotes(clipMap, ids, label)
      const newIds = addNotes(clipMap, Object.values(notesObj), label)
      sel.current = new Set(newIds); bump()
    }
    const setOnSel = (ids: string[], patch: Partial<Note>, label: string) =>
      updateNotes(clipMap, ids.map(id => [id, patch]), label)

    const VELS = [0.3, 0.6, 0.85, 1]
    const chip = (active: boolean, label: string, fn: () => void) =>
      <button key={label} className={`ctx-chip ${active ? 'on' : ''}`} onClick={fn}>{label}</button>

    const hit = !loc.inLane ? hitNote(atTick, loc.pitch) : null

    if (hit) {
      // operate on the whole selection if the clicked note is part of it
      let ids: string[]
      if (sel.current.has(hit[0])) ids = [...sel.current]
      else { sel.current = new Set([hit[0]]); ids = [hit[0]]; bump() }
      const n = ids.length

      openMenu(e, [
        { label: <><Icon name="note" size={12} /> Duplicate {n > 1 ? `selection (${n})` : ''}</>, fn: () => duplicateIds(ids) },
        { label: <><Icon name="grid" size={12} /> Copy {n > 1 ? `(${n})` : ''}</>, fn: () => copyIds(ids) },
        { label: <><Icon name="grid" size={12} /> Cut</>, fn: () => { copyIds(ids); deleteNotes(clipMap, ids, 'Cut notes'); sel.current = new Set(); bump() } },
        { label: <><Icon name="grid" size={12} /> Paste here</>, fn: () => pasteAt(atTick), disabled: !noteClip.length },
        { label: <><Icon name="close" size={12} /> Delete</>, fn: () => { deleteNotes(clipMap, ids, 'Delete notes'); sel.current = new Set(); bump() }, danger: true },
        'sep',
        { label: <><Icon name="chord" size={12} /> Chordify</>, fn: () => replaceSel(ids, tools.chordify(selEntries(ids), root, scaleId), 'Chordify') },
        { label: <><Icon name="arpUp" size={12} /> Arpeggiate</>, fn: () => replaceSel(ids, tools.arpeggiate(selEntries(ids), gridTicks, 'up'), 'Arpeggiate') },
        { label: <><Icon name="dice" size={12} /> Humanize</>, fn: () => setOnSelHumanize(ids) },
        { label: <><Icon name="grid" size={12} /> Quantize</>, fn: () => updateNotes(clipMap, tools.quantize(selEntries(ids), gridTicks, 1), 'Quantize') },
        { label: <><Icon name="legato" size={12} /> Legato</>, fn: () => updateNotes(clipMap, tools.legato(selEntries(ids), clipLen), 'Legato') },
        { label: <><Icon name="reverse" size={12} /> Reverse</>, fn: () => updateNotes(clipMap, tools.reverse(selEntries(ids), clipLen), 'Reverse') },
        'sep',
        { custom: <div className="ctx-row"><span className="ctx-row-label">Velocity</span>{VELS.map(v => chip(false, `${Math.round(v * 100)}`, () => setOnSel(ids, { v }, 'Set velocity')))}</div> },
        { custom: <div className="ctx-row"><span className="ctx-row-label">Chance</span>{[0.25, 0.5, 0.75, 1].map(v => chip(false, `${Math.round(v * 100)}`, () => setOnSel(ids, { pr: v }, 'Set chance')))}</div> },
      ])
      return
    }

    // ---- blank-space menu ----
    const items: any[] = [
      { label: <><Icon name="pencil" size={12} /> Add note here</>, fn: () => addAt(atTick, loc.pitch) },
      { label: <><Icon name="grid" size={12} /> Paste here</>, fn: () => pasteAt(atTick), disabled: !noteClip.length },
      { label: <><Icon name="note" size={12} /> Select all</>, fn: () => { sel.current = new Set(notesOf(clipMap).map(([id]) => id)); bump() } },
    ]
    if (drawMode) {
      items.push('sep')
      items.push({ custom: <div className="ctx-row ctx-row-head"><Icon name="pencil" size={11} /> Pen defaults</div> })
      items.push({ custom: <div className="ctx-row"><span className="ctx-row-label">Length</span>{GRID_OPTIONS.map(g => chip(gridTicks === g.ticks, g.label, () => setUI({ gridTicks: g.ticks })))}</div> })
      items.push({ custom: <div className="ctx-row"><span className="ctx-row-label">Velocity</span>{VELS.map(v => chip(Math.abs(ui.velo - v) < 0.06, `${Math.round(v * 100)}`, () => setUI({ velo: v })))}</div> })
      items.push({ custom: <div className="ctx-row"><span className="ctx-row-label">Chance</span>{[0.25, 0.5, 0.75, 1].map(v => chip(Math.abs(ui.drawProb - v) < 0.06, `${Math.round(v * 100)}`, () => setUI({ drawProb: v })))}</div> })
    }
    openMenu(e, items)

    function setOnSelHumanize(ids: string[]) {
      updateNotes(clipMap!, tools.humanize(selEntries(ids)), 'Humanize')
    }
  }

  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault()
    needsDraw.current = true
    if (e.ctrlKey || e.metaKey) {
      view.current.pxPerBar = clamp(view.current.pxPerBar * (e.deltaY > 0 ? 0.86 : 1.16), 40, 600)
    } else if (e.shiftKey) {
      view.current.scrollX = Math.max(0, view.current.scrollX + e.deltaY)
    } else {
      view.current.scrollX = Math.max(0, view.current.scrollX + e.deltaX)
      view.current.scrollY = clamp(view.current.scrollY + e.deltaY, 0, Math.max(0, pitches.length * rowH - 200))
    }
  }

  // ---- keyboard editing (only while editor is open) ----
  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement
      if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable)) return
      if (!clipMap || !ui.detailOpen) return
      const mod = e.metaKey || e.ctrlKey
      if ((e.key === 'Delete' || e.key === 'Backspace') && sel.current.size) {
        deleteNotes(clipMap, [...sel.current], 'Delete notes')
        sel.current = new Set()
        bump()
        e.preventDefault()
      } else if (mod && e.key.toLowerCase() === 'a') {
        sel.current = new Set(notesOf(clipMap).map(([id]) => id))
        bump()
        e.preventDefault()
      } else if (mod && e.key.toLowerCase() === 'd') {
        const entries = notesOf(clipMap).filter(([id]) => sel.current.has(id))
        if (!entries.length) return
        const minS = Math.min(...entries.map(([, n]) => n.s))
        const maxE = Math.max(...entries.map(([, n]) => n.s + n.d))
        const span = Math.max(gridTicks, Math.ceil((maxE - minS) / gridTicks) * gridTicks)
        const ids = addNotes(clipMap, entries.map(([, n]) => ({ ...n, s: n.s + span })), 'Duplicate notes')
        sel.current = new Set(ids)
        bump()
        e.preventDefault()
      } else if (e.key.startsWith('Arrow') && sel.current.size) {
        e.preventDefault()
        const entries = notesOf(clipMap).filter(([id]) => sel.current.has(id))
        let patches: [string, Partial<Note>][] = []
        if (e.key === 'ArrowLeft') patches = entries.map(([id, n]) => [id, { s: Math.max(0, n.s - gridTicks) }])
        if (e.key === 'ArrowRight') patches = entries.map(([id, n]) => [id, { s: n.s + gridTicks }])
        if (e.key === 'ArrowUp') patches = tools.transpose(entries, e.shiftKey ? 12 : 1, snapScale && !e.shiftKey && !isDrum, root, scaleId)
        if (e.key === 'ArrowDown') patches = tools.transpose(entries, e.shiftKey ? -12 : -1, snapScale && !e.shiftKey && !isDrum, root, scaleId)
        updateNotes(clipMap, patches, 'Nudge notes')
        // "moved up/down" via keyboard — audition each note at its new pitch
        if (ui.notePreview && (e.key === 'ArrowUp' || e.key === 'ArrowDown')) {
          for (const [pid, patch] of patches) {
            if (typeof patch.p !== 'number') continue
            const vel = entries.find(([id]) => id === pid)?.[1].v ?? 0.85
            previewNote(patch.p, vel)
          }
        }
      }
    }
    window.addEventListener('keydown', h)
    return () => window.removeEventListener('keydown', h)
  }, [clipMap, gridTicks, snapScale, isDrum, root, scaleId])

  if (!selClip || !clipMap) {
    return <div className="roll-empty">Select a clip to edit — or double-click an empty slot to create one</div>
  }

  // ---------------- toolbar ----------------
  const entriesSel = () => {
    const all = notesOf(clipMap)
    const s = all.filter(([id]) => sel.current.has(id))
    return s.length ? s : all
  }
  const applyPatches = (patches: [string, Partial<Note>][], label: string) => updateNotes(clipMap, patches, label)
  const replaceSelWith = (notes: Record<string, Note>, label: string) => {
    const target = entriesSel()
    deleteNotes(clipMap, target.map(([id]) => id), label)
    const ids = addNotes(clipMap, Object.values(notes), label)
    sel.current = new Set(ids)
    bump()
  }
  const lenBars = Math.round((clipMap.get('len') ?? barTicks()) / barTicks() * 4) / 4

  const toolsMenu = (e: React.MouseEvent) => openMenu(e, [
    { label: <><Icon name="chord" size={12} /> Chordify (diatonic)</>, fn: () => replaceSelWith(tools.chordify(entriesSel(), root, scaleId), 'Chordify') },
    { label: <><Icon name="arpUp" size={12} /> Arpeggiate up</>, fn: () => replaceSelWith(tools.arpeggiate(entriesSel(), gridTicks, 'up'), 'Arpeggiate') },
    { label: <><Icon name="arpDown" size={12} /> Arpeggiate down</>, fn: () => replaceSelWith(tools.arpeggiate(entriesSel(), gridTicks, 'down'), 'Arpeggiate') },
    { label: <><Icon name="arpUpDown" size={12} /> Arpeggiate up-down</>, fn: () => replaceSelWith(tools.arpeggiate(entriesSel(), gridTicks, 'updown'), 'Arpeggiate') },
    { label: <><Icon name="strum" size={12} /> Strum</>, fn: () => applyPatches(tools.strum(entriesSel()), 'Strum') },
    'sep',
    { label: <><Icon name="dice" size={12} /> Humanize</>, fn: () => applyPatches(tools.humanize(entriesSel()), 'Humanize') },
    { label: <><Icon name="grid" size={12} /> Quantize 100%</>, fn: () => applyPatches(tools.quantize(entriesSel(), gridTicks, 1), 'Quantize') },
    { label: <><Icon name="grid" size={12} /> Quantize 50%</>, fn: () => applyPatches(tools.quantize(entriesSel(), gridTicks, 0.5), 'Quantize 50%') },
    { label: <><Icon name="legato" size={12} /> Legato</>, fn: () => applyPatches(tools.legato(entriesSel(), clipMap.get('len') ?? BAR), 'Legato') },
    { label: <><Icon name="reverse" size={12} /> Reverse</>, fn: () => applyPatches(tools.reverse(entriesSel(), clipMap.get('len') ?? BAR), 'Reverse') },
    { label: <><Icon name="rampUp" size={12} /> Velocity ramp up</>, fn: () => applyPatches(tools.velocityRamp(entriesSel(), 0.4, 1), 'Velocity ramp') },
    { label: <><Icon name="rampDown" size={12} /> Velocity ramp down</>, fn: () => applyPatches(tools.velocityRamp(entriesSel(), 1, 0.4), 'Velocity ramp') },
    'sep',
    {
      label: '×2 Double loop (copy notes)', fn: () => {
        const len = clipMap.get('len') ?? BAR
        addNotes(clipMap, tools.shiftedCopies(notesOf(clipMap), len), 'Double loop')
        setClipField(selClip, 'len', len * 2, 'Double loop')
      },
    },
    {
      label: '÷2 Halve loop', fn: () => {
        const len = Math.max(BAR / 4, (clipMap.get('len') ?? BAR) / 2)
        setClipField(selClip, 'len', len, 'Halve loop')
      },
    },
  ])

  const followMenu = (e: React.MouseEvent) => {
    const f = followOf(clipMap) ?? { on: false, bars: 1, action: 0, chance: 1 }
    const fchip = (active: boolean, label: string, fn: () => void) =>
      <button key={label} className={`ctx-chip ${active ? 'on' : ''}`} onClick={fn}>{label}</button>
    openMenu(e, [
      { label: <><Icon name={f.on ? 'starFill' : 'follow'} size={12} /> {f.on ? 'Following — on' : 'Enable follow'}</>, fn: () => setFollow(clipMap, { on: !f.on }) },
      'sep',
      { custom: <div className="ctx-row"><span className="ctx-row-label">After bars</span>{[1, 2, 4, 8].map(b => fchip(f.bars === b, `${b}`, () => setFollow(clipMap, { bars: b })))}</div> },
      { custom: <div className="ctx-row"><span className="ctx-row-label">Action</span>{FOLLOW_ACTIONS.map((a, i) => fchip(f.action === i, a, () => setFollow(clipMap, { action: i })))}</div> },
      { custom: <div className="ctx-row"><span className="ctx-row-label">Chance</span>{[0.25, 0.5, 0.75, 1].map(c => fchip(Math.abs(f.chance - c) < 0.06, `${Math.round(c * 100)}`, () => setFollow(clipMap, { chance: c })))}</div> },
    ])
  }

  return (
    <div className="roll">
      <div className="roll-toolbar">
        <input
          className="roll-name"
          value={clipMap.get('name') ?? ''}
          onChange={e => setClipField(selClip, 'name', e.target.value, 'Rename clip')}
          data-info="Clip name"
        />
        <button className="color-dot" style={{ background: CLIP_COLORS[clipMap.get('color') ?? 0] }}
          data-info="Clip color"
          onClick={e => openMenu(e, [{ custom: <ColorRow colors={CLIP_COLORS} onPick={i => setClipField(selClip, 'color', i)} /> }])} />
        <button className={`tbtn ${notePreview ? 'on' : ''}`} onClick={() => setUI({ notePreview: !notePreview })}
          data-info="Play a note when it's selected or moved up/down — helps you find the right pitch">
          <Icon name="headphones" size={13} />
        </button>
        <label className="roll-field" data-info="Clip length in bars">
          Len
          <select value={lenBars} onChange={e => setClipField(selClip, 'len', Math.round(+e.target.value * barTicks()), 'Clip length')}>
            {[0.5, 1, 2, 4, 8, 16].map(b => <option key={b} value={b}>{b} bar{b !== 1 ? 's' : ''}</option>)}
          </select>
        </label>
        <label className="roll-field" data-info="Grid resolution for drawing & snapping">
          Grid
          <select value={gridTicks} onChange={e => setUI({ gridTicks: +e.target.value })}>
            {GRID_OPTIONS.map(g => <option key={g.label} value={g.ticks}>{g.label}</option>)}
          </select>
        </label>
        <label className="roll-field" data-info="Length of newly drawn notes">
          Length
          <select value={drawLen} onChange={e => setUI({ drawLen: +e.target.value })}>
            {GRID_OPTIONS.map(g => <option key={g.label} value={g.ticks}>{g.label}</option>)}
          </select>
        </label>
        <button className={`tbtn ${drawMode ? 'on' : ''}`} onClick={() => setUI({ drawMode: !drawMode })}
          data-info="Draw mode (B): click to add notes, click notes to erase"><Icon name="pencil" size={12} /> Draw</button>
        {!isDrum && (
          <>
            <button className={`tbtn ${snapScale ? 'on' : ''}`} onClick={() => setUI({ snapScale: !snapScale })}
              data-info={`Snap new/transposed notes to ${getScale(scaleId).label}`}><Icon name="note" size={12} /> Scale</button>
            <button className={`tbtn ${fold ? 'on' : ''}`} onClick={() => setFold(!fold)}
              data-info="Fold: only show in-scale rows">Fold</button>
          </>
        )}
        {isDrum && (
          <button className={`tbtn ${stepView ? 'on' : ''}`} onClick={() => setStepView(!stepView)}
            data-info="Step sequencer view (drum grid) vs piano roll"><Icon name="steps" size={12} /> Steps</button>
        )}
        {!(isDrum && stepView) && (
          <div className="lane-toggle" data-info="Bottom lane: velocity, chance, or draw automation for a parameter">
            <button className={`tbtn ${lane === 'vel' && !autoMode ? 'on' : ''}`} onClick={() => { setAutoTarget(null); setUI({ lane: 'vel' }) }}>Vel</button>
            <button className={`tbtn ${lane === 'prob' && !autoMode ? 'on' : ''}`} onClick={() => { setAutoTarget(null); setUI({ lane: 'prob' }) }}>Chance</button>
            <select className="auto-select" value={autoTarget ?? ''} data-info="Draw automation: pick a parameter to draw an envelope over the clip"
              onChange={e => setAutoTarget(e.target.value || null)}>
              <option value="">Auto…</option>
              {targetsList.map(t => <option key={t.key} value={t.key}>{t.label}</option>)}
            </select>
            {autoMode && <button className="tbtn" data-info="Clear this automation envelope"
              onClick={() => { setEnvPoints(clipMap, autoTarget!, []); autoPaint.current = null; bump() }}><Icon name="close" size={11} /></button>}
          </div>
        )}
        {selClip.kind === 'session' && (
          <button className={`tbtn ${followOf(clipMap)?.on ? 'on' : ''}`} onClick={followMenu}
            data-info="Follow action: auto-trigger another clip after N bars"><Icon name="follow" size={12} /> Follow</button>
        )}
        <button className="tbtn" onClick={toolsMenu} data-info="MIDI tools: chordify, arp, strum, humanize, quantize…"><Icon name="tools" size={12} /> Tools</button>
        <span className="roll-selcount">{sel.current.size > 0 ? `${sel.current.size} selected` : ''}</span>
        <button className="icon-btn roll-close" onClick={() => setUI({ detailOpen: false })} data-info="Close editor (Esc)"><Icon name="close" size={12} /></button>
      </div>
      <div className="roll-canvas-box" ref={boxRef}>
        {isDrum && stepView
          ? <StepGrid clipMap={clipMap} trackId={trackId} selClip={selClip} />
          : <canvas
              ref={canvasRef}
              onPointerDown={onPointerDown}
              onPointerMove={onPointerMove}
              onPointerUp={onPointerUp}
              onDoubleClick={onDoubleClick}
              onContextMenu={onContextMenu}
              onWheel={onWheel}
            />}
      </div>
    </div>
  )
}
