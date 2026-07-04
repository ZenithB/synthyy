// Bottom-panel device chain for the selected track: instrument module with
// schema-driven knobs (drum kits get a pad selector), then the effect chain
// with bypass / reorder / remove, then an add-effect menu.

import React, { useRef, useState, useEffect } from 'react'
import * as Y from 'yjs'
import { DRUM_PADS, clamp } from '../types'
import {
  trackById, setInstParam, setInstrument, addFx, removeFx, moveFx, setFxParam, setFxOn,
  setInstOut, setFxOut,
  addLfo, removeLfo, setLfoField, addLfoTarget, removeLfoTarget, lfoTargetList,
  addMidiFx, removeMidiFx, setMidiFxParam, setMidiFxOn, midifxOf,
  ensureMacros, macrosOf, setMacroValue, addMacroTarget, removeMacroTarget, clearMacroTargets, setMacroName,
  setSamplerSample, setDrumPadSample, busList,
  clips, clipKey, isAudioClip, masterFx, tracks, setDuckSource,
} from '../state/doc'
import { attemptBusSend, assignKitToTrack } from './actions'
import { useUI, toast } from '../state/store'
import { useY, useRaf } from './hooks'
import { Knob, openMenu, beginVDrag, MenuItem } from './widgets'
import {
  INSTRUMENTS, EFFECTS, instSchema, fxSchema, defaultsFor,
  LFO_SHAPES, LFO_DIVS, LFO_PARAMS, MIDI_FX, midiFxSchema, mixSpec, fmtPct, fmtDb, ParamSpec,
  EQ7_BANDS, EQ7_FREQS, eq7BandType, RATE_MODES, RATE_SYNC_SPEC, RATE_HI_SPEC,
} from '../audio/schema'
import { INST_PRESETS, DRUM_KITS } from '../packs'
import { engine } from '../audio/engine'
import { saveUserPreset } from '../userlib'
import { importSampleFile, startSampleRecording, stopSampleRecording, isRecordingSample } from '../audio/samples'
import { Icon } from './icons'

const MACRO_SPEC: ParamSpec = { key: 'm', label: '', min: 0, max: 1, def: 0, fmt: fmtPct }
const OUT_SPEC: ParamSpec = { key: 'out', label: 'Out', min: -30, max: 30, def: 0, fmt: fmtDb }

// Per-device output meter (level shown AFTER the output knob), styled like the
// LFO scope. Polls only while its device rack is mounted (selected track).
function DevMeter({ trackId, deviceId }: { trackId: string; deviceId: string }) {
  const ref = useRef<HTMLDivElement>(null)
  useRaf(() => {
    if (!ref.current) return
    const db = engine.deviceMeterDb(trackId, deviceId)
    const norm = Math.max(0, Math.min(1, (db + 60) / 66))
    const col = db > -3 ? 'var(--danger)' : db > -10 ? 'var(--warn)' : 'var(--ok)'
    ref.current.style.width = `${norm * 100}%`
    ref.current.style.background = col
    ref.current.style.boxShadow = norm > 0.02 ? `0 0 5px ${col}` : 'none'
  }, true, 2) // 30Hz — smoothed meter, one per device card
  return <div className="dev-meter"><div ref={ref} className="dev-meter-fill" /></div>
}

// Meter + pre-meter output knob footer shared by every device card.
function DeviceOut({ trackId, deviceId, out, onChange }: { trackId: string; deviceId: string; out: number; onChange: (v: number) => void }) {
  return (
    <div className="device-out" data-info="Output level (pre-meter) — trim this device before the next">
      <DevMeter trackId={trackId} deviceId={deviceId} />
      <Knob spec={OUT_SPEC} value={out} onChange={onChange} size={30} />
    </div>
  )
}

function targetRange(track: Y.Map<any>, dest: string, fxId: string, pkey: string): [number, number] {
  let spec
  if (dest === 'inst') spec = instSchema(track.get('inst').get('type')).params.find(s => s.key === pkey)
  else if (dest === 'mix') spec = mixSpec(pkey)
  else if (dest === 'send') return [0, 1]         // A/B send level
  else if (dest === 'lfo') spec = LFO_PARAMS[2]   // another LFO's rate (Hz)
  else {
    const fxArr = track.get('fx') as Y.Array<Y.Map<any>>
    for (let i = 0; i < fxArr.length; i++) if (fxArr.get(i).get('id') === fxId) { spec = fxSchema(fxArr.get(i).get('type')).params.find(s => s.key === pkey); break }
  }
  return spec ? [spec.min, spec.max] : [0, 1]
}

// All continuous (non-enum) parameters on a track that an LFO or macro can
// modulate — instrument params, effect params, and the A/B send levels.
type ModTarget = { dest: 'inst' | 'fx' | 'lfo' | 'send' | 'macro'; fxId: string; pkey: string; label: string }
function modTargets(track: Y.Map<any>): ModTarget[] {
  const out: ModTarget[] = []
  const instSch = instSchema(track.get('inst').get('type'))
  instSch.params.filter(p => !p.steps).forEach(p =>
    out.push({ dest: 'inst', fxId: '', pkey: p.key, label: `${instSch.label} · ${p.label}` }))
  ;(track.get('fx') as Y.Array<Y.Map<any>>).forEach(f => {
    const sch = fxSchema(f.get('type'))
    sch.params.filter(p => !p.steps).forEach(p =>
      out.push({ dest: 'fx', fxId: f.get('id'), pkey: p.key, label: `${sch.label} · ${p.label}` }))
  })
  out.push({ dest: 'send', fxId: '', pkey: 'A', label: 'Mix · Send A' })
  out.push({ dest: 'send', fxId: '', pkey: 'B', label: 'Mix · Send B' })
  return out
}
const targetVal = (t: ModTarget) => `${t.dest}|${t.fxId}|${t.pkey}`

function LfoScope({ lfoId }: { lfoId: string }) {
  const ref = useRef<HTMLDivElement>(null)
  useRaf(() => {
    if (ref.current) ref.current.style.left = `${((engine.lfoValue(lfoId) + 1) / 2) * 100}%`
  })
  return <div className="lfo-scope"><span className="lfo-scope-mid" /><div ref={ref} className="lfo-scope-dot" /></div>
}

