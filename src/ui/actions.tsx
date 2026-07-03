// Cross-cutting UI actions shared by SessionView, Browser, the command
// palette and keyboard shortcuts.

import * as Y from 'yjs'
import { ClipRef, CLIP_COLORS, BAR, clamp } from '../types'
import {
  addTrack, addScene, clipKey, clips, duplicateClipTo, getClipMap, clipToJSON,
  jsonToClipMap, loadProject, mutate, scenes, setInstrument, trackById, ClipJSON, TrackJSON,
  addAudioTrack, createAudioClip, addBusTrack, tracks, busCanReach, setTrackOutput, setBusSend,
  setTrackColor, duplicateTrack, moveTrack, removeTrack, assignDrumKit, addArrClip, arr, id8,
  splitArrClip,
} from '../state/doc'
import { ColorRow, MenuItem } from './widgets'
import { setUI, toast, ui } from '../state/store'
import { setPresence } from '../state/net'
import { engine } from '../audio/engine'
import { meta } from '../state/doc'
import { getSampleBuffer, importSampleFilesBulk, looksLikeAudio } from '../audio/samples'
import { DRUM_PACKS, packSampleId, ROLE_LABEL, RoleName } from '../audio/drumpacks'
import { DEFAULT_PROJECT, demoProject, DRUM_KITS, InstPreset, MidiLoop, Progression, progressionClip, clipInKey } from '../packs'

// Shared track-header right-click menu, so Session and Arrangement headers offer
// the same options. `onRename` lets each view drive its own inline editor;
// `vertical` flips the reorder labels (Session = columns, Arrangement = rows).
export function trackHeaderMenu(trackId: string, onRename: () => void, vertical = false): MenuItem[] {
  const t = trackById(trackId)
  const locked = !!t?.get('locked')   // built-in A/B buses: no duplicate / delete
  const items: MenuItem[] = [
    { label: 'Rename', fn: onRename },
    { custom: <ColorRow colors={CLIP_COLORS} onPick={i => setTrackColor(trackId, i)} /> },
  ]
  if (!locked) {
    items.push(
      { label: 'Duplicate track', fn: () => duplicateTrack(trackId) },
      { label: vertical ? '↑ Move up' : '← Move left', fn: () => moveTrack(trackId, -1) },
      { label: vertical ? '↓ Move down' : '→ Move right', fn: () => moveTrack(trackId, 1) },
      'sep',
      { label: 'Delete track', fn: () => { if (confirm(`Delete "${t?.get('name')}"?`)) removeTrack(trackId) }, danger: true },
    )
  }
  return items
}

// Drop a whole sampled kit onto a drum track: kick→kick, snare→snare, hats→hats,
// clap→clap (or a 2nd snare if the kit has none), perc→perc (or a high tom if
// none). The device's pads become samplers for these sounds.
export function assignKitToTrack(trackId: string, kitId: string) {
  const t = trackById(trackId)
  if (!t || t.get('kind') !== 'drum') { toast('Drop drum kits onto a drum track'); return }
  const pack = DRUM_PACKS.find(p => p.id === kitId)
  if (!pack) return
  const has = (r: RoleName) => !!pack.roles[r]
  const sid = (r: RoleName) => packSampleId(kitId, r)
  const nm = (r: RoleName) => `${pack.name} ${ROLE_LABEL[r]}`
  const a: { pad: number; sampleId: string | null; name: string; tune?: number }[] = []
  const put = (pad: number, r: RoleName, tune = 0) => a.push({ pad, sampleId: sid(r), name: nm(r), tune })
  const clear = (pad: number) => a.push({ pad, sampleId: null, name: '' })
  put(0, 'kick'); put(1, 'snare')
  has('clap') ? put(2, 'clap') : put(2, 'snare')                 // no clap → another snare
  put(3, 'chat'); put(4, 'ohat')
  has('tom') ? put(5, 'tom') : clear(5)
  has('perc') ? put(6, 'perc') : has('tom') ? put(6, 'tom', 7) : clear(6) // no perc → high tom
  has('crash') ? put(7, 'crash') : clear(7)
  assignDrumKit(trackId, a, `Kit: ${pack.name}`)
  setUI({ selTrackId: trackId, detailOpen: true, detailTab: 'devices' })
  toast(`${pack.name} → ${t.get('name')}`)
}

