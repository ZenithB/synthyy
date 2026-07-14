// The Session View: tracks as columns, scenes as rows, launchable clips with
// quantized triggering — Ableton's signature surface.

import React, { useRef, useState, useSyncExternalStore } from 'react'
import * as Y from 'yjs'
import { CLIP_COLORS } from '../types'
import {
  tracks, scenes, clips, clipKey, createClip, deleteClipAt, duplicateClipTo,
  renameTrack, setTrackMix, moveTrackBefore,
  addScene, removeScene, renameScene, duplicateScene, sendClipToArr, sendSceneToArr,
  setMetaField, meta, setClipField, trackById, moveArrClip, busList,
} from '../state/doc'
import { engine } from '../audio/engine'
import { setUI, ui, useUI, toast } from '../state/store'
import { useY, useYShallow, useRaf } from './hooks'
import { Fader, MeterBar, Knob, openMenu, ColorRow, MenuItem, InlineRename } from './widgets'
import { peersList, subscribeAwareness, awarenessVersion, setPresence } from '../state/net'
import { Icon } from './icons'
import { TrackInput } from './TrackInput'
import {
  selectClip, selectTrack, copyClipRef, pasteClipTo, hasClipboard,
  addSynthTrack, addDrumTrack, addAudio, addBus, attemptSetOutput,
  duplicateClipToNextScene, loadLoop, loadProgression, trackHeaderMenu, assignKitToTrack,
  placeSampleInSlot,
} from './actions'
import { MIDI_LOOPS, INST_PRESETS, DRUM_KITS, PROGRESSIONS } from '../packs'
import { applyPreset, applyDrumKit } from './actions'

const PAN_SPEC = { key: 'pan', label: 'Pan', min: -1, max: 1, def: 0, fmt: (v: number) => (Math.abs(v) < 0.02 ? 'C' : v < 0 ? `${Math.round(-v * 50)}L` : `${Math.round(v * 50)}R`) }

const NOTE_NM = ['C', 'C#', 'D', 'D#', 'E', 'F', 'F#', 'G', 'G#', 'A', 'A#', 'B']
function Analyzer() {
  const [mode, setMode] = useState<'spectrum' | 'scope' | 'tuner'>('spectrum')
  const ref = React.useRef<HTMLCanvasElement>(null)
  useRaf(() => {
    const c = ref.current
    if (!c) return
    const dpr = window.devicePixelRatio || 1
    const W = 108, H = 46
    if (c.width !== W * dpr) { c.width = W * dpr; c.height = H * dpr; c.style.width = W + 'px'; c.style.height = H + 'px' }
    const ctx = c.getContext('2d')!
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0)
    const css = getComputedStyle(document.documentElement)
    ctx.fillStyle = css.getPropertyValue('--bg0').trim()
    ctx.fillRect(0, 0, W, H)
    const accent = css.getPropertyValue('--accent').trim()
    const accent2 = css.getPropertyValue('--accent2').trim()
    if (mode === 'spectrum') {
      const fft = engine.getSpectrum()
      const bins = 48
      ctx.fillStyle = accent
      for (let i = 0; i < bins; i++) {
        const idx = Math.floor(Math.pow(i / bins, 2) * (fft.length * 0.5))
        const db = fft[idx] ?? -140
        const h = Math.max(0, Math.min(1, (db + 100) / 80)) * H
        ctx.fillRect(i * (W / bins), H - h, W / bins - 0.5, h)
      }
    } else if (mode === 'scope') {
      const w = engine.getWaveform()
      ctx.strokeStyle = accent2
      ctx.lineWidth = 1.2
      ctx.beginPath()
      for (let i = 0; i < W; i++) {
        const v = w[Math.floor(i / W * w.length)] ?? 0
        const y = H / 2 - v * H * 0.45
        i === 0 ? ctx.moveTo(i, y) : ctx.lineTo(i, y)
      }
      ctx.stroke()
    } else {
      const hz = engine.getPitchHz()
      ctx.textAlign = 'center'
      if (hz > 20) {
        const midi = 69 + 12 * Math.log2(hz / 440)
        const nearest = Math.round(midi)
        const cents = Math.round((midi - nearest) * 100)
        ctx.fillStyle = Math.abs(cents) < 6 ? accent2 : accent
        ctx.font = 'bold 18px sans-serif'
        ctx.fillText(`${NOTE_NM[((nearest % 12) + 12) % 12]}${Math.floor(nearest / 12) - 1}`, W / 2, 21)
        ctx.fillStyle = css.getPropertyValue('--dim').trim()
        ctx.font = '10px sans-serif'
        ctx.fillText(`${cents > 0 ? '+' : ''}${cents}¢`, W / 2, 38)
        ctx.strokeStyle = accent2
        ctx.beginPath(); ctx.moveTo(W / 2, 42); ctx.lineTo(W / 2 + cents * 0.8, 42); ctx.stroke()
      } else {
        ctx.fillStyle = css.getPropertyValue('--dim').trim()
        ctx.font = '10px sans-serif'
        ctx.fillText('— play a note —', W / 2, 26)
      }
    }
  })
  const next = () => setMode(m => m === 'spectrum' ? 'scope' : m === 'scope' ? 'tuner' : 'spectrum')
  return (
    <div className="analyzer" onClick={next} data-info="Master analyzer — click to cycle spectrum · scope · tuner">
      <canvas ref={ref} />
      <span className="analyzer-mode">{mode}</span>
    </div>
  )
}

