// Dumps demoProject() (src/packs.ts — the "Neon Alley" demo song) to
// src-tauri/assets/demo-project.json so the desktop smoke paths
// (SF_SMOKE=1, sf-io examples/play-project.rs) have a real, audible project
// to load without a webview. Modeled on dump-default-project.mjs.
//
//   node scripts/dump-demo-project.mjs
//
// packs.ts transitively imports src/state/doc.ts, which touches yjs,
// y-indexeddb, nanoid and browser globals at module scope. None of that is
// needed for the demoProject() literal, so those modules are aliased to
// minimal stubs and `location` is faked before importing the bundle.
import { rolldown } from 'rolldown'
import { mkdtemp, writeFile, rm, mkdir } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')
const tmp = await mkdtemp(join(tmpdir(), 'sf-packs-'))

// Just enough Yjs surface for doc.ts's module scope (getMap/getArray + the
// builder helpers). Methods used only inside functions can be missing.
const yjsStub = `
class YBase {
  constructor() { this.m = new Map(); this.a = [] }
  get size() { return this.m.size }
  get length() { return this.a.length }
  set(k, v) { this.m.set(k, v) }
  get(k) { return this.m.get(k) }
  has(k) { return this.m.has(k) }
  delete(k) { this.m.delete(k) }
  forEach(fn) { this.m.forEach(fn) }
  push(items) { this.a.push(...items) }
  toArray() { return [...this.a] }
  observe() {} observeDeep() {} unobserve() {} unobserveDeep() {}
}
export class Map_ extends YBase {}
export class Array_ extends YBase {}
export { Map_ as Map, Array_ as Array }
export class Doc {
  constructor() { this.shared = new globalThis.Map() }
  _get(name, Ctor) {
    if (!this.shared.has(name)) this.shared.set(name, new Ctor())
    return this.shared.get(name)
  }
  getMap(name) { return this._get(name, Map_) }
  getArray(name) { return this._get(name, Array_) }
  transact(fn) { fn() }
  on() {} off() {}
}
export class UndoManager {
  constructor() {}
  on() {} off() {} undo() {} redo() {} stopCapturing() {} clear() {}
  get undoStack() { return [] }
  get redoStack() { return [] }
}
`
const idbStub = `export class IndexeddbPersistence { constructor() {} on() {} once() {} whenSynced = Promise.resolve(this) }`
const nanoidStub = `let i = 0; export const nanoid = () => 'stub' + (i++).toString(36).padStart(6, '0')`

await writeFile(join(tmp, 'yjs.mjs'), yjsStub)
await writeFile(join(tmp, 'y-indexeddb.mjs'), idbStub)
await writeFile(join(tmp, 'nanoid.mjs'), nanoidStub)

const bundle = await rolldown({
  input: join(root, 'src/packs.ts'),
  logLevel: 'silent',
  resolve: {
    alias: {
      yjs: join(tmp, 'yjs.mjs'),
      'y-indexeddb': join(tmp, 'y-indexeddb.mjs'),
      nanoid: join(tmp, 'nanoid.mjs'),
    },
  },
})
const { output } = await bundle.generate({ format: 'esm' })
await bundle.close()

const mod = join(tmp, 'packs.mjs')
await writeFile(mod, output[0].code)
globalThis.location = { hash: '' }
const packs = await import(pathToFileURL(mod).href)
await rm(tmp, { recursive: true, force: true })

const project = packs.demoProject()
const outPath = join(root, 'src-tauri/assets/demo-project.json')
await mkdir(dirname(outPath), { recursive: true })
await writeFile(outPath, JSON.stringify(project, null, 2) + '\n')
console.log(`wrote ${outPath}: ${project.tracks.length} tracks, ${Object.keys(project.clips).length} clips`)