function LfoCard({ trackId, track, lfo }: { trackId: string; track: Y.Map<any>; lfo: Y.Map<any> }) {
  const id = lfo.get('id') as string
  const on = !!lfo.get('on')
  const sync = !!lfo.get('sync')
  const targets = modTargets(track)
  // With >1 LFO, this LFO can also modulate ANOTHER LFO's rate (its own is
  // excluded — an LFO can't drive itself). Rate-modulation only bites on a
  // free-running target; a synced target keeps its tempo division.
  const lfoRateTargets: ModTarget[] = (track.get('lfos') as Y.Array<Y.Map<any>>).toArray()
    .map((l, i) => ({ lid: l.get('id') as string, i }))
    .filter(x => x.lid !== id)
    .map(x => ({ dest: 'lfo', fxId: x.lid, pkey: 'hz', label: `LFO ${x.i + 1} · Rate` }))
  // ...and any macro knob's VALUE (pkey = macro index) — the LFO sweeps the
  // macro, which fans out to everything the macro is mapped to
  const macroTargets: ModTarget[] = (macrosOf(track)?.toArray() ?? [])
    .map((m, i) => ({ dest: 'macro' as const, fxId: '', pkey: String(i), label: `Macros · ${m.get('name') || `M${i + 1}`}` }))
  const allTargets = [...targets, ...lfoRateTargets, ...macroTargets]

  // multi-target mapping: menu lists every parameter, a • marks assigned ones,
  // clicking toggles the assignment on/off
  const assigned = lfoTargetList(lfo)
  const assignedKeys = new Set(assigned.map(tg => `${tg.dest}|${tg.fxId}|${tg.pkey}`))
  const mapMenu = (e: React.MouseEvent) => openMenu(e, [
    ...(assigned.length
      ? [{ label: <><Icon name="close" size={11} /> Clear {assigned.length} mapping{assigned.length > 1 ? 's' : ''}</>, fn: () => assigned.forEach(tg => removeLfoTarget(trackId, id, tg.dest, tg.fxId, tg.pkey)) } as const, 'sep' as const]
      : []),
    ...allTargets.map(t => ({
      label: (assignedKeys.has(targetVal(t)) ? '• ' : '') + t.label,
      fn: () => assignedKeys.has(targetVal(t))
        ? removeLfoTarget(trackId, id, t.dest, t.fxId, t.pkey)
        : addLfoTarget(trackId, id, t.dest, t.fxId, t.pkey),
    })),
  ])

  return (
    <div className={`device lfo-device ${on ? '' : 'bypassed'}`}>
      <div className="device-head">
        <button className={`power ${on ? 'on' : ''}`} data-info="Enable / disable this LFO" onClick={() => setLfoField(trackId, id, 'on', on ? 0 : 1, on ? 'Disable LFO' : 'Enable LFO')}><Icon name="power" size={13} /></button>
        <span className="device-title"><Icon name="lfo" size={13} /> LFO</span>
        <LfoScope lfoId={id} />
        <span className="device-actions">
          <button className="icon-btn" data-info="Remove this LFO" onClick={() => removeLfo(trackId, id)}><Icon name="close" size={11} /></button>
        </span>
      </div>
      <div className="lfo-row">
        <select className="device-select" value={lfo.get('shape') | 0} data-info="LFO waveform shape"
          onChange={e => setLfoField(trackId, id, 'shape', +e.target.value)}>
          {LFO_SHAPES.map((s, i) => <option key={s} value={i}>{s}</option>)}
        </select>
        <button className={`tbtn ${sync ? 'on' : ''}`} data-info="Sync rate to song tempo (off = free-running Hz)"
          onClick={() => setLfoField(trackId, id, 'sync', sync ? 0 : 1)}>Sync</button>
        {sync
          ? <select className="device-select" value={lfo.get('rate') | 0} data-info="Tempo-synced rate"
              onChange={e => setLfoField(trackId, id, 'rate', +e.target.value)}>
              {LFO_DIVS.map((d, i) => <option key={d} value={i}>{d}</option>)}
            </select>
          : <Knob spec={LFO_PARAMS[2]} value={lfo.get('hz') ?? 1} onChange={v => setLfoField(trackId, id, 'hz', v)} size={34} />}
      </div>
      <div className="lfo-row">
        <Knob spec={LFO_PARAMS[0]} value={lfo.get('depth') ?? 0.5} onChange={v => setLfoField(trackId, id, 'depth', v)} size={36} />
        <Knob spec={LFO_PARAMS[1]} value={lfo.get('phase') ?? 0} onChange={v => setLfoField(trackId, id, 'phase', v)} size={36} />
        <button className={`macro-label lfo-target ${assigned.length ? 'mapped' : ''}`}
          data-info="Map this LFO to one or more parameters — • marks assigned ones; click an entry to toggle"
          onClick={mapMenu}>
          {assigned.length ? `${assigned.length} mapped` : 'map to…'}
        </button>
      </div>
    </div>
  )
}

