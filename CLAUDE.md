# Synthyy

**Standalone desktop DAW — a FORK of Synthtagram** (upstream: github.com/ZenithB/synthtagram, the browser app). This repo is its own project: the Tauri 2 shell + Rust audio engine (`crates/`, `src-tauri/`) plus the forked React UI. **Never push to `upstream`** and never assume changes here reach the browser app — the Synthtagram browser repo at `~/Documents/synthFriends` must stay untouched.

The in-repo web build (`npm run dev`/`build`) still exists because the Tauri webview serves it and the browser engine (`engine-tone.ts`) is the parity reference for the Rust engine; it is NOT deployed anywhere from this repo.

## Commands

- `npm run dev` — web dev server on :5173 (launch config: `.claude/launch.json`)
- `npm run build` — `tsc --noEmit && vite build`; must pass before any commit
- Desktop (once `crates/` exists): `cargo test && cargo clippy -- -D warnings`

No auto-deploys from this repo. **Never break the web build** — the Tauri webview depends on it.

## Architecture map

- `src/audio/engine.ts` — live Tone.js engine (graph build, transport, mute/solo, sends, master fx, modulation, freeze)
- `src/audio/render.ts` — offline export (mixdown/stems). **Render-path parity is law**: every playback feature must land here too, same behavior as live
- `src/audio/schema.ts` — device/param specs (11 instruments, 23 effects, 5 MIDI FX). Param keys are the automation/modulation address space
- `src/state/doc.ts` — Yjs document: tracks, clips, scenes, arrangement, buses, masterFx/masterAuto, undo scope
- `public/sf-fx.js`, `public/sf-mbdyn.js` — AudioWorklets. **These are the canonical DSP**; Tone composites are fallback/spec. Do not reintroduce Tone PitchShift's tuning fudge
- Timing: tick-based, PPQ = 96, 4/4 (`src/types.ts`)

## Hard rules

- **The project schema is frozen.** Yjs doc shape, device type strings, param keys, tick units — changing any of these breaks saved projects and web↔desktop compatibility. Additive changes only, with migration.
- **The browser engine is reference behavior** during the Rust migration. Port faithfully, then improve. If something sounds wrong, fix it in the browser engine first so both engines and the golden fixtures move together.
- **Dig for root cause.** Read library source (Tone.js is in node_modules) and audit recent commits for self-inflicted regressions before proposing workarounds.
- Sample packs are **synthesized procedurally** — never bundle or redistribute copyrighted/royalty-free audio samples.
- Commit and push only when asked. Scheduling errors to know: Tone `Timeline.add` throws "must be greater than or equal" (GTE); `Source.start` throws "strictly greater" (GT) — both are fixed by bounded parts + try/catch around `inst.trigger` (see git history around 120baa4); don't unwind those guards.

## Verifying audio changes

You can't listen. Verify with:
- Offline renders: fastest error-surfacing is a **mixdown** render (stems = 6× renders + throttled waits)
- The preview tab is usually **hidden**: rAF paused, timers throttled — renders crawl and polling misses. Front the tab, or use long poll intervals
- `AudioContext` needs a real user gesture — synthetic pointer events don't count; click a real element (e.g. `.session-scroll`)
- `window.__engine` is exposed; the Yjs doc is not — drive things through the UI or app code paths
- The preview console buffer persists across reloads — capture errors fresh, don't chase stale ones
- Desktop (once live): golden-fixture null tests in `cargo test` (`docs/RUST_REWRITE_PLAN.md` §8)
