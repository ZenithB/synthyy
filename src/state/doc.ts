// The shared project document (Yjs CRDT) + every mutation the app performs on it.
// All edits go through mutate() so they are undoable, labeled for the history
// panel, and tagged with LOCAL origin (remote peers' edits are not undoable by us).

import * as Y from 'yjs'
import { IndexeddbPersistence } from 'y-indexeddb'
import { nanoid } from 'nanoid'
import { BAR, PPQ, CLIP_COLORS, Note, ClipRef, TrackKind } from '../types'

export const LOCAL = { local: true }

// ---------- rooms ----------
export function roomIdFromHash(): string | null {
  const m = location.hash.match(/r=([A-Za-z0-9_-]+)/)
  return m ? m[1] : null
}
export const roomId = roomIdFromHash()
export const docName = roomId ? `sf-room-${roomId}` : 'sf-local'

export const doc = new Y.Doc()
export const meta = doc.getMap<any>('meta')
export const tracks = doc.getArray<Y.Map<any>>('tracks')
export const scenes = doc.getArray<Y.Map<any>>('scenes')
export const clips = doc.getMap<Y.Map<any>>('clips') // key: `${trackId}|${sceneId}`
export const arr = doc.getMap<Y.Map<any>>('arr')     // key: arrangement clip id
export const chat = doc.getArray<any>('chat')
// Master-bus effect chain — the whole mix passes through these before the
// limiter, live and in exports. Same fx-map shape as a track's `fx`.
export const masterFx = doc.getArray<Y.Map<any>>('masterFx')
// Master-bus arrangement automation: paramKey → Y.Array<{t,v}>, mirroring a
// track's `auto` map. Its open/selected-param UI state lives on `meta`
// (masterAutoOpen / masterAutoParam) since the master isn't a `tracks` entry.
export const masterAuto = doc.getMap<Y.Array<any>>('masterAuto')

export const idb = new IndexeddbPersistence(docName, doc)

// ---------- labeled transactions (for the undo-history panel) ----------
let pendingLabel: string | null = null
export function mutate(label: string, fn: () => void) {
  pendingLabel = label
  try {
    doc.transact(fn, LOCAL)
  } finally {
    pendingLabel = null
  }
}
export function takePendingLabel() {
  return pendingLabel
}

export const id8 = () => nanoid(8)
export const clipKey = (trackId: string, sceneId: string) => `${trackId}|${sceneId}`

// ---------- JSON shapes (copy/paste, packs, import/export) ----------
export type AudioClipData = {
  sampleId: string; sampleName: string
  gainDb: number; pitch: number; rev: number; loop: number; fadeIn: number; fadeOut: number
  offset?: number; dur?: number; cents?: number; xfade?: number
}
export type ClipJSON = {
  name: string; color: number; len: number; notes: Record<string, Note>
  audio?: AudioClipData
  env?: Record<string, { t: number; v: number }[]>
  follow?: { on: boolean; bars: number; action: number; chance: number }
}
// srcTrack/srcPitch: sidechain trigger source (duck fx) — track id ('' = tempo
// mode) and pad index within a drum track (-1 = any note on the track).
export type FxJSON = { id?: string; type: string; on: boolean; params: Record<string, number>; out?: number; srcTrack?: string; srcPitch?: number }
export type LfoJSON = { id?: string; on: number; shape: number; sync: number; rate: number; hz: number; depth: number; phase: number; dest: string; fxId: string; pkey: string; targets?: { dest: string; fxId: string; pkey: string }[] }
export type MacroJSON = { name: string; value: number; targets: { dest: string; fxId: string; pkey: string }[] }
export type MidiFxJSON = { id?: string; type: string; on: boolean; params: Record<string, number> }
export type ReturnJSON = { id?: string; name: string; fxType: string; params: Record<string, number>; gain: number }
export type TrackJSON = {
  id?: string; name: string; color: number; kind: TrackKind
  inst: { type: string; params: Record<string, number>; sampleId?: string; sampleName?: string; out?: number; padSamples?: Record<string, string>; padNames?: Record<string, string> }
  fx: FxJSON[]
  gain: number; pan: number; mute: boolean; solo: boolean
  sendA?: number; sendB?: number
  output?: string                       // routing target for buses: 'master' | busTrackId
  sends?: Record<string, number>        // busTrackId → send level (sends into user buses)
  locked?: boolean                      // built-in (e.g. the A/B send buses) — cannot be deleted
  send?: 'A' | 'B'                      // marks the built-in A/B send buses (id-independent)
  lfos?: LfoJSON[]; macros?: MacroJSON[]; midifx?: MidiFxJSON[]
  // arrangement automation: paramId ("dest|fxId|pkey") → breakpoints in absolute song ticks
  auto?: Record<string, { t: number; v: number }[]>
  autoOpen?: boolean; autoParam?: string
}
export type ProjectJSON = {
  meta: { title: string; bpm: number; swing: number; swingSubdivision?: string; humanize?: number; root: number; scale: string; launchQ: number; masterGain?: number; loopOn?: boolean; loopStart?: number; loopEnd?: number; masterAutoOpen?: boolean; masterAutoParam?: string; tsTop?: number; tsBottom?: number }
  tracks: TrackJSON[]
  scenes: { id?: string; name: string }[]
  clips: Record<string, ClipJSON>
  arr: Record<string, ClipJSON & { trackId: string; start: number }>
  returns?: ReturnJSON[]
  masterFx?: FxJSON[]
  masterAuto?: Record<string, { t: number; v: number }[]>
}

// The two built-in send buses every session starts with — undeletable, full fx
// chains. A track's A/B sends route into these (A = reverb, B = ping-pong delay).
// Tagged with `send` so the engine finds them by marker, not by (regenerated) id.
export const DEFAULT_BUSES: TrackJSON[] = [
  { id: 'busA', name: 'A', color: 7, kind: 'bus', inst: { type: 'audiobus', params: {} }, fx: [{ type: 'reverb', on: true, params: { size: 3.6, mix: 1 } }], gain: 0, pan: 0, mute: false, solo: false, output: 'master', locked: true, send: 'A' },
  { id: 'busB', name: 'B', color: 9, kind: 'bus', inst: { type: 'audiobus', params: {} }, fx: [{ type: 'pingpong', on: true, params: { time: 3, fb: 0.4, mix: 1 } }], gain: 0, pan: 0, mute: false, solo: false, output: 'master', locked: true, send: 'B' },
]

// ---------- Y builders ----------
function yNotes(notes: Record<string, Note>) {
  const m = new Y.Map<Note>()
  for (const [nid, n] of Object.entries(notes)) m.set(nid, { ...n })
  return m
}

export function jsonToClipMap(json: ClipJSON, extra?: Record<string, any>) {
  const m = new Y.Map<any>()
  m.set('id', id8())
  m.set('name', json.name)
  m.set('color', json.color)
  m.set('len', json.len)
  m.set('notes', yNotes(json.notes))
  if (json.audio) {
    m.set('audio', true)
    for (const [k, v] of Object.entries(json.audio)) m.set(k, v)
  }
  applyClipExtras(m, json)
  if (extra) for (const [k, v] of Object.entries(extra)) m.set(k, v)
  return m
}

export function clipToJSON(m: Y.Map<any>): ClipJSON {
  const notes: Record<string, Note> = {}
  const nm = m.get('notes') as Y.Map<Note>
  if (nm) nm.forEach((n, k) => { notes[k] = { ...n } })
  const out: ClipJSON = { name: m.get('name') ?? 'Clip', color: m.get('color') ?? 0, len: m.get('len') ?? BAR, notes }
  if (m.get('audio')) {
    out.audio = {
      sampleId: m.get('sampleId') ?? '', sampleName: m.get('sampleName') ?? 'Audio',
      gainDb: m.get('gainDb') ?? 0, pitch: m.get('pitch') ?? 0, rev: m.get('rev') ?? 0,
      loop: m.get('loop') ?? 1, fadeIn: m.get('fadeIn') ?? 0, fadeOut: m.get('fadeOut') ?? 0,
      offset: m.get('offset') ?? 0, dur: m.get('dur') ?? 0, cents: m.get('cents') ?? 0, xfade: m.get('xfade') ?? 0,
    }
  }
  const env = m.get('env') as Y.Map<any> | undefined
  if (env && env.size) {
    out.env = {}
    env.forEach((arr2, k) => { out.env![k] = (arr2 as Y.Array<any>).toArray().map(p => ({ t: p.t, v: p.v })) })
  }
  const f = m.get('follow') as Y.Map<any> | undefined
  if (f) out.follow = { on: !!f.get('on'), bars: f.get('bars') ?? 1, action: f.get('action') ?? 0, chance: f.get('chance') ?? 1 }
  return out
}

export function isAudioClip(m: Y.Map<any> | null | undefined): boolean {
  return !!m?.get('audio')
}

function yFx(fx: FxJSON) {
  const m = new Y.Map<any>()
  m.set('id', fx.id ?? id8())
  m.set('type', fx.type)
  m.set('on', fx.on)
  m.set('out', fx.out ?? 0)
  if (fx.srcTrack) { m.set('srcTrack', fx.srcTrack); m.set('srcPitch', fx.srcPitch ?? -1) }
  const pm = new Y.Map<number>()
  for (const [k, v] of Object.entries(fx.params)) pm.set(k, v)
  m.set('params', pm)
  return m
}