function useEngineTick() {
  return useSyncExternalStore(engine.subscribe, () => engine.version)
}
function usePresence() {
  return useSyncExternalStore(subscribeAwareness, awarenessVersion)
}

// progress strip on a playing clip (rAF, no react re-render)
function ClipProgress({ trackId }: { trackId: string }) {
  const ref = React.useRef<HTMLDivElement>(null)
  useRaf(() => {
    const p = engine.clipProgress(trackId)
    if (ref.current) ref.current.style.width = p === null ? '0%' : `${p * 100}%`
  })
  return <div className="clip-progress"><div ref={ref} /></div>
}

function ClipSlot({ track, sceneId }: { track: Y.Map<any>; sceneId: string }) {
  useEngineTick()
  usePresence()
  const trackId = track.get('id') as string
  const key = clipKey(trackId, sceneId)
  const cm = clips.get(key) as Y.Map<any> | undefined
  // Each slot follows its own clip's direct keys (name/color/len). Note edits
  // live in the nested `notes` map and stay invisible here — the grid observes
  // `clips` shallowly, so a velocity drag no longer re-renders every slot.
  useYShallow(cm ?? null)
  const st = engine.clipState(trackId, sceneId)
  const isSel = useUI(s => !!(s.selClip && s.selClip.kind === 'session' && s.selClip.trackId === trackId && s.selClip.sceneId === sceneId))
  const [renaming, setRenaming] = useState(false)

  const remoteEditors = peersList().filter(p => !p.me && p.state.sel?.clipKey === key)

  const open = () => {
    if (!cm) createClip(trackId, sceneId)
    selectClip({ kind: 'session', trackId, sceneId }, true)
  }

  const onDrop = (e: React.DragEvent) => {
    e.preventDefault()
    const clipSrc = e.dataTransfer.getData('stg/clip')
    const loopName = e.dataTransfer.getData('stg/loop')
    const progName = e.dataTransfer.getData('stg/prog')
    const sample = e.dataTransfer.getData('stg/sample')
    if (sample) {
      // a bank sample dropped on a slot becomes an audio clip (audio tracks only)
      const [sid, nm] = sample.split('::')
      placeSampleInSlot(trackId, sceneId, sid, nm || 'Sample')
      return
    }
    if (clipSrc && clipSrc !== key) {
      const [srcT, srcS] = clipSrc.split('|')
      duplicateClipTo({ kind: 'session', trackId: srcT, sceneId: srcS }, trackId, sceneId)
      if (!e.altKey) deleteClipAt(srcT, srcS)
      selectClip({ kind: 'session', trackId, sceneId })
    } else if (loopName) {
      const loop = MIDI_LOOPS.find(l => l.name === loopName)
      if (loop) loadLoop(loop, trackId, sceneId)
    } else if (progName) {
      const prog = PROGRESSIONS.find(p => p.name === progName)
      if (prog) loadProgression(prog, trackId, sceneId)
    }
  }

  if (!cm) {
    return (
      <div
        className="slot empty"
        data-info="Double-click: create a clip. Drag loops from the browser here."
        onDoubleClick={open}
        onClick={() => selectTrack(trackId)}
        onDragOver={e => e.preventDefault()}
        onDrop={onDrop}
        onContextMenu={e => openMenu(e, [
          { label: 'Create clip', fn: open },
          { label: 'Paste clip', fn: () => pasteClipTo(trackId, sceneId), disabled: !hasClipboard() },
        ])}
      >
        {engine.clipState(trackId, sceneId).playing ? null : <span className="slot-stop-ghost"><Icon name="stopOutline" size={9} /></span>}
      </div>
    )
  }

  const color = CLIP_COLORS[cm.get('color') ?? 0]
  const name = cm.get('name') ?? 'Clip'
  const menuItems: MenuItem[] = [
    { label: <><Icon name="play" size={12} /> Launch</>, fn: () => engine.launchClip(trackId, sceneId) },
    { label: <><Icon name="stopOutline" size={12} /> Stop track</>, fn: () => engine.stopClip(trackId) },
    { label: <><Icon name="pencil" size={12} /> Edit notes</>, fn: open },
    'sep',
    { label: 'Rename', fn: () => setRenaming(true) },
    { custom: <ColorRow colors={CLIP_COLORS} onPick={i => setClipField({ kind: 'session', trackId, sceneId }, 'color', i)} /> },
    { label: 'Duplicate ↓ next scene', fn: () => duplicateClipToNextScene({ kind: 'session', trackId, sceneId }) },
    { label: 'Copy', fn: () => copyClipRef({ kind: 'session', trackId, sceneId }) },
    { label: 'Paste here', fn: () => pasteClipTo(trackId, sceneId), disabled: !hasClipboard() },
    'sep',
    { label: '→ Send to Arrangement @ playhead', fn: () => { sendClipToArr(trackId, sceneId, Math.round(engine.arrSeekTicks)); toast('Sent to arrangement') } },
    { label: 'Delete', fn: () => deleteClipAt(trackId, sceneId), danger: true },
  ]

  return (
    <div
      className={`slot filled ${isSel ? 'selected' : ''} ${st.playing ? 'playing' : ''} ${st.queued ? 'queued' : ''}`}
      style={{ borderLeftColor: color, background: `color-mix(in srgb, ${color} ${st.playing ? 30 : 16}%, var(--bg2))` }}
      data-info="Click ▶ to launch (quantized). Double-click to edit notes. Right-click for more."
      draggable
      onDragStart={e => { e.dataTransfer.setData('stg/clip', key); e.dataTransfer.effectAllowed = 'copyMove' }}
      onDragOver={e => e.preventDefault()}
      onDrop={onDrop}
      onClick={() => selectClip({ kind: 'session', trackId, sceneId })}
      onDoubleClick={open}
      onContextMenu={e => openMenu(e, menuItems)}
    >
      <button
        className="slot-play"
        style={{ color: st.playing ? 'var(--ok)' : color }}
        onClick={e => {
          e.stopPropagation()
          if (st.playing && !st.queued) engine.stopClip(trackId)
          else engine.launchClip(trackId, sceneId)
        }}
      >
        <Icon name={st.playing && !st.stopQueued ? 'stop' : 'play'} size={11} />
      </button>
      {renaming
        ? <InlineRename value={name} onDone={v => { setRenaming(false); if (v) setClipField({ kind: 'session', trackId, sceneId }, 'name', v) }} />
        : <span className="slot-name">{name}</span>}
      {st.playing && <ClipProgress trackId={trackId} />}
      {remoteEditors.length > 0 && (
        <div className="remote-dots">
          {remoteEditors.map(p => <span key={p.id} className="remote-dot" style={{ background: p.state.color }} title={`${p.state.name} is here`} />)}
        </div>
      )}
    </div>
  )
}

