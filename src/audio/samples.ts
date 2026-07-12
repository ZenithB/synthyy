// Local sample store / sample bank. Audio bytes live in IndexedDB (per
// browser) keyed by id; a parallel 'meta' store holds {name, folder} so the
// browser can list the whole bank without touching the bytes. Decoded
// AudioBuffers are cached in memory on first use. The doc only stores sample
// ids + names. v1 is local-first: a collaborator who lacks a sample hears
// silence on that track until they load their own (P2P transfer = future).

import { nanoid } from 'nanoid'
import * as Tone from 'tone'
import { synthDrumSample, parsePackId, packSampleName } from './drumpacks'

const mem = new Map<string, AudioBuffer>()
const pending = new Set<string>()
let onReadyCb: ((id: string) => void) | null = null
export function onSampleReady(cb: (id: string) => void) { onReadyCb = cb }

let dbp: Promise<IDBDatabase> | null = null
function db(): Promise<IDBDatabase> {
  if (!dbp) {
    dbp = new Promise((res, rej) => {
      const req = indexedDB.open('synthtagram-samples', 2)
      req.onupgradeneeded = () => {
        const d = req.result
        if (!d.objectStoreNames.contains('samples')) d.createObjectStore('samples')
        if (!d.objectStoreNames.contains('meta')) {
          // v1 → v2: add the listing store and backfill names from the byte
          // records (inside the versionchange transaction).
          const metaStore = d.createObjectStore('meta')
          const tx = req.transaction!
          const cur = tx.objectStore('samples').openCursor()
          cur.onsuccess = () => {
            const c = cur.result
            if (c) {
              metaStore.put({ name: (c.value as any)?.name ?? 'Sample', folder: '' }, c.key as string)
              c.continue()
            }
          }
        }
      }
      req.onsuccess = () => res(req.result)
      req.onerror = () => rej(req.error)
    })
  }
  return dbp
}

async function idbPut(storeName: 'samples' | 'meta', id: string, rec: any) {
  const d = await db()
  await new Promise<void>((res, rej) => {
    const tx = d.transaction(storeName, 'readwrite')
    tx.objectStore(storeName).put(rec, id)
    tx.oncomplete = () => res()
    tx.onerror = () => rej(tx.error)
  })
}
async function idbGet(storeName: 'samples' | 'meta', id: string): Promise<any> {
  const d = await db()
  return new Promise((res, rej) => {
    const tx = d.transaction(storeName, 'readonly')
    const r = tx.objectStore(storeName).get(id)
    r.onsuccess = () => res(r.result)
    r.onerror = () => rej(r.error)
  })
}
async function idbDelete(storeName: 'samples' | 'meta', id: string) {
  const d = await db()
  await new Promise<void>((res, rej) => {
    const tx = d.transaction(storeName, 'readwrite')
    tx.objectStore(storeName).delete(id)
    tx.oncomplete = () => res()
    tx.onerror = () => rej(tx.error)
  })
}

function decode(bytes: ArrayBuffer): Promise<AudioBuffer> {
  // slice() so the persisted copy isn't detached by decodeAudioData
  const ctx = (Tone.getContext().rawContext as unknown as AudioContext)
  return ctx.decodeAudioData(bytes.slice(0))
}

/** Synchronous lookup; if missing, kicks off a background restore from IndexedDB. */
export function getSampleBuffer(id: string): AudioBuffer | undefined {
  if (!id) return undefined
  const b = mem.get(id)
  if (b) return b
  // virtual drum-pack samples (`dp:kit:role`) are synthesized on demand — no
  // IndexedDB, no download; deterministic so they reproduce live, in exports and
  // for collaborators.
  if (id.startsWith('dp:')) {
    const p = parsePackId(id)
    const buf = p ? synthDrumSample(p.kitId, p.role) : undefined
    if (buf) mem.set(id, buf)
    return buf
  }
  if (!pending.has(id)) {
    pending.add(id)
    idbGet('samples', id).then(async rec => {
      if (rec) {
        try {
          const buf = await decode(rec.bytes)
          mem.set(id, buf)
          onReadyCb?.(id)
        } catch { /* undecodable */ }
      }
      pending.delete(id)
    }).catch(() => pending.delete(id))
  }
  return undefined
}