function yLfo(l: LfoJSON) {
  const m = new Y.Map<any>()
  m.set('id', l.id ?? id8())
  m.set('on', l.on); m.set('shape', l.shape); m.set('sync', l.sync); m.set('rate', l.rate)
  m.set('hz', l.hz); m.set('depth', l.depth); m.set('phase', l.phase)
  m.set('dest', l.dest); m.set('fxId', l.fxId); m.set('pkey', l.pkey)
  const targets = new Y.Array<Y.Map<any>>()
  if (l.targets?.length) targets.push(l.targets.map(tg => {
    const tm = new Y.Map<any>(); tm.set('dest', tg.dest); tm.set('fxId', tg.fxId); tm.set('pkey', tg.pkey); return tm
  }))
  m.set('targets', targets)
  return m
}
function yMacro(mc: MacroJSON) {
  const m = new Y.Map<any>()
  m.set('name', mc.name); m.set('value', mc.value)
  const targets = new Y.Array<Y.Map<any>>()
  targets.push(mc.targets.map(tg => { const tm = new Y.Map<any>(); tm.set('dest', tg.dest); tm.set('fxId', tg.fxId); tm.set('pkey', tg.pkey); return tm }))
  m.set('targets', targets)
  return m
}
function yMidiFx(d: MidiFxJSON) {
  const m = new Y.Map<any>()
  m.set('id', d.id ?? id8()); m.set('type', d.type); m.set('on', d.on)
  const pm = new Y.Map<number>()
  for (const [k, v] of Object.entries(d.params)) pm.set(k, v)
  m.set('params', pm)
  return m
}

function yTrack(t: TrackJSON) {
  const m = new Y.Map<any>()
  m.set('id', t.id ?? id8())
  m.set('name', t.name)
  m.set('color', t.color)
  m.set('kind', t.kind)
  const inst = new Y.Map<any>()
  inst.set('type', t.inst.type)
  const pm = new Y.Map<number>()
  for (const [k, v] of Object.entries(t.inst.params)) pm.set(k, v)
  inst.set('params', pm)
  inst.set('out', t.inst.out ?? 0)
  if (t.inst.sampleId) { inst.set('sampleId', t.inst.sampleId); inst.set('sampleName', t.inst.sampleName ?? '') }
  if (t.inst.padSamples && Object.keys(t.inst.padSamples).length) {
    const psm = new Y.Map<string>()
    for (const [k, v] of Object.entries(t.inst.padSamples)) psm.set(k, v)
    inst.set('padSamples', psm)
    const pnm = new Y.Map<string>()
    for (const [k, v] of Object.entries(t.inst.padNames ?? {})) pnm.set(k, v)
    inst.set('padNames', pnm)
  }
  m.set('inst', inst)
  const fxArr = new Y.Array<Y.Map<any>>()
  fxArr.push(t.fx.map(yFx))
  m.set('fx', fxArr)
  const lfoArr = new Y.Array<Y.Map<any>>()
  if (t.lfos?.length) lfoArr.push(t.lfos.map(yLfo))
  m.set('lfos', lfoArr)
  if (t.macros?.length) { const ma = new Y.Array<Y.Map<any>>(); ma.push(t.macros.map(yMacro)); m.set('macros', ma) }
  if (t.midifx?.length) { const mx = new Y.Array<Y.Map<any>>(); mx.push(t.midifx.map(yMidiFx)); m.set('midifx', mx) }
  m.set('gain', t.gain)
  m.set('pan', t.pan)
  m.set('mute', t.mute)
  m.set('solo', t.solo)
  m.set('sendA', t.sendA ?? 0)
  m.set('sendB', t.sendB ?? 0)
  m.set('output', t.output ?? 'master')
  const sends = new Y.Map<number>()
  if (t.sends) for (const [k, v] of Object.entries(t.sends)) sends.set(k, v)
  m.set('sends', sends)
  if (t.locked) m.set('locked', true)
  if (t.send) m.set('send', t.send)
  if (t.auto && Object.keys(t.auto).length) {
    const am = new Y.Map<any>()
    for (const [k, pts] of Object.entries(t.auto)) { const a = new Y.Array<any>(); a.push(pts.map(p => ({ ...p }))); am.set(k, a) }
    m.set('auto', am)
  }
  if (t.autoOpen) m.set('autoOpen', true)
  if (t.autoParam) m.set('autoParam', t.autoParam)
  return m
}

function applyClipExtras(m: Y.Map<any>, json: ClipJSON) {
  if (json.env) {
    const env = new Y.Map<any>()
    for (const [k, pts] of Object.entries(json.env)) { const a = new Y.Array<any>(); a.push(pts.map(p => ({ ...p }))); env.set(k, a) }
    m.set('env', env)
  }
  if (json.follow) {
    const f = new Y.Map<any>()
    f.set('on', json.follow.on); f.set('bars', json.follow.bars); f.set('action', json.follow.action); f.set('chance', json.follow.chance)
    m.set('follow', f)
  }
}

// ---------- lookups ----------
// id → track map, rebuilt lazily and dropped on any membership change.
// trackById is called from hot paths (the engine's per-frame modulation loop,
// mute/solo, bus rewires) where the old linear scan multiplied with track count.
let trackIdCache: Map<string, Y.Map<any>> | null = null
tracks.observe(() => { trackIdCache = null })
export function trackById(trackId: string): Y.Map<any> | undefined {
  if (!trackIdCache) {
    trackIdCache = new Map()
    for (let i = 0; i < tracks.length; i++) trackIdCache.set(tracks.get(i).get('id'), tracks.get(i))
  }
  const hit = trackIdCache.get(trackId)
  // Verify the hit is still attached: inside an uncommitted transaction the
  // observer hasn't fired yet, so a just-deleted map could still be cached
  // (e.g. moveTrack deletes + reinserts the same id in one transaction).
  if (hit && !(hit as any)._item?.deleted) return hit
  for (let i = 0; i < tracks.length; i++) {
    const t = tracks.get(i)
    if (t.get('id') === trackId) { trackIdCache.set(trackId, t); return t }
  }
  if (hit) trackIdCache.delete(trackId)
  return undefined
}
export function trackIndex(trackId: string) {
  for (let i = 0; i < tracks.length; i++) if (tracks.get(i).get('id') === trackId) return i
  return -1
}
export function sceneIndex(sceneId: string) {
  for (let i = 0; i < scenes.length; i++) if (scenes.get(i).get('id') === sceneId) return i
  return -1
}
export function getClipMap(ref: ClipRef | null): Y.Map<any> | null {
  if (!ref) return null
  if (ref.kind === 'session') return (clips.get(clipKey(ref.trackId, ref.sceneId)) as Y.Map<any>) ?? null
  return (arr.get(ref.id) as Y.Map<any>) ?? null
}

// ---------- meta ----------
export function setMetaField(label: string, k: string, v: any) {
  mutate(label, () => meta.set(k, v))
}
export const setBpm = (v: number) => setMetaField('Change tempo', 'bpm', Math.round(v * 10) / 10)
export const setSwing = (v: number) => setMetaField('Change swing', 'swing', v)
export const setHumanize = (v: number) => setMetaField('Change humanize', 'humanize', v)
export const setSwingSubdivision = (v: string) => setMetaField('Swing timing', 'swingSubdivision', v)
export const setTitle = (v: string) => setMetaField('Rename project', 'title', v)
export const setLaunchQ = (bars: number) => setMetaField('Launch quantize', 'launchQ', bars)
export function setKeyScale(root: number, scale: string) {
  mutate('Change key', () => { meta.set('root', root); meta.set('scale', scale) })
}

// ---------- meter (time signature) ----------
// Stored on shared meta as tsTop/tsBottom (default 4/4). PPQ stays the quarter
// note; the bottom numeral picks the counted note value (2=half … 16=16th) and
// the top numeral how many of them make a bar. All positions/lengths remain
// plain ticks — the meter only changes how ticks group into beats and bars
// (grids, ruler, metronome, launch quantize, bar-based lengths).
export const TS_TOP_MAX = 30
export const TS_BOTTOMS = [2, 4, 8, 16]
export function timeSig(): { top: number; bottom: number } {
  const top = clampInt(meta.get('tsTop') ?? 4, 1, TS_TOP_MAX)
  const b = meta.get('tsBottom') ?? 4
  const bottom = TS_BOTTOMS.includes(b) ? b : 4
  return { top, bottom }
}
const clampInt = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(hi, Math.round(v)))
/** Ticks in one beat (the bottom-numeral note value). PPQ=96 ⇒ ♩=96, ♪=48… */
export function beatTicks(): number {
  return (PPQ * 4) / timeSig().bottom
}
/** Ticks in one bar under the current meter (4/4 ⇒ 384 = the legacy BAR). */
export function barTicks(): number {
  const { top, bottom } = timeSig()
  return top * ((PPQ * 4) / bottom)
}
export function setTimeSignature(top: number, bottom: number) {
  mutate('Time signature', () => {
    meta.set('tsTop', clampInt(top, 1, TS_TOP_MAX))
    meta.set('tsBottom', TS_BOTTOMS.includes(bottom) ? bottom : 4)
  })
}
export function setLoopRegion(start: number, end: number, on: boolean) {
  mutate('Loop region', () => { meta.set('loopStart', start); meta.set('loopEnd', end); meta.set('loopOn', on) })
}