function InstrumentPanel({ trackId, track }: { trackId: string; track: Y.Map<any> }) {
  const inst = track.get('inst') as Y.Map<any>
  const type = inst.get('type') as string
  const params = inst.get('params') as Y.Map<number>
  const schema = instSchema(type)
  const kind = track.get('kind')
  const [pad, setPad] = useState(0)
  const padSamples = inst.get('padSamples') as Y.Map<string> | undefined
  const padNames = inst.get('padNames') as Y.Map<string> | undefined

  // Right-click a drum pad → drop an audio clip on it (that pad becomes a
  // sampler; the rest of the kit stays synthesized).
  const pickPadSample = (i: number) => {
    const input = document.createElement('input')
    input.type = 'file'; input.accept = 'audio/*'
    input.onchange = async () => {
      const f = input.files?.[0]; if (!f) return
      try { const ref = await importSampleFile(f); setDrumPadSample(trackId, i, ref.id, ref.name); toast(`${DRUM_PADS[i]} → “${ref.name}”`) }
      catch { toast('Could not decode that audio file') }
    }
    input.click()
  }

  const presetMatches = (kind === 'drum' ? [] : INST_PRESETS).map(p => p.name)
  const kitNames = DRUM_KITS.map(k => k.name)

  const onPreset = (name: string) => {
    if (kind === 'drum') {
      const kit = DRUM_KITS.find(k => k.name === name)
      if (kit) setInstrument(trackId, 'drum', { ...kit.params }, `Kit: ${name}`)
    } else {
      const p = INST_PRESETS.find(x => x.name === name)
      if (p) setInstrument(trackId, p.type, { ...p.params }, `Preset: ${name}`)
    }
  }

  return (
    <div className="device inst-device">
      <div className="device-head">
        <span className="device-title"><Icon name={schema.icon} size={13} /> {kind === 'audio' ? 'Audio Track' : schema.label}</span>
        {kind === 'synth' && (
          <select
            className="device-select"
            value={type}
            data-info="Swap the instrument module (resets its knobs)"
            onChange={e => setInstrument(trackId, e.target.value, defaultsFor(instSchema(e.target.value).params), `Instrument: ${e.target.value}`)}
          >
            {INSTRUMENTS.filter(i => i.type !== 'drum' && i.type !== 'audiobus').map(i => <option key={i.type} value={i.type}>{i.label}</option>)}
          </select>
        )}
        {kind !== 'audio' && (
          <select
            className="device-select"
            value=""
            data-info={kind === 'drum' ? 'Load a drum kit preset' : 'Load an instrument preset'}
            onChange={e => { if (e.target.value) onPreset(e.target.value) }}
          >
            <option value="">{kind === 'drum' ? 'Kits…' : 'Presets…'}</option>
            {(kind === 'drum' ? kitNames : presetMatches).map(n => <option key={n} value={n}>{n}</option>)}
          </select>
        )}
        {kind === 'audio' && <span className="device-audio-hint">audio clips · stereo · fx + sends apply</span>}
        {kind === 'synth' && type !== 'sampler' && type !== 'ksampler' && (
          <button className="icon-btn" data-info="Save these settings as your own preset"
            onClick={() => {
              const name = prompt('Save preset as…', `My ${schema.label}`)
              if (name) { saveUserPreset({ name, type, params: Object.fromEntries(params.entries()) }); toast(`Saved “${name}” to My Sounds`) }
            }}><Icon name="star" size={13} /></button>
        )}
      </div>

      {(type === 'sampler' || type === 'ksampler' || type === 'granular') && <SamplerControls trackId={trackId} inst={inst} />}

      {kind === 'drum' ? (
        <div className="drum-panel"
          onDragOver={e => { if (e.dataTransfer.types.includes('stg/drumkit')) e.preventDefault() }}
          onDrop={e => { const dk = e.dataTransfer.getData('stg/drumkit'); if (dk) assignKitToTrack(trackId, dk) }}>
          <div className="pad-grid">
            {DRUM_PADS.map((name, i) => {
              const sampled = !!padSamples?.get(String(i))
              return (
                <button
                  key={i}
                  className={`pad ${pad === i ? 'on' : ''} ${sampled ? 'sampled' : ''}`}
                  data-info={sampled
                    ? `${name} → sample “${padNames?.get(String(i)) || 'audio'}”. Right-click to replace or clear. Drop a sample to assign.`
                    : `${name} — click to audition. Right-click or drop a drum sample here to assign it.`}
                  onClick={() => { setPad(i); engine.previewOn(trackId, i, 0.9) }}
                  onDragOver={e => { const ty = e.dataTransfer.types; if (ty.includes('stg/sample') || ty.includes('stg/clip')) { e.preventDefault(); e.stopPropagation() } }}
                  onDrop={e => {
                    e.stopPropagation()
                    const s = e.dataTransfer.getData('stg/sample')
                    if (s) { const [sid, nm] = s.split('::'); setDrumPadSample(trackId, i, sid, nm || 'Sample'); return }
                    const clipSrc = e.dataTransfer.getData('stg/clip')
                    if (clipSrc) {
                      const [srcT, srcS] = clipSrc.split('|')
                      const cm = clips.get(clipKey(srcT, srcS)) as Y.Map<any> | undefined
                      if (cm && isAudioClip(cm)) setDrumPadSample(trackId, i, cm.get('sampleId'), cm.get('sampleName') || 'Sample')
                      else toast('Only audio clips can be routed to a pad')
                    }
                  }}
                  onContextMenu={e => openMenu(e, [
                    { label: <><Icon name="sampler" size={12} /> {sampled ? 'Replace sample…' : 'Load audio clip…'}</>, fn: () => pickPadSample(i) },
                    ...(sampled ? [{ label: <><Icon name="close" size={12} /> Clear sample (back to synth)</>, fn: () => setDrumPadSample(trackId, i, null), danger: true }] : []),
                  ])}
                >
                  {name}
                  {sampled && <span className="pad-sample-dot" />}
                </button>
              )
            })}
          </div>
          <div className="knob-row">
            {['tune', 'decay', 'level'].map(suffix => {
              const spec = schema.params.find(p => p.key === `p${pad}_${suffix}`)!
              // sampled pad: Tune repitches the sample, Level scales it, but the
              // whole sample always plays — Decay does nothing, so grey it out.
              const padIsSampled = !!padSamples?.get(String(pad))
              return (
                <Knob
                  key={spec.key}
                  spec={{ ...spec, label: suffix[0].toUpperCase() + suffix.slice(1) }}
                  value={params.get(spec.key) ?? spec.def}
                  onChange={v => setInstParam(trackId, spec.key, v)}
                  size={40}
                  disabled={padIsSampled && suffix === 'decay'}
                  disabledInfo="Decay has no effect on a sampled pad — the whole sample plays (Tune repitches it, Level sets its volume)"
                />
              )
            })}
          </div>
        </div>
      ) : (
        <div className="knob-row">
          {schema.params.map(spec => (
            <Knob key={spec.key} spec={spec} value={params.get(spec.key) ?? spec.def} onChange={v => setInstParam(trackId, spec.key, v)} size={38} />
          ))}
        </div>
      )}
      <DeviceOut trackId={trackId} deviceId="inst" out={inst.get('out') ?? 0} onChange={v => setInstOut(trackId, v)} />
    </div>
  )
}

// Live gain-reduction meter for compressor-type effects (comp / opto). Fills
// downward as the compressor pulls the signal; reads the engine each frame.
function GrMeter({ trackId, fxId }: { trackId: string; fxId: string }) {
  const fill = useRef<HTMLDivElement>(null)
  const val = useRef<HTMLSpanElement>(null)
  useRaf(() => {
    const gr = Math.max(0, -engine.deviceReductionDb(trackId, fxId))   // dB of reduction
    if (fill.current) fill.current.style.width = `${Math.min(100, (gr / 24) * 100)}%`
    if (val.current) val.current.textContent = gr > 0.05 ? `−${gr.toFixed(1)}` : '0.0'
  })
  return (
    <div className="gr-meter" data-info="Gain reduction (dB)">
      <span className="gr-cap">GR</span>
      <div className="gr-track"><div ref={fill} className="gr-fill" /></div>
      <span ref={val} className="gr-val">0.0</span>
    </div>
  )
}

