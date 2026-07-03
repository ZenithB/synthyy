// Top bar: transport, tempo/swing/key, view switch, undo, export, share, presence.

import React, { useRef, useState, useSyncExternalStore } from 'react'
import { LAUNCH_Q_OPTIONS, clamp } from '../types'
import { meta, setBpm, setSwing, setHumanize, setSwingSubdivision, setTitle, setKeyScale, setLaunchQ, setTimeSignature, timeSig, barTicks, beatTicks, TS_TOP_MAX, TS_BOTTOMS } from '../state/doc'
import { undoMgr } from '../state/undo'
import { engine } from '../audio/engine'
import { setUI, ui, useUI } from '../state/store'
import { useY, useRaf } from './hooks'
import { NumberDrag, Knob, openMenu } from './widgets'
import { Icon } from './icons'
import { NOTE_NAMES, SCALES } from '../theory'
import { ExportDialog } from './ExportDialog'
import { captureToClip, setKbdEnabled, isKbdEnabled } from '../audio/input'
import { peersList, subscribeAwareness, awarenessVersion } from '../state/net'
import { ticksToBBS } from '../types'

const SWING_SPEC = { key: 'swing', label: 'Swing', min: 0, max: 1, def: 0, fmt: (v: number) => `${Math.round(v * 100)}%` }
const HUMAN_SPEC = { key: 'humanize', label: 'Human', min: 0, max: 1, def: 0, fmt: (v: number) => `${Math.round(v * 100)}%` }
const SWING_SUBS = [{ label: '1/4 note', val: '4n' }, { label: '1/8 note', val: '8n' }, { label: '1/16 note', val: '16n' }]

function PlayButton() {
  const ref = useRef<HTMLButtonElement>(null)
  useRaf(() => {
    if (ref.current) ref.current.classList.toggle('playing', engine.playing)
  })
  return (
    <button ref={ref} className="transport-btn play" data-info="Play / Stop (Space)" onClick={() => engine.togglePlay()}>
      <span className="ico-play"><Icon name="play" /></span>
      <span className="ico-stop"><Icon name="stop" /></span>
    </button>
  )
}

function Position() {
  const ref = useRef<HTMLSpanElement>(null)
  useRaf(() => {
    if (ref.current) ref.current.textContent = ticksToBBS(Math.round(engine.playing ? engine.positionTicks() : engine.arrSeekTicks), barTicks(), beatTicks())
  })
  return <span ref={ref} className="position">1.1.1</span>
}

// Time signature: the two numerals stacked, click either to pick. Top numeral
// (1–30) = counted notes per bar; bottom (2/4/8/16) = which note value counts
// the beat (2 = half, 4 = quarter, 8 = eighth, 16 = sixteenth).
const BOTTOM_NAMES: Record<number, string> = { 2: 'half', 4: 'quarter', 8: 'eighth', 16: '16th' }
function MeterControl() {
  const { top, bottom } = timeSig()
  const pickTop = (e: React.MouseEvent) => openMenu(e, Array.from({ length: TS_TOP_MAX }, (_, i) => ({
    label: `${i + 1 === top ? '✓ ' : '  '}${i + 1}`,
    fn: () => setTimeSignature(i + 1, timeSig().bottom),
  })))
  const pickBottom = (e: React.MouseEvent) => openMenu(e, TS_BOTTOMS.map(b => ({
    label: `${b === bottom ? '✓ ' : '  '}${b} — ${BOTTOM_NAMES[b]} notes`,
    fn: () => setTimeSignature(timeSig().top, b),
  })))
  return (
    <div className="meter-ctl" data-info={`Time signature: ${top} ${BOTTOM_NAMES[bottom]}-notes per bar. Click the top numeral (beats per bar) or the bottom one (counted note value) to change.`}>
      <button className="meter-num" onClick={pickTop}>{top}</button>
      <button className="meter-num" onClick={pickBottom}>{bottom}</button>
    </div>
  )
}