// ---------- tracks ----------
export function addTrack(t: TrackJSON): string {
  const tid = t.id ?? id8()
  const m = yTrack({ ...t, id: tid })
  mutate(`Add track "${t.name}"`, () => tracks.push([m]))
  return tid
}

export function removeTrack(trackId: string) {
  if (trackById(trackId)?.get('locked')) return   // built-in A/B send buses can't be deleted
  mutate('Delete track', () => {
    const i = trackIndex(trackId)
    if (i >= 0) tracks.delete(i)
    const keys: string[] = []
    clips.forEach((_v, k) => { if (k.startsWith(trackId + '|')) keys.push(k) })
    keys.forEach(k => clips.delete(k))
    const arrIds: string[] = []
    arr.forEach((v, k) => { if (v.get('trackId') === trackId) arrIds.push(k) })
    arrIds.forEach(k => arr.delete(k))
  })
}

export function duplicateTrack(trackId: string): string | null {
  const t = trackById(trackId)
  if (!t) return null
  const newId = id8()
  // full deep copy via toJSON (keeps sends/output/lfos/macros/midifx/automation/
  // pad-samples), minus the built-in A/B markers — a copy must not be a 2nd locked send bus.
  const json = { ...(t.toJSON() as TrackJSON), id: newId, name: t.get('name') + ' copy', solo: false }
  delete json.locked; delete json.send
  const m = yTrack(json)
  mutate('Duplicate track', () => {
    const i = trackIndex(trackId)
    tracks.insert(i + 1, [m])
    clips.forEach((v, k) => {
      if (k.startsWith(trackId + '|')) {
        const sceneId = k.split('|')[1]
        clips.set(clipKey(newId, sceneId), jsonToClipMap(clipToJSON(v)))
      }
    })
  })
  return newId
}

export const renameTrack = (trackId: string, name: string) =>
  mutate('Rename track', () => trackById(trackId)?.set('name', name))
export const setTrackColor = (trackId: string, color: number) =>
  mutate('Track color', () => trackById(trackId)?.set('color', color))

export function moveTrack(trackId: string, dir: -1 | 1) {
  if (trackById(trackId)?.get('locked')) return   // pinned A/B buses don't reorder
  const i = trackIndex(trackId)
  const j = i + dir
  if (i < 0 || j < 0 || j >= tracks.length) return
  mutate('Move track', () => {
    // Y arrays can't move; delete + reinsert a FULL deep copy so routing (sends/
    // output), the locked/send markers, lfos/macros/midifx, automation and pad
    // samples all survive the reorder (a subset copy silently dropped them).
    const fresh = yTrack(tracks.get(i).toJSON() as TrackJSON)
    tracks.delete(i)
    tracks.insert(j, [fresh])
  })
}

/** Drag-reorder: place `trackId` just before `beforeId` (null = end of the list).
 *  Display order groups instrument tracks before buses, so a drop at the end of
 *  either group maps to the array end — filtering preserves relative order. */
export function moveTrackBefore(trackId: string, beforeId: string | null) {
  if (trackById(trackId)?.get('locked')) return   // pinned A/B buses don't reorder
  const i = trackIndex(trackId)
  if (i < 0 || trackId === beforeId) return
  const target = beforeId == null ? tracks.length : trackIndex(beforeId)
  if (target < 0) return
  const j = target > i ? target - 1 : target      // insertion index once removed
  if (j === i) return
  mutate('Move track', () => {
    // Same full-deep-copy dance as moveTrack: Y arrays can't move in place.
    const fresh = yTrack(tracks.get(i).toJSON() as TrackJSON)
    tracks.delete(i)
    tracks.insert(j, [fresh])
  })
}

export function setTrackMix(trackId: string, patch: Partial<{ gain: number; pan: number; mute: boolean; solo: boolean }>) {
  mutate('Mixer', () => {
    const t = trackById(trackId)
    if (!t) return
    for (const [k, v] of Object.entries(patch)) t.set(k, v)
  })
}

export function setInstrument(trackId: string, type: string, params: Record<string, number>, label = 'Change instrument') {
  mutate(label, () => {
    const t = trackById(trackId)
    if (!t) return
    const inst = new Y.Map<any>()
    inst.set('type', type)
    const pm = new Y.Map<number>()
    for (const [k, v] of Object.entries(params)) pm.set(k, v)
    inst.set('params', pm)
    t.set('inst', inst)
  })
}

export function setInstParam(trackId: string, key: string, val: number) {
  mutate('Tweak instrument', () => {
    const t = trackById(trackId)
    if (!t) return
    ;(t.get('inst').get('params') as Y.Map<number>).set(key, val)
  })
}

// Effect-chain mutators work on either a track id OR the special 'master' bus,
// so the master fader reuses the entire effect UI + engine path.
function fxArrayFor(target: string): Y.Array<Y.Map<any>> | undefined {
  if (target === 'master') return masterFx
  return trackById(target)?.get('fx') as Y.Array<Y.Map<any>> | undefined
}
function fxIndexIn(fx: Y.Array<Y.Map<any>>, fxId: string) {
  for (let i = 0; i < fx.length; i++) if (fx.get(i).get('id') === fxId) return i
  return -1
}

export function addFx(trackId: string, type: string, params: Record<string, number>) {
  mutate(`Add ${type}`, () => {
    fxArrayFor(trackId)?.push([yFx({ type, on: true, params })])
  })
}

export function removeFx(trackId: string, fxId: string) {
  mutate('Remove effect', () => {
    const fx = fxArrayFor(trackId)
    if (!fx) return
    const i = fxIndexIn(fx, fxId)
    if (i >= 0) fx.delete(i)
  })
}

export function moveFx(trackId: string, fxId: string, dir: -1 | 1) {
  mutate('Reorder effects', () => {
    const fx = fxArrayFor(trackId)
    if (!fx) return
    const i = fxIndexIn(fx, fxId)
    const j = i + dir
    if (i < 0 || j < 0 || j >= fx.length) return
    const f = fx.get(i)
    const json = { id: f.get('id'), type: f.get('type'), on: f.get('on'), out: f.get('out') ?? 0, params: Object.fromEntries((f.get('params') as Y.Map<number>).entries()) }
    fx.delete(i)
    fx.insert(j, [yFx(json)])
  })
}

export function setFxParam(trackId: string, fxId: string, key: string, val: number) {
  mutate('Tweak effect', () => {
    const fx = fxArrayFor(trackId)
    if (!fx) return
    const i = fxIndexIn(fx, fxId)
    if (i >= 0) (fx.get(i).get('params') as Y.Map<number>).set(key, val)
  })
}

/** Sidechain (duck) trigger source: a track id ('' = tempo mode) and a pad
 *  index within a drum track (-1 = any note on the track). */
export function setDuckSource(trackId: string, fxId: string, srcTrack: string, srcPitch: number) {
  mutate('Sidechain source', () => {
    const fx = fxArrayFor(trackId)
    if (!fx) return
    const i = fxIndexIn(fx, fxId)
    if (i < 0) return
    fx.get(i).set('srcTrack', srcTrack)
    fx.get(i).set('srcPitch', srcPitch)
  })
}

export function setFxOn(trackId: string, fxId: string, on: boolean) {
  mutate(on ? 'Enable effect' : 'Bypass effect', () => {
    const fx = fxArrayFor(trackId)
    if (!fx) return
    const i = fxIndexIn(fx, fxId)
    if (i >= 0) fx.get(i).set('on', on)
  })
}

/** Per-device output level (dB), applied pre-meter. */
export function setInstOut(trackId: string, db: number) {
  mutate('Device output', () => {
    const t = trackById(trackId)
    if (t) (t.get('inst') as Y.Map<any>).set('out', db)
  })
}

export function setFxOut(trackId: string, fxId: string, db: number) {
  mutate('Device output', () => {
    const fx = fxArrayFor(trackId)
    if (!fx) return
    const i = fxIndexIn(fx, fxId)
    if (i >= 0) fx.get(i).set('out', db)
  })
}

// ---------- LFOs (modulation sources) ----------
function lfoArr(t: Y.Map<any>): Y.Array<Y.Map<any>> {
  let a = t.get('lfos') as Y.Array<Y.Map<any>> | undefined
  if (!a) { a = new Y.Array<Y.Map<any>>(); t.set('lfos', a) } // legacy tracks
  return a
}
function lfoIndex(t: Y.Map<any>, lfoId: string) {
  const a = t.get('lfos') as Y.Array<Y.Map<any>> | undefined
  if (!a) return -1
  for (let i = 0; i < a.length; i++) if (a.get(i).get('id') === lfoId) return i
  return -1
}

export function addLfo(trackId: string): string {
  const id = id8()
  mutate('Add LFO', () => {
    const t = trackById(trackId)
    if (!t) return
    const m = new Y.Map<any>()
    m.set('id', id)
    m.set('on', true)
    m.set('shape', 0)   // sine
    m.set('sync', 1)
    m.set('rate', 5)    // 1/4
    m.set('hz', 1)
    m.set('depth', 0.5)
    m.set('phase', 0)
    m.set('dest', '')   // legacy single-target fields (kept empty on new LFOs)
    m.set('fxId', '')
    m.set('pkey', '')
    m.set('targets', new Y.Array<Y.Map<any>>())   // multi-target mapping list
    lfoArr(t).push([m])
  })
  return id
}

