// Arrangement View: the linear timeline. Clips drag/resize/duplicate with bar
// snapping, the ruler seeks, the loop brace sets the loop region, and you can
// see your collaborators' playheads drift by as ghosts.

import React, { useEffect, useRef, useState, useSyncExternalStore } from 'react'
import * as Y from 'yjs'
import { BAR, CLIP_COLORS, clamp, ticksToBBS } from '../types'
import {
  tracks, arr, meta, addArrClip, moveArrClip, resizeArrClip, deleteArrClip,
  duplicateArrClip, setClipField, setLoopRegion, arrEndTicks, setTrackMix, clipToJSON,
  clips, clipKey, setAutoOpen, setAutoParam, renameTrack, setMetaField, trackAutoKeys, masterAuto,
  barTicks, beatTicks, isAudioClip, notesOf, setAudioField, mutate,
  markersList, addMarker, updateMarker, removeMarker,
} from '../state/doc'
import { getSampleBuffer } from '../audio/samples'
import { autoTargets } from '../audio/params'
import { AutomationLane } from './AutomationLane'
import { engine } from '../audio/engine'
import { setUI, ui, useUI, toast } from '../state/store'
import { useY, useRaf } from './hooks'
import { openMenu, ColorRow, MenuItem, capturePointer, Knob, HFader, InlineRename, beginVDrag } from './widgets'
import { selectClip, trackHeaderMenu, assignKitToTrack, addSampleToArr, copyArrSelection, hasArrClipboard, pasteArrClipboard, splitArrClipAt } from './actions'
import { Icon } from './icons'
import { peersList, subscribeAwareness, awarenessVersion } from '../state/net'
import { MIDI_LOOPS, PROGRESSIONS, progressionClip, clipInKey } from '../packs'
import { loadLoop } from './actions'

const LANE_H = 88
const HEAD_W = 168

// Pan control spec (mirrors Session view): -1 full left … +1 full right.
const PAN_SPEC = { key: 'pan', label: 'Pan', min: -1, max: 1, def: 0, fmt: (v: number) => (Math.abs(v) < 0.02 ? 'C' : v < 0 ? `${Math.round(-v * 50)}L` : `${Math.round(v * 50)}R`) }

// A timeline row: instrument/audio/drum tracks (which hold clips), then bus
// tracks, then the master bus. Buses + master carry no clips but get a head and
// an automation lane. `track` is undefined for the synthetic master row.
type ArrRow = { id: string; name: string; color: number; kind: string; clips: boolean; track?: Y.Map<any>; autoOpen: boolean; autoParam: string; mute: boolean; solo: boolean }
function toRow(t: Y.Map<any>): ArrRow {
  return {
    id: t.get('id'), name: t.get('name'), color: t.get('color') ?? 0, kind: t.get('kind'),
    clips: t.get('kind') !== 'bus', track: t,
    autoOpen: !!t.get('autoOpen'), autoParam: (t.get('autoParam') as string) ?? 'mix||gain',
    mute: !!t.get('mute'), solo: !!t.get('solo'),
  }
}

function usePresence() {
  return useSyncExternalStore(subscribeAwareness, awarenessVersion)
}

function Playhead({ pxPerTick }: { pxPerTick: number }) {
  const ref = useRef<HTMLDivElement>(null)
  useRaf(() => {
    if (!ref.current) return
    const t = engine.mode === 'arr' && engine.playing ? engine.positionTicks() : engine.arrSeekTicks
    ref.current.style.transform = `translateX(${t * pxPerTick}px)`
  })
  return <div ref={ref} className="playhead" />
}

function GhostPlayheads({ pxPerTick }: { pxPerTick: number }) {
  usePresence()
  const peers = peersList().filter(p => !p.me && p.state.ph && p.state.ph.mode === 'arr')
  return (
    <>
      {peers.map(p => (
        <div key={p.id} className="playhead ghost" style={{ transform: `translateX(${(p.state.ph!.ticks) * pxPerTick}px)`, background: p.state.color }} title={p.state.name} />
      ))}
    </>
  )
}

// ---------------- clip content previews ----------------

// Peak cache for waveform previews: 256 abs-max buckets per cropped region.
// Computed once per (sample, crop); redraws just rescale the buckets.
const peakCache = new Map<string, Float32Array>()
function wavePeaks(sampleId: string, buf: AudioBuffer, offset: number, dur: number): Float32Array {
  const sr = buf.sampleRate
  const s0 = Math.max(0, Math.min(buf.length - 1, Math.floor(offset * sr)))
  const n = dur > 0 ? Math.max(1, Math.min(buf.length - s0, Math.floor(dur * sr))) : buf.length - s0
  const key = `${sampleId}|${s0}|${n}`
  const hit = peakCache.get(key)
  if (hit) return hit
  const B = 256
  const out = new Float32Array(B)
  const data = buf.getChannelData(0)
  const step = n / B
  for (let b = 0; b < B; b++) {
    const i0 = s0 + Math.floor(b * step)
    const i1 = s0 + Math.floor((b + 1) * step)
    const stride = Math.max(1, Math.floor((i1 - i0) / 64))   // long buckets sample sparsely
    let peak = 0
    for (let i = i0; i < i1; i += stride) { const v = Math.abs(data[i] ?? 0); if (v > peak) peak = v }
    out[b] = peak
  }
  peakCache.set(key, out)
  return out
}

/** Waveform preview inside an audio clip box (crop + reverse aware). */
function ClipWave({ m, w, h }: { m: Y.Map<any>; w: number; h: number }) {
  const ref = useRef<HTMLCanvasElement>(null)
  const sampleId = m.get('sampleId') || ''
  const offset = m.get('offset') ?? 0
  const dur = m.get('dur') ?? 0
  const rev = !!m.get('rev')
  const W = Math.max(1, Math.round(w)), H = Math.max(1, Math.round(h))
  useEffect(() => {
    let iv = 0
    const draw = () => {
      const canvas = ref.current
      if (!canvas) return true
      const buf = getSampleBuffer(sampleId)   // kicks off a lazy IDB restore when missing
      if (!buf) return false
      canvas.width = W; canvas.height = H
      const ctx = canvas.getContext('2d')!
      const peaks = wavePeaks(sampleId, buf, offset, dur)
      ctx.fillStyle = 'rgba(255,255,255,0.35)'
      const mid = H / 2
      for (let x = 0; x < W; x++) {
        const bi = Math.min(255, Math.floor((x / W) * 256))
        const p = peaks[rev ? 255 - bi : bi]
        const hh = Math.max(0.5, p * (H / 2 - 1))
        ctx.fillRect(x, mid - hh, 1, hh * 2)
      }
      return true
    }
    if (!draw()) iv = window.setInterval(() => { if (draw()) { clearInterval(iv); iv = 0 } }, 300)
    return () => { if (iv) clearInterval(iv) }
  }, [sampleId, offset, dur, rev, W, H])
  return <canvas ref={ref} className="arr-clip-canvas" />
}