// ---- Multiband: graphical band editor ----
const MB_FMIN = 20, MB_FMAX = 20000
const mbX = (f: number, W: number) => (Math.log(f / MB_FMIN) / Math.log(MB_FMAX / MB_FMIN)) * W
const mbF = (x: number, W: number) => MB_FMIN * Math.pow(MB_FMAX / MB_FMIN, clamp(x, 0, W) / W)
const mbY = (db: number, H: number) => (-db / 60) * H          // 0 dB top → −60 dB bottom
const mbDb = (y: number, H: number) => clamp(-(y / H) * 60, -60, 0)

// Live per-band gain-reduction bar, drawn descending from the top of the band.
function MbGrBar({ trackId, fxId, band, x0, x1, H }: { trackId: string; fxId: string; band: number; x0: number; x1: number; H: number }) {
  const ref = useRef<SVGRectElement>(null)
  useRaf(() => {
    const grs = engine.deviceBandReduction(trackId, fxId)
    const gr = Math.max(0, -(grs[band] ?? 0))
    if (ref.current) ref.current.setAttribute('height', String(Math.min(H, (gr / 24) * H)))
  })
  return <rect ref={ref} className="mb-gr" x={x0} y={0} width={Math.max(0, x1 - x0)} height={0} />
}

function MultibandCard({ trackId, fxId, params }: { trackId: string; fxId: string; params: Y.Map<number> }) {
  const svgRef = useRef<SVGSVGElement>(null)
  const [, force] = useState(0)
  const W = 432, H = 132
  const g = (k: string, d: number) => params.get(k) ?? d
  const set = (k: string, v: number) => { setFxParam(trackId, fxId, k, v); force(n => n + 1) }
  const sch = fxSchema('mbcomp')
  const spec = (k: string) => sch.params.find(p => p.key === k)!

  const mode = g('mode', 0) | 0
  const xlo = g('xlo', 250), xhi = g('xhi', 2500)
  const xloX = mbX(xlo, W), xhiX = mbX(xhi, W)
  const bands = [
    { i: 0, name: 'Low', x0: 0, x1: xloX, th: g('b0_thresh', -24) },
    { i: 1, name: 'Mid', x0: xloX, x1: xhiX, th: g('b1_thresh', -24) },
    { i: 2, name: 'High', x0: xhiX, x1: W, th: g('b2_thresh', -24) },
  ]

  const loc = (e: PointerEvent | React.PointerEvent) => {
    const r = svgRef.current!.getBoundingClientRect()
    return { x: (e.clientX - r.left) * (W / r.width), y: (e.clientY - r.top) * (H / r.height) }
  }
  const onDown = (e: React.PointerEvent) => {
    if (e.button !== 0) return
    e.stopPropagation()
    const { x } = loc(e)
    if (Math.abs(x - xloX) <= 7) {       // drag low/mid crossover
      beginVDrag(ev => set('xlo', clamp(mbF(loc(ev).x, W), 60, Math.min(1000, g('xhi', 2500) - 20))))
      return
    }
    if (Math.abs(x - xhiX) <= 7) {       // drag mid/high crossover
      beginVDrag(ev => set('xhi', clamp(mbF(loc(ev).x, W), Math.max(1000, g('xlo', 250) + 20), 12000)))
      return
    }
    // otherwise drag the threshold of whichever band the pointer is over
    const band = bands.find(b => x >= b.x0 && x <= b.x1) ?? bands[2]
    const setTh = (ev: PointerEvent | React.PointerEvent) => set(`b${band.i}_thresh`, Math.round(mbDb(loc(ev).y, H)))
    setTh(e)
    beginVDrag(setTh)
  }

  return (
    <div className="mb-card">
      <div className="mb-graph">
        <svg ref={svgRef} width={W} height={H} viewBox={`0 0 ${W} ${H}`} className="mb-svg" onPointerDown={onDown}
          data-info="Drag inside a band to set its threshold; drag the dividers to move crossovers. Bars show live gain reduction.">
          {bands.map(b => (
            <g key={b.i}>
              <rect className={`mb-band ${b.i % 2 ? 'alt' : ''}`} x={b.x0} y={0} width={Math.max(0, b.x1 - b.x0)} height={H} />
              <MbGrBar trackId={trackId} fxId={fxId} band={b.i} x0={b.x0} x1={b.x1} H={H} />
              <line className="mb-thresh" x1={b.x0 + 2} y1={mbY(b.th, H)} x2={b.x1 - 2} y2={mbY(b.th, H)} />
              <text className="mb-blabel" x={(b.x0 + b.x1) / 2} y={H - 6}>{b.name}</text>
              <text className="mb-thlabel" x={(b.x0 + b.x1) / 2} y={mbY(b.th, H) - 4}>{Math.round(b.th)}dB</text>
            </g>
          ))}
          <line className="mb-xover" x1={xloX} y1={0} x2={xloX} y2={H} />
          <line className="mb-xover" x1={xhiX} y1={0} x2={xhiX} y2={H} />
          <text className="mb-xlabel" x={xloX} y={11}>{xlo >= 1000 ? `${(xlo / 1000).toFixed(1)}k` : `${Math.round(xlo)}`}</text>
          <text className="mb-xlabel" x={xhiX} y={11}>{xhi >= 1000 ? `${(xhi / 1000).toFixed(1)}k` : `${Math.round(xhi)}`}</text>
        </svg>
      </div>
      <div className="mb-controls">
        <button className={`tbtn mb-mode ${mode ? 'expand' : ''}`} data-info="Switch between multiband compression and downward expansion"
          onClick={() => set('mode', mode ? 0 : 1)}>{mode ? 'Expand' : 'Comp'}</button>
        <Knob spec={spec('attack')} value={g('attack', 0.02)} onChange={v => set('attack', v)} size={34} />
        <Knob spec={spec('release')} value={g('release', 0.18)} onChange={v => set('release', v)} size={34} />
        {[0, 1, 2].map(i => (
          <div key={i} className="mb-bandctl">
            <Knob spec={{ ...spec(`b${i}_ratio`), label: 'Ratio' }} value={g(`b${i}_ratio`, 2)} onChange={v => set(`b${i}_ratio`, v)} size={32} />
            <Knob spec={{ ...spec(`b${i}_gain`), label: 'Gain' }} value={g(`b${i}_gain`, 0)} onChange={v => set(`b${i}_gain`, v)} size={32} />
          </div>
        ))}
      </div>
    </div>
  )
}