// ---- LFO multi-target mapping ----
function lfoTargetsArr(l: Y.Map<any>): Y.Array<Y.Map<any>> {
  let a = l.get('targets') as Y.Array<Y.Map<any>> | undefined
  if (!a) { a = new Y.Array<Y.Map<any>>(); l.set('targets', a) }   // legacy LFOs
  return a
}
/** Migrate a legacy single-field mapping into the targets array, then clear it. */
function migrateLegacyLfoTarget(l: Y.Map<any>) {
  const dest = l.get('dest') as string, pkey = l.get('pkey') as string
  if (!dest || !pkey) return
  const a = lfoTargetsArr(l)
  const fxId = (l.get('fxId') as string) || ''
  let dup = false
  a.forEach(tg => { if (tg.get('dest') === dest && (tg.get('fxId') || '') === fxId && tg.get('pkey') === pkey) dup = true })
  if (!dup) {
    const tm = new Y.Map<any>()
    tm.set('dest', dest); tm.set('fxId', fxId); tm.set('pkey', pkey)
    a.push([tm])
  }
  l.set('dest', ''); l.set('fxId', ''); l.set('pkey', '')
}
export function addLfoTarget(trackId: string, lfoId: string, dest: string, fxId: string, pkey: string) {
  mutate('Map LFO', () => {
    const t = trackById(trackId); if (!t) return
    const i = lfoIndex(t, lfoId); if (i < 0) return
    const l = (t.get('lfos') as Y.Array<Y.Map<any>>).get(i)
    migrateLegacyLfoTarget(l)
    const a = lfoTargetsArr(l)
    let dup = false
    a.forEach(tg => { if (tg.get('dest') === dest && (tg.get('fxId') || '') === (fxId || '') && tg.get('pkey') === pkey) dup = true })
    if (dup) return
    const tm = new Y.Map<any>()
    tm.set('dest', dest); tm.set('fxId', fxId); tm.set('pkey', pkey)
    a.push([tm])
  })
}
export function removeLfoTarget(trackId: string, lfoId: string, dest: string, fxId: string, pkey: string) {
  mutate('Unmap LFO', () => {
    const t = trackById(trackId); if (!t) return
    const i = lfoIndex(t, lfoId); if (i < 0) return
    const l = (t.get('lfos') as Y.Array<Y.Map<any>>).get(i)
    migrateLegacyLfoTarget(l)   // normalize first so legacy mappings are removable too
    const a = lfoTargetsArr(l)
    for (let j = a.length - 1; j >= 0; j--) {
      const tg = a.get(j)
      if (tg.get('dest') === dest && (tg.get('fxId') || '') === (fxId || '') && tg.get('pkey') === pkey) a.delete(j)
    }
  })
}
/** All of an LFO's target mappings (targets array + any legacy single field). */
export function lfoTargetList(l: Y.Map<any>): { dest: string; fxId: string; pkey: string }[] {
  const out: { dest: string; fxId: string; pkey: string }[] = []
  const seen = new Set<string>()
  const push = (dest: string, fxId: string, pkey: string) => {
    if (!dest || !pkey) return
    const k = `${dest}|${fxId}|${pkey}`
    if (seen.has(k)) return
    seen.add(k)
    out.push({ dest, fxId, pkey })
  }
  ;(l.get('targets') as Y.Array<Y.Map<any>> | undefined)?.forEach(tg => push(tg.get('dest') || '', tg.get('fxId') || '', tg.get('pkey') || ''))
  push((l.get('dest') as string) || '', (l.get('fxId') as string) || '', (l.get('pkey') as string) || '')
  return out
}

export function removeLfo(trackId: string, lfoId: string) {
  mutate('Remove LFO', () => {
    const t = trackById(trackId)
    if (!t) return
    const i = lfoIndex(t, lfoId)
    if (i >= 0) (t.get('lfos') as Y.Array<Y.Map<any>>).delete(i)
  })
}

export function setLfoField(trackId: string, lfoId: string, key: string, val: any, label = 'Edit LFO') {
  mutate(label, () => {
    const t = trackById(trackId)
    if (!t) return
    const i = lfoIndex(t, lfoId)
    if (i >= 0) (t.get('lfos') as Y.Array<Y.Map<any>>).get(i).set(key, val)
  })
}

export function setLfoTarget(trackId: string, lfoId: string, dest: string, fxId: string, pkey: string) {
  mutate('LFO target', () => {
    const t = trackById(trackId)
    if (!t) return
    const i = lfoIndex(t, lfoId)
    if (i < 0) return
    const m = (t.get('lfos') as Y.Array<Y.Map<any>>).get(i)
    m.set('dest', dest); m.set('fxId', fxId); m.set('pkey', pkey)
  })
}

export function lfosOf(t: Y.Map<any>): Y.Array<Y.Map<any>> | undefined {
  return t.get('lfos') as Y.Array<Y.Map<any>> | undefined
}

// ---------- send / return buses ----------
export const returns = doc.getArray<Y.Map<any>>('returns')

// Send A/B are now built-in bus tracks (see DEFAULT_BUSES), so we no longer seed
// the legacy return channels. Old projects that saved `returns` still load them
// (loadProject) and the engine falls back to wiring the A/B sends into them.
export function ensureReturns() { /* no-op: A/B are bus tracks now */ }
/** Drop the superseded legacy A/B return channels once the project has the
 *  dedicated A/B send buses — they no longer receive any sends, they just waste a
 *  reverb/delay node and show up as orphaned "A · Reverb" / "B · Delay" devices.
 *  `returns` isn't undo-tracked, so this won't appear in undo history. */
export function cleanupLegacyReturns() {
  if (returns.length === 0) return
  const hasABuses = busList().some(b => b.get('send') === 'A' || b.get('send') === 'B')
  if (hasABuses) mutate('Clean up legacy returns', () => returns.delete(0, returns.length))
}
export function returnAt(i: number): Y.Map<any> | undefined { return returns.get(i) }
export function setReturnGain(i: number, v: number) {
  mutate('Return volume', () => returns.get(i)?.set('gain', v))
}
export function setReturnFxType(i: number, type: string, params: Record<string, number>) {
  mutate('Return effect', () => {
    const r = returns.get(i); if (!r) return
    r.set('fxType', type)
    const pm = new Y.Map<number>()
    for (const [k, v] of Object.entries(params)) pm.set(k, v)
    r.set('params', pm)
  })
}
export function setReturnParam(i: number, key: string, v: number) {
  mutate('Return effect', () => (returns.get(i)?.get('params') as Y.Map<number>)?.set(key, v))
}
// ---------- buses ----------
export function busList(): Y.Map<any>[] {
  return tracks.toArray().filter(t => t.get('kind') === 'bus')
}
/** Set a track's bus-send level (busTrackId → 0..1). */
export function setBusSend(trackId: string, busId: string, level: number) {
  mutate('Bus send', () => {
    const t = trackById(trackId); if (!t) return
    let sm = t.get('sends') as Y.Map<number> | undefined
    if (!sm) { sm = new Y.Map<number>(); t.set('sends', sm) }
    sm.set(busId, level)
  })
}
/** Route a bus's output: 'master' or another bus's id. */
export function setTrackOutput(trackId: string, target: string) {
  mutate('Bus output', () => trackById(trackId)?.set('output', target))
}
/**
 * Would audio leaving `fromBusId` ever reach `toBusId` along the current routing
 * (bus outputs + bus→bus sends)? Used to detect feedback before committing a new
 * route from `toBusId` into `fromBusId`.
 */
export function busCanReach(fromBusId: string, toBusId: string): boolean {
  const seen = new Set<string>()
  const visit = (id: string): boolean => {
    if (id === toBusId) return true
    if (seen.has(id)) return false
    seen.add(id)
    const t = trackById(id)
    if (!t || t.get('kind') !== 'bus') return false
    const out = t.get('output') as string | undefined
    if (out && out !== 'master' && visit(out)) return true
    const sm = t.get('sends') as Y.Map<number> | undefined
    if (sm) for (const [bid, lvl] of sm.entries()) {
      if ((lvl ?? 0) > 0 && trackById(bid)?.get('kind') === 'bus' && visit(bid)) return true
    }
    return false
  }
  return visit(fromBusId)
}