/** Mini piano-roll preview inside a MIDI clip box. */
function ClipNotes({ m, w, h }: { m: Y.Map<any>; w: number; h: number }) {
  const ref = useRef<HTMLCanvasElement>(null)
  // no deps: the arrange view re-renders on any arr change, and redrawing a
  // handful of tiny rects is cheaper than tracking note-level versions
  useEffect(() => {
    const canvas = ref.current
    if (!canvas) return
    const W = Math.max(1, Math.round(w)), H = Math.max(1, Math.round(h))
    canvas.width = W; canvas.height = H
    const entries = notesOf(m)
    if (!entries.length) return
    const ctx = canvas.getContext('2d')!
    const len = m.get('len') ?? BAR
    let lo = 127, hi = 0
    for (const [, n] of entries) { if (n.p < lo) lo = n.p; if (n.p > hi) hi = n.p }
    if (hi - lo < 10) { const c = (hi + lo) / 2; lo = Math.max(0, c - 5); hi = lo + 10 }
    const rowH = Math.max(1.5, Math.min(4, H / (hi - lo + 3)))
    ctx.fillStyle = 'rgba(255,255,255,0.55)'
    for (const [, n] of entries) {
      const x = (n.s / len) * W
      const nw = Math.max(1.5, (n.d / len) * W - 0.5)
      const y = H - ((n.p - lo + 1) / (hi - lo + 2)) * H
      ctx.globalAlpha = 0.3 + 0.5 * n.v
      ctx.fillRect(x, y, nw, rowH)
    }
    ctx.globalAlpha = 1
  })
  return <canvas ref={ref} className="arr-clip-canvas" />
}

/** Fade handles + gain line drawn over an audio clip (drag to edit). */
function AudioClipOverlay({ id, m, w, h, pxPerTick }: { id: string; m: Y.Map<any>; w: number; h: number; pxPerTick: number }) {
  const [gainTip, setGainTip] = useState<number | null>(null)
  const len = m.get('len') ?? BAR
  const fadeIn = m.get('fadeIn') ?? 0
  const fadeOut = m.get('fadeOut') ?? 0
  const gain = m.get('gainDb') ?? 0
  const fiPx = clamp(fadeIn * pxPerTick, 0, w)
  const foPx = clamp(fadeOut * pxPerTick, 0, w)
  // gain line: +24dB at the top edge, −24dB at the bottom, 0dB mid
  const gainY = clamp(h / 2 - (gain / 24) * (h / 2 - 3), 3, h - 3)
  const ref = { kind: 'arr' as const, id }

  const dragFade = (which: 'fadeIn' | 'fadeOut') => (e: React.PointerEvent) => {
    if (e.button !== 0) return
    e.stopPropagation(); e.preventDefault()
    const z = ui.uiZoom || 1
    const startX = e.clientX
    const start = which === 'fadeIn' ? fadeIn : fadeOut
    beginVDrag(ev => {
      const dt = ((ev.clientX - startX) / z) / pxPerTick
      const ticks = clamp(start + (which === 'fadeIn' ? dt : -dt), 0, len)
      setAudioField(ref, which, Math.round(ticks), 'Clip fade')
    })
  }
  const dragGain = (e: React.PointerEvent) => {
    if (e.button !== 0) return
    e.stopPropagation(); e.preventDefault()
    const z = ui.uiZoom || 1
    const startY = e.clientY
    const start = gain
    setGainTip(start)
    beginVDrag(
      ev => {
        const v = clamp(start - ((ev.clientY - startY) / z) / (h / 2 - 3) * 24, -24, 24)
        setGainTip(v)
        setAudioField(ref, 'gainDb', Math.round(v * 10) / 10, 'Clip gain')
      },
      () => setGainTip(null),
    )
  }

  return (
    <svg className="arr-fade-svg" width={Math.max(1, Math.round(w))} height={h}>
      {fiPx > 1 && <path className="arr-fade-shade" d={`M 0 ${h} L 0 0 L ${fiPx} 0 Z`} transform={`translate(0,0) scale(1,1)`} />}
      {fiPx > 1 && <line className="arr-fade-line" x1={0} y1={h} x2={fiPx} y2={0} />}
      {foPx > 1 && <path className="arr-fade-shade" d={`M ${w} ${h} L ${w} 0 L ${w - foPx} 0 Z`} />}
      {foPx > 1 && <line className="arr-fade-line" x1={w} y1={h} x2={w - foPx} y2={0} />}
      <line className="arr-gain-vis" x1={0} y1={gainY} x2={w} y2={gainY} />
      <line className="arr-gain-line" x1={0} y1={gainY} x2={w} y2={gainY} onPointerDown={dragGain}
        data-info="Clip gain — drag up/down (dB)" />
      <circle className="arr-fade-handle" cx={clamp(fiPx, 4, w - 4)} cy={4} r={3.5} onPointerDown={dragFade('fadeIn')}
        data-info="Fade-in — drag right" />
      <circle className="arr-fade-handle" cx={clamp(w - foPx, 4, w - 4)} cy={4} r={3.5} onPointerDown={dragFade('fadeOut')}
        data-info="Fade-out — drag left" />
      {gainTip !== null && <text className="arr-gain-tip" x={6} y={clamp(gainY - 4, 10, h - 4)}>{`${gainTip > 0 ? '+' : ''}${gainTip.toFixed(1)} dB`}</text>}
    </svg>
  )
}

type MoveItem = { id: string; origStart: number; origLane: number }
type DragState =
  | { type: 'move'; id: string; startX: number; startY: number; origStart: number; origLane: number; dx: number; dLane: number; copy: boolean; items: MoveItem[] }
  | { type: 'resize'; id: string; startX: number; origLen: number; dLen: number }
  | { type: 'loop'; edge: 'start' | 'end' | 'mid'; startX: number; origStart: number; origEnd: number; cur: [number, number] }
  | { type: 'marquee'; x0: number; y0: number; x1: number; y1: number; moved: boolean }
  | { type: 'timesel'; t0: number; t1: number; moved: boolean }
  | null

