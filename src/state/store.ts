// Local (non-collaborative) UI state: selection, view, tool settings, user identity.
// Tiny external store consumed via useSyncExternalStore. Selectors must return
// primitives or refs stored in the state (never freshly-built objects).

import { useSyncExternalStore } from 'react'
import { ClipRef, STEP16, randomName, USER_COLORS } from '../types'

export type UIState = {
  view: 'session' | 'arr'
  selTrackId: string | null
  armTrackId: string | null
  selClip: ClipRef | null
  selArrIds: string[]            // arrangement marquee multi-selection (clip ids)
  timeSel: [number, number] | null   // arrangement time-range selection (ticks)
  detailOpen: boolean
  detailTab: 'clip' | 'devices'
  detailHeight: number
  uiZoom: number
  gridTicks: number
  drawLen: number
  drawMode: boolean
  drawProb: number
  snapScale: boolean
  notePreview: boolean          // piano-roll: play a note when selected or moved up/down
  lane: 'vel' | 'prob'
  recording: boolean
  recQuantize: boolean
  metronome: boolean
  theme: 'dark' | 'light'
  chatOpen: boolean
  historyOpen: boolean
  paletteOpen: boolean
  shareOpen: boolean
  helpOpen: boolean
  audioSettingsOpen: boolean
  perfMonitorOpen: boolean
  feedbackPrompt: { msg: string; apply: () => void } | null
  userName: string
  userColor: string
  zoomPxPerBar: number
  octave: number
  velo: number
  infoText: string
  audioReady: boolean
  audioError: string | null
  midi: 'unsupported' | 'available' | 'on'
  peerCount: number
  netStatus: 'local' | 'connecting' | 'online'
  chatUnread: number
}

const savedName = localStorage.getItem('sf-name')
const savedColor = localStorage.getItem('sf-color')
const savedTheme = (localStorage.getItem('sf-theme') as 'dark' | 'light') || 'dark'
const savedHeight = Number(localStorage.getItem('sf-detail-h')) || 318
const savedZoom = Number(localStorage.getItem('sf-zoom')) || 1

export const ui: UIState = {
  view: 'session',
  selTrackId: null,
  armTrackId: null,
  selClip: null,
  selArrIds: [],
  timeSel: null,
  detailOpen: false,
  detailTab: 'clip',
  detailHeight: savedHeight,
  uiZoom: savedZoom,
  gridTicks: STEP16,
  drawLen: STEP16,
  drawMode: false,
  drawProb: 1,
  snapScale: false,
  notePreview: true,
  lane: 'vel',
  recording: false,
  recQuantize: true,
  metronome: false,
  theme: savedTheme,
  chatOpen: false,
  historyOpen: false,
  paletteOpen: false,
  shareOpen: false,
  helpOpen: false,
  audioSettingsOpen: false,
  perfMonitorOpen: false,
  feedbackPrompt: null,
  userName: savedName || randomName(),
  userColor: savedColor || USER_COLORS[Math.floor(Math.random() * USER_COLORS.length)],
  zoomPxPerBar: 96,
  octave: 4,
  velo: 0.85,
  infoText: '',
  audioReady: false,
  audioError: null,
  midi: 'unsupported',
  peerCount: 0,
  netStatus: 'local',
  chatUnread: 0,
}

const listeners = new Set<() => void>()

export function setUI(patch: Partial<UIState>) {
  Object.assign(ui, patch)
  if (patch.userName !== undefined) localStorage.setItem('sf-name', ui.userName)
  if (patch.userColor !== undefined) localStorage.setItem('sf-color', ui.userColor)
  if (patch.theme !== undefined) localStorage.setItem('sf-theme', ui.theme)
  if (patch.detailHeight !== undefined) localStorage.setItem('sf-detail-h', String(ui.detailHeight))
  if (patch.uiZoom !== undefined) localStorage.setItem('sf-zoom', String(ui.uiZoom))
  listeners.forEach(l => l())
}

export function subscribeUI(fn: () => void) {
  listeners.add(fn)
  return () => { listeners.delete(fn) }
}

export function useUI<T>(sel: (s: UIState) => T): T {
  return useSyncExternalStore(subscribeUI, () => sel(ui))
}

// ---- toasts ----
export type Toast = { id: number; text: string }
export const toasts: Toast[] = []
let toastId = 1
const toastListeners = new Set<() => void>()

export function toast(text: string) {
  toasts.push({ id: toastId++, text })
  if (toasts.length > 4) toasts.shift()
  toastListeners.forEach(l => l())
  setTimeout(() => {
    const i = toasts.findIndex(t => t.text === text)
    if (i >= 0) toasts.splice(i, 1)
    toastListeners.forEach(l => l())
  }, 3500)
}

export function subscribeToasts(fn: () => void) {
  toastListeners.add(fn)
  return () => { toastListeners.delete(fn) }
}

export function useToasts() {
  return useSyncExternalStore(subscribeToasts, () => toasts.length ? toasts[toasts.length - 1].id : 0)
}