// ---------- macro racks ----------
function macrosArr(t: Y.Map<any>): Y.Array<Y.Map<any>> {
  let a = t.get('macros') as Y.Array<Y.Map<any>> | undefined
  if (!a) {
    a = new Y.Array<Y.Map<any>>()
    const fresh: Y.Map<any>[] = []
    for (let i = 0; i < 8; i++) {
      const m = new Y.Map<any>()
      m.set('name', `Macro ${i + 1}`)
      m.set('value', 0)
      m.set('targets', new Y.Array<Y.Map<any>>())
      fresh.push(m)
    }
    a.push(fresh)
    t.set('macros', a)
  }
  return a
}
export function ensureMacros(trackId: string) {
  mutate('Init macros', () => { const t = trackById(trackId); if (t) macrosArr(t) })
}
export function macrosOf(t: Y.Map<any>): Y.Array<Y.Map<any>> | undefined {
  return t.get('macros') as Y.Array<Y.Map<any>> | undefined
}
/** Set a macro's value AND write every mapped parameter (lerp across its range). */
export function setMacroValue(trackId: string, idx: number, value: number, ranges: (key: string, dest: string, fxId: string, pkey: string) => [number, number]) {
  mutate('Macro', () => {
    const t = trackById(trackId); if (!t) return
    const m = macrosArr(t).get(idx); if (!m) return
    m.set('value', value)
    const targets = m.get('targets') as Y.Array<Y.Map<any>>
    targets.forEach(tg => {
      const dest = tg.get('dest'), fxId = tg.get('fxId') || '', pkey = tg.get('pkey')
      const [lo, hi] = ranges('', dest, fxId, pkey)
      const v = lo + (hi - lo) * value
      if (dest === 'inst') (t.get('inst').get('params') as Y.Map<number>).set(pkey, v)
      else if (dest === 'mix') t.set(pkey, v)
      else if (dest === 'send') t.set(pkey === 'B' ? 'sendB' : 'sendA', v)
      else if (dest === 'midi') {
        const mfx = t.get('midifx') as Y.Array<Y.Map<any>> | undefined
        if (mfx) for (let i = 0; i < mfx.length; i++) if (mfx.get(i).get('id') === fxId) { (mfx.get(i).get('params') as Y.Map<number>).set(pkey, v); break }
      }
      else {
        const fx = t.get('fx') as Y.Array<Y.Map<any>>
        const i = fxIndexIn(fx, fxId)
        if (i >= 0) (fx.get(i).get('params') as Y.Map<number>).set(pkey, v)
      }
    })
  })
}
export function addMacroTarget(trackId: string, idx: number, dest: string, fxId: string, pkey: string) {
  mutate('Map macro', () => {
    const t = trackById(trackId); if (!t) return
    const m = macrosArr(t).get(idx); if (!m) return
    const a = m.get('targets') as Y.Array<Y.Map<any>>
    let dup = false
    a.forEach(x => { if (x.get('dest') === dest && (x.get('fxId') || '') === (fxId || '') && x.get('pkey') === pkey) dup = true })
    if (dup) return
    const tg = new Y.Map<any>()
    tg.set('dest', dest); tg.set('fxId', fxId); tg.set('pkey', pkey)
    a.push([tg])
  })
}
export function removeMacroTarget(trackId: string, idx: number, dest: string, fxId: string, pkey: string) {
  mutate('Unmap macro', () => {
    const t = trackById(trackId); if (!t) return
    const m = macrosArr(t).get(idx); if (!m) return
    const a = m.get('targets') as Y.Array<Y.Map<any>>
    for (let j = a.length - 1; j >= 0; j--) {
      const tg = a.get(j)
      if (tg.get('dest') === dest && (tg.get('fxId') || '') === (fxId || '') && tg.get('pkey') === pkey) a.delete(j)
    }
  })
}
export function clearMacroTargets(trackId: string, idx: number) {
  mutate('Clear macro', () => {
    const t = trackById(trackId); if (!t) return
    const m = macrosArr(t).get(idx); if (!m) return
    const targets = m.get('targets') as Y.Array<Y.Map<any>>
    targets.delete(0, targets.length)
  })
}
export function setMacroName(trackId: string, idx: number, name: string) {
  mutate('Rename macro', () => { const t = trackById(trackId); if (t) macrosArr(t).get(idx)?.set('name', name) })
}

// ---------- live MIDI effects ----------
function midifxArr(t: Y.Map<any>): Y.Array<Y.Map<any>> {
  let a = t.get('midifx') as Y.Array<Y.Map<any>> | undefined
  if (!a) { a = new Y.Array<Y.Map<any>>(); t.set('midifx', a) }
  return a
}
export function midifxOf(t: Y.Map<any>): Y.Array<Y.Map<any>> | undefined {
  return t.get('midifx') as Y.Array<Y.Map<any>> | undefined
}
function midifxIndex(t: Y.Map<any>, id: string) {
  const a = t.get('midifx') as Y.Array<Y.Map<any>> | undefined
  if (!a) return -1
  for (let i = 0; i < a.length; i++) if (a.get(i).get('id') === id) return i
  return -1
}
export function addMidiFx(trackId: string, type: string, params: Record<string, number>) {
  mutate(`Add MIDI ${type}`, () => {
    const t = trackById(trackId); if (!t) return
    const m = new Y.Map<any>()
    m.set('id', id8()); m.set('type', type); m.set('on', true)
    const pm = new Y.Map<number>()
    for (const [k, v] of Object.entries(params)) pm.set(k, v)
    m.set('params', pm)
    midifxArr(t).push([m])
  })
}
export function removeMidiFx(trackId: string, id: string) {
  mutate('Remove MIDI effect', () => {
    const t = trackById(trackId); if (!t) return
    const i = midifxIndex(t, id)
    if (i >= 0) (t.get('midifx') as Y.Array<Y.Map<any>>).delete(i)
  })
}
export function setMidiFxParam(trackId: string, id: string, key: string, v: number) {
  mutate('Tweak MIDI effect', () => {
    const t = trackById(trackId); if (!t) return
    const i = midifxIndex(t, id)
    if (i >= 0) ((t.get('midifx') as Y.Array<Y.Map<any>>).get(i).get('params') as Y.Map<number>).set(key, v)
  })
}
export function setMidiFxOn(trackId: string, id: string, on: boolean) {
  mutate(on ? 'Enable MIDI effect' : 'Bypass MIDI effect', () => {
    const t = trackById(trackId); if (!t) return
    const i = midifxIndex(t, id)
    if (i >= 0) (t.get('midifx') as Y.Array<Y.Map<any>>).get(i).set('on', on)
  })
}

// ---------- clip automation envelopes ----------
export function clipEnv(clipMap: Y.Map<any>): Y.Map<any> | undefined {
  return clipMap.get('env') as Y.Map<any> | undefined
}
export function setEnvPoints(clipMap: Y.Map<any>, key: string, points: { t: number; v: number }[]) {
  mutate('Edit automation', () => {
    let env = clipMap.get('env') as Y.Map<any> | undefined
    if (!env) { env = new Y.Map<any>(); clipMap.set('env', env) }
    if (points.length === 0) { env.delete(key); return }
    const arr2 = new Y.Array<any>()
    arr2.push(points.slice().sort((a, b) => a.t - b.t).map(p => ({ t: Math.round(p.t), v: p.v })))
    env.set(key, arr2)
  })
}
export function envPoints(clipMap: Y.Map<any>, key: string): { t: number; v: number }[] {
  const env = clipMap.get('env') as Y.Map<any> | undefined
  const arr2 = env?.get(key) as Y.Array<any> | undefined
  return arr2 ? arr2.toArray().map(p => ({ ...p })) : []
}
export function envKeys(clipMap: Y.Map<any>): string[] {
  const env = clipMap.get('env') as Y.Map<any> | undefined
  if (!env) return []
  const out: string[] = []
  env.forEach((_v, k) => out.push(k))
  return out
}

// ---------- arrangement (track-timeline) automation ----------
// Same {t,v} breakpoint shape as clip envelopes, but stored per TRACK keyed by
// paramId, with t in ABSOLUTE song ticks (independent of clips).
// The master bus isn't a `tracks` entry — its automation lives in the top-level
// `masterAuto` map and its lane UI state in `meta`. These helpers transparently
// route trackId === 'master' there so the arrangement + engine treat it like any
// other track's automation.
function autoMapFor(trackId: string): Y.Map<any> | undefined {
  if (trackId === 'master') return masterAuto as unknown as Y.Map<any>
  return trackById(trackId)?.get('auto') as Y.Map<any> | undefined
}
export function trackAutoKeys(trackId: string): string[] {
  const am = autoMapFor(trackId)
  if (!am) return []
  const out: string[] = []
  am.forEach((_v, k) => out.push(k))
  return out
}
export function trackAutoPoints(trackId: string, key: string): { t: number; v: number }[] {
  const a = autoMapFor(trackId)?.get(key) as Y.Array<any> | undefined
  return a ? a.toArray().map(p => ({ ...p })) : []
}
export function setTrackAutoPoints(trackId: string, key: string, points: { t: number; v: number }[]) {
  mutate('Edit automation', () => {
    let am: Y.Map<any> | undefined
    if (trackId === 'master') am = masterAuto as unknown as Y.Map<any>
    else { const t = trackById(trackId); if (!t) return; am = t.get('auto') as Y.Map<any> | undefined; if (!am) { am = new Y.Map<any>(); t.set('auto', am) } }
    if (!am) return
    if (points.length === 0) { am.delete(key); return }
    const a = new Y.Array<any>()
    a.push(points.slice().sort((x, y) => x.t - y.t).map(p => ({ t: Math.round(p.t), v: p.v })))
    am.set(key, a)
  })
}
export const setAutoOpen = (trackId: string, v: boolean) => {
  if (trackId === 'master') { mutate('Toggle automation', () => meta.set('masterAutoOpen', v)); return }
  const t = trackById(trackId); if (t) mutate('Toggle automation', () => t.set('autoOpen', v))
}
export const setAutoParam = (trackId: string, key: string) => {
  if (trackId === 'master') { mutate('Automation parameter', () => meta.set('masterAutoParam', key)); return }
  const t = trackById(trackId); if (t) mutate('Automation parameter', () => t.set('autoParam', key))
}

