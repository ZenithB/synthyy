// Dumps the frozen device/param schema (src/audio/schema.ts) to
// fixtures/schema-dump.json so the Rust param registry (crates/sf-project)
// can be validated against the TypeScript source of truth.
//
//   node scripts/dump-schema.mjs
//
// schema.ts is pure data with no imports; rolldown (already a dependency)
// transpiles it to ESM in-memory. `fmt` functions are display-only and are
// not part of the dump.
import { rolldown } from 'rolldown'
import { mkdtemp, writeFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join, dirname } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const root = join(dirname(fileURLToPath(import.meta.url)), '..')

const bundle = await rolldown({ input: join(root, 'src/audio/schema.ts'), logLevel: 'silent' })
const { output } = await bundle.generate({ format: 'esm' })
await bundle.close()

const tmp = await mkdtemp(join(tmpdir(), 'sf-schema-'))
const mod = join(tmp, 'schema.mjs')
await writeFile(mod, output[0].code)
const schema = await import(pathToFileURL(mod).href)
await rm(tmp, { recursive: true, force: true })

const spec = (s) => {
  const out = { key: s.key, label: s.label, min: s.min, max: s.max, def: s.def }
  if (s.exp) out.exp = true
  if (s.int) out.int = true
  if (s.steps) out.steps = s.steps
  if (s.rateSwitch) out.rateSwitch = true
  return out
}
const device = (d) => ({ type: d.type, params: d.params.map(spec) })

const dump = {
  instruments: schema.INSTRUMENTS.map(device),
  effects: schema.EFFECTS.map(device),
  midifx: schema.MIDI_FX.map(device),
}

const outPath = join(root, 'fixtures/schema-dump.json')
await writeFile(outPath, JSON.stringify(dump, null, 2) + '\n')
console.log(
  `wrote ${outPath}: ${dump.instruments.length} instruments, ` +
  `${dump.effects.length} effects, ${dump.midifx.length} midi fx`,
)