export function sampleName(id: string) { return id.startsWith('dp:') ? packSampleName(id) : id }

// ---------------- sample library (the bank the browser lists) ----------------
// In-memory mirror of the 'meta' store: id → {name, folder}. `folder` is a
// '/'-separated path ('' = bank root). Loaded once, then kept in sync by every
// mutation; React follows via subscribe/version (useSyncExternalStore).

export type SampleMeta = { id: string; name: string; folder: string }

const lib = new Map<string, { name: string; folder: string }>()
let libLoaded = false
let libVersion = 0
let libList: SampleMeta[] | null = null
const libListeners = new Set<() => void>()
function emitLib() {
  libVersion++
  libList = null
  libListeners.forEach(l => l())
}
export function subscribeSampleLib(fn: () => void) {
  libListeners.add(fn)
  return () => { libListeners.delete(fn) }
}
export const sampleLibVersion = () => libVersion

async function loadLib() {
  if (libLoaded) return
  libLoaded = true
  try {
    const d = await db()
    await new Promise<void>((res, rej) => {
      const tx = d.transaction('meta', 'readonly')
      const cur = tx.objectStore('meta').openCursor()
      cur.onsuccess = () => {
        const c = cur.result
        if (c) {
          const v = c.value as { name?: string; folder?: string }
          lib.set(c.key as string, { name: v?.name ?? 'Sample', folder: v?.folder ?? '' })
          c.continue()
        } else res()
      }
      cur.onerror = () => rej(cur.error)
    })
    emitLib()
  } catch { /* listing is best-effort */ }
}
export const libReady: Promise<void> = loadLib()

/** Every sample in the bank, folder-then-name sorted. Stable ref between changes. */
export function listSamples(): SampleMeta[] {
  if (!libList) {
    libList = [...lib.entries()]
      .map(([id, m]) => ({ id, name: m.name, folder: m.folder }))
      .sort((a, b) => a.folder.localeCompare(b.folder) || a.name.localeCompare(b.name))
  }
  return libList
}

// Empty folders the user created explicitly (folders otherwise derive from the
// samples' paths). Persisted locally — folder layout is a per-user preference.
const XF_KEY = 'sf-sample-folders'
function extraFolders(): string[] {
  try { return JSON.parse(localStorage.getItem(XF_KEY) || '[]') } catch { return [] }
}
function setExtraFolders(f: string[]) {
  localStorage.setItem(XF_KEY, JSON.stringify([...new Set(f)].sort()))
}

/** All folder paths (from samples + user-created empties), incl. ancestors. */
export function listSampleFolders(): string[] {
  const out = new Set<string>(extraFolders())
  lib.forEach(m => { if (m.folder) out.add(m.folder) })
  // ensure every ancestor path exists in the set
  for (const f of [...out]) {
    const parts = f.split('/')
    for (let i = 1; i < parts.length; i++) out.add(parts.slice(0, i).join('/'))
  }
  return [...out].sort()
}

export function createSampleFolder(path: string) {
  const p = path.replace(/^\/+|\/+$/g, '')
  if (!p) return
  setExtraFolders([...extraFolders(), p])
  emitLib()
}

export async function renameSample(id: string, name: string) {
  const m = lib.get(id)
  if (!m || !name.trim()) return
  m.name = name.trim().slice(0, 40)
  await idbPut('meta', id, { ...m }).catch(() => {})
  emitLib()
}

export async function moveSample(id: string, folder: string) {
  const m = lib.get(id)
  if (!m) return
  m.folder = folder.replace(/^\/+|\/+$/g, '')
  await idbPut('meta', id, { ...m }).catch(() => {})
  emitLib()
}

/** Remove a sample from the bank. Tracks/clips referencing it fall silent
 *  (exactly like a collaborator who never had the file). */
export async function deleteSample(id: string) {
  lib.delete(id)
  mem.delete(id)
  await idbDelete('samples', id).catch(() => {})
  await idbDelete('meta', id).catch(() => {})
  emitLib()
}