function Avatars() {
  useSyncExternalStore(subscribeAwareness, awarenessVersion)
  const peers = peersList()
  return (
    <div className="avatars" data-info="Who's in this jam — share the link to invite friends" onClick={() => setUI({ shareOpen: true })}>
      {peers.slice(0, 6).map(p => (
        <span key={p.id} className="avatar" style={{ background: p.state.color }} title={p.state.name + (p.me ? ' (you)' : '')}>
          {(p.state.name || '?').slice(0, 1).toUpperCase()}
        </span>
      ))}
      {peers.length > 6 && <span className="avatar more">+{peers.length - 6}</span>}
    </div>
  )
}

export function Topbar() {
  useY(meta)
  const view = useUI(s => s.view)
  const recording = useUI(s => s.recording)
  const metronome = useUI(s => s.metronome)
  const theme = useUI(s => s.theme)
  const chatUnread = useUI(s => s.chatUnread)
  const uiZoom = useUI(s => s.uiZoom)
  const taps = useRef<number[]>([])
  const [kbdOn, setKbdOn] = useState(isKbdEnabled())
  const nudgeZoom = (d: number) => setUI({ uiZoom: clamp(Math.round((ui.uiZoom + d) * 20) / 20, 0.6, 1.6) })

  const moreMenu = (e: React.MouseEvent) => openMenu(e, [
    { label: <><Icon name="clock" size={12} /> Undo history</>, fn: () => setUI({ historyOpen: !ui.historyOpen }) },
    { label: <><Icon name="more" size={12} /> Command palette &nbsp;⌘K</>, fn: () => setUI({ paletteOpen: true }) },
    { label: <><Icon name="keys" size={12} /> Keyboard piano: {kbdOn ? 'On' : 'Off'}</>, fn: () => { setKbdEnabled(!isKbdEnabled()); setKbdOn(isKbdEnabled()) } },
    { label: <><Icon name={theme === 'dark' ? 'sun' : 'moon'} size={12} /> Theme: {theme === 'dark' ? 'Dark' : 'Light'}</>, fn: () => setUI({ theme: theme === 'dark' ? 'light' : 'dark' }) },
    'sep',
    { label: <><Icon name="tools" size={12} /> Audio settings</>, fn: () => setUI({ audioSettingsOpen: true }) },
    { label: <><Icon name="spectrum" size={12} /> Performance monitor</>, fn: () => setUI({ perfMonitorOpen: true }) },
    { label: <><Icon name="map" size={12} /> Help & shortcuts</>, fn: () => setUI({ helpOpen: true }) },
  ])

  const tap = () => {
    const now = performance.now()
    taps.current = taps.current.filter(t => now - t < 3000)
    taps.current.push(now)
    if (taps.current.length >= 3) {
      const iv = taps.current.slice(1).map((t, i) => t - taps.current[i])
      const avg = iv.reduce((a, b) => a + b, 0) / iv.length
      setBpm(Math.min(240, Math.max(40, 60000 / avg)))
    }
  }

  const [exportOpen, setExportOpen] = useState(false)

  return (
    <div className="topbar">
      <div className="brand" data-info="Synthtagram — make a song together"><Icon name="logo" size={18} /><b>Synthtagram</b></div>
      <input
        className="title-input"
        value={meta.get('title') ?? ''}
        onChange={e => setTitle(e.target.value)}
        data-info="Project title"
      />

      <div className="tgroup transport">
        <PlayButton />
        <button
          className={`transport-btn rec ${recording ? 'on' : ''}`}
          data-info="Record: notes you play go into the armed track's playing clip"
          onClick={() => setUI({ recording: !recording })}
        ><Icon name="rec" /></button>
        <button className="transport-btn" data-info="Capture the last 30s you played into a clip — even though you weren't recording"
          onClick={() => captureToClip()}><Icon name="capture" /></button>
        <button className={`transport-btn ${metronome ? 'on' : ''}`} data-info="Metronome (M)"
          onClick={() => setUI({ metronome: !metronome })}><Icon name="metro" /></button>
        <Position />
      </div>

      <div className="tgroup">
        <label className="tlabel" data-info="Tempo — drag the number or tap">BPM</label>
        <NumberDrag value={meta.get('bpm') ?? 120} min={40} max={240} step={1} onChange={setBpm} info="Tempo: drag vertically, double-click to type" />
        <button className="tbtn" onClick={tap} data-info="Tap tempo — tap 3+ times">TAP</button>
        <MeterControl />
        <div
          data-info="Swing groove — right-click to set its timing (1/4, 1/8, 1/16 note)"
          onContextMenu={e => {
            const cur = (meta.get('swingSubdivision') as string) ?? '16n'
            openMenu(e, SWING_SUBS.map(s => ({ label: `${cur === s.val ? '✓ ' : '   '}Swing on ${s.label}`, fn: () => setSwingSubdivision(s.val) })))
          }}
        >
          <Knob spec={SWING_SPEC} value={meta.get('swing') ?? 0} onChange={setSwing} size={26} />
        </div>
        <Knob spec={HUMAN_SPEC} value={meta.get('humanize') ?? 0} onChange={setHumanize} size={26} />
      </div>

      <div className="tgroup">
        <select className="tselect" value={meta.get('root') ?? 9} data-info="Project key root — drives scale highlighting & snap"
          onChange={e => setKeyScale(+e.target.value, meta.get('scale') ?? 'minor')}>
          {NOTE_NAMES.map((n, i) => <option key={n} value={i}>{n}</option>)}
        </select>
        <select className="tselect" value={meta.get('scale') ?? 'minor'} data-info="Project scale"
          onChange={e => setKeyScale(meta.get('root') ?? 9, e.target.value)}>
          {SCALES.map(s => <option key={s.id} value={s.id}>{s.label}</option>)}
        </select>
        <select className="tselect" value={meta.get('launchQ') ?? 1} data-info="Launch quantize: clips wait for this boundary so nothing comes in off-beat"
          onChange={e => setLaunchQ(+e.target.value)}>
          {LAUNCH_Q_OPTIONS.map(o => <option key={o.bars} value={o.bars}>Q: {o.label}</option>)}
        </select>
      </div>

      <div className="tgroup view-tabs">
        <button className={`vtab ${view === 'session' ? 'on' : ''}`} data-info="Session view: launch clips & jam (Tab)"
          onClick={() => setUI({ view: 'session' })}>Session</button>
        <button className={`vtab ${view === 'arr' ? 'on' : ''}`} data-info="Arrangement view: the song timeline (Tab)"
          onClick={() => setUI({ view: 'arr' })}>Arrange</button>
      </div>

      <div className="spacer" />

      <div className="tgroup zoom-group">
        <button className="icon-btn" data-info="Zoom the interface out" onClick={() => nudgeZoom(-0.1)}><Icon name="zoomOut" /></button>
        <span className="zoom-pct" data-info="Interface zoom — click to reset to 100%" onClick={() => setUI({ uiZoom: 1 })}>{Math.round(uiZoom * 100)}%</span>
        <button className="icon-btn" data-info="Zoom the interface in" onClick={() => nudgeZoom(0.1)}><Icon name="zoomIn" /></button>
      </div>

      <div className="tgroup">
        <button className="icon-btn" data-info="Undo your last edit (Ctrl/Cmd+Z) — only undoes YOUR changes" onClick={() => undoMgr.undo()}><Icon name="undo" /></button>
        <button className="icon-btn" data-info="Redo (Ctrl/Cmd+Shift+Z)" onClick={() => undoMgr.redo()}><Icon name="redo" /></button>
        <button className="icon-btn" data-info="Export audio & project" onClick={() => setExportOpen(true)}><Icon name="download" /></button>
        <button className="icon-btn" data-info="More: history, palette, keyboard piano, theme, help" onClick={moreMenu}><Icon name="more" /></button>
      </div>

      <div className="tgroup">
        <button className="share-btn" data-info="Invite friends — everyone edits the same project live" onClick={() => setUI({ shareOpen: true })}>
          <Icon name="link" /> Share
        </button>
        <Avatars />
        <button className="icon-btn chat-toggle" data-info="Chat with your collaborators" onClick={() => setUI({ chatOpen: !ui.chatOpen, chatUnread: 0 })}>
          <Icon name="chat" />{chatUnread > 0 && <span className="badge">{chatUnread}</span>}
        </button>
      </div>

      {exportOpen && <ExportDialog onClose={() => setExportOpen(false)} />}
    </div>
  )
}