/** Place a sample as an audio clip on an arrangement track at a tick position. */
export function addSampleToArr(trackId: string, atTicks: number, sampleId: string, name: string) {
  const buf = getSampleBuffer(sampleId)
  const bpm = meta.get('bpm') ?? 120
  const durSec = buf?.duration ?? 0.5
  const durTicks = Math.max(BAR / 8, Math.round(durSec * (bpm / 60) * 96))
  addArrClip(trackId, Math.max(0, atTicks), {
    name, color: 5, len: durTicks, notes: {},
    audio: { sampleId, sampleName: name, gainDb: 0, pitch: 0, rev: 0, loop: 0, fadeIn: 0, fadeOut: 0, offset: 0, dur: 0, cents: 0, xfade: 0 },
  }, 'Sample to arrangement')
  toast(`“${name}” → arrangement`)
}

export function selectTrack(trackId: string | null) {
  setUI({ selTrackId: trackId })
  setPresence({ sel: trackId ? { trackId } : null })
}

// ---------------- audio import (into the sample bank — never auto-creates tracks) ----------------

/** Bulk-import audio files into the sample bank, preserving folder paths. */
export async function importAudioToBank(items: { file: File; folder: string }[]) {
  if (!items.length) { toast('No audio files found'); return }
  if (items.length > 3) toast(`Importing ${items.length} samples…`)
  const { ok, failed } = await importSampleFilesBulk(items)
  if (ok) toast(`Added ${ok} sample${ok > 1 ? 's' : ''} to the bank${failed ? ` (${failed} unreadable)` : ''}`)
  else toast('Could not decode those files')
}

/** File picker (multi-select) → sample bank. */
export function pickAudioFiles(folder = '') {
  const input = document.createElement('input')
  input.type = 'file'
  input.accept = 'audio/*'
  input.multiple = true
  input.onchange = () => {
    const files = [...(input.files ?? [])].filter(looksLikeAudio)
    importAudioToBank(files.map(file => ({ file, folder })))
  }
  input.click()
}

/** Folder picker → sample bank, keeping the folder's internal structure. */
export function pickAudioFolder(baseFolder = '') {
  const input = document.createElement('input')
  input.type = 'file'
  ;(input as any).webkitdirectory = true
  input.onchange = () => {
    const items = [...(input.files ?? [])].filter(looksLikeAudio).map(file => {
      // webkitRelativePath = "PackName/Sub/kick.wav" → folder path without the file
      const rel = ((file as any).webkitRelativePath as string) || file.name
      const dir = rel.split('/').slice(0, -1).join('/')
      return { file, folder: [baseFolder, dir].filter(Boolean).join('/') }
    })
    importAudioToBank(items)
  }
  input.click()
}

/**
 * Split an arrangement clip at `cutTicks` from its start, handling both kinds:
 * MIDI clips partition notes/envelopes in doc.splitArrClip; audio clips also
 * need their sample crop recomputed so each half keeps playing its own slice
 * (the clip stretches its cropped region across its tick length, so the cut
 * maps proportionally — same math as the AudioEditor's Split button).
 */
export function splitArrClipAt(aid: string, cutTicks: number): string | null {
  const m = arr.get(aid) as Y.Map<any> | undefined
  if (!m) return null
  const len = m.get('len') ?? BAR
  if (cutTicks <= 2 || cutTicks >= len - 2) return null
  if (!m.get('audio')) return splitArrClip(aid, cutTicks)
  const buf = getSampleBuffer(m.get('sampleId') || '')
  const bufDur = buf?.duration ?? 0
  if (!bufDur) { toast('Sample still loading — try again in a moment'); return null }
  const offset = m.get('offset') ?? 0
  const durRaw = m.get('dur') ?? 0
  const regionStart = Math.max(0, Math.min(offset, bufDur))
  const regionDur = durRaw > 0 ? Math.min(durRaw, bufDur - regionStart) : bufDur - regionStart
  const cutSec = (cutTicks / len) * regionDur
  if (cutSec <= 0.02 || cutSec >= regionDur - 0.02) return null
  return splitArrClip(aid, cutTicks, { leftDur: cutSec, rightOffset: regionStart + cutSec, rightDur: regionDur - cutSec })
}

