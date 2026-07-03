// Arrangement View: the linear timeline. Clips drag/resize/duplicate with bar
// snapping, the ruler seeks, the loop brace sets the loop region, and you can
// see your collaborators' playheads drift by as ghosts.

import React, { useRef, useState, useSyncExternalStore } from 'react'
import * as Y from 'yjs'
import { BAR, CLIP_COLORS, clamp, ticksToBBS } from '../types'
import {
  tracks, arr, meta, addArrClip, moveArrClip, resizeArrClip, deleteArrClip,
  duplicateArrClip, setClipField, setLoopRegion, arrEndTicks, setTrackMix, clipToJSON,
  clips, clipKey, setAutoOpen, setAutoParam, renameTrack, setMetaField, trackAutoKeys, masterAuto,
  barTicks, beatTicks,
} from '../state/doc'
import { autoTargets } from '../audio/params'
import { AutomationLane } from './AutomationLane'
import { engine } from '../audio/engine'
import { setUI, ui, useUI, toast } from '../state/store'
import { useY, useRaf } from './hooks'
import { openMenu, ColorRow, MenuItem, capturePointer, Knob, HFader, InlineRename } from './widgets'
import { selectClip, trackHeaderMenu, assignKitToTrack, addSampleToArr, copyArrSelection, hasArrClipboard, pasteArrClipboard } from './actions'
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

type MoveItem = { id: string; origStart: number; origLane: number }
type DragState =
  | { type: 'move'; id: string; startX: number; startY: number; origStart: number; origLane: number; dx: number; dLane: number; copy: boolean; items: MoveItem[] }
  | { type: 'resize'; id: string; startX: number; origLen: number; dLen: number }
  | { type: 'loop'; edge: 'start' | 'end' | 'mid'; startX: number; origStart: number; origEnd: number; cur: [number, number] }
  | { type: 'marquee'; x0: number; y0: number; x1: number; y1: number; moved: boolean }
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
        data-info="Drag to move (Alt = copy, Shift = fine). Drag right edge to resize. Double-click to edit."
      >
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