// swallow the click that trails a completed column-reorder drag (it would open devices)
let colClickGuard = false

function TrackHeader({ track, onReorder, dragging }: { track: Y.Map<any>; onReorder?: (e: React.PointerEvent) => void; dragging?: boolean }) {
  useEngineTick()   // freeze state lives on the engine
  const trackId = track.get('id') as string
  const isArmed = useUI(s => s.armTrackId === trackId)
  const isSel = useUI(s => s.selTrackId === trackId)
  const [renaming, setRenaming] = useState(false)
  const color = CLIP_COLORS[track.get('color') ?? 0]
  const kind = track.get('kind')

  const menu = trackHeaderMenu(trackId, () => setRenaming(true))

  return (
    <div
      className={`track-head ${isSel ? 'selected' : ''} ${dragging ? 'dragging' : ''}`}
      style={{ borderTopColor: color }}
      onClick={() => { if (colClickGuard) return; selectTrack(trackId); setUI({ detailOpen: true, detailTab: 'devices' }) }}
      onPointerDown={onReorder}
      onContextMenu={e => openMenu(e, menu)}
      onDragOver={e => e.preventDefault()}
      onDrop={e => {
        const presetName = e.dataTransfer.getData('stg/preset')
        const kitName = e.dataTransfer.getData('stg/kit')
        const drumPack = e.dataTransfer.getData('stg/drumkit')
        if (presetName) { selectTrack(trackId); const p = INST_PRESETS.find(x => x.name === presetName); if (p) applyPreset(p) }
        if (kitName) { selectTrack(trackId); applyDrumKit(kitName) }
        if (drumPack) assignKitToTrack(trackId, drumPack)
      }}
      data-info="Click to select & open devices. Right-click for track options."
    >
      <div className="track-title">
        <span className="track-icon"><Icon name={kind === 'drum' ? 'drum' : kind === 'audio' ? 'sampler' : 'wave'} size={12} /></span>
        {renaming
          ? <InlineRename value={track.get('name')} onDone={v => { setRenaming(false); if (v) renameTrack(trackId, v) }} />
          : <span className="track-name" onDoubleClick={() => setRenaming(true)}>{track.get('name')}</span>}
        {engine.isFrozen(trackId) && (
          <span style={{ marginLeft: 4, fontSize: 10, opacity: 0.85, cursor: 'default' }}
            data-info="Frozen — device processing is bounced to audio (right-click to unfreeze)">❄</span>
        )}
      </div>
      <div className="track-btns">
        {kind !== 'bus' && <button
          className={`tbtn arm ${isArmed ? 'on' : ''}`}
          data-info="Arm: route your keyboard/MIDI to this track (and record into it)"
          onClick={e => { e.stopPropagation(); setUI({ armTrackId: isArmed ? null : trackId }) }}
        ><Icon name="rec" size={9} /></button>}
        <button
          className={`tbtn mute ${track.get('mute') ? 'on' : ''}`}
          data-info="Mute track"
          onClick={e => { e.stopPropagation(); setTrackMix(trackId, { mute: !track.get('mute') }) }}
        >M</button>
        <button
          className={`tbtn solo ${track.get('solo') ? 'on' : ''}`}
          data-info="Solo track"
          onClick={e => { e.stopPropagation(); setTrackMix(trackId, { solo: !track.get('solo') }) }}
        >S</button>
      </div>
      <div className="track-mix" onClick={e => e.stopPropagation()}>
        <Fader value={track.get('gain') ?? 0} onChange={v => setTrackMix(trackId, { gain: v })} height={64} />
        <MeterBar getDb={() => engine.meterDb(trackId)} height={64} />
        <Knob spec={PAN_SPEC} value={track.get('pan') ?? 0} onChange={v => setTrackMix(trackId, { pan: v })} size={26} />
      </div>
      {kind === 'audio' && <TrackInput trackId={trackId} />}
      {kind === 'bus' && (
        <div className="bus-out" onClick={e => e.stopPropagation()} data-info="Where this bus's output is routed">
          <span className="bus-out-label">Output ▾</span>
          <select className="device-select" value={track.get('output') ?? 'master'}
            onChange={e => attemptSetOutput(trackId, e.target.value)}>
            <option value="master">Master</option>
            {busList().filter(b => b.get('id') !== trackId).map(b =>
              <option key={b.get('id')} value={b.get('id')}>{b.get('name')}</option>)}
          </select>
        </div>
      )}
    </div>
  )
}