/** Rename a folder (and re-path every sample + subfolder under it). */
export async function renameSampleFolder(oldPath: string, newPath: string) {
  const from = oldPath.replace(/^\/+|\/+$/g, '')
  const to = newPath.replace(/^\/+|\/+$/g, '')
  if (!from || !to || from === to) return
  const jobs: Promise<void>[] = []
  lib.forEach((m, id) => {
    if (m.folder === from || m.folder.startsWith(from + '/')) {
      m.folder = to + m.folder.slice(from.length)
      jobs.push(idbPut('meta', id, { ...m }).catch(() => {}))
    }
  })
  setExtraFolders(extraFolders().map(f =>
    f === from || f.startsWith(from + '/') ? to + f.slice(from.length) : f))
  await Promise.all(jobs)
  emitLib()
}

/** Delete a folder and every sample inside it (the UI confirms first). */
export async function deleteSampleFolder(path: string) {
  const p = path.replace(/^\/+|\/+$/g, '')
  if (!p) return
  const doomed: string[] = []
  lib.forEach((m, id) => { if (m.folder === p || m.folder.startsWith(p + '/')) doomed.push(id) })
  for (const id of doomed) await deleteSample(id)
  setExtraFolders(extraFolders().filter(f => f !== p && !f.startsWith(p + '/')))
  emitLib()
}

// ---------------- import ----------------

async function store(name: string, bytes: ArrayBuffer, folder = ''): Promise<{ id: string; name: string }> {
  const id = nanoid(10)
  const buf = await decode(bytes)
  mem.set(id, buf)
  await idbPut('samples', id, { name, bytes }).catch(() => {})
  await idbPut('meta', id, { name, folder }).catch(() => {})
  lib.set(id, { name, folder })
  emitLib()
  onReadyCb?.(id)
  return { id, name }
}

/** Store locally-generated audio (the Basics test tones) like an imported file. */
export const addGeneratedSample = (name: string, folder: string, bytes: ArrayBuffer) => store(name, bytes, folder)

const cleanName = (fileName: string) => fileName.replace(/\.[^.]+$/, '').slice(0, 40)

export async function importSampleFile(file: File, folder = ''): Promise<{ id: string; name: string }> {
  const bytes = await file.arrayBuffer()
  return store(cleanName(file.name), bytes, folder)
}

const AUDIO_EXT = /\.(wav|mp3|ogg|oga|m4a|aac|flac|aif|aiff|webm|opus)$/i
export function looksLikeAudio(file: File) {
  return file.type.startsWith('audio') || AUDIO_EXT.test(file.name)
}

/**
 * Bulk import (folder uploads). Decodes each file once to validate it but does
 * NOT keep the decoded PCM in memory — a 300-file pack would otherwise pin
 * hundreds of MB; buffers restore lazily from IndexedDB on first play.
 */
export async function importSampleFilesBulk(
  items: { file: File; folder: string }[],
  onProgress?: (done: number, total: number) => void,
): Promise<{ ok: number; failed: number }> {
  let ok = 0, failed = 0
  for (let i = 0; i < items.length; i++) {
    const { file, folder } = items[i]
    try {
      const bytes = await file.arrayBuffer()
      await decode(bytes)   // validate only; decoded result is discarded
      const id = nanoid(10)
      const name = cleanName(file.name)
      await idbPut('samples', id, { name, bytes })
      await idbPut('meta', id, { name, folder })
      lib.set(id, { name, folder })
      ok++
    } catch { failed++ }
    onProgress?.(i + 1, items.length)
  }
  if (ok) emitLib()
  return { ok, failed }
}

/**
 * Expand a drag-drop DataTransfer into audio files with their folder paths,
 * walking directory entries recursively (dropping a whole sample-pack folder
 * imports its structure). `baseFolder` prefixes everything (drop onto a folder
 * row in the browser).
 */