// ---------- arrangement markers & regions ----------
// Stored as a plain array on shared meta: small, syncs to collaborators, and
// rides the existing undo manager (meta is a tracked type). A marker with
// len === 0 is a point flag; len > 0 makes it a named region span.
export type Marker = { id: string; t: number; len: number; name: string; color: number }
export function markersList(): Marker[] {
  const raw = meta.get('markers')
  return Array.isArray(raw) ? raw as Marker[] : []
}
function writeMarkers(label: string, next: Marker[]) {
  mutate(label, () => meta.set('markers', [...next].sort((a, b) => a.t - b.t)))
}
export function addMarker(t: number, name: string, len = 0, color = 4): string {
  const id = id8()
  writeMarkers(len > 0 ? 'Add region' : 'Add marker', [...markersList(), { id, t: Math.max(0, Math.round(t)), len: Math.max(0, Math.round(len)), name, color }])
  return id
}
export function updateMarker(id: string, patch: Partial<Omit<Marker, 'id'>>) {
  writeMarkers('Edit marker', markersList().map(mk => mk.id === id ? { ...mk, ...patch, t: Math.max(0, Math.round(patch.t ?? mk.t)) } : mk))
}
export function removeMarker(id: string) {
  writeMarkers('Delete marker', markersList().filter(mk => mk.id !== id))
}

// ---------- follow actions (session clips) ----------
export function setFollow(clipMap: Y.Map<any>, patch: Record<string, any>) {
  mutate('Follow action', () => {
    let f = clipMap.get('follow') as Y.Map<any> | undefined
    if (!f) {
      f = new Y.Map<any>()
      f.set('on', false); f.set('bars', 1); f.set('action', 0); f.set('chance', 1)
      clipMap.set('follow', f)
    }
    for (const [k, v] of Object.entries(patch)) f.set(k, v)
  })
}
export function followOf(clipMap: Y.Map<any>): { on: boolean; bars: number; action: number; chance: number } | null {
  const f = clipMap.get('follow') as Y.Map<any> | undefined
  if (!f) return null
  return { on: !!f.get('on'), bars: f.get('bars') ?? 1, action: f.get('action') ?? 0, chance: f.get('chance') ?? 1 }
}

// ---------- sampler reference ----------
export function setSamplerSample(trackId: string, sampleId: string, name: string) {
  mutate('Load sample', () => {
    const t = trackById(trackId); if (!t) return
    const inst = t.get('inst') as Y.Map<any>
    inst.set('sampleId', sampleId)
    inst.set('sampleName', name)
  })
}

/** Map an audio sample onto one drum pad (sampleId=null clears it back to synth). */
export function setDrumPadSample(trackId: string, pad: number, sampleId: string | null, name = '') {
  mutate(sampleId ? 'Sample drum pad' : 'Clear drum pad sample', () => {
    const t = trackById(trackId); if (!t) return
    const inst = t.get('inst') as Y.Map<any>
    const key = String(pad)
    if (sampleId) {
      let ps = inst.get('padSamples') as Y.Map<string> | undefined
      let pn = inst.get('padNames') as Y.Map<string> | undefined
      if (!ps) { ps = new Y.Map<string>(); inst.set('padSamples', ps) }
      if (!pn) { pn = new Y.Map<string>(); inst.set('padNames', pn) }
      ps.set(key, sampleId)
      pn.set(key, name)
    } else {
      ;(inst.get('padSamples') as Y.Map<string> | undefined)?.delete(key)
      ;(inst.get('padNames') as Y.Map<string> | undefined)?.delete(key)
    }
  })
}

/** Map a whole sampled kit onto a drum track's pads in one undoable step. */
export function assignDrumKit(trackId: string, assigns: { pad: number; sampleId: string | null; name: string; tune?: number }[], label = 'Load drum kit') {
  mutate(label, () => {
    const t = trackById(trackId); if (!t) return
    const inst = t.get('inst') as Y.Map<any>
    let ps = inst.get('padSamples') as Y.Map<string> | undefined
    let pn = inst.get('padNames') as Y.Map<string> | undefined
    const params = inst.get('params') as Y.Map<number>
    for (const a of assigns) {
      const key = String(a.pad)
      if (a.sampleId) {
        if (!ps) { ps = new Y.Map<string>(); inst.set('padSamples', ps) }
        if (!pn) { pn = new Y.Map<string>(); inst.set('padNames', pn) }
        ps.set(key, a.sampleId); pn.set(key, a.name)
        params.set(`p${a.pad}_tune`, a.tune ?? 0)
      } else {
        ps?.delete(key); pn?.delete(key)
        params.set(`p${a.pad}_tune`, 0)   // reverting to synth: clear any leftover tune offset
      }
    }
  })
}

// ---------- audio tracks & clips ----------
export function addAudioTrack(name: string, color: number): string {
  return addTrack({
    name, color, kind: 'audio',
    inst: { type: 'audiobus', params: {} },
    fx: [], gain: 0, pan: 0, mute: false, solo: false,
  })
}

// A bus is a clip-less track: the audiobus passthrough is its input (sends land
// there), it has an fx chain, and its fader routes to `output` (master/another bus).
export function addBusTrack(name: string, color: number): string {
  return addTrack({
    name, color, kind: 'bus',
    inst: { type: 'audiobus', params: {} },
    fx: [], gain: 0, pan: 0, mute: false, solo: false, output: 'master',
  })
}

export function createAudioClip(trackId: string, sceneId: string, sampleId: string, sampleName: string, durTicks: number, color: number): ClipRef {
  const m = jsonToClipMap({
    name: sampleName, color, len: durTicks, notes: {},
    audio: { sampleId, sampleName, gainDb: 0, pitch: 0, rev: 0, loop: 1, fadeIn: 0, fadeOut: 0, offset: 0, dur: 0, cents: 0, xfade: 0 },
  })
  mutate('Add audio clip', () => clips.set(clipKey(trackId, sceneId), m))
  return { kind: 'session', trackId, sceneId }
}

export function setAudioField(ref: ClipRef, key: string, val: number, label = 'Edit audio clip') {
  mutate(label, () => getClipMap(ref)?.set(key, val))
}

// ---------- scenes ----------
export function addScene(afterIndex?: number): string {
  const m = new Y.Map<any>()
  const sid = id8()
  m.set('id', sid)
  m.set('name', `Scene ${scenes.length + 1}`)
  mutate('Add scene', () => {
    const at = afterIndex === undefined ? scenes.length : afterIndex + 1
    scenes.insert(at, [m])
  })
  return sid
}

export function removeScene(sceneId: string) {
  mutate('Delete scene', () => {
    const i = sceneIndex(sceneId)
    if (i >= 0) scenes.delete(i)
    const keys: string[] = []
    clips.forEach((_v, k) => { if (k.endsWith('|' + sceneId)) keys.push(k) })
    keys.forEach(k => clips.delete(k))
  })
}

export const renameScene = (sceneId: string, name: string) =>
  mutate('Rename scene', () => {
    const i = sceneIndex(sceneId)
    if (i >= 0) scenes.get(i).set('name', name)
  })

export function duplicateScene(sceneId: string): string {
  const newId = id8()
  mutate('Duplicate scene', () => {
    const i = sceneIndex(sceneId)
    if (i < 0) return
    const m = new Y.Map<any>()
    m.set('id', newId)
    m.set('name', scenes.get(i).get('name') + ' copy')
    scenes.insert(i + 1, [m])
    clips.forEach((v, k) => {
      if (k.endsWith('|' + sceneId)) {
        const trackId = k.split('|')[0]
        clips.set(clipKey(trackId, newId), jsonToClipMap(clipToJSON(v)))
      }
    })
  })
  return newId
}

// ---------- session clips ----------
export function createClip(trackId: string, sceneId: string, json?: ClipJSON): ClipRef {
  const t = trackById(trackId)
  const sIdx = sceneIndex(sceneId)
  const base: ClipJSON = json ?? {
    name: `${t?.get('name') ?? 'Clip'} ${sIdx + 1}`,
    color: t?.get('color') ?? 0,
    len: barTicks(),   // one bar of the CURRENT meter
    notes: {},
  }
  mutate(json ? 'Paste clip' : 'Create clip', () => {
    clips.set(clipKey(trackId, sceneId), jsonToClipMap(base))
  })
  return { kind: 'session', trackId, sceneId }
}

export function deleteClipAt(trackId: string, sceneId: string) {
  mutate('Delete clip', () => clips.delete(clipKey(trackId, sceneId)))
}

export function setClipField(ref: ClipRef, k: 'name' | 'color' | 'len', v: any, label?: string) {
  mutate(label ?? `Clip ${k}`, () => getClipMap(ref)?.set(k, v))
}

export function duplicateClipTo(src: ClipRef, trackId: string, sceneId: string) {
  const m = getClipMap(src)
  if (!m) return
  const json = clipToJSON(m)
  mutate('Duplicate clip', () => clips.set(clipKey(trackId, sceneId), jsonToClipMap(json)))
}

// ---------- notes (session or arrangement clips alike) ----------
export function addNote(clipMap: Y.Map<any>, n: Note, label = 'Add note'): string {
  const nid = id8()
  mutate(label, () => (clipMap.get('notes') as Y.Map<Note>).set(nid, { ...n }))
  return nid
}

export function updateNotes(clipMap: Y.Map<any>, entries: [string, Partial<Note>][], label = 'Edit notes') {
  mutate(label, () => {
    const nm = clipMap.get('notes') as Y.Map<Note>
    for (const [nid, patch] of entries) {
      const cur = nm.get(nid)
      if (cur) nm.set(nid, { ...cur, ...patch })
    }
  })
}