function SceneCell({ scene, index }: { scene: Y.Map<any>; index: number }) {
  const sceneId = scene.get('id') as string
  const [renaming, setRenaming] = useState(false)
  return (
    <div
      className="scene-cell"
      data-info={`Launch scene ${index + 1} (also: press ${index + 1} on your keyboard)`}
      onContextMenu={e => openMenu(e, [
        { label: 'Rename', fn: () => setRenaming(true) },
        { label: 'Duplicate scene', fn: () => duplicateScene(sceneId) },
        { label: '→ Send scene to Arrangement', fn: () => { sendSceneToArr(sceneId, Math.round(engine.arrSeekTicks)); toast('Scene sent to arrangement') } },
        'sep',
        { label: 'Delete scene', fn: () => removeScene(sceneId), danger: true },
      ])}
    >
      <button className="scene-play" onClick={() => engine.launchScene(sceneId)}><Icon name="play" size={11} /></button>
      {renaming
        ? <InlineRename value={scene.get('name')} onDone={v => { setRenaming(false); if (v) renameScene(sceneId, v) }} />
        : <span className="scene-name" onDoubleClick={() => setRenaming(true)}>{scene.get('name')}</span>}
    </div>
  )
}

export function SessionView() {
  useY(tracks)
  useY(scenes)
  // Shallow: slot creation/deletion only. Content edits (notes, envelopes)
  // re-render the individual ClipSlot via its own shallow subscription.
  useYShallow(clips)
  useY(meta)
  useEngineTick()
  const masterSel = useUI(s => s.selTrackId === 'master')

  const all = tracks.toArray()
  const instr = all.filter(t => t.get('kind') !== 'bus')
  const buses = all.filter(t => t.get('kind') === 'bus')   // bus tracks group on the right, beside master

  // ---- drag a track header left/right to reorder columns ----
  // A vertical preview line marks the drop slot; the move commits on release.
  // Columns reorder only within their display group (instruments / buses).
  const scrollInnerRef = useRef<HTMLDivElement>(null)
  const [colDrag, setColDrag] = useState<{ tid: string; x: number } | null>(null)
  const beginColDrag = (e: React.PointerEvent, tid: string, isBus: boolean) => {
    if (e.button !== 0) return
    // interactive children (arm/M/S, fader, pan, bus output…) keep their gestures
    if ((e.target as HTMLElement).closest('button, input, select, .knob, .fader, .hfader, .bus-out')) return
    if (trackById(tid)?.get('locked')) return
    const cont = scrollInnerRef.current
    if (!cont) return
    const z = ui.uiZoom || 1
    const startX = e.clientX, startY = e.clientY
    let active = false
    let before: string | null = tid   // current drop target (== tid ⇒ no-op)
    const move = (ev: PointerEvent) => {
      if (!active && Math.abs(ev.clientX - startX) + Math.abs(ev.clientY - startY) < 5) return
      active = true
      const cols = Array.from(cont.querySelectorAll<HTMLElement>(`.track-col[data-kind="${isBus ? 'bus' : 'instr'}"]`))
      if (cols.length < 2) return
      // drop slots: the left edge of each group member, plus the group's right end
      const slots = cols.map(c => ({ x: c.getBoundingClientRect().left, before: c.dataset.tid as string | null }))
      slots.push({ x: cols[cols.length - 1].getBoundingClientRect().right, before: null })
      let best = slots[0]
      for (const s of slots) if (Math.abs(s.x - ev.clientX) < Math.abs(best.x - ev.clientX)) best = s
      before = best.before
      setColDrag({ tid, x: (best.x - cont.getBoundingClientRect().left) / z })
    }
    const up = () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
      setColDrag(null)
      if (active) {
        colClickGuard = true
        setTimeout(() => { colClickGuard = false }, 150)
        moveTrackBefore(tid, before)
      }
    }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
  }

  // `push` makes a column the leader of the right-aligned group (margin-left:auto)
  const trackCol = (t: Y.Map<any>, push = false) => {
    const isBus = t.get('kind') === 'bus'
    const tid = t.get('id') as string
    return (
      <div className={`track-col ${isBus ? 'bus-col' : ''} ${push ? 'push' : ''}`} key={tid} data-tid={tid} data-kind={isBus ? 'bus' : 'instr'}>
        <TrackHeader track={t} dragging={colDrag?.tid === tid}
          onReorder={e => beginColDrag(e, tid, isBus)} />
        {isBus
          ? <div className="slots bus-slots" data-info="Buses carry audio from sends — they hold no clips" />
          : <>
              <div className="slots">
                {scenes.toArray().map(s => (
                  <ClipSlot key={s.get('id')} track={t} sceneId={s.get('id')} />
                ))}
              </div>
              <button className="track-stop" data-info="Stop this track's clip" onClick={() => engine.stopClip(t.get('id'))}><Icon name="stopOutline" size={11} /></button>
            </>}
      </div>
    )
  }

  return (
    <div className="session" onClick={e => { if (e.target === e.currentTarget) selectClip(null) }}>
      <div className="session-scroll" ref={scrollInnerRef}>
        {colDrag && <div className="session-drop-line" style={{ left: colDrag.x }} />}
        {instr.map(t => trackCol(t))}

        <div className="track-col add-col">
          <div className="add-track-box">
            <button className="add-btn" onClick={() => addSynthTrack()} data-info="Add a synth track">＋ Synth</button>
            <button className="add-btn" onClick={() => addDrumTrack()} data-info="Add a drum track">＋ Drums</button>
            <button className="add-btn" onClick={() => addAudio()} data-info="Add a blank audio track (import or record audio clips)">＋ Audio</button>
            <button className="add-btn" onClick={() => addBus()} data-info="Add a bus — an fx-only track that other tracks send to">＋ Bus</button>
          </div>
        </div>

        {buses.map((t, bi) => trackCol(t, bi === 0))}

        <div className={`track-col scene-col ${buses.length ? '' : 'push'}`}>
          {/* Master sits at the top styled like a track header, so the scene
              launch buttons below line up row-for-row with the clip slots. */}
          <div className={`track-head master-head ${masterSel ? 'selected' : ''}`} data-info="Master output, analyzer & volume">
            <div className="track-title master-title" data-info="Click to edit master-bus effects (the whole mix runs through them)"
              onClick={() => { selectTrack('master'); setUI({ detailOpen: true, detailTab: 'devices' }) }}>
              <span className="track-icon"><Icon name="spectrum" size={12} /></span>
              <span className="track-name">Master</span>
              <Icon name="chevR" size={11} />
            </div>
            <Analyzer />
            <div className="master-mix">
              <Fader value={meta.get('masterGain') ?? 0} onChange={v => setMetaField('Master volume', 'masterGain', v)} height={58} />
              <MeterBar getDb={() => engine.masterDb()} height={58} />
              <button className="stop-all" onClick={() => engine.stopAllClips()} data-info="Stop all clips (transport keeps rolling)"><Icon name="stopOutline" size={11} /> All</button>
            </div>
          </div>
          <div className="slots">
            {scenes.toArray().map((s, i) => <SceneCell key={s.get('id')} scene={s} index={i} />)}
          </div>
          <button className="add-scene" onClick={() => addScene()} data-info="Add a scene (row of clips)">＋ Scene</button>
        </div>
      </div>
    </div>
  )
}