// ---- EQ Seven: graphic 7-band parametric editor + RTA ----
// Log-frequency axis 20Hz–20kHz, ±18dB curve axis. Drag a band dot to set its
// freq/gain; mouse-wheel over the graph tweaks the NEAREST band's Q. The
// combined response curve is computed with RBJ biquad math (matching the
// audio-thread filters), and the RTA behind it draws the live pre-EQ spectrum
// from the effect's analyser tap.
const EQ_FMIN = 20, EQ_FMAX = 20000, EQ_DB = 18
const eqX = (f: number, W: number) => (Math.log(f / EQ_FMIN) / Math.log(EQ_FMAX / EQ_FMIN)) * W
const eqF = (x: number, W: number) => EQ_FMIN * Math.pow(EQ_FMAX / EQ_FMIN, clamp(x, 0, W) / W)
const eqY = (db: number, H: number) => (0.5 - db / (2 * EQ_DB)) * H
const eqDb = (y: number, H: number) => clamp((0.5 - y / H) * 2 * EQ_DB, -15, 15)

type BiquadC = { b0: number; b1: number; b2: number; a0: number; a1: number; a2: number }
function eqCoeffs(type: 'lowshelf' | 'peaking' | 'highshelf', f: number, q: number, gainDb: number, sr: number): BiquadC {
  const A = Math.pow(10, gainDb / 40)
  const w0 = (2 * Math.PI * Math.min(f, sr * 0.49)) / sr
  const cw = Math.cos(w0), sw = Math.sin(w0)
  if (type === 'peaking') {
    const alpha = sw / (2 * Math.max(0.05, q))
    return { b0: 1 + alpha * A, b1: -2 * cw, b2: 1 - alpha * A, a0: 1 + alpha / A, a1: -2 * cw, a2: 1 - alpha / A }
  }
  // Web Audio shelves use a fixed slope (S=1) and ignore Q
  const alpha = (sw / 2) * Math.SQRT2
  const sq = 2 * Math.sqrt(A) * alpha
  if (type === 'lowshelf') {
    return {
      b0: A * ((A + 1) - (A - 1) * cw + sq), b1: 2 * A * ((A - 1) - (A + 1) * cw), b2: A * ((A + 1) - (A - 1) * cw - sq),
      a0: (A + 1) + (A - 1) * cw + sq, a1: -2 * ((A - 1) + (A + 1) * cw), a2: (A + 1) + (A - 1) * cw - sq,
    }
  }
  return {
    b0: A * ((A + 1) + (A - 1) * cw + sq), b1: -2 * A * ((A - 1) + (A + 1) * cw), b2: A * ((A + 1) + (A - 1) * cw - sq),
    a0: (A + 1) - (A - 1) * cw + sq, a1: 2 * ((A - 1) - (A + 1) * cw), a2: (A + 1) - (A - 1) * cw - sq,
  }
}
function eqMagDb(c: BiquadC, w: number): number {
  const cw = Math.cos(w), sw = Math.sin(w), c2 = Math.cos(2 * w), s2 = Math.sin(2 * w)
  const nr = c.b0 + c.b1 * cw + c.b2 * c2, ni = -(c.b1 * sw + c.b2 * s2)
  const dr = c.a0 + c.a1 * cw + c.a2 * c2, di = -(c.a1 * sw + c.a2 * s2)
  return 10 * Math.log10(Math.max(1e-12, (nr * nr + ni * ni) / (dr * dr + di * di)))
}

// Live pre-EQ spectrum, drawn on a canvas behind the response curve.
function Rta({ trackId, fxId, W, H }: { trackId: string; fxId: string; W: number; H: number }) {
  const ref = useRef<HTMLCanvasElement>(null)
  const buf = useRef<Float32Array | null>(null)
  useRaf(() => {
    const canvas = ref.current
    const an = engine.deviceAnalyser(trackId, fxId)
    if (!canvas || !an) return
    if (!buf.current || buf.current.length !== an.frequencyBinCount) buf.current = new Float32Array(an.frequencyBinCount)
    an.getFloatFrequencyData(buf.current as any)
    const ctx = canvas.getContext('2d')!
    ctx.clearRect(0, 0, W, H)
    const sr = engine.sampleRate || 44100
    ctx.beginPath()
    ctx.moveTo(0, H)
    for (let x = 0; x <= W; x += 3) {
      const f = eqF(x, W)
      const bin = Math.min(buf.current.length - 1, Math.round((f / (sr / 2)) * buf.current.length))
      const db = buf.current[bin]
      const y = clamp(((-10 - db) / 80) * H, 0, H)   // −10dBFS top … −90 bottom
      ctx.lineTo(x, y)
    }
    ctx.lineTo(W, H)
    ctx.closePath()
    ctx.fillStyle = 'rgba(140, 180, 255, 0.13)'
    ctx.fill()
  }, true, 2)
  return <canvas ref={ref} className="eq7-rta" width={W} height={H} />
}

