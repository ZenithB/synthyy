// App shell: layout, global shortcuts, presence heartbeat, info-view hover.

import React, { useEffect } from 'react'
import { Topbar } from './Topbar'
import { Browser } from './Browser'
import { SessionView } from './SessionView'
import { ArrangementView } from './ArrangementView'
import { PianoRoll } from './PianoRoll'
import { AudioEditor } from './AudioEditor'
import { DeviceRack } from './DeviceRack'
import { Toasts, UndoPanel, CommandPalette, ShareDialog, HelpModal, ChatPanel, Onboard, StatusBar, AudioErrorBanner, FeedbackModal } from './panels'
import { AudioSettings } from './AudioSettings'
import { PerfMonitor } from './PerfMonitor'
import { ContextMenuHost } from './widgets'
import { setUI, ui, useUI } from '../state/store'
import { engine } from '../audio/engine'
import { undoMgr } from '../state/undo'
import { chat, scenes, tracks, deleteClipAt, deleteArrClip, duplicateArrClip, getClipMap, isAudioClip } from '../state/doc'
import { setPresence } from '../state/net'
import { copyClipRef, pasteClipTo, duplicateClipToNextScene, importAudioToBank, copyArrSelection, hasArrClipboard, pasteArrClipboard } from './actions'
import { collectDroppedAudio } from '../audio/samples'