export function deleteNotes(clipMap: Y.Map<any>, ids: string[], label = 'Delete notes') {
  mutate(label, () => {
    const nm = clipMap.get('notes') as Y.Map<Note>
    ids.forEach(nid => nm.delete(nid))
  })
}

export function addNotes(clipMap: Y.Map<any>, notes: Note[], label = 'Add notes'): string[] {
  const ids: string[] = []
  mutate(label, () => {
    const nm = clipMap.get('notes') as Y.Map<Note>
    for (const n of notes) {
      const nid = id8()
      ids.push(nid)
      nm.set(nid, { ...n })
    }
  })
  return ids
}

export function replaceNotes(clipMap: Y.Map<any>, notes: Record<string, Note>, label = 'Transform notes') {
  mutate(label, () => {
    const nm = clipMap.get('notes') as Y.Map<Note>
    const old: string[] = []
    nm.forEach((_n, k) => old.push(k))
    old.forEach(k => nm.delete(k))
    for (const [nid, n] of Object.entries(notes)) nm.set(nid, { ...n })
  })
}

export function notesOf(clipMap: Y.Map<any>): [string, Note][] {
  const out: [string, Note][] = []
  const nm = clipMap.get('notes') as Y.Map<Note>
  if (nm) nm.forEach((n, k) => out.push([k, n]))
  return out
}

// ---------- arrangement ----------
export function addArrClip(trackId: string, start: number, json: ClipJSON, label = 'Add to arrangement'): string {
  const aid = id8()
  const m = jsonToClipMap(json, { trackId, start, id: aid })
  mutate(label, () => arr.set(aid, m))
  return aid
}

export function moveArrClip(aid: string, patch: Partial<{ start: number; trackId: string }>) {
  mutate('Move clip', () => {
    const m = arr.get(aid) as Y.Map<any>
    if (!m) return
    for (const [k, v] of Object.entries(patch)) m.set(k, v)
  })
}

export function resizeArrClip(aid: string, len: number) {
  mutate('Resize clip', () => (arr.get(aid) as Y.Map<any>)?.set('len', Math.max(BAR / 4, len)))
}

export function deleteArrClip(aid: string) {
  mutate('Delete clip', () => arr.delete(aid))
}

export function duplicateArrClip(aid: string): string | null {
  const m = arr.get(aid) as Y.Map<any>
  if (!m) return null
  return addArrClip(m.get('trackId'), m.get('start') + m.get('len'), clipToJSON(m), 'Duplicate clip')
}

/**
 * Cut an arrangement clip in two at `cutTicks` from its start. For audio clips
 * the caller passes the sample crop for each half (computed from the buffer) so
 * playback stays continuous across the cut. MIDI clips partition their notes
 * and envelopes: a note spanning the cut is truncated in the left half and
 * continues as a new note in the right. Returns the new right-hand clip id.
 */
export function splitArrClip(aid: string, cutTicks: number, audio?: { leftDur: number; rightOffset: number; rightDur: number }): string | null {
  const m = arr.get(aid) as Y.Map<any>
  if (!m) return null
  const start = m.get('start') ?? 0
  const len = m.get('len') ?? BAR
  if (cutTicks <= 0 || cutTicks >= len) return null
  const rid = id8()
  mutate('Split clip', () => {
    const json = clipToJSON(m)
    json.len = len - cutTicks
    if (json.audio && audio) { json.audio.offset = audio.rightOffset; json.audio.dur = audio.rightDur }
    if (!json.audio) {
      // right half: notes at/after the cut (rebased); spanning notes continue
      const right: Record<string, Note> = {}
      for (const [nid, n] of Object.entries(json.notes)) {
        if (n.s >= cutTicks) right[nid] = { ...n, s: n.s - cutTicks }
        else if (n.s + n.d > cutTicks) right[id8()] = { ...n, s: 0, d: n.s + n.d - cutTicks }
      }
      json.notes = right
      if (json.env) {
        const env: NonNullable<ClipJSON['env']> = {}
        for (const [k, pts] of Object.entries(json.env)) {
          const r = pts.filter(p => p.t >= cutTicks).map(p => ({ t: p.t - cutTicks, v: p.v }))
          if (r.length) env[k] = r
        }
        json.env = env
      }
    }
    arr.set(rid, jsonToClipMap(json, { trackId: m.get('trackId'), start: start + cutTicks, id: rid }))
    m.set('len', cutTicks)
    if (json.audio && audio) m.set('dur', audio.leftDur)
    if (!m.get('audio')) {
      // left half: drop notes past the cut, truncate the spanning ones
      const nm = m.get('notes') as Y.Map<Note>
      const del: string[] = []
      const patch: [string, Note][] = []
      nm.forEach((n, nid) => {
        if (n.s >= cutTicks) del.push(nid)
        else if (n.s + n.d > cutTicks) patch.push([nid, { ...n, d: cutTicks - n.s }])
      })
      del.forEach(nid => nm.delete(nid))
      patch.forEach(([nid, n]) => nm.set(nid, n))
      const envM = m.get('env') as Y.Map<any> | undefined
      envM?.forEach((a: Y.Array<any>, k: string) => {
        const keep = a.toArray().filter((p: any) => p.t <= cutTicks)
        if (keep.length !== a.length) { a.delete(0, a.length); if (keep.length) a.push(keep) }
      })
    }
  })
  return rid
}

export function sendClipToArr(trackId: string, sceneId: string, at: number) {
  const m = clips.get(clipKey(trackId, sceneId)) as Y.Map<any>
  if (!m) return
  addArrClip(trackId, at, clipToJSON(m), 'Send to arrangement')
}

export function sendSceneToArr(sceneId: string, at: number) {
  mutate('Scene to arrangement', () => {
    clips.forEach((v, k) => {
      if (k.endsWith('|' + sceneId)) {
        const trackId = k.split('|')[0]
        arr.set(id8(), jsonToClipMap(clipToJSON(v), { trackId, start: at }))
      }
    })
  })
}

export function arrEndTicks(): number {
  let end = 0
  arr.forEach(m => { end = Math.max(end, (m.get('start') ?? 0) + (m.get('len') ?? 0)) })
  return end
}

// ---------- chat ----------
export function sendChat(name: string, color: string, text: string) {
  doc.transact(() => {
    chat.push([{ id: id8(), name, color, text, t: Date.now() }])
    if (chat.length > 200) chat.delete(0, chat.length - 200)
  }, LOCAL)
}