function Eq7Card({ trackId, fxId, params }: { trackId: string; fxId: string; params: Y.Map<number> }) {
  const svgRef = useRef<SVGSVGElement>(null)
  const [, force] = useState(0)
  const W = 432, H = 150
  const sr = engine.sampleRate || 44100
  const g = (k: string, d: number) => params.get(k) ?? d
  const set = (k: string, v: number) => { setFxParam(trackId, fxId, k, v); force(n => n + 1) }
  const bands = Array.from({ length: EQ7_BANDS }, (_, i) => ({
    i,
    type: eq7BandType(i),
    f: g(`b${i}_freq`, EQ7_FREQS[i]),
    gain: g(`b${i}_gain`, 0),
    q: g(`b${i}_q`, 1),
  }))

  // combined response curve (sum of per-band dB responses)
  const coeffs = bands.map(b => eqCoeffs(b.type, b.f, b.q, b.gain, sr))
  const pts: string[] = []
  for (let x = 0; x <= W; x += 4) {
    const w = (2 * Math.PI * eqF(x, W)) / sr
    let db = 0
    for (const c of coeffs) db += eqMagDb(c, w)
    pts.push(`${x},${eqY(clamp(db, -EQ_DB, EQ_DB), H).toFixed(1)}`)
  }

  const loc = (e: PointerEvent | React.PointerEvent | React.WheelEvent) => {
    const r = svgRef.current!.getBoundingClientRect()
    return { x: (e.clientX - r.left) * (W / r.width), y: (e.clientY - r.top) * (H / r.height) }
  }
  const nearestBand = (x: number) => {
    let best = 0, bd = Infinity
    bands.forEach(b => { const d = Math.abs(eqX(b.f, W) - x); if (d < bd) { bd = d; best = b.i } })
    return best
  }
  const onDown = (e: React.PointerEvent) => {
    if (e.button !== 0) return
    e.stopPropagation()
    const { x } = loc(e)
    const i = nearestBand(x)
    const apply = (ev: PointerEvent | React.PointerEvent) => {
      const p = loc(ev)
      set(`b${i}_freq`, clamp(eqF(p.x, W), 20, 20000))
      set(`b${i}_gain`, Math.round(eqDb(p.y, H) * 10) / 10)
    }
    apply(e)
    beginVDrag(apply)
  }
  const onWheel = (e: React.WheelEvent) => {
    e.preventDefault(); e.stopPropagation()
    const { x } = loc(e)
    const i = nearestBand(x)
    const q = clamp(g(`b${i}_q`, 1) * (e.deltaY > 0 ? 0.88 : 1.14), 0.3, 12)
    set(`b${i}_q`, Math.round(q * 100) / 100)
  }

  const gridFreqs = [50, 100, 200, 500, 1000, 2000, 5000, 10000]
  return (
    <div className="eq7-card">
      <div className="eq7-graph" style={{ width: W, height: H }}>
        <Rta trackId={trackId} fxId={fxId} W={W} H={H} />
        <svg ref={svgRef} width={W} height={H} viewBox={`0 0 ${W} ${H}`} className="eq7-svg"
          onPointerDown={onDown} onWheel={onWheel}
          data-info="Drag a dot: frequency + gain. Mouse wheel: Q of the nearest band. Shaded area = live input spectrum (RTA).">
          {gridFreqs.map(f => <line key={f} className="eq7-grid" x1={eqX(f, W)} y1={0} x2={eqX(f, W)} y2={H} />)}
          <line className="eq7-zero" x1={0} y1={H / 2} x2={W} y2={H / 2} />
          <polyline className="eq7-curve" points={pts.join(' ')} />
          {bands.map(b => (
            <g key={b.i} className="eq7-band">
              <circle className="eq7-dot" cx={eqX(b.f, W)} cy={eqY(clamp(b.gain, -EQ_DB, EQ_DB), H)} r={5.5} />
              <text className="eq7-dotlabel" x={eqX(b.f, W)} y={eqY(clamp(b.gain, -EQ_DB, EQ_DB), H) - 8}>{b.i + 1}</text>
            </g>
          ))}
          {gridFreqs.map(f => (
            <text key={'t' + f} className="eq7-flabel" x={eqX(f, W) + 2} y={H - 4}>{f >= 1000 ? `${f / 1000}k` : f}</text>
          ))}
        </svg>
      </div>
    </div>
  )
}

// Switchable Rate knob: Sync (tempo division) / Low (0.01–300Hz) / High
// (301Hz–1kHz). The little mode button cycles; the knob re-skins to match.
// Each mode keeps its own stored value so switching back doesn't lose it.
function RateControl({ trackId, fxId, params, spec }: { trackId: string; fxId: string; params: Y.Map<number>; spec: ParamSpec }) {
  const mode = ((params.get('rateMode') ?? 1) | 0) % 3
  const knobSpec = mode === 0 ? RATE_SYNC_SPEC : mode === 2 ? RATE_HI_SPEC : spec
  return (
    <div className="rate-ctl">
      <Knob spec={knobSpec} value={params.get(knobSpec.key) ?? knobSpec.def}
        onChange={v => setFxParam(trackId, fxId, knobSpec.key, v)} size={38} />
      <button className="rate-mode" onClick={() => setFxParam(trackId, fxId, 'rateMode', (mode + 1) % 3)}
        data-info="Rate mode — Sync: tempo divisions · Low: 0.01–300Hz · High: 301Hz–1kHz">
        {RATE_MODES[mode]}
      </button>
    </div>
  )
}

// Sidechain trigger source picker: Tempo (classic beat-synced pump), any track,
// or one specific pad of a drum track (e.g. "duck the bass from THIS kick").
function DuckSource({ trackId, fxId, fx }: { trackId: string; fxId: string; fx: Y.Map<any> }) {
  const src = (fx.get('srcTrack') as string) || ''
  const pitch = (fx.get('srcPitch') as number) ?? -1
  const srcT = src ? trackById(src) : undefined
  const label = !src ? 'Tempo'
    : !srcT ? 'missing track'
    : pitch >= 0 ? `${srcT.get('name')} · ${DRUM_PADS[pitch] ?? `Pad ${pitch + 1}`}`
    : srcT.get('name')
  const pick = (e: React.MouseEvent) => {
    const items: MenuItem[] = [
      { label: (!src ? '• ' : '') + 'Tempo (pump on the beat)', fn: () => setDuckSource(trackId, fxId, '', -1) },
      'sep',
    ]
    tracks.toArray().forEach(t => {
      const tid = t.get('id') as string
      if (t.get('kind') === 'bus' || tid === trackId) return   // buses play no notes; self-trigger is meaningless
      const nm = t.get('name') as string
      items.push({ label: ((src === tid && pitch < 0) ? '• ' : '') + `${nm} — any note`, fn: () => setDuckSource(trackId, fxId, tid, -1) })
      if (t.get('kind') === 'drum') DRUM_PADS.forEach((pn, i) =>
        items.push({ label: ((src === tid && pitch === i) ? '• ' : '') + `${nm} · ${pn} pad`, fn: () => setDuckSource(trackId, fxId, tid, i) }))
    })
    openMenu(e, items)
  }
  return (
    <div className="duck-src">
      <span className="duck-src-label">Trigger</span>
      <button className={`tbtn ${src ? 'on' : ''}`} onClick={pick}
        data-info="What fires the duck: the beat grid (Tempo), any note on a track, or one drum pad (classic kick-sidechain)">
        {label} ▾
      </button>
    </div>
  )
}