export function App() {
  const view = useUI(s => s.view)
  const detailOpen = useUI(s => s.detailOpen)
  const detailTab = useUI(s => s.detailTab)
  const selClip = useUI(s => s.selClip)
  const detailHeight = useUI(s => s.detailHeight)
  const uiZoom = useUI(s => s.uiZoom)
  const theme = useUI(s => s.theme)

  useEffect(() => {
    document.documentElement.dataset.theme = theme
  }, [theme])

  useEffect(() => {
    // CSS zoom scales the whole UI (incl. portaled menus/modals) and reflows.
    ;(document.documentElement.style as any).zoom = String(uiZoom)
  }, [uiZoom])

  // Drag the divider above the detail panel to resize it. Derives the zoom
  // factor from the element itself so it stays accurate under UI zoom.
  const startDetailResize = (e: React.PointerEvent) => {
    e.preventDefault()
    const detailEl = (e.currentTarget as HTMLElement).parentElement as HTMLElement
    const rect = detailEl.getBoundingClientRect()
    const ratio = (rect.height / (ui.detailHeight || rect.height)) || 1
    const startY = e.clientY
    const startH = ui.detailHeight
    const move = (ev: PointerEvent) => {
      const delta = (startY - ev.clientY) / ratio
      setUI({ detailHeight: Math.max(150, Math.min(900, Math.round(startH + delta))) })
    }
    const up = () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
    }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
  }

  // Info View: hover help in the status bar (the Ableton trick)
  useEffect(() => {
    let last = ''
    const h = (e: MouseEvent) => {
      const el = (e.target as HTMLElement).closest?.('[data-info]') as HTMLElement | null
      const txt = el?.dataset.info ?? ''
      if (txt !== last) {
        last = txt
        setUI({ infoText: txt })
      }
    }
    document.addEventListener('mouseover', h)
    return () => document.removeEventListener('mouseover', h)
  }, [])

  // presence: view + playhead heartbeat
  useEffect(() => {
    setPresence({ view })
  }, [view])
  useEffect(() => {
    const iv = setInterval(() => {
      setPresence({ ph: engine.playing ? { mode: engine.mode, ticks: Math.round(engine.positionTicks()) } : null })
    }, 400)
    return () => clearInterval(iv)
  }, [])

  // chat unread counter
  useEffect(() => {
    const h = () => { if (!ui.chatOpen) setUI({ chatUnread: ui.chatUnread + 1 }) }
    chat.observe(h)
    return () => chat.unobserve(h)
  }, [])

  // drag audio files — or entire folders — anywhere onto the app: they land in
  // the sample bank (Samples section of the browser); no track is auto-created.
  useEffect(() => {
    const over = (e: DragEvent) => { if (e.dataTransfer?.types.includes('Files')) { e.preventDefault(); document.body.classList.add('drag-audio') } }
    const leave = (e: DragEvent) => { if (e.relatedTarget === null) document.body.classList.remove('drag-audio') }
    const drop = async (e: DragEvent) => {
      document.body.classList.remove('drag-audio')
      if (!e.dataTransfer?.types.includes('Files')) return
      // browser sections handle their own file drops (drop into a folder)
      if ((e.target as HTMLElement)?.closest?.('.bsamples')) return
      e.preventDefault()
      const items = await collectDroppedAudio(e.dataTransfer)
      if (items.length) importAudioToBank(items)
    }
    window.addEventListener('dragover', over)
    window.addEventListener('dragleave', leave)
    window.addEventListener('drop', drop)
    return () => { window.removeEventListener('dragover', over); window.removeEventListener('dragleave', leave); window.removeEventListener('drop', drop) }
  }, [])

  // global shortcuts
  useEffect(() => {
    const h = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement
      const typing = el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.isContentEditable)
      const mod = e.metaKey || e.ctrlKey
      if (mod && e.key.toLowerCase() === 'k') { setUI({ paletteOpen: !ui.paletteOpen }); e.preventDefault(); return }
      if (typing) return
      if (e.key === ' ') { e.preventDefault(); engine.togglePlay(); return }
      if (e.key === 'Tab') { e.preventDefault(); setUI({ view: ui.view === 'session' ? 'arr' : 'session' }); return }
      if (mod && e.key.toLowerCase() === 'z') { e.preventDefault(); if (e.shiftKey) undoMgr.redo(); else undoMgr.undo(); return }
      if (mod && e.key.toLowerCase() === 'y') { e.preventDefault(); undoMgr.redo(); return }
      if (e.key === 'Escape') {
        if (ui.paletteOpen || ui.shareOpen || ui.helpOpen || ui.historyOpen) {
          setUI({ paletteOpen: false, shareOpen: false, helpOpen: false, historyOpen: false })
        } else if (ui.timeSel) setUI({ timeSel: null })
        else if (ui.detailOpen) setUI({ detailOpen: false })
        return
      }
      if (e.key === '?') { setUI({ helpOpen: !ui.helpOpen }); return }
      if (e.key.toLowerCase() === 'b' && !mod) { setUI({ drawMode: !ui.drawMode }); return }
      if (e.key.toLowerCase() === 'm' && !mod) { setUI({ metronome: !ui.metronome }); return }
      if (/^[1-9]$/.test(e.key) && !mod) {
        const idx = +e.key - 1
        if (idx < scenes.length) engine.launchScene(scenes.get(idx).get('id'))
        return
      }
      // arrangement marquee selection (multiple clips) takes priority
      if (!ui.detailOpen && ui.selArrIds.length) {
        if (e.key === 'Delete' || e.key === 'Backspace') {
          ui.selArrIds.forEach(id => deleteArrClip(id))
          setUI({ selArrIds: [] })
          return
        }
        if (mod && e.key.toLowerCase() === 'd') {
          e.preventDefault()
          ui.selArrIds.forEach(id => duplicateArrClip(id))
          return
        }
        if (mod && e.key.toLowerCase() === 'c') { copyArrSelection(ui.selArrIds); return }
        if (mod && e.key.toLowerCase() === 'x') {
          copyArrSelection(ui.selArrIds)
          ui.selArrIds.forEach(id => deleteArrClip(id))
          setUI({ selArrIds: [] })
          return
        }
      }
      if (!ui.detailOpen && ui.selClip) {
        if (e.key === 'Delete' || e.key === 'Backspace') {
          if (ui.selClip.kind === 'session') deleteClipAt(ui.selClip.trackId, ui.selClip.sceneId)
          else deleteArrClip(ui.selClip.id)
          setUI({ selClip: null })
          return
        }
        if (mod && e.key.toLowerCase() === 'd') {
          e.preventDefault()
          if (ui.selClip.kind === 'session') duplicateClipToNextScene(ui.selClip)
          else duplicateArrClip(ui.selClip.id)
          return
        }
        if (mod && e.key.toLowerCase() === 'c') {
          if (ui.selClip.kind === 'arr') copyArrSelection([ui.selClip.id])
          else copyClipRef(ui.selClip)
          return
        }
      }
      if (mod && e.key.toLowerCase() === 'v') {
        if (ui.selClip?.kind === 'session') { pasteClipTo(ui.selClip.trackId, ui.selClip.sceneId); return }
        if (ui.view === 'arr' && hasArrClipboard()) {
          const laneIds = tracks.toArray().filter(t => t.get('kind') !== 'bus').map(t => t.get('id'))
          const tid = (ui.selTrackId && laneIds.includes(ui.selTrackId)) ? ui.selTrackId : laneIds[0]
          if (tid) pasteArrClipboard(Math.round(engine.arrSeekTicks), tid)
        }
      }
    }
    window.addEventListener('keydown', h)
    return () => window.removeEventListener('keydown', h)
  }, [])

  return (
    <div className="app">
      <Topbar />
      <AudioErrorBanner />
      <div className="main">
        <Browser />
        <div className="center">
          {view === 'session' ? <SessionView /> : <ArrangementView />}
        </div>
        <ChatPanel />
        <UndoPanel />
      </div>
      {detailOpen && (
        <div className="detail" style={{ height: detailHeight }}>
          <div className="detail-resize" data-info="Drag up/down to resize the editor panel" onPointerDown={startDetailResize} />
          <div className="detail-tabs">
            <button className={`dtab ${detailTab === 'clip' ? 'on' : ''}`} onClick={() => setUI({ detailTab: 'clip' })} data-info="Edit the selected clip's notes">Clip</button>
            <button className={`dtab ${detailTab === 'devices' ? 'on' : ''}`} onClick={() => setUI({ detailTab: 'devices' })} data-info="The selected track's instrument & effect chain">Devices</button>
          </div>
          <div className="detail-body">
            {detailTab === 'devices' ? <DeviceRack /> : (isAudioClip(getClipMap(selClip)) ? <AudioEditor /> : <PianoRoll />)}
          </div>
        </div>
      )}
      <StatusBar />
      <Toasts />
      <ContextMenuHost />
      <CommandPalette />
      <ShareDialog />
      <HelpModal />
      <AudioSettings />
      <PerfMonitor />
      <FeedbackModal />
      <Onboard />
    </div>
  )
}
