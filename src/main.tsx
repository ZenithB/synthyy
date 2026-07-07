import React from 'react'
import { createRoot } from 'react-dom/client'
import './styles.css'
import { App } from './ui/App'
import { idb, initIfEmpty, isDocEmpty, maybeTakeCarried, roomId, cleanupLegacyReturns, ensureFeedbackBus } from './state/doc'
import { startP2P, setPresence, requestState } from './state/net'
import { DEFAULT_PROJECT } from './packs'
import { initKeyboardPiano, initWebMidi } from './audio/input'
import { engine } from './audio/engine'
import { ui } from './state/store'

async function boot() {
  await idb.whenSynced

  // moving a project into a fresh share-room carries the FULL document across
  // the reload (lossless — sends, LFOs, macros, returns, automation and all)
  maybeTakeCarried()

  // one-time migration: drop legacy A/B return channels (replaced by A/B buses)
  cleanupLegacyReturns()

  if (roomId) {
    // Joining someone's room: wait for the project to arrive over P2P. Crucially
    // we don't just sit waiting for the host's one-shot push (which can be dropped
    // on a flaky cellular link, leaving us blank forever) — while the doc is empty
    // and a peer is present we actively RE-REQUEST the full state (pull). Hard caps
    // guarantee we never stay blank: alone → seed a fresh default quickly; peer
    // present but no state after the cap → seed default as a last resort (Yjs
    // merges cleanly if the real project then shows up late).
    const t0 = performance.now()
    const ALONE_GIVEUP = 9000     // no peer this long → we're genuinely alone
    const HARD_CAP = 22000        // peer present but silent this long → stop waiting
    const tick = () => {
      if (!isDocEmpty()) return                       // project arrived → done
      const elapsed = performance.now() - t0
      if (ui.peerCount > 0) {
        requestState()                                // pull: re-ask peers for the project
        if (elapsed < HARD_CAP) { setTimeout(tick, 1500); return }
      } else if (elapsed < ALONE_GIVEUP) {
        setTimeout(tick, 1500); return
      }
      initIfEmpty(DEFAULT_PROJECT)                     // never leave the user on a blank screen
    }
    setTimeout(tick, 1500)
  } else {
    initIfEmpty(DEFAULT_PROJECT)
    ensureFeedbackBus()   // add the Feedback bus to projects saved before it existed
  }

  startP2P()
  initKeyboardPiano()
  initWebMidi()
  setPresence({ name: ui.userName, color: ui.userColor, view: ui.view, sel: null, ph: null })

  // browsers require a user gesture before audio — grab the first one
  const wake = () => { engine.ensureStarted() }
  window.addEventListener('pointerdown', wake, { once: true })
  window.addEventListener('keydown', wake, { once: true })
  // Reload only for EXTERNAL hash changes (pasting a room link, editing the
  // URL). Programmatic room transitions set __sfNav and reload themselves —
  // letting this fire too would double-reload and drop the carried project.
  window.addEventListener('hashchange', () => { if (!(window as any).__sfNav) location.reload() })

  createRoot(document.getElementById('root')!).render(<App />)
}

boot()
