// Secondary surfaces: toasts, undo-history panel, command palette, share
// dialog, help, chat drawer, onboarding card, status bar (info view).

import React, { useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react'
import { USER_COLORS } from '../types'
import { setUI, ui, useUI, toasts, useToasts, toast } from '../state/store'
import { undoMgr, undoHistory, undoTo, subscribeHistory, historyVersion, redoCount } from '../state/undo'
import { chat, sendChat, roomId, createRoomAndGo, leaveRoomAndGo, meta } from '../state/doc'
import { useY } from './hooks'
import { Modal } from './widgets'
import { setPresence } from '../state/net'
import { engine } from '../audio/engine'
import { loadDemo, newProject, addSynthTrack, addDrumTrack, importProjectFile, applyPreset } from './actions'
import { exportAudio, exportProjectFile } from '../audio/render'
import { captureToClip, enableMidi } from '../audio/input'
import { addFx } from '../state/doc'
import { EFFECTS, defaultsFor } from '../audio/schema'
import { INST_PRESETS } from '../packs'
import { Icon } from './icons'

// ---------------- toasts ----------------
export function Toasts() {
  useToasts()
  return (
    <div className="toasts">
      {toasts.map(t => <div key={t.id} className="toast">{t.text}</div>)}
    </div>
  )
}

// ---------------- feedback-loop warning (routing a cycle) ----------------
export function FeedbackModal() {
  const prompt = useUI(s => s.feedbackPrompt)
  if (!prompt) return null
  const close = () => setUI({ feedbackPrompt: null })
  return (
    <Modal title="⚠ Feedback loop" onClose={close} width={400}>
      <div className="feedback-modal">
        <p>{prompt.msg}</p>
        <p className="feedback-sub">Routing a signal back into itself can build up quickly. Continue anyway, or undo this change?</p>
        <div className="feedback-actions">
          <button className="export-btn ghost" onClick={close}>Undo</button>
          <button className="export-btn record" onClick={() => { prompt.apply(); close() }}>Continue anyway</button>
        </div>
      </div>
    </Modal>
  )
}

// ---------------- audio-failure banner (persistent, must be acted on) ----------------
export function AudioErrorBanner() {
  const err = useUI(s => s.audioError)
  if (!err) return null
  return (
    <div className="audio-error" role="alert">
      <i className="dot warn" />
      <span className="audio-error-msg">{err}</span>
      <button className="audio-error-retry" onClick={() => { setUI({ audioError: null }); engine.ensureStarted() }}>Retry</button>
      <button className="audio-error-x" onClick={() => setUI({ audioError: null })} aria-label="Dismiss">×</button>
    </div>
  )
}

// ---------------- undo history ----------------
export function UndoPanel() {
  useSyncExternalStore(subscribeHistory, historyVersion)
  const open = useUI(s => s.historyOpen)
  if (!open) return null
  const items = undoHistory()
  return (
    <div className="undo-panel">
      <div className="panel-head">
        <span>Your edit history</span>
        <button className="icon-btn" onClick={() => setUI({ historyOpen: false })}><Icon name="close" size={12} /></button>
      </div>
      <div className="undo-list">
        {items.length === 0 && <div className="undo-empty">No edits yet — go make some noise</div>}
        {items.map((it, i) => (
          <button key={i} className="undo-item" onClick={() => undoTo(i)}
            data-info="Click to rewind your edits back to this point">
            <span className="undo-label">{it.label}</span>
            <span className="undo-time">{it.t ? new Date(it.t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' }) : ''}</span>
          </button>
        )).reverse()}
      </div>
      <div className="undo-foot">
        <button className="bbtn" disabled={!items.length} onClick={() => undoMgr.undo()}>↶ Undo</button>
        <button className="bbtn" disabled={redoCount() === 0} onClick={() => undoMgr.redo()}>↷ Redo ({redoCount()})</button>
      </div>
    </div>
  )
}

// ---------------- command palette ----------------
type Cmd = { title: string; hint?: string; run: () => void }

function buildCommands(): Cmd[] {
  const cmds: Cmd[] = [
    { title: '▶ Play / Stop', hint: 'Space', run: () => engine.togglePlay() },
    { title: 'Switch view: Session ↔ Arrangement', hint: 'Tab', run: () => setUI({ view: ui.view === 'session' ? 'arr' : 'session' }) },
    { title: 'Add synth track', run: () => addSynthTrack() },
    { title: 'Add drum track', run: () => addDrumTrack() },
    { title: 'Capture last played notes → clip', run: () => captureToClip() },
    { title: 'Toggle metronome', hint: 'M', run: () => setUI({ metronome: !ui.metronome }) },
    { title: 'Toggle draw mode', hint: 'B', run: () => setUI({ drawMode: !ui.drawMode }) },
    { title: 'Toggle record', run: () => setUI({ recording: !ui.recording }) },
    { title: 'Share / invite friends', run: () => setUI({ shareOpen: true }) },
    { title: 'Open chat', run: () => setUI({ chatOpen: true, chatUnread: 0 }) },
    { title: 'Undo history panel', run: () => setUI({ historyOpen: true }) },
    { title: 'Toggle theme (light/dark)', run: () => setUI({ theme: ui.theme === 'dark' ? 'light' : 'dark' }) },
    { title: 'Load demo song', run: loadDemo },
    { title: 'New project', run: newProject },
    { title: 'Export WAV — arrangement', run: () => exportAudio({ kind: 'arr' }, { format: 'wav', channels: 'stereo' }) },
    { title: 'Export WAV — loop region', run: () => exportAudio({ kind: 'loop' }, { format: 'wav', channels: 'stereo' }) },
    { title: 'Export stems (WAV)', run: () => exportAudio({ kind: 'arr' }, { format: 'wav', channels: 'stereo', stems: true }) },
    { title: 'Save project file', run: exportProjectFile },
    { title: 'Import project file', run: importProjectFile },
    { title: 'Help & shortcuts', hint: '?', run: () => setUI({ helpOpen: true }) },
  ]
  EFFECTS.forEach(ef => cmds.push({
    title: `Add effect: ${ef.label}`,
    hint: `${ef.group} · selected track`,
    run: () => { if (ui.selTrackId) addFx(ui.selTrackId, ef.type, defaultsFor(ef.params)); else toast('Select a track first') },
  }))
  INST_PRESETS.forEach(p => cmds.push({
    title: `Preset: ${p.name}`,
    hint: p.cat,
    run: () => applyPreset(p),
  }))
  return cmds
}

export function CommandPalette() {
  const open = useUI(s => s.paletteOpen)
  const [q, setQ] = useState('')
  const [idx, setIdx] = useState(0)
  const all = useMemo(buildCommands, [open])
  useEffect(() => { if (open) { setQ(''); setIdx(0) } }, [open])
  if (!open) return null
  const filtered = all.filter(c => c.title.toLowerCase().includes(q.toLowerCase())).slice(0, 14)
  const run = (c: Cmd) => { setUI({ paletteOpen: false }); c.run() }
  return (
    <div className="modal-backdrop palette-backdrop" onPointerDown={e => { if (e.target === e.currentTarget) setUI({ paletteOpen: false }) }}>
      <div className="palette">
        <input
          autoFocus
          placeholder="Type a command… (add effect, preset, export…)"
          value={q}
          onChange={e => { setQ(e.target.value); setIdx(0) }}
          onKeyDown={e => {
            if (e.key === 'ArrowDown') { setIdx(i => Math.min(i + 1, filtered.length - 1)); e.preventDefault() }
            if (e.key === 'ArrowUp') { setIdx(i => Math.max(i - 1, 0)); e.preventDefault() }
            if (e.key === 'Enter' && filtered[idx]) run(filtered[idx])
            if (e.key === 'Escape') setUI({ paletteOpen: false })
          }}
        />
        <div className="palette-list">
          {filtered.map((c, i) => (
            <button key={c.title} className={`palette-item ${i === idx ? 'active' : ''}`} onPointerEnter={() => setIdx(i)} onClick={() => run(c)}>
              <span>{c.title}</span>
              {c.hint && <span className="palette-hint">{c.hint}</span>}
            </button>
          ))}
          {!filtered.length && <div className="palette-none">No matches</div>}
        </div>
      </div>
    </div>
  )
}

// ---------------- share dialog ----------------
export function ShareDialog() {
  const open = useUI(s => s.shareOpen)
  const name = useUI(s => s.userName)
  const color = useUI(s => s.userColor)
  const status = useUI(s => s.netStatus)
  const peers = useUI(s => s.peerCount)
  if (!open) return null
  const link = location.href
  return (
    <Modal title="Make a song together" onClose={() => setUI({ shareOpen: false })}>
      <div className="share-body">
        <label className="share-row">
          <span>Your name</span>
          <input value={name} onChange={e => { setUI({ userName: e.target.value }); setPresence({ name: e.target.value }) }} />
        </label>
        <div className="share-row">
          <span>Your color</span>
          <div className="color-row">
            {USER_COLORS.map(c => (
              <button key={c} className={`color-swatch ${c === color ? 'sel' : ''}`} style={{ background: c }}
                onClick={() => { setUI({ userColor: c }); setPresence({ color: c }) }} />
            ))}
          </div>
        </div>
        <hr />
        {roomId ? (
          <>
            <p>This project is live. Anyone with the link joins instantly — edits sync note-by-note, presence and all. <b>{peers}</b> {peers === 1 ? 'person is' : 'people are'} here now ({status}).</p>
            <div className="share-link">
              <input readOnly value={link} onFocus={e => e.target.select()} />
              <button className="bbtn" onClick={() => { navigator.clipboard.writeText(link).then(() => toast('Link copied')) }}>Copy</button>
            </div>
            <p className="share-small">Peer-to-peer via WebRTC — no server keeps your music. Each visitor keeps a local copy (autosaved), so the project survives everyone leaving.</p>
            <button className="bbtn" onClick={leaveRoomAndGo}>Leave room (back to local project)</button>
          </>
        ) : (
          <>
            <p>Create a share link and this project becomes a live multiplayer session — like a Google Doc, but it slaps.</p>
            <button className="bbtn primary" onClick={() => { createRoomAndGo() }}><Icon name="link" size={13} />Create share link</button>
            <p className="share-small">Your current project comes with you. Playback stays local to each person; the music data syncs in real time.</p>
          </>
        )}
      </div>
    </Modal>
  )
}

// ---------------- help ----------------
const SHORTCUTS: [string, string][] = [
  ['Space', 'Play / stop'],
  ['Tab', 'Session ↔ Arrangement'],
  ['A–K rows', 'Play notes (Z/X octave, C/V velocity)'],
  ['1–9', 'Launch scene'],
  ['B', 'Draw mode'],
  ['M', 'Metronome'],
  ['Ctrl/Cmd+Z / +Shift+Z', 'Undo / redo (your edits only)'],
  ['Ctrl/Cmd+K', 'Command palette'],
  ['Ctrl/Cmd+D', 'Duplicate notes / clip'],
  ['Ctrl/Cmd+A', 'Select all notes'],
  ['Delete', 'Delete selection'],
  ['Arrows', 'Nudge notes (Shift = octave)'],
  ['Alt+drag', 'Copy notes / clips'],
  ['Shift+drag', 'Fine drag (knobs & timeline)'],
  ['Double-click', 'Create / edit clips & notes'],
  ['Right-click', 'Context menus everywhere'],
]

export function HelpModal() {
  const open = useUI(s => s.helpOpen)
  if (!open) return null
  return (
    <Modal title="Help & shortcuts" onClose={() => setUI({ helpOpen: false })} width={520}>
      <div className="help-body">
        <p><b>The 30-second tour:</b> the grid is your jam space — double-click a slot, draw notes, hit ▶. Clips launch on the next bar so everything stays locked. When a loop feels good, right-click → send it to the Arrangement, then arrange the song on the timeline. Share the link and friends edit with you live.</p>
        <table className="help-table">
          <tbody>
            {SHORTCUTS.map(([k, v]) => <tr key={k}><td><code>{k}</code></td><td>{v}</td></tr>)}
          </tbody>
        </table>
        <p className="share-small">Hover anything to see what it does in the bar below — borrowed straight from Ableton's Info View.</p>
      </div>
    </Modal>
  )
}

// ---------------- chat ----------------
export function ChatPanel() {
  const open = useUI(s => s.chatOpen)
  useY(chat)
  const [text, setText] = useState('')
  const listRef = useRef<HTMLDivElement>(null)
  useEffect(() => {
    listRef.current?.scrollTo(0, 1e9)
  })
  if (!open) return null
  const send = () => {
    const t = text.trim()
    if (!t) return
    sendChat(ui.userName, ui.userColor, t)
    setText('')
  }
  return (
    <div className="chat">
      <div className="panel-head">
        <span>Chat</span>
        <button className="icon-btn" onClick={() => setUI({ chatOpen: false })}><Icon name="close" size={12} /></button>
      </div>
      <div className="chat-list" ref={listRef}>
        {chat.toArray().map((m: any) => (
          <div key={m.id} className="chat-msg">
            <span className="chat-name" style={{ color: m.color }}>{m.name}</span>
            <span className="chat-text">{m.text}</span>
          </div>
        ))}
        {chat.length === 0 && <div className="undo-empty">Say hi — chat syncs with the project</div>}
      </div>
      <div className="chat-input">
        <input value={text} placeholder="Message…" onChange={e => setText(e.target.value)}
          onKeyDown={e => { if (e.key === 'Enter') send(); e.stopPropagation() }} />
        <button className="bbtn" onClick={send}>Send</button>
      </div>
    </div>
  )
}

// ---------------- onboarding ----------------
export function Onboard() {
  // Never onboard when opening a share link — you're JOINING a live session, not
  // starting fresh. Showing the demo prompt here would invite a collaborator to
  // overwrite the shared project with the demo.
  const [open, setOpen] = useState(() => !roomId && localStorage.getItem('stg-onboarded') !== '1')
  const name = useUI(s => s.userName)
  if (!open) return null
  const done = (loadDemoToo: boolean) => {
    localStorage.setItem('stg-onboarded', '1')
    setOpen(false)
    setPresence({ name: ui.userName })
    if (loadDemoToo) loadDemo()
  }
  return (
    <Modal title="Welcome to Synthtagram" onClose={() => done(false)} width={460}>
      <div className="onboard">
        <p>A tiny Ableton-style studio that lives in a link. Launch clips, twist synths, and invite friends to edit the same song <i>live</i>.</p>
        <label className="share-row">
          <span>Your artist name</span>
          <input value={name} onChange={e => setUI({ userName: e.target.value })} />
        </label>
        <ul className="onboard-tips">
          <li><Icon name="keys" size={13} />Keys <b>A–K</b> play the selected track</li>
          <li><Icon name="play" size={13} />Click slot triangles — clips launch on the beat</li>
          <li><Icon name="link" size={13} /><b>Share</b> turns this into a multiplayer session</li>
        </ul>
        <div className="onboard-btns">
          <button className="bbtn primary" onClick={() => done(true)}><Icon name="spark" size={13} />Start with the demo song</button>
          <button className="bbtn" onClick={() => done(false)}>Start from scratch</button>
        </div>
      </div>
    </Modal>
  )
}

// ---------------- status bar (Info View) ----------------
export function StatusBar() {
  const info = useUI(s => s.infoText)
  const status = useUI(s => s.netStatus)
  const peers = useUI(s => s.peerCount)
  const audioReady = useUI(s => s.audioReady)
  const audioError = useUI(s => s.audioError)
  const midi = useUI(s => s.midi)
  const octave = useUI(s => s.octave)
  const velo = useUI(s => s.velo)
  const recording = useUI(s => s.recording)
  // Live output-latency readout (refreshes — outputLatency only populates once
  // audio is flowing, so re-read it periodically rather than once at mount).
  const [latMs, setLatMs] = useState(0)
  useEffect(() => {
    const upd = () => setLatMs(engine.outputLatencyMs())
    upd()
    const iv = setInterval(upd, 2000)
    return () => clearInterval(iv)
  }, [])
  return (
    <div className="statusbar">
      <span className="status-info">{info || 'Hover anything to learn what it does · double-click slots to make clips · A–K plays notes'}</span>
      <span className="status-right">
        {recording && <span className="status-pill rec"><i className="dot rec" />REC</span>}
        <span className="status-pill" data-info="Computer-keyboard octave (Z/X) and velocity (C/V)">Oct {octave} · Vel {Math.round(velo * 100)}</span>
        {midi === 'available' && <button className="status-pill midi-btn" onClick={() => enableMidi()} data-info="Connect a MIDI controller (asks the browser for permission)">Enable MIDI</button>}
        {midi === 'on' && <span className="status-pill ok" data-info="MIDI controller input is active">MIDI ✓</span>}
        {audioError
          ? <span className="status-pill warn" data-info="The audio engine failed to start — see the banner at the top">⚠ audio failed</span>
          : audioReady
            ? <span className="status-pill ok" data-info="Audio engine sample rate. '2x' = oversampling for alias-free FM & distortion (toggle in Audio settings). ~ms is output latency (audio buffer + device) — live notes trigger near-immediately, this is the floor the browser sets.">{Math.round((engine.sampleRate || 0) / 100) / 10} kHz{engine.sampleRate >= 88000 ? ' · 2x' : ''}{latMs ? ` · ~${latMs} ms` : ''}</span>
            : <span className="status-pill warn"><i className="dot warn" />click anywhere to enable audio</span>}
        <span className={`status-pill net-${status}`} data-info="Local: just you (autosaved). Online: synced with friends via P2P">
          {status === 'local' ? <><i className="dot" />Local project</> : status === 'connecting' ? <><i className="dot warn" />Looking for peers…</> : <><i className="dot ok" />Online · {peers}</>}
        </span>
      </span>
    </div>
  )
}
