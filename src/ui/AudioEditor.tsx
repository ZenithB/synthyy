// Audio-clip editor: an interactive waveform (crop / slip / fade handles + a
// split cursor) plus gain, pitch (±48 st) with a fine-tune (cents) option,
// loop crossfade, reverse and loop. Every parameter is stored on the clip and
// played back identically live and in the offline export (see audioclip.ts).

import React, { useEffect, useRef, useState } from 'react'
import * as Y from 'yjs'
import { BAR } from '../types'
import { getClipMap, setAudioField, setClipField, splitArrClip, meta } from '../state/doc'
import { useUI, setUI, ui } from '../state/store'
import { useY } from './hooks'
import { Knob, NumberDrag } from './widgets'
import { Icon } from './icons'
import { getSampleBuffer } from '../audio/samples'
import { ParamSpec, fmtDb } from '../audio/schema'
import { fitCanvas } from './canvas'

const PPQ = BAR / 4
const GAIN_SPEC: ParamSpec = { key: 'gainDb', label: 'Gain', min: -24, max: 6, def: 0, fmt: fmtDb }
const XFADE_SPEC: ParamSpec = { key: 'xfade', label: 'Xfade', min: 0, max: 0.5, def: 0, fmt: v => `${Math.round(v * 1000)}ms` }
const MINDUR = 0.02 // shortest crop region (s)