function FxCard({ trackId, fx }: { trackId: string; fx: Y.Map<any> }) {
  const type = fx.get('type') as string
  const fxId = fx.get('id') as string
  const on = !!fx.get('on')
  const params = fx.get('params') as Y.Map<number>
  const schema = fxSchema(type)
  const isComp = type === 'comp' || type === 'opto'
  return (
    <div className={`device fx-device ${on ? '' : 'bypassed'} ${type === 'mbcomp' || type === 'eq7' ? 'mbcomp-device' : ''}`}>
      <div className="device-head">
        <button className={`power ${on ? 'on' : ''}`} data-info="Bypass effect" onClick={() => setFxOn(trackId, fxId, !on)}><Icon name="power" size={13} /></button>
        <span className="device-title"><Icon name={schema.icon} size={13} /> {schema.label}</span>
        <span className="device-actions">
          <button className="icon-btn" data-info="Move effect earlier in the chain" onClick={() => moveFx(trackId, fxId, -1)}><Icon name="chevL" size={11} /></button>
          <button className="icon-btn" data-info="Move effect later in the chain" onClick={() => moveFx(trackId, fxId, 1)}><Icon name="chevR" size={11} /></button>
          <button className="icon-btn" data-info="Remove effect" onClick={() => removeFx(trackId, fxId)}><Icon name="close" size={11} /></button>
        </span>
      </div>
      {type === 'mbcomp' ? (
        <MultibandCard trackId={trackId} fxId={fxId} params={params} />
      ) : type === 'eq7' ? (
        <Eq7Card trackId={trackId} fxId={fxId} params={params} />
      ) : (
        <>
          <div className="knob-row">
            {schema.params.map(spec => spec.rateSwitch
              ? <RateControl key={spec.key} trackId={trackId} fxId={fxId} params={params} spec={spec} />
              : <Knob key={spec.key} spec={spec} value={params.get(spec.key) ?? spec.def} onChange={v => setFxParam(trackId, fxId, spec.key, v)} size={38} />)}
          </div>
          {type === 'duck' && <DuckSource trackId={trackId} fxId={fxId} fx={fx} />}
          {isComp && <GrMeter trackId={trackId} fxId={fxId} />}
        </>
      )}
      <DeviceOut trackId={trackId} deviceId={fxId} out={fx.get('out') ?? 0} onChange={v => setFxOut(trackId, fxId, v)} />
    </div>
  )
}

function SamplerControls({ trackId, inst }: { trackId: string; inst: Y.Map<any> }) {
  const [recording, setRecording] = useState(isRecordingSample())
  const name = inst.get('sampleName') as string | undefined
  const pickFile = () => {
    const input = document.createElement('input')
    input.type = 'file'; input.accept = 'audio/*'
    input.onchange = async () => {
      const f = input.files?.[0]; if (!f) return
      try { const ref = await importSampleFile(f); setSamplerSample(trackId, ref.id, ref.name); toast(`Loaded “${ref.name}”`) }
      catch { toast('Could not decode that audio file') }
    }
    input.click()
  }
  const toggleRec = async () => {
    if (isRecordingSample()) {
      const ref = await stopSampleRecording(); setRecording(false)
      if (ref) { setSamplerSample(trackId, ref.id, ref.name); toast('Recorded a sample') }
    } else {
      try { await startSampleRecording(); setRecording(true); toast('Recording… click again to stop') }
      catch { toast('Microphone permission denied') }
    }
  }
  return (
    <div className="sampler-controls"
      onDragOver={e => { const ty = e.dataTransfer.types; if (ty.includes('stg/sample') || ty.includes('stg/clip')) { e.preventDefault(); e.stopPropagation() } }}
      onDrop={e => {
        e.stopPropagation()
        const s = e.dataTransfer.getData('stg/sample')
        if (s) { const [sid, nm] = s.split('::'); setSamplerSample(trackId, sid, nm || 'Sample'); toast(`Loaded “${nm || 'Sample'}”`); return }
        const clipSrc = e.dataTransfer.getData('stg/clip')
        if (clipSrc) {
          const [srcT, srcS] = clipSrc.split('|')
          const cm = clips.get(clipKey(srcT, srcS)) as Y.Map<any> | undefined
          if (cm && isAudioClip(cm)) setSamplerSample(trackId, cm.get('sampleId'), cm.get('sampleName') || 'Sample')
          else toast('Only audio clips can load into the sampler')
        }
      }}>
      <span className="sampler-name" data-info="Loaded sample (plays pitched by the notes you play). Drag a sample here, or Load / Rec.">{name || 'No sample — drag a sample here'}</span>
      <button className="tbtn" onClick={pickFile} data-info="Import an audio file as the sample"><Icon name="folder" size={12} /> Load</button>
      <button className={`tbtn ${recording ? 'on' : ''}`} onClick={toggleRec} data-info="Record from your microphone"><Icon name="mic" size={12} /> {recording ? 'Stop' : 'Rec'}</button>
    </div>
  )
}

function MidiFxCard({ trackId, fx }: { trackId: string; fx: Y.Map<any> }) {
  const type = fx.get('type') as string
  const id = fx.get('id') as string
  const on = !!fx.get('on')
  const params = fx.get('params') as Y.Map<number>
  const schema = midiFxSchema(type)
  return (
    <div className={`device midifx-device ${on ? '' : 'bypassed'}`}>
      <div className="device-head">
        <button className={`power ${on ? 'on' : ''}`} data-info="Bypass" onClick={() => setMidiFxOn(trackId, id, !on)}><Icon name="power" size={13} /></button>
        <span className="device-title"><Icon name={schema.icon} size={13} /> {schema.label}</span>
        <span className="device-actions">
          <button className="icon-btn" data-info="Remove" onClick={() => removeMidiFx(trackId, id)}><Icon name="close" size={11} /></button>
        </span>
      </div>
      {schema.params.length === 0
        ? <div className="midifx-note">Forces notes into the project scale</div>
        : <div className="knob-row">
            {schema.params.map(spec => (
              <Knob key={spec.key} spec={spec} value={params.get(spec.key) ?? spec.def} onChange={v => setMidiFxParam(trackId, id, spec.key, v)} size={34} />
            ))}
          </div>}
    </div>
  )
}

function SendsCard({ trackId, track }: { trackId: string; track: Y.Map<any> }) {
  const sends = track.get('sends') as Y.Map<number> | undefined
  // Every bus is a send target now — A and B are just pre-set buses, sent to via
  // the same per-bus send level as any user bus (no more dedicated →A/→B knobs).
  const buses = busList().filter(b => b.get('id') !== trackId)
  return (
    <div className="device sends-device">
      <div className="device-head"><span className="device-title"><Icon name="send" size={13} /> Sends</span></div>
      <div className="knob-row">
        {buses.length === 0
          ? <span className="device-audio-hint">No buses to send to</span>
          : buses.map(b => {
            const bid = b.get('id') as string
            const spec: ParamSpec = { key: `bus-${bid}`, label: `→ ${b.get('name')}`, min: 0, max: 1, def: 0, fmt: fmtPct }
            return <Knob key={bid} spec={spec} value={(sends?.get(bid) as number) ?? 0} onChange={v => attemptBusSend(trackId, bid, v)} size={36} />
          })}
      </div>
    </div>
  )
}