// ---------- project load/save ----------
export function exportProject(): ProjectJSON {
  const clipsJson: Record<string, ClipJSON> = {}
  clips.forEach((v, k) => { clipsJson[k] = clipToJSON(v) })
  const arrJson: ProjectJSON['arr'] = {}
  arr.forEach((v, k) => {
    arrJson[k] = { ...clipToJSON(v), trackId: v.get('trackId'), start: v.get('start') }
  })
  return {
    meta: {
      title: meta.get('title') ?? 'Untitled Jam',
      bpm: meta.get('bpm') ?? 120,
      swing: meta.get('swing') ?? 0,
      swingSubdivision: meta.get('swingSubdivision') ?? '16n',
      humanize: meta.get('humanize') ?? 0,
      root: meta.get('root') ?? 9,
      scale: meta.get('scale') ?? 'minor',
      launchQ: meta.get('launchQ') ?? 1,
      masterGain: meta.get('masterGain') ?? 0,
      loopOn: !!meta.get('loopOn'), loopStart: meta.get('loopStart') ?? 0, loopEnd: meta.get('loopEnd') ?? BAR * 4,
      tsTop: meta.get('tsTop') ?? 4, tsBottom: meta.get('tsBottom') ?? 4,
      ...(meta.get('masterAutoOpen') ? { masterAutoOpen: true } : {}),
      ...(meta.get('masterAutoParam') ? { masterAutoParam: meta.get('masterAutoParam') } : {}),
    },
    tracks: tracks.toArray().map(t => {
      const inst = t.get('inst') as Y.Map<any>
      const instJson: TrackJSON['inst'] = { type: inst.get('type'), params: Object.fromEntries((inst.get('params') as Y.Map<number>).entries()), out: inst.get('out') ?? 0 }
      if (inst.get('sampleId')) { instJson.sampleId = inst.get('sampleId'); instJson.sampleName = inst.get('sampleName') ?? '' }
      const psm = inst.get('padSamples') as Y.Map<string> | undefined
      if (psm && psm.size) {
        instJson.padSamples = Object.fromEntries(psm.entries())
        const pnm = inst.get('padNames') as Y.Map<string> | undefined
        instJson.padNames = pnm ? Object.fromEntries(pnm.entries()) : {}
      }
      const lfos = (t.get('lfos') as Y.Array<Y.Map<any>> | undefined)?.toArray().map(l => ({
        id: l.get('id'), on: l.get('on'), shape: l.get('shape'), sync: l.get('sync'), rate: l.get('rate'),
        hz: l.get('hz'), depth: l.get('depth'), phase: l.get('phase'), dest: l.get('dest'), fxId: l.get('fxId'), pkey: l.get('pkey'),
        targets: lfoTargetList(l),
      })) ?? []
      const macros = (t.get('macros') as Y.Array<Y.Map<any>> | undefined)?.toArray().map(mc => ({
        name: mc.get('name'), value: mc.get('value'),
        targets: (mc.get('targets') as Y.Array<Y.Map<any>>).toArray().map(tg => ({ dest: tg.get('dest'), fxId: tg.get('fxId'), pkey: tg.get('pkey') })),
      })) ?? []
      const midifx = (t.get('midifx') as Y.Array<Y.Map<any>> | undefined)?.toArray().map(d => ({
        id: d.get('id'), type: d.get('type'), on: d.get('on'), params: Object.fromEntries((d.get('params') as Y.Map<number>).entries()),
      })) ?? []
      return {
        id: t.get('id'), name: t.get('name'), color: t.get('color'), kind: t.get('kind'),
        inst: instJson,
        fx: (t.get('fx') as Y.Array<Y.Map<any>>).toArray().map(f => ({
          id: f.get('id'), type: f.get('type'), on: f.get('on'), out: f.get('out') ?? 0,
          params: Object.fromEntries((f.get('params') as Y.Map<number>).entries()),
        })),
        gain: t.get('gain'), pan: t.get('pan'), mute: t.get('mute'), solo: t.get('solo'),
        sendA: t.get('sendA') ?? 0, sendB: t.get('sendB') ?? 0,
        ...(t.get('locked') ? { locked: true } : {}),
        ...(t.get('send') ? { send: t.get('send') } : {}),
        output: t.get('output') ?? 'master',
        sends: (() => { const sm = t.get('sends') as Y.Map<number> | undefined; const o: Record<string, number> = {}; sm?.forEach((v, k) => { o[k] = v }); return o })(),
        lfos, macros, midifx,
        ...(() => {
          const am = t.get('auto') as Y.Map<any> | undefined
          if (!am || am.size === 0) return { autoOpen: !!t.get('autoOpen'), autoParam: t.get('autoParam') ?? undefined }
          const auto: Record<string, { t: number; v: number }[]> = {}
          am.forEach((arr2, k) => { auto[k] = (arr2 as Y.Array<any>).toArray().map(p => ({ ...p })) })
          return { auto, autoOpen: !!t.get('autoOpen'), autoParam: t.get('autoParam') ?? undefined }
        })(),
      }
    }),
    scenes: scenes.toArray().map(s => ({ id: s.get('id'), name: s.get('name') })),
    clips: clipsJson,
    arr: arrJson,
    returns: returns.toArray().map(r => ({
      id: r.get('id'), name: r.get('name'), fxType: r.get('fxType'),
      params: Object.fromEntries((r.get('params') as Y.Map<number>).entries()), gain: r.get('gain') ?? 0,
    })),
    masterFx: masterFx.toArray().map(f => ({
      id: f.get('id'), type: f.get('type'), on: f.get('on'), out: f.get('out') ?? 0,
      params: Object.fromEntries((f.get('params') as Y.Map<number>).entries()),
    })),
    masterAuto: (() => { const o: Record<string, { t: number; v: number }[]> = {}; masterAuto.forEach((a, k) => { o[k] = (a as Y.Array<any>).toArray().map(p => ({ ...p })) }); return o })(),
  }
}

export function loadProject(json: ProjectJSON, label = 'Load project') {
  mutate(label, () => {
    // wipe
    tracks.delete(0, tracks.length)
    scenes.delete(0, scenes.length)
    const ck: string[] = []
    clips.forEach((_v, k) => ck.push(k))
    ck.forEach(k => clips.delete(k))
    const ak: string[] = []
    arr.forEach((_v, k) => ak.push(k))
    ak.forEach(k => arr.delete(k))
    if (returns.length) returns.delete(0, returns.length)
    if (masterFx.length) masterFx.delete(0, masterFx.length)
    masterAuto.forEach((_v, k) => masterAuto.delete(k))
    // meta
    for (const [k, v] of Object.entries(json.meta)) meta.set(k, v)
    meta.set('inited', true)
    // master-bus effect chain + automation
    if (json.masterFx?.length) masterFx.push(json.masterFx.map(f => yFx(f)))
    if (json.masterAuto) for (const [k, pts] of Object.entries(json.masterAuto)) { const a = new Y.Array<any>(); a.push(pts.map(p => ({ ...p }))); masterAuto.set(k, a) }
    // Legacy A/B return channels are NOT restored — they've been replaced by the
    // dedicated A/B send buses (added below via DEFAULT_BUSES). Old project files
    // keep their `returns` data but it's ignored on load.
    // id remapping (packs may use friendly ids)
    const tidMap = new Map<string, string>()
    const sidMap = new Map<string, string>()
    // pass 1: allocate fresh ids so bus-routing refs (output target + sends keys,
    // which hold bus track ids) can be remapped — including forward references.
    const idFor = (t: TrackJSON) => t.id ?? t.name
    for (const t of json.tracks) tidMap.set(idFor(t), id8())
    // pass 2: build tracks, remapping bus output + sends keys through tidMap
    for (const t of json.tracks) {
      const tid = tidMap.get(idFor(t))!
      const output = t.output && t.output !== 'master' ? (tidMap.get(t.output) ?? 'master') : (t.output ?? 'master')
      const sends = t.sends
        ? Object.fromEntries(Object.entries(t.sends).flatMap(([k, v]) => { const nk = tidMap.get(k); return nk ? [[nk, v]] : [] }))
        : t.sends
      tracks.push([yTrack({ ...t, id: tid, output, sends })])
    }
    // migration: ensure the built-in A/B send buses exist (projects predating them,
    // or imports lacking them) so each track's A/B sends always resolve to a bus.
    const haveSend = (s: 'A' | 'B') => json.tracks.some(t => t.kind === 'bus' && t.send === s)
    for (const b of DEFAULT_BUSES) if (!haveSend(b.send!)) tracks.push([yTrack({ ...b, id: id8() })])
    for (const s of json.scenes) {
      const m = new Y.Map<any>()
      const sid = id8()
      m.set('id', sid)
      m.set('name', s.name)
      sidMap.set(s.id ?? s.name, sid)
      scenes.push([m])
    }
    for (const [key, c] of Object.entries(json.clips)) {
      const [tOld, sOld] = key.split('|')
      const tNew = tidMap.get(tOld)
      const sNew = sidMap.get(sOld)
      if (tNew && sNew) clips.set(clipKey(tNew, sNew), jsonToClipMap(c))
    }
    for (const a of Object.values(json.arr)) {
      const tNew = tidMap.get(a.trackId)
      if (tNew) arr.set(id8(), jsonToClipMap(a, { trackId: tNew, start: a.start }))
    }
  })
}

export function isDocEmpty() {
  return !meta.get('inited')
}

export function initIfEmpty(defaultProject: ProjectJSON): boolean {
  if (!isDocEmpty()) return false
  loadProject(defaultProject, 'New project')
  return true
}

// ---------- lossless full-document snapshot ----------
// The ProjectJSON path (exportProject/loadProject) is intentionally lossy — it
// predates sends, LFOs, macros, MIDI fx, return buses, automation and follow
// actions. For the room hand-off we carry the ENTIRE Yjs document as a binary
// update instead, so nothing is dropped and all internal ids stay valid.
function u8ToB64(u8: Uint8Array): string {
  let s = ''
  const chunk = 0x8000
  for (let i = 0; i < u8.length; i += chunk) s += String.fromCharCode(...u8.subarray(i, i + chunk))
  return btoa(s)
}
function b64ToU8(b64: string): Uint8Array {
  const s = atob(b64)
  const u8 = new Uint8Array(s.length)
  for (let i = 0; i < s.length; i++) u8[i] = s.charCodeAt(i)
  return u8
}
export function encodeDocState(): string {
  return u8ToB64(Y.encodeStateAsUpdate(doc))
}
export function applyDocState(b64: string) {
  Y.applyUpdate(doc, b64ToU8(b64), LOCAL)
}

// ---------- room creation (carry current project into a fresh shared room) ----------
// Set a flag so the global hashchange listener doesn't ALSO reload — otherwise
// the page reloads twice and the second boot (after the carry was consumed)
// lands on an empty doc, wiping the project.
export function createRoomAndGo(): string {
  const rid = nanoid(10)
  // full lossless snapshot — every track device, send, LFO, macro, return bus,
  // automation envelope and follow action comes along.
  sessionStorage.setItem('sf-carry-bin', encodeDocState())
  ;(window as any).__sfNav = true
  location.hash = `r=${rid}`
  location.reload()
  return rid
}

export function leaveRoomAndGo() {
  sessionStorage.setItem('sf-carry-bin', encodeDocState())
  ;(window as any).__sfNav = true
  location.hash = ''
  location.reload()
}

/** Apply a project carried across a room transition into the (fresh) doc. Returns true if applied. */
export function maybeTakeCarried(): boolean {
  const bin = sessionStorage.getItem('sf-carry-bin')
  if (bin) {
    sessionStorage.removeItem('sf-carry-bin')
    if (isDocEmpty()) { applyDocState(bin); return true }
    return false
  }
  // legacy JSON carry (older tabs mid-transition)
  const raw = sessionStorage.getItem('sf-carry')
  if (raw) {
    sessionStorage.removeItem('sf-carry')
    if (isDocEmpty()) { try { loadProject(JSON.parse(raw), 'Start shared session'); return true } catch { /* ignore */ } }
  }
  return false
}