export async function collectDroppedAudio(dt: DataTransfer, baseFolder = ''): Promise<{ file: File; folder: string }[]> {
  const out: { file: File; folder: string }[] = []
  const seen = new Set<string>()   // path dedupe across items/entries
  const addFile = (file: File, folder: string) => {
    if (!looksLikeAudio(file)) return
    const key = `${folder}/${file.name}/${file.size}`
    if (seen.has(key)) return
    seen.add(key)
    out.push({ file, folder: folder.replace(/^\/+|\/+$/g, '') })
  }
  const walk = (entry: any, folder: string): Promise<void> => new Promise(resolve => {
    if (!entry) return resolve()
    if (entry.isFile) {
      entry.file((f: File) => { addFile(f, folder); resolve() }, () => resolve())
    } else if (entry.isDirectory) {
      const sub = folder ? `${folder}/${entry.name}` : entry.name
      const reader = entry.createReader()
      const readAll = () => {
        reader.readEntries(async (entries: any[]) => {
          if (!entries.length) return resolve()
          for (const e of entries) await walk(e, sub)
          readAll()   // readEntries returns batches of ≤100; drain them all
        }, () => resolve())
      }
      readAll()
    } else resolve()
  })
  const items = dt.items ? [...dt.items] : []
  const entries = items.map(it => (it as any).webkitGetAsEntry?.()).filter(Boolean)
  if (entries.length) {
    for (const e of entries) await walk(e, baseFolder)
  } else {
    for (const f of [...(dt.files || [])]) addFile(f, baseFolder)
  }
  return out
}

// ---------------- mic recording ----------------
let rec: MediaRecorder | null = null
let chunks: Blob[] = []
let stream: MediaStream | null = null

export function isRecordingSample() { return !!rec }

export async function startSampleRecording() {
  if (rec) return
  stream = await navigator.mediaDevices.getUserMedia({ audio: true })
  chunks = []
  rec = new MediaRecorder(stream)
  rec.ondataavailable = e => { if (e.data.size) chunks.push(e.data) }
  rec.start()
}

/** Record from a specific input device into the bank (for audio-track capture).
 *  deviceId '' = the system default input. Returns a handle you stop() to finalize. */
export async function startInputRecording(deviceId: string): Promise<{ stop: () => Promise<{ id: string; name: string; durationSec: number } | null> } | null> {
  const constraint: MediaTrackConstraints = deviceId ? { deviceId: { exact: deviceId } } : {}
  // record clean: no processing that would fight a musical input
  const audio: MediaStreamConstraints['audio'] = { ...constraint, echoCancellation: false, noiseSuppression: false, autoGainControl: false }
  const s = await navigator.mediaDevices.getUserMedia({ audio })
  const chunkList: Blob[] = []
  const r = new MediaRecorder(s)
  const startedAt = performance.now()
  r.ondataavailable = e => { if (e.data.size) chunkList.push(e.data) }
  r.start()
  return {
    stop: () => new Promise(resolve => {
      r.onstop = async () => {
        const durationSec = (performance.now() - startedAt) / 1000
        const blob = new Blob(chunkList, { type: r.mimeType || 'audio/webm' })
        s.getTracks().forEach(t => t.stop())
        try {
          const bytes = await blob.arrayBuffer()
          const stamp = new Date().toISOString().slice(11, 19)
          const rec = await store(`Rec ${stamp}`, bytes, 'Recordings')
          resolve({ ...rec, durationSec })
        } catch { resolve(null) }
      }
      try { r.stop() } catch { resolve(null) }
    }),
  }
}

export async function stopSampleRecording(): Promise<{ id: string; name: string } | null> {
  if (!rec) return null
  const r = rec
  return new Promise(resolve => {
    r.onstop = async () => {
      const blob = new Blob(chunks, { type: r.mimeType || 'audio/webm' })
      stream?.getTracks().forEach(t => t.stop())
      rec = null; stream = null; chunks = []
      try {
        const bytes = await blob.arrayBuffer()
        const stamp = new Date().toISOString().slice(11, 19)
        resolve(await store(`Rec ${stamp}`, bytes, 'Recordings'))
      } catch { resolve(null) }
    }
    r.stop()
  })
}