/** Place a bank sample as a session audio clip (used by slot drag-drops). */
export function placeSampleInSlot(trackId: string, sceneId: string, sampleId: string, name: string) {
  const t = trackById(trackId)
  if (!t) return
  if (t.get('kind') !== 'audio') { toast('Drop samples onto an audio track (＋ Audio)'); return }
  const buf = getSampleBuffer(sampleId)
  const bpm = meta.get('bpm') ?? 120
  const durTicks = buf ? Math.max(BAR / 4, Math.round(buf.duration * (bpm / 60) * 96)) : BAR * 2
  const ref = createAudioClip(trackId, sceneId, sampleId, name, durTicks, t.get('color') ?? 5)
  selectClip(ref, true)
}

export function selectClip(ref: ClipRef | null, openDetail = false) {
  const trackId = ref ? (ref.kind === 'session' ? ref.trackId : (getClipMap(ref)?.get('trackId') ?? null)) : null
  setUI({
    selClip: ref,
    selTrackId: trackId ?? ui.selTrackId,
    ...(openDetail ? { detailOpen: true, detailTab: 'clip' as const } : {}),
  })
  setPresence({
    sel: ref
      ? { trackId: trackId ?? undefined, clipKey: ref.kind === 'session' ? clipKey(ref.trackId, ref.sceneId) : ref.id }
      : trackId ? { trackId } : null,
  })
}

// ---------------- clipboard ----------------
let clipboard: ClipJSON | null = null

export function copyClipRef(ref: ClipRef) {
  const m = getClipMap(ref)
  if (!m) return
  clipboard = clipToJSON(m)
  toast(`Copied "${clipboard.name}"`)
}

export function pasteClipTo(trackId: string, sceneId: string) {
  if (!clipboard) { toast('Clipboard is empty'); return }
  mutate('Paste clip', () => {
    clips.set(clipKey(trackId, sceneId), jsonToClipMap(clipboard!))
  })
  selectClip({ kind: 'session', trackId, sceneId })
}

export function hasClipboard() {
  return clipboard !== null
}

// ---------------- arrangement clipboard (multi-clip) ----------------
// Copies preserve the RELATIVE layout of a multi-clip selection (time offset +
// track-lane offset from the earliest/topmost clip), so pasting elsewhere keeps
// the group's shape instead of stacking every clip onto one track/tick.
let arrClipboard: { dTick: number; dLane: number; json: ClipJSON }[] | null = null

// Lane order for clip-carrying tracks (buses/master hold no clips), matching
// ArrangementView's row layout — used to translate a track id to/from an offset.
function clipLaneIds(): string[] {
  return tracks.toArray().filter(t => t.get('kind') !== 'bus').map(t => t.get('id'))
}

export function copyArrSelection(ids: string[]) {
  const items = ids.map(id => arr.get(id) as Y.Map<any> | undefined).filter((m): m is Y.Map<any> => !!m)
  if (!items.length) return
  const laneIds = clipLaneIds()
  const laneIdx = (tid: string) => laneIds.indexOf(tid)
  const minStart = Math.min(...items.map(m => m.get('start') ?? 0))
  const minLane = Math.min(...items.map(m => laneIdx(m.get('trackId'))))
  arrClipboard = items.map(m => ({
    dTick: (m.get('start') ?? 0) - minStart,
    dLane: laneIdx(m.get('trackId')) - minLane,
    json: clipToJSON(m),
  }))
  toast(`Copied ${items.length} clip${items.length > 1 ? 's' : ''}`)
}

export function hasArrClipboard() {
  return !!arrClipboard && arrClipboard.length > 0
}

/** Paste the arrangement clipboard anchored at (atTick, atTrackId); the copied
 *  clips' relative time/lane offsets are preserved, clamped onto real tracks. */
