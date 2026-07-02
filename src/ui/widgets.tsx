// Shared controls: Knob, Fader, Meter, draggable number, modal, context menu.

import React, { useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { ParamSpec, normFromSpec, valueFromSpec } from '../audio/schema'
import { clamp } from '../types'
import { useRaf } from './hooks'
import { Icon } from './icons'

export function capturePointer(e: React.PointerEvent) {
  try { (e.target as HTMLElement).setPointerCapture(e.pointerId) } catch { /* synthetic events */ }
}

/**
 * Robust vertical drag for knobs/faders. Listens on window (not the element)
 * so the release is ALWAYS caught — even if the control re-renders mid-drag,
 * the pointer leaves the element, the window blurs, or the OS cancels the
 * gesture. This replaces the setPointerCapture approach, which silently leaked
 * captures when the knob re-rendered on each value change, leaving it "stuck"
 * to the cursor. Returns nothing; the caller wires onMove/onEnd.
 */
export function beginVDrag(onMove: (e: PointerEvent) => void, onEnd?: () => void) {
  // Coalesce move events to one onMove per animation frame. Pointer events can
  // fire at 120–250Hz; every onMove here is typically a doc mutation that fans
  // out to undo history, engine observers and the P2P mesh — anything faster
  // than the display refresh is pure waste. The final position always flushes
  // on release so the committed value never lags the cursor.
  let raf = 0
  let pending: PointerEvent | null = null
  const flush = () => {
    raf = 0
    if (pending) { const e = pending; pending = null; onMove(e) }
  }
  const move = (e: PointerEvent) => {
    pending = e
    if (!raf) raf = requestAnimationFrame(flush)
  }
  const end = () => {
    window.removeEventListener('pointermove', move)
    window.removeEventListener('pointerup', end)
    window.removeEventListener('pointercancel', end)
    window.removeEventListener('blur', end)
    document.removeEventListener('visibilitychange', onHidden)
    if (raf) cancelAnimationFrame(raf)
    flush()
    onEnd?.()
  }
  const onHidden = () => { if (document.hidden) end() }
  window.addEventListener('pointermove', move)
  window.addEventListener('pointerup', end)
  window.addEventListener('pointercancel', end)
  window.addEventListener('blur', end)
  document.addEventListener('visibilitychange', onHidden)
}

// ---------------- Knob ----------------

// Curve lives in schema.ts (shared with automation lanes + engine). The knob
// adds integer/enum snapping on top of the raw mapping.
function toNorm(v: number, s: ParamSpec) {
  return normFromSpec(s, v)
}
function fromNorm(n: number, s: ParamSpec) {
  let v = valueFromSpec(s, n)
  if (s.int || s.steps) v = Math.round(v)
  return clamp(v, s.min, s.max)
}

export function Knob({ spec, value, onChange, size = 36, accent, disabled = false, disabledInfo }: {
  spec: ParamSpec
  value: number
  onChange: (v: number) => void
  size?: number
  accent?: string
  disabled?: boolean        // greyed + inert (e.g. Decay on a sampled drum pad)
  disabledInfo?: string
}) {
  const [drag, setDrag] = useState(false)
  const norm = toNorm(clamp(value ?? spec.def, spec.min, spec.max), spec)

  const onPointerDown = (e: React.PointerEvent) => {
    if (disabled || e.button !== 0) return
    e.preventDefault()
    const startY = e.clientY
    const startNorm = norm
    setDrag(true)
    beginVDrag(
      ev => onChange(fromNorm(startNorm + (startY - ev.clientY) / (ev.shiftKey ? 900 : 180), spec)),
      () => setDrag(false),
    )
  }

  const a0 = -135
  const a1 = a0 + norm * 270
  const r = size / 2
  const arc = (deg: number) => {
    const rad = ((deg - 90) * Math.PI) / 180
    return [r + (r - 3) * Math.cos(rad), r + (r - 3) * Math.sin(rad)]
  }
  const [x1, y1] = arc(a0)
  const [x2, y2] = arc(a1)
  const large = a1 - a0 > 180 ? 1 : 0
  const display = spec.steps ? spec.steps[clamp(Math.round(value), 0, spec.steps.length - 1)] : (spec.fmt ? spec.fmt(value) : value.toFixed(2))

  return (
    <div className={`knob ${drag ? 'dragging' : ''} ${disabled ? 'disabled' : ''}`} style={{ width: size + 12 }}
      data-info={disabled ? (disabledInfo ?? `${spec.label} has no effect here`) : `${spec.label}: drag to change, Shift = fine, double-click = reset`}
      onDoubleClick={() => { if (!disabled) onChange(spec.def) }}>
      <svg width={size} height={size} onPointerDown={onPointerDown}>
        <circle cx={r} cy={r} r={r - 3} className="knob-track" />
        {norm > 0.004 && <path d={`M ${x1} ${y1} A ${r - 3} ${r - 3} 0 ${large} 1 ${x2} ${y2}`} className="knob-arc" style={accent ? { stroke: accent } : undefined} />}
        <line x1={r} y1={r} x2={r + (r - 7) * Math.cos(((a1 - 90) * Math.PI) / 180)} y2={r + (r - 7) * Math.sin(((a1 - 90) * Math.PI) / 180)} className="knob-needle" />
      </svg>
      <div className="knob-value">{drag ? display : spec.label}</div>
      {drag && <div className="knob-tip">{display}</div>}
    </div>
  )
}

// ---------------- Fader + Meter ----------------

export function Fader({ value, min = -48, max = 6, onChange, height = 76 }: {
  value: number; min?: number; max?: number; onChange: (v: number) => void; height?: number
}) {
  const norm = clamp((value - min) / (max - min), 0, 1)
  const ref = useRef<HTMLDivElement>(null)
  const drag = (e: React.PointerEvent) => {
    if (e.button !== 0) return
    e.preventDefault()
    const rect = ref.current!.getBoundingClientRect()
    const apply = (ev: PointerEvent) => {
      const n = clamp(1 - (ev.clientY - rect.top) / rect.height, 0, 1)
      onChange(Math.round((min + n * (max - min)) * 10) / 10)
    }
    apply(e.nativeEvent)
    beginVDrag(apply)
  }
  return (
    <div ref={ref} className="fader" style={{ height }} onPointerDown={drag}
      onDoubleClick={() => onChange(0)} data-info="Track volume (dB). Double-click resets to 0">
      <div className="fader-fill" style={{ height: `${norm * 100}%` }} />
      <div className="fader-handle" style={{ bottom: `calc(${norm * 100}% - 4px)` }} />
    </div>
  )
}

// Horizontal sibling of Fader — for narrow track heads where a vertical fader
// won't fit. Same dB range/snap and the same window-level drag handling.
export function HFader({ value, min = -48, max = 6, onChange, width = 100 }: {
  value: number; min?: number; max?: number; onChange: (v: number) => void; width?: number
}) {
  const norm = clamp((value - min) / (max - min), 0, 1)
  const ref = useRef<HTMLDivElement>(null)
  const drag = (e: React.PointerEvent) => {
    if (e.button !== 0) return
    e.preventDefault(); e.stopPropagation()
    const rect = ref.current!.getBoundingClientRect()
    const apply = (ev: PointerEvent) => {
      const n = clamp((ev.clientX - rect.left) / rect.width, 0, 1)
      onChange(Math.round((min + n * (max - min)) * 10) / 10)
    }
    apply(e.nativeEvent)
    beginVDrag(apply)
  }
  return (
    <div ref={ref} className="hfader" style={{ width }} onPointerDown={drag}
      onDoubleClick={e => { e.stopPropagation(); onChange(0) }} data-info="Track volume (dB). Drag left/right; double-click resets to 0">
      <div className="hfader-fill" style={{ width: `${norm * 100}%` }} />
      <div className="hfader-handle" style={{ left: `calc(${norm * 100}% - 3px)` }} />
    </div>
  )
}

export function MeterBar({ getDb, height = 76 }: { getDb: () => number; height?: number }) {
  const ref = useRef<HTMLDivElement>(null)
  // 30Hz: the meter signal is already smoothed, and each poll reads an analyser
  // buffer — with one meter per track that adds up.
  useRaf(() => {
    const db = getDb()
    const norm = clamp((db + 60) / 66, 0, 1)
    if (ref.current) {
      ref.current.style.height = `${norm * 100}%`
      ref.current.style.background = db > -3 ? 'var(--danger)' : db > -10 ? 'var(--warn)' : 'var(--ok)'
    }
  }, true, 2)
  return (
    <div className="meter" style={{ height }}>
      <div ref={ref} className="meter-fill" />
    </div>
  )
}

// ---------------- Draggable number (BPM etc.) ----------------

export function NumberDrag({ value, onChange, min, max, step = 1, suffix = '', info }: {
  value: number; onChange: (v: number) => void; min: number; max: number; step?: number; suffix?: string; info?: string
}) {
  const [drag, setDrag] = useState(false)
  return (
    <span
      className={`numdrag ${drag ? 'dragging' : ''}`}
      data-info={info ?? 'Drag vertically to change'}
      onPointerDown={e => {
        if (e.button !== 0) return
        e.preventDefault()
        const startY = e.clientY
        const startV = value
        setDrag(true)
        beginVDrag(
          ev => onChange(clamp(Math.round((startV + ((startY - ev.clientY) / 3) * step) / step) * step, min, max)),
          () => setDrag(false),
        )
      }}
      onDoubleClick={() => {
        const v = prompt('Value', String(value))
        if (v && !isNaN(+v)) onChange(clamp(+v, min, max))
      }}
    >
      {Number.isInteger(step) ? Math.round(value) : value.toFixed(1)}{suffix}
    </span>
  )
}

// ---------------- Modal ----------------

export function Modal({ title, onClose, children, width = 440 }: {
  title: string; onClose: () => void; children: React.ReactNode; width?: number
}) {
  useEffect(() => {
    const h = (e: KeyboardEvent) => { if (e.key === 'Escape') onClose() }
    window.addEventListener('keydown', h)
    return () => window.removeEventListener('keydown', h)
  }, [onClose])
  return createPortal(
    <div className="modal-backdrop" onPointerDown={e => { if (e.target === e.currentTarget) onClose() }}>
      <div className="modal" style={{ width }}>
        <div className="modal-head">
          <span>{title}</span>
          <button className="icon-btn" onClick={onClose}><Icon name="close" /></button>
        </div>
        <div className="modal-body">{children}</div>
      </div>
    </div>,
    document.body,
  )
}

// ---------------- Context menu ----------------

export type MenuItem =
  | { label: React.ReactNode; fn: () => void; danger?: boolean; disabled?: boolean }
  | { custom: React.ReactNode }
  | 'sep'

type MenuState = { x: number; y: number; items: MenuItem[] } | null
let setMenuGlobal: ((m: MenuState) => void) | null = null

export function openMenu(e: { clientX: number; clientY: number; preventDefault?: () => void }, items: MenuItem[]) {
  ;(e as any).preventDefault?.()
  setMenuGlobal?.({ x: e.clientX, y: e.clientY, items })
}

export function ContextMenuHost() {
  const [menu, setMenu] = useState<MenuState>(null)
  setMenuGlobal = setMenu
  useEffect(() => {
    if (!menu) return
    const close = () => setMenu(null)
    window.addEventListener('pointerdown', close)
    window.addEventListener('blur', close)
    return () => {
      window.removeEventListener('pointerdown', close)
      window.removeEventListener('blur', close)
    }
  }, [menu])
  if (!menu) return null
  const x = Math.min(menu.x, window.innerWidth - 230)
  const y = Math.min(menu.y, window.innerHeight - menu.items.length * 30 - 20)
  return createPortal(
    <div className="ctx-menu" style={{ left: x, top: y }} onPointerDown={e => e.stopPropagation()}>
      {menu.items.map((it, i) => {
        if (it === 'sep') return <div key={i} className="ctx-sep" />
        if ('custom' in it) return <div key={i} className="ctx-custom" onPointerDown={e => e.stopPropagation()}>{it.custom}</div>
        return (
          <button key={i} className={`ctx-item ${it.danger ? 'danger' : ''}`} disabled={it.disabled}
            onClick={() => { setMenu(null); it.fn() }}>
            {it.label}
          </button>
        )
      })}
    </div>,
    document.body,
  )
}

// ---------------- Color swatch row ----------------

export function ColorRow({ colors, onPick }: { colors: string[]; onPick: (idx: number) => void }) {
  return (
    <div className="color-row">
      {colors.map((c, i) => (
        <button key={i} className="color-swatch" style={{ background: c }} onClick={() => onPick(i)} />
      ))}
    </div>
  )
}

// Inline text editor used by track/scene/clip headers. Commits on Enter/blur,
// cancels on Escape; onDone(null) means "no change".
export function InlineRename({ value, onDone }: { value: string; onDone: (v: string | null) => void }) {
  const [v, setV] = useState(value)
  return (
    <input
      className="inline-rename" autoFocus value={v}
      onChange={e => setV(e.target.value)}
      onBlur={() => onDone(v.trim() || null)}
      onKeyDown={e => {
        if (e.key === 'Enter') onDone(v.trim() || null)
        if (e.key === 'Escape') onDone(null)
        e.stopPropagation()
      }}
      onClick={e => e.stopPropagation()}
      onPointerDown={e => e.stopPropagation()}
    />
  )
}
