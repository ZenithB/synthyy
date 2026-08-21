# Golden fixtures — the parity backbone

See `docs/RUST_REWRITE_PLAN.md` §8/§8a. Each fixture in `projects/` is one JSON
file: `{ name, scope, project }` — a project exercising one device or feature,
plus the render scope. The **browser** engine renders each fixture to WAV; those
WAVs are the golden files the Rust engine (`sf-render`) null-tests against in
`cargo test`.

## Regenerating goldens

1. `npm run dev`, open `http://localhost:5173/#fixtures` in a **visible** tab
   (hidden tabs throttle timers and renders crawl).
2. Click **Render all** — each fixture downloads as `<name>.wav`.
3. Move the WAVs into `fixtures/golden/` and commit them together with the
   fixture change that motivated the regen.

Automation: the dev build exposes `window.__renderFixture(nameOrFixture)` →
`Promise<ArrayBuffer>` (WAV bytes). A real click on the page is still required
once before the first render (AudioContext gesture rule).

## Rules

- A fixture lands **with** its device port and its per-device tolerance
  (tight for worklet ports and samplers; spectral/decay-based for reverbs and
  modulated effects — stated in the Rust test, not here).
- Golden WAVs are 44.1kHz stereo (the export path's `resample`/`encodeWav`).
- If a device sounds wrong in the browser, fix it there first — both engines
  and the goldens move together (CLAUDE.md: browser engine is reference).
- Fixtures use synthesized instruments and procedural samples only — never
  bundle external audio.