function MacroPanel({ trackId, track }: { trackId: string; track: Y.Map<any> }) {
  const macros = macrosOf(track)
  if (!macros) {
    return <button className="add-fx add-macro" data-info="Add 8 macro knobs — one knob morphs many parameters"
      onClick={() => ensureMacros(trackId)}><Icon name="macro" size={12} /> Macros</button>
  }
  const ranges = (_k: string, dest: string, fxId: string, pkey: string) => targetRange(track, dest, fxId, pkey)
  const targets = modTargets(track)
  return (
    <div className="device macro-device">
      <div className="device-head">
        <span className="device-title"><Icon name="macro" size={13} /> Macros</span>
        <span className="device-actions">
          <button className="icon-btn" data-info="Randomize all macros (happy accidents)"
            onClick={() => macros.toArray().forEach((m, i) => { if ((m.get('targets') as Y.Array<any>).length) setMacroValue(trackId, i, Math.random(), ranges) })}><Icon name="dice" size={12} /></button>
        </span>
      </div>
      <div className="macro-grid">
        {macros.toArray().map((m, i) => {
          const mTargets = (m.get('targets') as Y.Array<Y.Map<any>>).toArray()
          const nTargets = mTargets.length
          const mKeys = new Set(mTargets.map(x => `${x.get('dest')}|${x.get('fxId') || ''}|${x.get('pkey')}`))
          // menu shared by the label click AND a right-click on the knob itself:
          // • marks assigned params; clicking an entry toggles the assignment
          const mapMenu = (e: React.MouseEvent) => openMenu(e, [
            ...(nTargets ? [{ label: <><Icon name="close" size={11} /> Clear {nTargets} mapping{nTargets > 1 ? 's' : ''}</>, fn: () => clearMacroTargets(trackId, i) } as const, 'sep' as const] : []),
            ...targets.map(t => ({
              label: (mKeys.has(targetVal(t)) ? '• ' : '') + t.label,
              fn: () => mKeys.has(targetVal(t))
                ? removeMacroTarget(trackId, i, t.dest, t.fxId, t.pkey)
                : addMacroTarget(trackId, i, t.dest, t.fxId, t.pkey),
            })),
          ])
          return (
            <div key={i} className="macro-cell">
              <span onContextMenu={mapMenu} data-info="Right-click to map this macro to parameters">
                <Knob spec={MACRO_SPEC} value={m.get('value') ?? 0} onChange={v => setMacroValue(trackId, i, v, ranges)} size={34}
                  accent={nTargets ? 'var(--accent2)' : undefined} />
              </span>
              <button className={`macro-label ${nTargets ? 'mapped' : ''}`}
                data-info={nTargets ? `${nTargets} mapped — click to remap (• = assigned, click to toggle)` : 'Click to map this macro to parameters'}
                onClick={mapMenu}>{m.get('name') || `M${i + 1}`}</button>
            </div>
          )
        })}
      </div>
    </div>
  )
}

// Master-bus effect chain. The whole mix passes through these (after every
// track + return) before the limiter — live and in exports. Reuses FxCard with
// the special 'master' target, so every effect (incl. the new comp/opto/
// multiband + their meters) works here unchanged.
function MasterRack() {
  useY(masterFx)
  return (
    <div className="rack">
      <div className="bus-input-label" data-info="The entire mix passes through these effects before the output limiter"><Icon name="spectrum" size={13} /> Master bus</div>
      <div className="chain-arrow">→</div>
      {masterFx.toArray().map(f => <FxCard key={f.get('id')} trackId="master" fx={f} />)}
      <button className="add-fx" data-info="Add an effect to the master bus (applies to the whole mix)"
        onClick={e => openMenu(e, EFFECTS.map(ef => ({
          label: <><Icon name={ef.icon} size={12} /> {ef.label}</>,
          fn: () => addFx('master', ef.type, defaultsFor(ef.params)),
        })))}>
        <Icon name="plus" size={12} /> Effect
      </button>
    </div>
  )
}

export function DeviceRack() {
  const selTrackId = useUI(s => s.selTrackId)
  const track = selTrackId && selTrackId !== 'master' ? trackById(selTrackId) : undefined
  useY(track)
  // Only the open device rack runs per-device meters (engine scopes the analysers
  // to this track); release them when the rack closes or the selection changes.
  useEffect(() => {
    engine.setMeteredTrack(selTrackId ?? null)
    return () => engine.setMeteredTrack(null)
  }, [selTrackId])
  if (selTrackId === 'master') return <MasterRack />
  if (!selTrackId || !track) {
    return <div className="roll-empty">Select a track to see its instrument & effects</div>
  }
  const fxArr = track.get('fx') as Y.Array<Y.Map<any>>
  const lfoArr = track.get('lfos') as Y.Array<Y.Map<any>> | undefined
  const midiArr = midifxOf(track)
  const kind = track.get('kind')
  return (
    <div className="rack">
      {kind !== 'drum' && kind !== 'bus' && (
        <>
          {midiArr?.toArray().map(m => <MidiFxCard key={m.get('id')} trackId={selTrackId} fx={m} />)}
          <button className="add-fx add-midi" data-info="Add a live MIDI effect (processes notes before the instrument)"
            onClick={e => openMenu(e, MIDI_FX.map(mf => ({
              label: <><Icon name={mf.icon} size={12} /> {mf.label}</>,
              fn: () => addMidiFx(selTrackId, mf.type, defaultsFor(mf.params)),
            })))}>
            <Icon name="plus" size={12} /> MIDI
          </button>
          <div className="chain-arrow">→</div>
        </>
      )}
      {kind === 'bus'
        ? <div className="bus-input-label" data-info="Audio arriving from sends enters here"><Icon name="send" size={13} /> Bus input</div>
        : <InstrumentPanel trackId={selTrackId} track={track} />}
      <div className="chain-arrow">→</div>
      {fxArr.toArray().map(f => <FxCard key={f.get('id')} trackId={selTrackId} fx={f} />)}
      <button
        className="add-fx"
        data-info="Add an effect to this track's chain"
        onClick={e => openMenu(e, EFFECTS.map(ef => ({
          label: <><Icon name={ef.icon} size={12} /> {ef.label}</>,
          fn: () => addFx(selTrackId, ef.type, defaultsFor(ef.params)),
        })))}
      >
        <Icon name="plus" size={12} /> Effect
      </button>
      {lfoArr?.toArray().map(l => <LfoCard key={l.get('id')} trackId={selTrackId} track={track} lfo={l} />)}
      <button className="add-fx add-lfo" data-info="Add an LFO — modulate any knob on this track around its set value"
        onClick={() => addLfo(selTrackId)}>
        <Icon name="lfo" size={12} /> LFO
      </button>
      <MacroPanel trackId={selTrackId} track={track} />
      <SendsCard trackId={selTrackId} track={track} />
    </div>
  )
}
