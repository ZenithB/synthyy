// Dev-only golden-fixture harness (docs/RUST_REWRITE_PLAN.md §8/§8a).
//
// A fixture is a JSON file under fixtures/projects/: a project plus the render
// scope, exercising one device or feature. The BROWSER engine renders each one
// to WAV — those WAVs are the golden files the Rust engine's `cargo test`
// null-tests against. This module is the render half; the /#fixtures page
// (src/ui/FixturesPage.tsx) is the manual driver.
//
// Also installs `window.__renderFixture(fixture)` so automated tabs can drive
// renders without the page — remember the AudioContext still needs one real
// user click first, and a hidden tab throttles the render (CLAUDE.md).
//
// This module must only be imported behind `import.meta.env.DEV` guards: it
// (and the fixture JSONs) must never reach the production bundle.

import { loadProject, scenes, ProjectJSON } from '../state/doc'
import { renderMixdown, RenderScope } from './render'
import { resample, encodeWav } from './encode'
import { engine } from './engine'
import { instSchema, fxSchema, midiFxSchema, defaultsFor } from './schema'

export type Fixture = {
  /** File-stem-style name; the golden WAV is `<name>.wav`. */
  name: string
  /** What to render. Scene scopes render the scene loop twice (render.ts). */
  scope: RenderScope
  project: ProjectJSON
}

/** Device constructors expect a FULL param map (the app fills schema defaults
 *  at creation time) — fill any params a hand-written fixture omitted so
 *  fixtures only need to state the values they exercise. */
function withDefaults(project: ProjectJSON): ProjectJSON {
  const fill = (defs: Record<string, number>, p: Record<string, number> | undefined) => ({ ...defs, ...(p ?? {}) })
  return {
    ...project,
    tracks: project.tracks.map(t => ({
      ...t,
      inst: { ...t.inst, params: fill(defaultsFor(instSchema(t.inst.type).params), t.inst.params) },
      fx: t.fx.map(f => ({ ...f, params: fill(defaultsFor(fxSchema(f.type).params), f.params) })),
      midifx: t.midifx?.map(m => ({ ...m, params: fill(defaultsFor(midiFxSchema(m.type).params), m.params) })),
    })),
    masterFx: project.masterFx?.map(f => ({ ...f, params: fill(defaultsFor(fxSchema(f.type).params), f.params) })),
  }
}

/** Load the fixture's project into the live doc and render its scope to a
 *  44.1kHz stereo WAV — the exact bytes committed under fixtures/golden/. */
export async function renderFixtureWav(fixture: Fixture): Promise<Blob> {
  await engine.ensureStarted()
  loadProject(withDefaults(fixture.project), `Load fixture ${fixture.name}`)
  // let sample-library restores / worklet registration settle before rendering
  await new Promise(r => setTimeout(r, 250))
  // loadProject regenerates ALL ids — remap a scene scope's sceneId (the
  // fixture-file id) to the freshly allocated one via the scene's position.
  let scope = fixture.scope
  if (scope.kind === 'scene') {
    const idx = fixture.project.scenes.findIndex(s => (s.id ?? s.name) === (scope as { sceneId: string }).sceneId)
    if (idx < 0) throw new Error(`Scene "${scope.sceneId}" not in fixture`)
    scope = { kind: 'scene', sceneId: scenes.get(idx).get('id') as string }
  }
  const buf = await renderMixdown(scope)
  return encodeWav(await resample(buf), 'stereo')
}

/** Discover fixtures checked into fixtures/projects/ (eager: they're tiny). */
export function allFixtures(): Fixture[] {
  const mods = import.meta.glob('../../fixtures/projects/*.json', { eager: true }) as Record<string, { default: Fixture }>
  return Object.entries(mods)
    .map(([path, m]) => ({ ...(m.default as Fixture), name: m.default.name || path.replace(/^.*\/|\.json$/g, '') }))
    .sort((a, b) => a.name.localeCompare(b.name))
}

/** Automation hook: render by fixture name or a literal fixture object, and
 *  get the WAV bytes back (ArrayBuffer — easy to hash or save from a driver). */
export function installFixtureHook() {
  ;(window as any).__renderFixture = async (f: string | Fixture): Promise<ArrayBuffer> => {
    const fixture = typeof f === 'string'
      ? allFixtures().find(x => x.name === f)
      : f
    if (!fixture) throw new Error(`No fixture named "${f}"`)
    return (await renderFixtureWav(fixture)).arrayBuffer()
  }
}