export function pasteArrClipboard(atTick: number, atTrackId: string) {
  if (!arrClipboard || !arrClipboard.length) { toast('Clipboard is empty'); return }
  const laneIds = clipLaneIds()
  const baseLane = laneIds.indexOf(atTrackId)
  if (baseLane < 0) return
  const items = arrClipboard
  const newIds: string[] = []
  // One transaction (direct Y writes, not nested addArrClip calls) so the whole
  // paste is a single undo step.
  mutate(items.length > 1 ? 'Paste clips' : 'Paste clip', () => {
    for (const item of items) {
      const lane = clamp(baseLane + item.dLane, 0, laneIds.length - 1)
      const tid = laneIds[lane]
      const start = Math.max(0, atTick + item.dTick)
      const nid = id8()
      arr.set(nid, jsonToClipMap(item.json, { trackId: tid, start, id: nid }))
      newIds.push(nid)
    }
  })
  setUI({ selArrIds: newIds, selClip: null })
  toast(`Pasted ${newIds.length} clip${newIds.length > 1 ? 's' : ''}`)
}

// ---------------- track creation ----------------
let colorCycle = 0
const nextColor = () => (colorCycle = (colorCycle + 5) % CLIP_COLORS.length)

export function addSynthTrack(preset?: InstPreset): string {
  const t: TrackJSON = {
    name: preset?.name ?? 'Synth',
    color: nextColor(),
    kind: 'synth',
    inst: preset ? { type: preset.type, params: { ...preset.params } } : { type: 'poly', params: { ...DEFAULT_PROJECT.tracks[3].inst.params } },
    fx: [], gain: -3, pan: 0, mute: false, solo: false,
  }
  const id = addTrack(t)
  selectTrack(id)
  return id
}

export function addDrumTrack(kitName?: string): string {
  const kit = DRUM_KITS.find(k => k.name === kitName) ?? DRUM_KITS[0]
  const id = addTrack({
    name: kitName ?? 'Drums',
    color: nextColor(),
    kind: 'drum',
    inst: { type: 'drum', params: { ...kit.params } },
    fx: [], gain: 0, pan: 0, mute: false, solo: false,
  })
  selectTrack(id)
  return id
}

export function addAudio(): string {
  const n = tracks.toArray().filter(t => t.get('kind') === 'audio').length + 1
  const id = addAudioTrack(`Audio ${n}`, nextColor())
  selectTrack(id)
  return id
}

export function addBus(): string {
  const n = tracks.toArray().filter(t => t.get('kind') === 'bus').length + 1
  const id = addBusTrack(`Bus ${n}`, nextColor())
  selectTrack(id)
  return id
}

/**
 * Set a bus output, popping the themed feedback-loop warning first if the route
 * would create an audio cycle. On Continue we apply anyway (Web Audio drops a
 * delay-less cycle, so it's harmless); on Undo we leave the route unchanged.
 */
export function attemptSetOutput(trackId: string, target: string) {
  const name = trackById(trackId)?.get('name') ?? 'bus'
  if (target !== 'master' && (target === trackId || busCanReach(target, trackId))) {
    const to = trackById(target)?.get('name') ?? 'that bus'
    setUI({ feedbackPrompt: { msg: `Routing “${name}” → “${to}” creates a feedback loop.`, apply: () => setTrackOutput(trackId, target) } })
  } else setTrackOutput(trackId, target)
}

/** Set a bus-send level; only a send FROM a bus can loop, so guard those. */
export function attemptBusSend(trackId: string, busId: string, level: number) {
  const src = trackById(trackId)
  const isBusSource = src?.get('kind') === 'bus'
  if (level > 0 && isBusSource && (busId === trackId || busCanReach(busId, trackId))) {
    const name = src?.get('name') ?? 'bus'
    const to = trackById(busId)?.get('name') ?? 'that bus'
    setUI({ feedbackPrompt: { msg: `“${name}” sending to “${to}” creates a feedback loop.`, apply: () => setBusSend(trackId, busId, level) } })
  } else setBusSend(trackId, busId, level)
}

// ---------------- browser items → project ----------------

export function applyPreset(preset: InstPreset) {
  const t = ui.selTrackId ? trackById(ui.selTrackId) : undefined
  if (t && t.get('kind') === 'synth' && ui.selTrackId) {
    setInstrument(ui.selTrackId, preset.type, { ...preset.params }, `Preset: ${preset.name}`)
    toast(`Loaded "${preset.name}"`)
  } else {
    addSynthTrack(preset)
    toast(`New track with "${preset.name}"`)
  }
}