export function AudioEditor() {
  const selClip = useUI(s => s.selClip)
  const clipMap = getClipMap(selClip) as Y.Map<any> | null
  useY(clipMap ?? undefined)
  useY(meta)
  const canvasRef = useRef<HTMLCanvasElement>(null)
  const boxRef = useRef<HTMLDivElement>(null)
  const [cursor, setCursor] = useState<number | null>(null) // split point, sample seconds
  const cursorRef = useRef<number | null>(null)
  cursorRef.current = cursor

  const sampleId = clipMap?.get('sampleId') || ''
  const buf = clipMap ? getSampleBuffer(sampleId) : undefined
  const bufDur = buf?.duration ?? 0

  // ---- current clip params ----
  const offset = clipMap?.get('offset') ?? 0
  const durRaw = clipMap?.get('dur') ?? 0
  const regionStart = Math.max(0, Math.min(offset, bufDur))
  const regionDur = bufDur ? (durRaw > 0 ? Math.min(durRaw, bufDur - regionStart) : bufDur - regionStart) : 0
  const regionEnd = regionStart + regionDur
  const rev = !!clipMap?.get('rev')
  const loop = !!clipMap?.get('loop')
  const fadeIn = clipMap?.get('fadeIn') ?? 0
  const fadeOut = clipMap?.get('fadeOut') ?? 0
  const bpm = meta.get('bpm') ?? 120
  const ticksToSec = (t: number) => (t / PPQ) * (60 / bpm)
  const secToTicks = (s: number) => Math.max(0, Math.round(s * (bpm / 60) * PPQ))

  // ---- draw ----
  // Cached min/max peaks per (buffer, pixel width) — computed once, not on every
  // redraw (the Audacity peak-cache trick). Crop/fade/cursor edits reuse it; only
  // a new sample or a resize recomputes. peaksRef survives effect re-runs.
  const peaksRef = useRef<{ key: string; min: Float32Array; max: Float32Array } | null>(null)
  useEffect(() => {
    const computePeaks = (data: Float32Array, W: number) => {
      const step = Math.max(1, Math.floor(data.length / W))
      const min = new Float32Array(W), max = new Float32Array(W)
      for (let x = 0; x < W; x++) {
        let lo = 1, hi = -1
        const base = x * step
        for (let j = 0; j < step; j++) { const v = data[base + j] || 0; if (v < lo) lo = v; if (v > hi) hi = v }
        min[x] = lo; max[x] = hi
      }
      return { min, max }
    }
    const draw = () => {
      const canvas = canvasRef.current, box = boxRef.current
      if (!canvas || !box) return
      const b = getSampleBuffer(sampleId)
      const W = box.clientWidth, H = box.clientHeight
      const { sx, sy } = fitCanvas(canvas, W, H)
      const ctx = canvas.getContext('2d')!
      ctx.setTransform(sx, 0, 0, sy, 0, 0)
      const css = getComputedStyle(document.documentElement)
      const C = (v: string, f = '#888') => css.getPropertyValue(v).trim() || f
      ctx.clearRect(0, 0, W, H)
      ctx.fillStyle = C('--bg0'); ctx.fillRect(0, 0, W, H)
      if (!b) { ctx.fillStyle = C('--dim'); ctx.font = '12px sans-serif'; ctx.fillText('loading waveform…', 10, H / 2); return }
      const dur = b.duration
      const xOf = (s: number) => (s / dur) * W
      // peak cache: recompute only when the buffer or the pixel width changes
      const key = `${sampleId}:${W}:${b.length}`
      if (!peaksRef.current || peaksRef.current.key !== key) {
        peaksRef.current = { key, ...computePeaks(b.getChannelData(0), W) }
      }
      const { min: peakMin, max: peakMax } = peaksRef.current
      const mid = H / 2
      const accent = C('--accent2', '#559DA0')
      const xS = xOf(regionStart), xE = xOf(regionEnd)
      // waveform (bright inside the region, dim outside)
      for (let x = 0; x < W; x++) {
        ctx.strokeStyle = accent
        ctx.globalAlpha = x >= xS && x <= xE ? 0.9 : 0.22
        ctx.beginPath(); ctx.moveTo(x + 0.5, mid + peakMin[x] * mid * 0.92); ctx.lineTo(x + 0.5, mid + peakMax[x] * mid * 0.92); ctx.stroke()
      }
      ctx.globalAlpha = 1
      // dim the cropped-out areas
      ctx.fillStyle = 'rgba(0,0,0,0.45)'
      if (xS > 0) ctx.fillRect(0, 0, xS, H)
      if (xE < W) ctx.fillRect(xE, 0, W - xE, H)
      // fade ramps
      const fInPx = xOf(regionStart + Math.min(ticksToSec(fadeIn), regionDur))
      const fOutPx = xOf(regionEnd - Math.min(ticksToSec(fadeOut), regionDur))
      ctx.strokeStyle = C('--accent', '#FFB02E'); ctx.fillStyle = 'rgba(255,176,46,0.12)'
      if (fInPx > xS) {
        ctx.beginPath(); ctx.moveTo(xS, H); ctx.lineTo(fInPx, 0); ctx.lineTo(xS, 0); ctx.closePath(); ctx.fill()
        ctx.beginPath(); ctx.moveTo(xS, H); ctx.lineTo(fInPx, 0); ctx.stroke()
      }
      if (fOutPx < xE) {
        ctx.beginPath(); ctx.moveTo(xE, H); ctx.lineTo(fOutPx, 0); ctx.lineTo(xE, 0); ctx.closePath(); ctx.fill()
        ctx.beginPath(); ctx.moveTo(xE, H); ctx.lineTo(fOutPx, 0); ctx.stroke()
      }
      // crop edges
      ctx.strokeStyle = accent; ctx.lineWidth = 2
      ctx.beginPath(); ctx.moveTo(xS, 0); ctx.lineTo(xS, H); ctx.moveTo(xE, 0); ctx.lineTo(xE, H); ctx.stroke()
      ctx.lineWidth = 1
      // fade handles (top dots)
      ctx.fillStyle = C('--accent', '#FFB02E')
      ctx.beginPath(); ctx.arc(fInPx, 4, 4, 0, 7); ctx.fill()
      ctx.beginPath(); ctx.arc(fOutPx, 4, 4, 0, 7); ctx.fill()
      // split cursor
      const cur = cursorRef.current
      if (cur != null && cur >= regionStart && cur <= regionEnd) {
        ctx.strokeStyle = C('--text', '#fff'); ctx.setLineDash([4, 3])
        ctx.beginPath(); const cx = xOf(cur); ctx.moveTo(cx, 0); ctx.lineTo(cx, H); ctx.stroke(); ctx.setLineDash([])
      }
    }
    draw()
    // Poll ONLY until the buffer decodes (then peaks cache and stay put); a
    // ResizeObserver handles width changes — no perpetual recompute timer.
    let id = 0 as any
    if (!getSampleBuffer(sampleId)) {
      id = setInterval(() => { const ready = !!getSampleBuffer(sampleId); draw(); if (ready) { clearInterval(id); id = 0 } }, 200)
    }
    const ro = new ResizeObserver(() => draw())
    if (boxRef.current) ro.observe(boxRef.current)
    return () => { if (id) clearInterval(id); ro.disconnect() }
  }, [sampleId, offset, durRaw, fadeIn, fadeOut, regionStart, regionEnd, regionDur, cursor, rev, bpm])

  if (!selClip || !clipMap || !clipMap.get('audio')) {
    return <div className="roll-empty">Select an audio clip to edit it</div>
  }

  // ---- waveform pointer interaction (crop / slip / fade / cursor) ----
  const onPointerDown = (e: React.PointerEvent) => {
    if (!buf || e.button !== 0) return
    const z = ui.uiZoom || 1
    const canvas = canvasRef.current!
    const W = canvas.clientWidth
    const rect = canvas.getBoundingClientRect()
    const xToSec = (clientX: number) => ((clientX - rect.left) / z) / W * bufDur
    const px = (s: number) => (s / bufDur) * W
    const x0 = (e.clientX - rect.left) / z
    const y0 = (e.clientY - rect.top) / z
    const xS = px(regionStart), xE = px(regionEnd)
    const fInX = px(regionStart + Math.min(ticksToSec(fadeIn), regionDur))
    const fOutX = px(regionEnd - Math.min(ticksToSec(fadeOut), regionDur))
    let mode: 'cropL' | 'cropR' | 'fadeIn' | 'fadeOut' | 'slip' | 'cursor'
    if (y0 < 14 && Math.abs(x0 - fInX) < 9) mode = 'fadeIn'
    else if (y0 < 14 && Math.abs(x0 - fOutX) < 9) mode = 'fadeOut'
    else if (Math.abs(x0 - xS) < 7) mode = 'cropL'
    else if (Math.abs(x0 - xE) < 7) mode = 'cropR'
    else if (x0 > xS && x0 < xE) mode = 'slip'
    else mode = 'cursor'

    const grabSec = xToSec(e.clientX)
    const origOffset = regionStart
    let moved = false
    try { canvas.setPointerCapture(e.pointerId) } catch { /* ok */ }
    const move = (ev: PointerEvent) => {
      moved = true
      const s = Math.max(0, Math.min(bufDur, xToSec(ev.clientX)))
      if (mode === 'cropL') {
        const ns = Math.max(0, Math.min(s, regionEnd - MINDUR))
        setAudioField(selClip, 'offset', ns, 'Crop clip'); setAudioField(selClip, 'dur', regionEnd - ns, 'Crop clip')
      } else if (mode === 'cropR') {
        const ne = Math.max(regionStart + MINDUR, Math.min(s, bufDur))
        setAudioField(selClip, 'dur', ne - regionStart, 'Crop clip')
      } else if (mode === 'fadeIn') {
        setAudioField(selClip, 'fadeIn', secToTicks(Math.max(0, Math.min(s - regionStart, regionDur))), 'Fade in')
      } else if (mode === 'fadeOut') {
        setAudioField(selClip, 'fadeOut', secToTicks(Math.max(0, Math.min(regionEnd - s, regionDur))), 'Fade out')
      } else if (mode === 'slip') {
        const ns = Math.max(0, Math.min(origOffset + (xToSec(ev.clientX) - grabSec), bufDur - regionDur))
        setAudioField(selClip, 'offset', ns, 'Move audio')
      }
    }
    const up = (ev: PointerEvent) => {
      window.removeEventListener('pointermove', move); window.removeEventListener('pointerup', up)
      if (!moved && mode === 'cursor') {
        const s = xToSec(ev.clientX)
        setCursor(s >= regionStart && s <= regionEnd ? s : null)
      } else if (!moved && mode === 'slip') {
        setCursor(xToSec(ev.clientX)) // a click inside the region sets the split point
      }
    }
    window.addEventListener('pointermove', move); window.addEventListener('pointerup', up)
  }

  const isArr = selClip.kind === 'arr'
  const doSplit = () => {
    if (!isArr || !bufDur) return
    const len = clipMap.get('len') ?? BAR
    const cutSec = (cursor ?? regionStart + regionDur / 2) - regionStart
    if (cutSec <= MINDUR || cutSec >= regionDur - MINDUR) return
    const cutTicks = Math.round((cutSec / regionDur) * len)
    splitArrClip(selClip.id, cutTicks, { leftDur: cutSec, rightOffset: regionStart + cutSec, rightDur: regionDur - cutSec })
    setCursor(null)
  }
  const nudgePitch = (d: number) => setAudioField(selClip, 'pitch', Math.max(-48, Math.min(48, (clipMap.get('pitch') ?? 0) + d)), 'Pitch')

  return (
    <div className="audio-editor">
      <div className="roll-toolbar">
        <span className="device-title"><Icon name="sampler" size={13} /> {clipMap.get('sampleName') || 'Audio'}</span>
        <button className={`tbtn ${loop ? 'on' : ''}`} data-info="Loop the sample for the clip's length"
          onClick={() => setAudioField(selClip, 'loop', loop ? 0 : 1)}><Icon name="loop" size={12} /> Loop</button>
        <button className={`tbtn ${rev ? 'on' : ''}`} data-info="Play the sample backwards"
          onClick={() => setAudioField(selClip, 'rev', rev ? 0 : 1)}><Icon name="reverse" size={12} /> Reverse</button>
        {isArr && <button className="tbtn" data-info="Cut the clip in two at the dashed cursor (click the waveform to place it)"
          onClick={doSplit}><Icon name="grid" size={12} /> Split</button>}
        <label className="roll-field" data-info="Clip length (loop region) in bars">
          Len
          <select value={Math.max(0.25, Math.round((clipMap.get('len') ?? BAR) / BAR * 4) / 4)} onChange={e => setClipField(selClip, 'len', Math.round(+e.target.value * BAR), 'Clip length')}>
            {[0.25, 0.5, 1, 2, 4, 8, 16].map(b => <option key={b} value={b}>{b} bar{b !== 1 ? 's' : ''}</option>)}
          </select>
        </label>
        <button className="icon-btn" style={{ marginLeft: 'auto' }} onClick={() => setUI({ detailOpen: false })} data-info="Close editor (Esc)"><Icon name="close" size={12} /></button>
      </div>
      <div className="audio-body">
        <div className="wave-box" ref={boxRef}
          data-info="Drag edges to crop · drag the middle to slip · drag the top dots to fade · click to set the split cursor">
          <canvas ref={canvasRef} onPointerDown={onPointerDown} />
        </div>
        <div className="audio-knobs">
          <Knob spec={GAIN_SPEC} value={clipMap.get('gainDb') ?? 0} onChange={v => setAudioField(selClip, 'gainDb', v)} size={40} />
          <div className="audio-pitch" data-info="Transpose ±48 semitones. Use Fine for cents.">
            <span className="audio-pitch-label">Pitch</span>
            <div className="audio-pitch-row">
              <button className="step-btn" onClick={() => nudgePitch(-12)} data-info="Down an octave">−12</button>
              <button className="step-btn" onClick={() => nudgePitch(-1)}>−</button>
              <NumberDrag value={clipMap.get('pitch') ?? 0} min={-48} max={48} suffix="st" info="Drag / double-click to set semitones (±48)"
                onChange={v => setAudioField(selClip, 'pitch', v)} />
              <button className="step-btn" onClick={() => nudgePitch(1)}>+</button>
              <button className="step-btn" onClick={() => nudgePitch(12)} data-info="Up an octave">+12</button>
            </div>
            <div className="audio-pitch-fine" data-info="Fine tune in cents (±100)">
              <span>Fine</span>
              <NumberDrag value={clipMap.get('cents') ?? 0} min={-100} max={100} suffix="¢" info="Drag / double-click — fine tune in cents"
                onChange={v => setAudioField(selClip, 'cents', v)} />
            </div>
          </div>
          <Knob spec={{ ...XFADE_SPEC, label: loop ? 'Xfade' : 'Xfade (loop)' }} value={clipMap.get('xfade') ?? 0} onChange={v => setAudioField(selClip, 'xfade', v)} size={40} />
        </div>
      </div>
    </div>
  )
}