export function ArrangementView() {
  useY(tracks)
  useY(arr)
  useY(meta)
  useY(masterAuto)   // master-bus automation lives outside `tracks`; observe it for the dot + lane
  const zoom = useUI(s => s.zoomPxPerBar)
  const uiZoom = useUI(s => s.uiZoom)
  // zoom stays "pixels per bar", so a bar keeps its width under any meter
  const barT = barTicks()
  const pxPerTick = zoom / barT
  // CSS UI-zoom factor: client coords are visual, the timeline lays out in
  // layout px — divide by this to map clicks/drags to ticks.
  const z = uiZoom || 1
  const [drag, setDrag] = useState<DragState>(null)
  const selArrIds = useUI(s => s.selArrIds)        // marquee multi-selection
  const timeSel = useUI(s => s.timeSel)            // time-range selection (ticks)
  const [renamingTid, setRenamingTid] = useState<string | null>(null)
  const scrollRef = useRef<HTMLDivElement>(null)
  const headsRef = useRef<HTMLDivElement>(null)
  const lanesRef = useRef<HTMLDivElement>(null)
  // Keep the left track headers in vertical lockstep with the lane area: they
  // live in a separate (non-scrolling) column, so translate their inner content
  // to mirror the scroll position. The corner stays pinned (like the ruler).
  const onScroll = (e: React.UIEvent<HTMLDivElement>) => {
    if (headsRef.current) headsRef.current.style.transform = `translateY(${-e.currentTarget.scrollTop}px)`
  }

  // Rows: clip tracks first, then bus tracks, then master. Clips live only on the
  // first `nClipRows`; buses + master are head + automation only.
  const allTracks = tracks.toArray()
  const instrTracks = allTracks.filter(t => t.get('kind') !== 'bus')
  const busTracks = allTracks.filter(t => t.get('kind') === 'bus')
  const rows: ArrRow[] = [
    ...instrTracks.map(toRow), ...busTracks.map(toRow),
    {
      id: 'master', name: 'Master', color: -1, kind: 'master', clips: false, track: undefined,
      autoOpen: !!meta.get('masterAutoOpen'), autoParam: (meta.get('masterAutoParam') as string) ?? 'mix||gain', mute: false, solo: false,
    },
  ]
  const nClipRows = instrTracks.length
  const laneOf = (tid: string) => rows.findIndex(r => r.id === tid)
  const totalBars = Math.max(33, Math.ceil(arrEndTicks() / barT) + 9)
  const width = totalBars * zoom

  // Vertical row layout: each row is one band (LANE_H); if its automation lane
  // is expanded, an equal-height band follows directly below it. Everything (clip
  // tops, lane backgrounds, hit-testing, the heads column) derives from this so
  // the two columns stay aligned.
  const trackTop: Record<string, number> = {}
  const autoTop: Record<string, number> = {}
  const bgRows: { tid: string; kind: 'track' | 'auto'; top: number; clips: boolean }[] = []
  let yAcc = 0
  for (const r of rows) {
    trackTop[r.id] = yAcc; bgRows.push({ tid: r.id, kind: 'track', top: yAcc, clips: r.clips }); yAcc += LANE_H
    if (r.autoOpen) { autoTop[r.id] = yAcc; bgRows.push({ tid: r.id, kind: 'auto', top: yAcc, clips: false }); yAcc += LANE_H }
  }
  const lanesHeight = Math.max(yAcc, LANE_H)
  const trackTopByIdx = (i: number) => { const r = rows[i]; return r ? trackTop[r.id] : 0 }
  const rowAtY = (y: number) => bgRows.find(r => y >= r.top && y < r.top + LANE_H)
  const trackIdxAtY = (y: number) => {
    const r = rowAtY(y)
    return r ? rows.findIndex(x => x.id === r.tid) : clamp(Math.floor(y / LANE_H), 0, rows.length - 1)
  }

  const loopOn = !!meta.get('loopOn')
  const loopStart = meta.get('loopStart') ?? 0
  const loopEnd = meta.get('loopEnd') ?? BAR * 4

  const snap = (t: number, fine: boolean) => {
    const g = fine ? ui.gridTicks : barT
    return Math.round(t / g) * g
  }

  const seekFromEvent = (e: React.MouseEvent, el: HTMLElement) => {
    const rect = el.getBoundingClientRect()
    const t = Math.max(0, (e.clientX - rect.left) / z / pxPerTick)
    engine.seekArr(snap(t, e.shiftKey))
  }

  const onWheel = (e: React.WheelEvent) => {
    if (e.ctrlKey || e.metaKey) {
      e.preventDefault()
      setUI({ zoomPxPerBar: clamp(zoom * (e.deltaY > 0 ? 0.85 : 1.18), 24, 480) })
    }
  }

  // ---- split at playhead (S) ----
  const playheadTicks = () => Math.round(engine.mode === 'arr' && engine.playing ? engine.positionTicks() : engine.arrSeekTicks)
  const splitAtPlayhead = () => {
    const cut = playheadTicks()
    // selected clips first; with no selection, split whatever is under the playhead
    const chosen = ui.selArrIds.length ? ui.selArrIds : (ui.selClip?.kind === 'arr' ? [ui.selClip.id] : [])
    const targets: string[] = chosen.length ? chosen : (() => {
      const out: string[] = []
      arr.forEach((m: Y.Map<any>, id: string) => {
        const s = m.get('start') ?? 0, l = m.get('len') ?? 0
        if (cut > s && cut < s + l) out.push(id)
      })
      return out
    })()
    let didSplit = false
    for (const id of targets) {
      const m = arr.get(id) as Y.Map<any> | undefined
      if (!m) continue
      const s = m.get('start') ?? 0, l = m.get('len') ?? 0
      if (cut > s && cut < s + l && splitArrClipAt(id, cut - s)) didSplit = true
    }
    if (didSplit) { setUI({ selArrIds: [], selClip: null }); toast('Split at playhead') }
  }
  React.useEffect(() => {
    const h = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement
      if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable)) return
      if (e.key.toLowerCase() === 's' && !e.metaKey && !e.ctrlKey && !e.altKey) splitAtPlayhead()
    }
    window.addEventListener('keydown', h)
    return () => window.removeEventListener('keydown', h)
  })

  // ---- auto-crossfade: overlapping audio clips on a track fade into each other ----
  const applyAutoCrossfades = (tids: Set<string>) => {
    type C = { m: Y.Map<any>; start: number; len: number }
    const byTrack = new Map<string, C[]>()
    arr.forEach((m: Y.Map<any>) => {
      if (!m.get('audio')) return
      const tid = m.get('trackId')
      if (!tids.has(tid)) return
      const list = byTrack.get(tid) ?? []
      list.push({ m, start: m.get('start') ?? 0, len: m.get('len') ?? 0 })
      byTrack.set(tid, list)
    })
    const jobs: Array<() => void> = []
    byTrack.forEach(list => {
      list.sort((a, b) => a.start - b.start)
      for (let i = 0; i < list.length - 1; i++) {
        const a = list[i], b = list[i + 1]
        const overlap = Math.round(a.start + a.len - b.start)
        if (overlap > 2 && overlap < Math.min(a.len, b.len)) {
          jobs.push(() => { a.m.set('fadeOut', overlap); b.m.set('fadeIn', overlap) })
        }
      }
    })
    if (jobs.length) mutate('Auto crossfade', () => jobs.forEach(j => j()))
  }

  // ---- time-range actions ----
  const deleteRange = (t0: number, t1: number) => {
    // split every clip crossing a bound, then delete what's fully inside
    const crossing = (bound: number) => {
      const out: { id: string; cut: number }[] = []
      arr.forEach((m: Y.Map<any>, id: string) => {
        const s = m.get('start') ?? 0, l = m.get('len') ?? 0
        if (s < bound - 2 && s + l > bound + 2) out.push({ id, cut: bound - s })
      })
      return out
    }
    crossing(t0).forEach(c => splitArrClipAt(c.id, c.cut))
    crossing(t1).forEach(c => splitArrClipAt(c.id, c.cut))
    const doomed: string[] = []
    arr.forEach((m: Y.Map<any>, id: string) => {
      const s = m.get('start') ?? 0, l = m.get('len') ?? 0
      if (s >= t0 - 2 && s + l <= t1 + 2) doomed.push(id)
    })
    doomed.forEach(id => deleteArrClip(id))
    setUI({ selArrIds: [], selClip: null })
    if (doomed.length) toast(`Removed ${doomed.length} clip${doomed.length > 1 ? 's' : ''}`)
  }
  const stripMenu = (e: React.MouseEvent) => {
    e.preventDefault()
    if ((e.target as HTMLElement).closest('.arr-marker')) return
    const ts = ui.timeSel
    const items: MenuItem[] = []
    if (ts) {
      items.push(
        { label: <><Icon name="loop" size={12} /> Loop this range</>, fn: () => setLoopRegion(ts[0], ts[1], true) },
        { label: <><Icon name="map" size={12} /> Create region from selection</>, fn: () => { const n = prompt('Region name', 'Region'); if (n != null) addMarker(ts[0], n.trim() || 'Region', ts[1] - ts[0]) } },
        { label: <><Icon name="close" size={12} /> Delete clips in range</>, fn: () => deleteRange(ts[0], ts[1]), danger: true },
        'sep',
        { label: 'Clear selection (Esc)', fn: () => setUI({ timeSel: null }) },
      )
    } else {
      items.push({ label: 'Drag here to select a time range · double-click adds a marker', fn: () => {}, disabled: true })
    }
    openMenu(e, items)
  }

  // ----- clip drag handlers -----
  const itemOf = (cid: string): MoveItem | null => {
    const m = arr.get(cid) as Y.Map<any> | undefined
    return m ? { id: cid, origStart: m.get('start') ?? 0, origLane: laneOf(m.get('trackId')) } : null
  }
  const beginMove = (e: React.PointerEvent, id: string) => {
    e.stopPropagation()
    const m = arr.get(id) as Y.Map<any>
    if (!m) return
    // Dragging a clip that's part of the marquee selection moves the whole group;
    // otherwise it becomes the single selection.
    const inGroup = selArrIds.includes(id) && selArrIds.length > 1
    const items = (inGroup ? selArrIds : [id]).map(itemOf).filter(Boolean) as MoveItem[]
    if (!inGroup) { selectClip({ kind: 'arr', id }); setUI({ selArrIds: [] }) }
    setDrag({ type: 'move', id, startX: e.clientX, startY: e.clientY, origStart: m.get('start') ?? 0, origLane: laneOf(m.get('trackId')), dx: 0, dLane: 0, copy: e.altKey, items })
    capturePointer(e)
  }
  // Marquee select: pointer-down on empty lane background drags a box; clips whose
  // rect intersects it become the selection. A click with no drag seeks + clears.
  const beginMarquee = (e: React.PointerEvent) => {
    // clips & automation lanes stopPropagation, so this only fires on the lane
    // background (the .arr-lanes element or its .lane-bg / .lane-grid overlays).
    if (e.button !== 0) return
    const el = e.target as HTMLElement
    if (el.closest('.arr-clip') || el.closest('.arr-auto-lane')) return
    const rect = lanesRef.current!.getBoundingClientRect()
    const x = (e.clientX - rect.left) / z
    const y = (e.clientY - rect.top) / z
    setDrag({ type: 'marquee', x0: x, y0: y, x1: x, y1: y, moved: false })
    capturePointer(e)
  }
  const beginResize = (e: React.PointerEvent, id: string) => {
    e.stopPropagation()
    const m = arr.get(id) as Y.Map<any>
    if (!m) return
    setDrag({ type: 'resize', id, startX: e.clientX, origLen: m.get('len'), dLen: 0 })
    capturePointer(e)
  }
  const onPointerMove = (e: React.PointerEvent) => {
    if (!drag) return
    if (drag.type === 'move') {
      const lr = lanesRef.current?.getBoundingClientRect()
      const targetIdx = lr ? trackIdxAtY((e.clientY - lr.top) / z) : drag.origLane
      setDrag({ ...drag, dx: (e.clientX - drag.startX) / z / pxPerTick, dLane: targetIdx - drag.origLane, copy: e.altKey || drag.copy })
    } else if (drag.type === 'marquee') {
      const rect = lanesRef.current!.getBoundingClientRect()
      setDrag({ ...drag, x1: (e.clientX - rect.left) / z, y1: (e.clientY - rect.top) / z, moved: true })
    } else if (drag.type === 'resize') {
      setDrag({ ...drag, dLen: (e.clientX - drag.startX) / z / pxPerTick })
    } else if (drag.type === 'timesel') {
      const rect = lanesRef.current!.getBoundingClientRect()
      setDrag({ ...drag, t1: Math.max(0, (e.clientX - rect.left) / z / pxPerTick), moved: true })
    } else if (drag.type === 'loop') {
      const dt = (e.clientX - drag.startX) / z / pxPerTick
      let s = drag.origStart
      let en = drag.origEnd
      if (drag.edge === 'start') s = clamp(snap(drag.origStart + dt, e.shiftKey), 0, en - barT)
      else if (drag.edge === 'end') en = Math.max(s + barT, snap(drag.origEnd + dt, e.shiftKey))
      else { const d = snap(dt, e.shiftKey); s = Math.max(0, drag.origStart + d); en = drag.origEnd + (s - drag.origStart) }
      setDrag({ ...drag, cur: [s, en] })
    }
  }
  const onPointerUp = (e: React.PointerEvent) => {
    if (!drag) return
    if (drag.type === 'move') {
      // snap the delta off the dragged clip, then apply it to the whole group
      const dStart = Math.max(0, snap(drag.origStart + drag.dx, e.shiftKey)) - drag.origStart
      const copies: string[] = []
      for (const it of drag.items) {
        const m = arr.get(it.id) as Y.Map<any> | undefined
        if (!m) continue
        const newStart = Math.max(0, it.origStart + dStart)
        // clips only land on clip rows (the first nClipRows); never on a bus/master row
        const newLane = clamp(it.origLane + drag.dLane, 0, Math.max(0, nClipRows - 1))
        const newTrack = rows[newLane]?.id ?? m.get('trackId')
        if (drag.copy) {
          const nid = addArrClip(newTrack, newStart, clipToJSON(m), drag.items.length > 1 ? 'Duplicate clips' : 'Duplicate clip')
          if (nid) copies.push(nid)
        } else {
          moveArrClip(it.id, { start: newStart, trackId: newTrack })
        }
      }
      if (drag.copy && drag.items.length > 1 && copies.length) setUI({ selArrIds: copies, selClip: null })
      // overlapping audio clips fade into each other (source + destination tracks)
      const affected = new Set<string>()
      for (const it of drag.items) {
        const m = arr.get(it.id) as Y.Map<any> | undefined
        if (m) affected.add(m.get('trackId'))
        const lane = rows[clamp(it.origLane + drag.dLane, 0, Math.max(0, nClipRows - 1))]
        if (lane) affected.add(lane.id)
        const orig = rows[it.origLane]
        if (orig) affected.add(orig.id)
      }
      applyAutoCrossfades(affected)
    } else if (drag.type === 'marquee') {
      if (!drag.moved) {
        engine.seekArr(snap(Math.max(0, Math.min(drag.x0, drag.x1) / pxPerTick), e.shiftKey))
        setUI({ selArrIds: [], selClip: null })
      } else {
        const x0 = Math.min(drag.x0, drag.x1), x1 = Math.max(drag.x0, drag.x1)
        const y0 = Math.min(drag.y0, drag.y1), y1 = Math.max(drag.y0, drag.y1)
        const hits: string[] = []
        arr.forEach((m: Y.Map<any>, id: string) => {
          const lane = laneOf(m.get('trackId'))
          if (lane < 0) return
          const cx0 = (m.get('start') ?? 0) * pxPerTick
          const cx1 = cx0 + (m.get('len') ?? BAR) * pxPerTick
          const cy0 = trackTopByIdx(lane)
          if (cx1 > x0 && cx0 < x1 && cy0 + LANE_H > y0 && cy0 < y1) hits.push(id)
        })
        setUI({ selArrIds: hits, selClip: null })
      }
    } else if (drag.type === 'resize') {
      const newLen = Math.max(BAR / 4, snap(drag.origLen + drag.dLen, e.shiftKey))
      resizeArrClip(drag.id, newLen)
      const m = arr.get(drag.id) as Y.Map<any> | undefined
      if (m) applyAutoCrossfades(new Set([m.get('trackId')]))
    } else if (drag.type === 'timesel') {
      if (!drag.moved || Math.abs(drag.t1 - drag.t0) < 4) setUI({ timeSel: null })   // click clears
      else {
        const a = snap(Math.min(drag.t0, drag.t1), true)
        const b = snap(Math.max(drag.t0, drag.t1), true)
        setUI({ timeSel: b - a > 2 ? [a, b] : null })
      }
    } else if (drag.type === 'loop') {
      setLoopRegion(drag.cur[0], drag.cur[1], true)
    }
    setDrag(null)
  }

  // ----- render clips -----
  const clipEls: React.ReactNode[] = []
  arr.forEach((m: Y.Map<any>, id: string) => {
    const lane = laneOf(m.get('trackId'))
    if (lane < 0) return
    let start = m.get('start') ?? 0
    let len = m.get('len') ?? BAR
    let laneIdx = lane
    let ghost = false
    if (drag?.type === 'move' && !drag.copy) {
      const it = drag.items.find(i => i.id === id)
      if (it) {
        const dStart = Math.max(0, snap(drag.origStart + drag.dx, false)) - drag.origStart
        start = Math.max(0, it.origStart + dStart)
        laneIdx = clamp(it.origLane + drag.dLane, 0, Math.max(0, nClipRows - 1))
        ghost = true
      }
    }
    if (drag?.type === 'resize' && drag.id === id) {
      len = Math.max(BAR / 4, snap(drag.origLen + drag.dLen, false))
      ghost = true
    }
    const color = CLIP_COLORS[m.get('color') ?? 0]
    const isSel = (ui.selClip?.kind === 'arr' && ui.selClip.id === id) || selArrIds.includes(id)
    const menu: MenuItem[] = [
      { label: <><Icon name="pencil" size={12} /> Edit notes</>, fn: () => selectClip({ kind: 'arr', id }, true) },
      {
        label: <><Icon name="strum" size={12} /> Split at playhead (S)</>,
        fn: () => {
          const cur = playheadTicks()
          const s = m.get('start') ?? 0, l = m.get('len') ?? 0
          if (cur > s && cur < s + l) splitArrClipAt(id, cur - s)
          else toast('Move the playhead inside the clip to split it')
        },
      },
      { label: 'Duplicate after', fn: () => duplicateArrClip(id) },
      { custom: <ColorRow colors={CLIP_COLORS} onPick={i => setClipField({ kind: 'arr', id }, 'color', i)} /> },
      'sep',
      { label: 'Delete', fn: () => deleteArrClip(id), danger: true },
    ]
    if (selArrIds.length > 1 && selArrIds.includes(id)) {
      menu.push({ label: `Delete ${selArrIds.length} selected`, fn: () => { selArrIds.forEach(gid => deleteArrClip(gid)); setUI({ selArrIds: [] }) }, danger: true })
    }
    clipEls.push(
      <div
        key={id}
        className={`arr-clip ${isSel ? 'selected' : ''} ${ghost ? 'ghost-drag' : ''}`}
        style={{
          left: start * pxPerTick,
          top: trackTopByIdx(laneIdx) + 2,
          width: Math.max(8, len * pxPerTick - 2),
          height: LANE_H - 5,
          background: `color-mix(in srgb, ${color} 26%, var(--bg2))`,
          borderColor: color,
        }}
        onPointerDown={e => { if (e.button === 0) beginMove(e, id) }}
        onDoubleClick={() => selectClip({ kind: 'arr', id }, true)}
        onContextMenu={e => openMenu(e, menu)}
        data-info="Drag to move (Alt = copy, Shift = fine). Drag right edge to resize. S splits at the playhead. Double-click to edit."
      >
        {isAudioClip(m)
          ? <>
              <ClipWave m={m} w={Math.max(8, len * pxPerTick - 2)} h={LANE_H - 7} />
              <AudioClipOverlay id={id} m={m} w={Math.max(8, len * pxPerTick - 2)} h={LANE_H - 7} pxPerTick={pxPerTick} />
            </>
          : <ClipNotes m={m} w={Math.max(8, len * pxPerTick - 2)} h={LANE_H - 7} />}
        <span className="arr-clip-name">{m.get('name')}</span>
        <div className="arr-clip-resize" onPointerDown={e => beginResize(e, id)} />
      </div>,
    )
  })

  const loopCur: [number, number] = drag?.type === 'loop' ? drag.cur : [loopStart, loopEnd]

  return (
    <div className="arrange" onWheel={onWheel}>
      <div className="arr-heads" style={{ width: HEAD_W }}>
        <div className="arr-corner">
          <button
            className={`tbtn ${loopOn ? 'on' : ''}`}
            data-info="Toggle loop region (playback loops between the braces)"
            onClick={() => setLoopRegion(loopStart, loopEnd, !loopOn)}
          >LOOP</button>
          <span className="arr-pos">{ticksToBBS(Math.round(engine.arrSeekTicks), barT, beatTicks())}</span>
        </div>
        <div className="arr-corner-sub" data-info="Markers & time selection live in the strip to the right" />
        <div className="arr-heads-inner" ref={headsRef}>
          {rows.map(r => {
            const tid = r.id
            const isMaster = r.kind === 'master'
            const autoOpen = r.autoOpen
            const borderColor = isMaster ? 'var(--accent2)' : CLIP_COLORS[r.color]
            // dot in the dropdown for any parameter that already holds automation
            const autoKeys = autoOpen ? new Set(trackAutoKeys(tid)) : new Set<string>()
            return (
              <React.Fragment key={tid}>
                <div className={`arr-head ${isMaster ? 'master' : ''} ${r.kind === 'bus' ? 'bus' : ''}`} style={{ height: LANE_H, borderLeftColor: borderColor }}
                  onClick={() => { setUI({ selTrackId: tid, detailOpen: true, detailTab: 'devices' }) }}
                  onContextMenu={e => { if (!isMaster) openMenu(e, trackHeaderMenu(tid, () => setRenamingTid(tid), true)) }}
                  onDragOver={e => { if (r.clips) e.preventDefault() }}
                  onDrop={e => { if (!r.clips) return; const dk = e.dataTransfer.getData('stg/drumkit'); if (dk) assignKitToTrack(tid, dk) }}
                  data-info={isMaster ? 'Master bus — click to edit master effects; toggle its automation lane below' : 'Click to select & open devices. Right-click for track options. Drop a drum kit here.'}>
                  <div className="arr-head-top">
                    {(!isMaster && renamingTid === tid)
                      ? <InlineRename value={r.name} onDone={v => { setRenamingTid(null); if (v) renameTrack(tid, v) }} />
                      : <span className="arr-head-name" onDoubleClick={e => { e.stopPropagation(); if (!isMaster) setRenamingTid(tid) }}>{r.name}</span>}
                    {!isMaster && (
                      <>
                        <button className={`tbtn mute ${r.mute ? 'on' : ''}`} data-info="Mute" onClick={e => { e.stopPropagation(); setTrackMix(tid, { mute: !r.mute }) }}>M</button>
                        <button className={`tbtn solo ${r.solo ? 'on' : ''}`} data-info="Solo" onClick={e => { e.stopPropagation(); setTrackMix(tid, { solo: !r.solo }) }}>S</button>
                      </>
                    )}
                  </div>
                  <div className="arr-head-mix" onClick={e => e.stopPropagation()} onDoubleClick={e => e.stopPropagation()}>
                    {isMaster
                      ? <HFader value={meta.get('masterGain') ?? 0} onChange={v => setMetaField('Master volume', 'masterGain', v)} />
                      : (
                        <>
                          <HFader value={r.track!.get('gain') ?? 0} onChange={v => setTrackMix(tid, { gain: v })} />
                          <Knob spec={PAN_SPEC} value={r.track!.get('pan') ?? 0} onChange={v => setTrackMix(tid, { pan: v })} size={20} />
                        </>
                      )}
                  </div>
                  <button className={`arr-auto-toggle ${autoOpen ? 'on' : ''}`}
                    data-info="Show/hide this row's automation lane"
                    onClick={e => { e.stopPropagation(); setAutoOpen(tid, !autoOpen) }}>
                    <span className="arr-auto-caret">▸</span> automation
                  </button>
                </div>
                {autoOpen && (
                  <div className={`arr-head-auto ${isMaster ? 'master' : ''}`} style={{ height: LANE_H, borderLeftColor: borderColor }}
                    onClick={e => e.stopPropagation()}>
                    <span className="arr-auto-title">Automation</span>
                    <select className="auto-select" value={r.autoParam}
                      onChange={e => setAutoParam(tid, e.target.value)}
                      data-info="Parameter automated by this lane · • marks parameters that already have automation">
                      {autoTargets(tid).map(o => <option key={o.key} value={o.key}>{(autoKeys.has(o.key) ? '• ' : '') + o.label}</option>)}
                    </select>
                  </div>
                )}
              </React.Fragment>
            )
          })}
        </div>
      </div>

      <div className="arr-scroll" ref={scrollRef} onScroll={onScroll}>
        <div className="arr-inner" style={{ width }} onPointerMove={onPointerMove} onPointerUp={onPointerUp}>
          <div
            className="arr-ruler"
            onClick={e => seekFromEvent(e, e.currentTarget)}
            data-info="Click to set the playhead. Drag the loop brace to set the loop region."
          >
            {Array.from({ length: totalBars }, (_, i) => (
              <div key={i} className="ruler-bar" style={{ left: i * zoom, width: zoom }}>
                <span>{i + 1}</span>
              </div>
            ))}
            <div
              className={`loop-brace ${loopOn ? 'on' : ''}`}
              style={{ left: loopCur[0] * pxPerTick, width: (loopCur[1] - loopCur[0]) * pxPerTick }}
              onPointerDown={e => {
                e.stopPropagation()
                const rect = (e.currentTarget as HTMLElement).getBoundingClientRect()
                const rel = (e.clientX - rect.left) / rect.width
                const edge = rel < 0.18 ? 'start' : rel > 0.82 ? 'end' : 'mid'
                setDrag({ type: 'loop', edge, startX: e.clientX, origStart: loopStart, origEnd: loopEnd, cur: [loopStart, loopEnd] })
                capturePointer(e)
              }}
            />
          </div>

          {/* markers / regions / time-selection strip (sticky, under the ruler) */}
          <div
            className="arr-markstrip"
            style={{ width }}
            onPointerDown={e => {
              if (e.button !== 0) return
              if ((e.target as HTMLElement).closest('.arr-marker')) return
              const rect = lanesRef.current!.getBoundingClientRect()
              const t = Math.max(0, (e.clientX - rect.left) / z / pxPerTick)
              setDrag({ type: 'timesel', t0: t, t1: t, moved: false })
              capturePointer(e)
            }}
            onDoubleClick={e => {
              if ((e.target as HTMLElement).closest('.arr-marker')) return
              const rect = lanesRef.current!.getBoundingClientRect()
              const t = Math.max(0, snap((e.clientX - rect.left) / z / pxPerTick, e.shiftKey))
              const name = prompt('Marker name', `Marker ${markersList().length + 1}`)
              if (name != null) addMarker(t, name.trim() || 'Marker')
            }}
            onContextMenu={stripMenu}
            data-info="Drag: select a time range (right-click it for loop/region/delete). Double-click: add a marker. Drag a marker to move it; click to jump there."
          >
            {markersList().map(mk => (
              <div
                key={mk.id}
                className={`arr-marker ${mk.len > 0 ? 'region' : ''}`}
                style={{
                  left: mk.t * pxPerTick,
                  width: mk.len > 0 ? Math.max(6, mk.len * pxPerTick) : undefined,
                  borderColor: CLIP_COLORS[mk.color] ?? 'var(--accent)',
                  background: mk.len > 0 ? `color-mix(in srgb, ${CLIP_COLORS[mk.color] ?? '#888'} 26%, transparent)` : undefined,
                }}
                onPointerDown={e => {
                  if (e.button !== 0) return
                  e.stopPropagation()
                  const startX = e.clientX
                  const t0 = mk.t
                  let moved = false
                  beginVDrag(
                    ev => {
                      const dt = ((ev.clientX - startX) / z) / pxPerTick
                      if (Math.abs(dt) > 2) moved = true
                      if (moved) updateMarker(mk.id, { t: Math.max(0, snap(t0 + dt, ev.shiftKey)) })
                    },
                    () => { if (!moved) engine.seekArr(t0) },
                  )
                }}
                onContextMenu={e => {
                  e.preventDefault(); e.stopPropagation()
                  openMenu(e, [
                    { label: <><Icon name="pencil" size={11} /> Rename</>, fn: () => { const n = prompt('Name', mk.name); if (n?.trim()) updateMarker(mk.id, { name: n.trim() }) } },
                    ...(mk.len > 0 ? [{ label: <><Icon name="loop" size={11} /> Loop this region</>, fn: () => setLoopRegion(mk.t, mk.t + mk.len, true) }] : []),
                    { custom: <ColorRow colors={CLIP_COLORS} onPick={i => updateMarker(mk.id, { color: i })} /> },
                    'sep',
                    { label: <><Icon name="close" size={11} /> Delete</>, fn: () => removeMarker(mk.id), danger: true },
                  ])
                }}
                data-info={mk.len > 0 ? 'Region — click: jump to start · drag: move · right-click: loop/rename/delete' : 'Marker — click: jump here · drag: move · right-click: rename/delete'}
              >
                <span className="arr-marker-name">{mk.name}</span>
              </div>
            ))}
            {(drag?.type === 'timesel' ? [Math.min(drag.t0, drag.t1), Math.max(drag.t0, drag.t1)] as [number, number] : timeSel) && (() => {
              const ts = drag?.type === 'timesel' ? [Math.min(drag.t0, drag.t1), Math.max(drag.t0, drag.t1)] as [number, number] : timeSel!
              return <div className="arr-timesel-seg" style={{ left: ts[0] * pxPerTick, width: Math.max(1, (ts[1] - ts[0]) * pxPerTick) }} />
            })()}
          </div>

          <div
            className="arr-lanes"
            ref={lanesRef}
            style={{ height: lanesHeight }}
            onPointerDown={beginMarquee}
            onDoubleClick={e => {
              const rect = e.currentTarget.getBoundingClientRect()
              const y = (e.clientY - rect.top) / z
              if (rowAtY(y)?.kind === 'auto') return   // automation lanes don't make clips
              const idx = trackIdxAtY(y)
              const row = rows[idx]
              if (!row || !row.clips) return   // buses + master hold no clips
              const t = snap((e.clientX - rect.left) / z / pxPerTick, false)
              const id = addArrClip(row.id, Math.max(0, t), { name: 'Clip', color: row.color, len: barT * 4, notes: {} })
              selectClip({ kind: 'arr', id }, true)
            }}
            onContextMenu={e => {
              // clips & automation lanes own their right-click menu; don't
              // double-fire when the event bubbles up from one of those.
              const el = e.target as HTMLElement
              if (el.closest('.arr-clip') || el.closest('.arr-auto-lane')) return
              const rect = e.currentTarget.getBoundingClientRect()
              const y = (e.clientY - rect.top) / z
              const row = rows[trackIdxAtY(y)]
              if (!row || !row.clips || rowAtY(y)?.kind === 'auto') return   // buses/master/automation hold no clips
              if (!selArrIds.length && !hasArrClipboard()) return   // nothing to copy, paste, or delete here
              const atTick = Math.max(0, snap((e.clientX - rect.left) / z / pxPerTick, e.shiftKey))
              const n = selArrIds.length
              const items: MenuItem[] = []
              if (n) {
                items.push(
                  { label: <><Icon name="grid" size={12} /> Copy {n > 1 ? `(${n})` : ''}</>, fn: () => copyArrSelection(selArrIds) },
                  { label: <><Icon name="grid" size={12} /> Cut</>, fn: () => { copyArrSelection(selArrIds); selArrIds.forEach(id => deleteArrClip(id)); setUI({ selArrIds: [] }) } },
                  { label: <><Icon name="note" size={12} /> Duplicate {n > 1 ? `(${n})` : ''}</>, fn: () => {
                      const newIds = selArrIds.map(id => duplicateArrClip(id)).filter(Boolean) as string[]
                      setUI({ selArrIds: newIds, selClip: null })
                    } },
                  'sep',
                )
              }
              items.push({ label: <><Icon name="grid" size={12} /> Paste</>, fn: () => pasteArrClipboard(atTick, row.id), disabled: !hasArrClipboard() })
              if (n) {
                items.push('sep', { label: <><Icon name="close" size={12} /> Delete {n > 1 ? `${n} selected` : ''}</>, fn: () => { selArrIds.forEach(id => deleteArrClip(id)); setUI({ selArrIds: [] }) }, danger: true })
              }
              openMenu(e, items)
            }}
            onDragOver={e => e.preventDefault()}
            onDrop={e => {
              const loopName = e.dataTransfer.getData('stg/loop')
              const progName = e.dataTransfer.getData('stg/prog')
              const clipSrc = e.dataTransfer.getData('stg/clip')
              const sample = e.dataTransfer.getData('stg/sample')
              const rect = e.currentTarget.getBoundingClientRect()
              const t = Math.max(0, snap((e.clientX - rect.left) / z / pxPerTick, false))
              const dropRow = rows[trackIdxAtY((e.clientY - rect.top) / z)]
              const tid = dropRow?.clips ? dropRow.id : undefined   // only clip rows accept drops
              if (sample && tid) {
                const [sid, nm] = sample.split('::')
                addSampleToArr(tid, t, sid, nm || 'Sample')
              } else if (loopName) {
                const loop = MIDI_LOOPS.find(l => l.name === loopName)
                if (loop && tid) {
                  const built = loop.forDrums ? loop.clip : clipInKey(loop.clip, meta.get('root') ?? 9)
                  addArrClip(tid, t, built, `Loop: ${loop.name}`)
                  toast(`"${built.name}" → arrangement`)
                }
              } else if (progName) {
                const prog = PROGRESSIONS.find(p => p.name === progName)
                if (prog && tid) {
                  const built = progressionClip(prog, meta.get('root') ?? 9)
                  addArrClip(tid, t, built, `Chords: ${prog.name}`)
                  toast(`"${prog.name}" → arrangement`)
                }
              } else if (clipSrc) {
                const [srcT, srcS] = clipSrc.split('|')
                const cm = clips.get(clipKey(srcT, srcS))
                if (cm && tid) {
                  addArrClip(tid, t, clipToJSON(cm as any), 'Send to arrangement')
                  toast('Clip placed in arrangement')
                }
              }
            }}
          >
            {bgRows.map((r, i) => <div key={i} className={`lane-bg ${r.kind === 'auto' ? 'auto' : ''} ${r.kind === 'track' && !r.clips ? 'summing' : ''}`} style={{ top: r.top, height: LANE_H }} />)}
            {Array.from({ length: totalBars }, (_, i) => (
              <div key={i} className="lane-grid" style={{ left: i * zoom }} />
            ))}
            {clipEls}
            {rows.filter(r => r.autoOpen).map(r => (
              <div key={'auto-' + r.id} className="arr-auto-lane" style={{ top: autoTop[r.id], height: LANE_H, width }}>
                <AutomationLane trackId={r.id} paramKey={r.autoParam}
                  width={width} pxPerTick={pxPerTick} height={LANE_H} snapTicks={ui.gridTicks} />
              </div>
            ))}
            {drag?.type === 'marquee' && drag.moved && (
              <div className="arr-marquee" style={{
                left: Math.min(drag.x0, drag.x1), top: Math.min(drag.y0, drag.y1),
                width: Math.abs(drag.x1 - drag.x0), height: Math.abs(drag.y1 - drag.y0),
              }} />
            )}
            {(() => {
              const ts = drag?.type === 'timesel' && drag.moved
                ? [Math.min(drag.t0, drag.t1), Math.max(drag.t0, drag.t1)] as [number, number]
                : timeSel
              return ts && <div className="arr-timesel-band" style={{ left: ts[0] * pxPerTick, width: Math.max(1, (ts[1] - ts[0]) * pxPerTick) }} />
            })()}
            <Playhead pxPerTick={pxPerTick} />
            <GhostPlayheads pxPerTick={pxPerTick} />
          </div>
        </div>
      </div>

      <div className="arr-zoom">
        <button className="icon-btn" onClick={() => setUI({ zoomPxPerBar: clamp(zoom * 0.8, 24, 480) })} data-info="Zoom out (or Ctrl+scroll)">−</button>
        <button className="icon-btn" onClick={() => setUI({ zoomPxPerBar: clamp(zoom * 1.25, 24, 480) })} data-info="Zoom in (or Ctrl+scroll)">＋</button>
      </div>
    </div>
  )
}