export function applyDrumKit(kitName: string) {
  const kit = DRUM_KITS.find(k => k.name === kitName)
  if (!kit) return
  const t = ui.selTrackId ? trackById(ui.selTrackId) : undefined
  if (t && t.get('kind') === 'drum' && ui.selTrackId) {
    setInstrument(ui.selTrackId, 'drum', { ...kit.params }, `Kit: ${kitName}`)
    toast(`Loaded kit "${kitName}"`)
  } else {
    addDrumTrack(kitName)
    toast(`New drum track with "${kitName}"`)
  }
}

export function loadLoop(loop: MidiLoop, targetTrackId?: string, targetSceneId?: string) {
  let trackId = targetTrackId ?? ui.selTrackId
  const t = trackId ? trackById(trackId) : undefined
  const needDrums = !!loop.forDrums
  const isDrum = t?.get('kind') === 'drum'
  if (!t || needDrums !== isDrum) {
    trackId = needDrums ? addDrumTrack() : addSynthTrack()
  }
  // find target slot: given scene, else selected, else first free, else scene 0
  let sceneId = targetSceneId ?? (ui.selClip?.kind === 'session' ? ui.selClip.sceneId : undefined)
  if (!sceneId) {
    for (let i = 0; i < scenes.length; i++) {
      const sid = scenes.get(i).get('id')
      if (!clips.get(clipKey(trackId!, sid))) { sceneId = sid; break }
    }
    sceneId = sceneId ?? scenes.get(0)?.get('id')
  }
  if (!sceneId) return
  // melodic loops follow the global key; drum loops are pitch-free
  const built = loop.forDrums ? loop.clip : clipInKey(loop.clip, meta.get('root') ?? 9)
  mutate(`Loop: ${loop.name}`, () => {
    clips.set(clipKey(trackId!, sceneId!), jsonToClipMap(built))
  })
  selectClip({ kind: 'session', trackId: trackId!, sceneId }, true)
  toast(`"${built.name}" → ${trackById(trackId!)?.get('name')}`)
}

/** Render a chord progression into the current project key and drop it in. */
export function loadProgression(prog: Progression, targetTrackId?: string, targetSceneId?: string) {
  const rootPc = meta.get('root') ?? 9
  const built = progressionClip(prog, rootPc)
  loadLoop({ name: built.name, cat: 'Chords', clip: built }, targetTrackId, targetSceneId)
}

// ---------------- project-level ----------------

export function loadDemo() {
  loadProject(demoProject(), 'Load demo song')
  selectTrack(null)
  setUI({ selClip: null })
  toast('Demo song loaded — press a scene number to jam')
}

export function newProject() {
  if (!confirm('Start a new empty project? (You can undo this)')) return
  engine.stopAll()
  loadProject({ ...DEFAULT_PROJECT, meta: { ...DEFAULT_PROJECT.meta } }, 'New project')
  setUI({ selClip: null })
  toast('Fresh project')
}

export function importProjectFile() {
  const input = document.createElement('input')
  input.type = 'file'
  input.accept = '.json,application/json'
  input.onchange = async () => {
    const f = input.files?.[0]
    if (!f) return
    try {
      const json = JSON.parse(await f.text())
      if (!json.meta || !json.tracks) throw new Error('bad file')
      engine.stopAll()
      loadProject(json, 'Import project')
      toast(`Imported "${json.meta.title}"`)
    } catch {
      toast('Could not read that file')
    }
  }
  input.click()
}

export function duplicateClipToNextScene(ref: ClipRef & { kind: 'session' }) {
  const idx = (() => {
    for (let i = 0; i < scenes.length; i++) if (scenes.get(i).get('id') === ref.sceneId) return i
    return -1
  })()
  if (idx < 0) return
  let nextScene = idx + 1 < scenes.length ? scenes.get(idx + 1).get('id') : null
  if (!nextScene) nextScene = addScene()
  duplicateClipTo(ref, ref.trackId, nextScene!)
  selectClip({ kind: 'session', trackId: ref.trackId, sceneId: nextScene! })
}
