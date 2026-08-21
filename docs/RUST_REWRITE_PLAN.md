# Synthtagram Desktop — Rust Rewrite Plan

Standalone Mac + Windows app. Rust audio engine with direct hardware access (CoreAudio / WASAPI / ASIO), existing React UI preserved inside a Tauri shell. The browser version stays alive as the reference implementation until parity is proven.

---

## Status

**Phases 0–6 (core) — done on macOS (2026-08-21), built via multi-agent orchestration.** The desktop app plays projects through the native Rust engine end-to-end: `npm run tauri dev` (see `docs/DESKTOP.md`).

- **Engine**: full track/bus/master graph (sends incl. Feedback loop, mute/solo, master fx → Tone.Limiter-matched limiter), tick transport (session + arrangement, loop region), sample-accurate scheduler with swing/humanize, MIDI fx (scale/chord/arp/velo/rand), block-rate modulation engine (clip envelopes, track/master automation, LFOs, macros), rtrb command lane + basedrop graph swaps, atomic meter/GR/playhead snapshots. RT rules hold by construction (assert_no_alloc enforcement still pending).
- **Devices**: all 11 instruments + 23 effects ported (`crates/sf-dsp`, one file each, 233 unit tests). Samplers/granular/audio clips stream webview-decoded PCM over IPC into a native SampleBank. comp is an operation-for-operation port of Chrome's DynamicsCompressor kernel; mbcomp/shift/autotune port the canonical worklets.
- **Parity**: 37 golden fixtures render with zero hard failures; spectral similarity 0.84–0.99; 29/37 within ±2 dB loudness after the calibration pass (`docs/PARITY.md`). Two goldens are known-bad from **browser offline-render bugs found during calibration** (offline export silently skips mbcomp and never ticks tempo-mode duck — flagged as a follow-up fix in the web app).
- **Desktop integration**: engine hosted in the cpal callback (FTZ/DAZ set), Tauri IPC (project push, param fast path, transport/clips/preview, 30 Hz snapshots, sample transfer, native offline export), `engine-native.ts` implements the EngineApi facade, selector serves it under Tauri. E2E smokes: `cargo run -p sf-io --example play-project` and `SF_SMOKE=1` both pass on hardware (demo project, 0 xruns, ~23% callback load at default buffers).
- **Verification**: 287 Rust tests + clippy `-D warnings` + `npm run build` all green; browser build untouched behavior-wise.

**Follow-up pass (2026-08-21)**: dest `lfo`/`midi` modulation routes ported (MIDI-fx re-expansion runs off-thread via `EngineControl::service()`, called from the IPC snapshot poll); `assert_no_alloc` enforcement landed — every fixture, param sweep, voice-steal storm, command burst, graph swap and pattern swap proven alloc-free in `cargo test` (zero violations found); native **input recording** (frame-exact takes, post-limiter master capture, loopback latency calibration, desktop track-record + Record Output flows); macOS **packaging/signing/notarization** wired (hardened runtime, entitlements, Info.plist, universal build, `scripts/release-macos.sh`, CI workflow, `docs/RELEASE.md`) — unsigned universal `.app`/`.dmg` build verified; only the Developer ID cert + notary credentials are missing. **30-min xrun soak PASSED** (64-frame buffer, 4 busy-loop CPU threads, release build): 1,240,386 callbacks, 0 xruns, 0 deadline misses, peak load 17% (max callback 249 µs of a 1451 µs budget).

**Remaining**: Windows leg (WASAPI test on real hardware; CI job is wired), input monitoring, collab-while-native soak, per-device CPU benchmark table, browser offline-render bug fixes (task chip filed).

**Phase 0 — done on macOS (2026-08-20).** Windows leg pending (needs a Windows machine).

- Engine facade seam: `src/audio/engine-api.ts` (EngineApi) + `engine.ts` selector + `engine-tone.ts` (Tone impl). The selector export is typed `EngineApi`, so `npm run build` is the completeness check. `recorder.ts` binds to the concrete Tone engine (captureTap is web-only).
- Golden-fixture harness: dev-only `/#fixtures` page + `window.__renderFixture(name)` hook + `renderMixdown()` in render.ts. `fixtures/projects/smoke-mono.json` renders **deterministically** (identical SHA-256 across runs). Gotchas encoded in `src/audio/fixtures.ts`: loadProject regenerates all ids (scene scopes remap by index) and device constructors need full param maps (schema defaults filled in).
- Rust workspace per §3 (`crates/sf-{dsp,engine,io,project,render}` + `src-tauri`), `cargo test && cargo clippy --workspace --all-targets -- -D warnings` green.
- `sf-io` (cpal/CoreAudio): device enum, manager thread owning the !Send stream, RT-clean callback with atomics-only stats. **Exit test passed**: `cargo run -p sf-io --example tone 64` → true 64-frame buffer @ 44.1kHz (1.45ms), 0 xruns, 0 deadline misses, 0.1% peak load.
- Tauri 2 shell (`npm run tauri dev`): React app loads unmodified in the webview; AudioSettings gains a desktop-only Native section (device picker, 64–512 buffer, test tone, live xrun/deadline-miss/load readout) via `audio_*` IPC commands.

Next: **Phase 1** — sf-engine track/bus/master graph, ProjectMirror + diff protocol, transport, meter snapshots.

---

## 1. Goals and non-negotiables

**Goals**

- Sub-10 ms round-trip latency at 64–128 sample buffers on ordinary hardware.
- No GC pauses, no allocation, no locks on the audio thread — ever.
- Use every core: pinned real-time callback + a worker pool for voice/track rendering.
- SIMD DSP (explicit where it pays, auto-vectorized elsewhere).

**Non-negotiables (carried over from how we build today)**

1. **Project-file compatibility.** The Yjs document schema and exported project JSON are the contract. A project made in the browser opens in the desktop app and vice versa. Param keys, tick units (PPQ = 96), device type strings — all unchanged.
2. **Render-path parity.** One graph implementation serves both the real-time callback and offline export. There is no separate "render engine" to drift out of sync — the offline path is the same graph pulled faster than real time.
3. **Worklets are canonical DSP.** `public/sf-fx.js` and `public/sf-mbdyn.js` are hand-written sample-loop code — they port to Rust nearly line-for-line and define correct behavior. Tone.js composites are the *spec* for the remaining devices, not the implementation. Do not reintroduce Tone's PitchShift tuning fudge.
4. **The browser build keeps working** from the same repo throughout the migration. Every phase ends with both targets green.

---

## 2. Architecture

```
┌─ Tauri app ──────────────────────────────────────────────────┐
│                                                              │
│  WebView (React UI — existing code, mostly unchanged)        │
│  ├─ Yjs doc + Trystero P2P collab   (stays in JS for v1)     │
│  ├─ engine facade: same API surface as src/audio/engine.ts,  │
│  │   but forwards to Rust instead of Tone.js                 │
│  └─ meters/GR/playhead drawn from engine snapshots           │
│                     ▲                    │                   │
│        events + shared meter buffer      │ commands (IPC)    │
│                     │                    ▼                   │
│  Rust core (in-process)                                      │
│  ├─ ProjectMirror: plain-Rust copy of the doc, updated by    │
│  │   diffs from the webview; owns the graph rebuild logic    │
│  ├─ Command lane: rtrb SPSC ring → audio thread              │
│  │   (param changes, transport, note on/off; coalesced)      │
│  ├─ AUDIO THREAD (pinned, RT priority)                       │
│  │   transport clock → event scheduler → track graph         │
│  │   [inst → fx chain → send taps → track fader]             │
│  │   → buses (A/B/custom) → master fx → limiter → out        │
│  ├─ Worker pool: renders independent tracks in parallel,     │
│  │   joined before the bus mixdown                           │
│  └─ Services (non-RT threads): sample decode (symphonia),    │
│      offline render, disk I/O, recording writer              │
└──────────────────────────────────────────────────────────────┘
```

**Key decision: the engine consumes the same project state the UI does.** The webview pushes state diffs (track/device/param changes) into Rust over IPC; Rust maintains a mirror and does what `engine.ts` does today — build/rebuild the graph reactively. The UI never talks to the audio thread directly; everything crosses one command ring.

**Two lanes of communication:**

- **Control lane (Tauri commands/events, ~UI rate):** graph edits, clip launches, transport, sample loads, export requests.
- **Real-time lane:** a lock-free SPSC ring (`rtrb`) into the audio thread for time-critical messages (live note input, param nudges), and a triple-buffered shared snapshot out of it (playhead ticks, per-track peak/RMS, per-device GR) that the UI polls at rAF rate. No per-frame IPC serialization for meters.

---

## 3. Repo layout

Monorepo — the web app stays where it is; Rust lands beside it.

```
synthFriends/
├─ src/                      # React UI + Yjs (existing; UI shared by both targets)
│  └─ audio/engine-native.ts # new: engine facade that speaks to Rust
├─ src-tauri/                # Tauri shell (config, window, menu, updater)
├─ crates/
│  ├─ sf-engine/             # graph, transport, scheduler, mixer (no I/O deps)
│  ├─ sf-dsp/                # all device DSP: pure `process(&mut [f32]...)` — no_std-ish, allocation-free
│  ├─ sf-io/                 # cpal glue: device enum, stream config, RT callback host
│  ├─ sf-render/             # offline render driver (same graph, block-pulled), WAV/OGG encode
│  └─ sf-project/            # serde types for project JSON, param registry, tick math
└─ docs/RUST_REWRITE_PLAN.md
```

`sf-dsp` depends on nothing but `core`/`libm` — every device is a struct with `prepare(sample_rate)`, `process(block)`, `set_param(key, value)`. That's what makes the parity harness and per-device benchmarks trivial.

---

## 4. Real-time engine rules (written down now, enforced forever)

- Audio callback: **no alloc, no dealloc, no locks, no syscalls, no `format!`.** CI runs the graph under `assert_no_alloc` in tests.
- All buffers, voices, and event queues pre-allocated at graph build time. Graph swaps are prepared on a worker thread and exchanged via atomic pointer swap; the old graph is dropped on a collector thread (`basedrop`), never in the callback.
- Voice pools per instrument (fixed max polyphony, voice stealing) — mirrors what Tone does implicitly, but bounded and measured.
- Every user-facing param goes through a one-pole smoother (~10–20 ms) in the device, so automation and knob drags are zipper-free by construction — this replaces Tone's `rampTo` scattered through `engine.ts`.
- Transport is sample-accurate and tick-based: `f64` sample position → ticks via tempo map. PPQ 96, 4/4, swing and humanize applied at schedule time exactly as the export path does today.
- Denormal protection (FTZ/DAZ) set on the audio thread and every worker.

---

## 5. Phases

Each phase has an exit test. Nothing ships to `main` without both the web build and the desktop build green.

### Phase 0 — Shell + proof of hardware access
- **Extract the engine facade interface first.** Before any Rust: read `src/audio/engine.ts` (~2,800 lines) and pull its public surface into a TypeScript interface (`src/audio/engine-api.ts`). `engine-tone.ts` (today's code) implements it; `engine-native.ts` (the IPC client) will too. This is the seam every later phase depends on — pin the contract before writing against it.
- **Build the golden-fixture harness early** (see §8a) — it must exist before Phase 2 needs its first golden WAV, and it uses the *browser* engine, so build it while that path is fresh.
- Tauri 2 workspace; existing React app runs unmodified inside it on macOS and Windows.
- `sf-io`: enumerate devices via cpal, open an output stream (CoreAudio / WASAPI), play a test tone; buffer-size and device pickers wired into AudioSettings UI.
- Measure and display actual callback latency + load (this becomes the PerfMonitor data source).
- **Exit:** tone at 64-sample buffer on both OSes; UI unchanged; xrun counter visible.

### Phase 1 — Engine skeleton: transport, mixer, meters
- `sf-engine`: track/bus/master graph with gain, pan, mute/solo, sends (A/B/custom, the current pre-set-bus model), master limiter.
- ProjectMirror + diff protocol from the webview; graph rebuild on structural change, param path for everything else.
- Transport (play/stop/loop, tempo), metronome, playhead + meter snapshots to UI.
- **Exit:** load a real project; track layout, faders, sends and meters all live against silence/test signals; mute/solo/pan behave identically to the browser.

### Phase 2 — Samplers and audio tracks first (least DSP, most plumbing)
- Sample service: decode via symphonia into shared immutable buffers; sample library moves from IndexedDB to app-data files with a one-time migration + import of existing user libraries.
- Devices: `sampler` (drum-pad style), `ksampler` (pitched keyboard), audio clips on `audiobus` tracks (warp-free playback, clip gain/fades as today).
- Event scheduler: session-clip looping and arrangement-clip bounding replicate `buildArrParts`/`scheduleClip` semantics (bounded parts — the exact bug class we fixed in the browser render stays fixed by construction).
- **Exit:** a drums + audio project plays back sample-accurately; A/B against browser by ear and by rendered null test on the drum bus.

### Phase 3 — Synthesizers
Port order (simple → hard): `mono` → `poly` → `duo` → `pluck` → `keys` → `fm` → drum-kit synthesis (the 8 procedural pads) → `granular`.
- Shared building blocks in `sf-dsp`: PolyBLEP/wavetable oscillators, SVF/ladder filters, ADSR, noise, LFO.
- Drum pads port from the current procedural definitions (they're synthesized, not samples — that stays true).
- Granular gets a proper grain scheduler (this is where owning the thread pool pays off).
- **Exit:** every instrument passes the parity harness (§8) within tolerance; polyphony ceiling measured and documented per device.

### Phase 4 — Effects
Port order:
1. **Worklet ports (canonical, near-mechanical):** everything in `sf-fx.js` + `sf-mbdyn.js` — including `mbcomp` (LR4 crossover + per-band comp/expand + GR reporting).
2. **Dynamics:** `comp`, `opto` (LA-2A model), `duck` (sidechain) — all with GR meter taps.
3. **Filters/EQ:** `eq`, `eq7`, `filter`, `autofilt`.
4. **Time/modulation:** `delay`, `pingpong`, `chorus`, `phaser`, `trem`, `autopan`, `vib`, `widen`.
5. **Reverbs:** `reverb`, `plate` (FDN designs; the current Tone convolution/plate sound is the reference to match, not the code).
6. **Nonlinear:** `dist`, `crush`, `cheby` — these self-oversample 4× today; in Rust give them proper polyphase up/down.
7. **Hard DSP last:** `shift` (freq shifter — Hilbert), `autotune` (pitch tracker + shifter; pitch detection via YIN, shifting via PSOLA or phase vocoder — budget real time for this one).
- MIDI FX (`scale`, `chord`, `arp`, `velo`, `rand`) are tick-domain, not audio-domain — port into the scheduler in an afternoon each.
- **Exit:** full device rack parity; GR meters live; per-device CPU benchmark table checked into docs.

### Phase 5 — Automation, modulation, freeze
- Automation lanes (tracks, buses, master) evaluated sample-block-accurately in the engine — better than today's control-rate loop.
- LFO/macro modulation matrix; idle-guard behavior preserved (no work when nothing is modulated).
- Track freeze = engine-local offline render of one track into a buffer, exactly the current architecture.
- **Exit:** automation golden-render tests; frozen tracks null against unfrozen renders.

### Phase 6 — Export and recording
- `sf-render`: mixdown + per-track stems, arrangement/loop/scene scopes, honoring swing subdivision + humanize (parity with `render.ts` today). Faster than real time; progress events to the ExportDialog.
- WAV always; OGG/MP3 via encoder crates for parity with current `encode.ts`.
- Recording: input streams via cpal, ring-buffered to a writer thread, **measured** round-trip latency compensation (loopback calibration beats the browser's estimated compensation).
- MIDI input via `midir`; computer-keyboard piano stays in the webview.
- **Exit:** exports from desktop and browser of the same project null (or match within stated tolerance); recorded takes land on the grid.

### Phase 7 — Cutover and packaging
- Feature flag flips: webview engine facade routes 100% to Rust; Tone.js is no longer loaded in the desktop build.
- Collab stays Yjs + Trystero in the webview (works fine in Tauri). *Optional later:* migrate to `yrs` + native WebRTC only if the JS side becomes the bottleneck — it is not on the critical path.
- macOS: universal binary, hardened runtime, notarization, mic entitlement. Windows: MSI/NSIS via Tauri bundler, code signing. Auto-update via Tauri updater.
- **Exit:** signed installers on both OSes; a browser project round-trips through desktop and back.

### Phase 8 — The payoff pass (after parity, not before)
- Parallel track rendering across the worker pool; benchmark scaling.
- SIMD hot loops (`std::simd` / intrinsics) guided by profiles, not guesses.
- ASIO backend behind a feature flag (needs Steinberg SDK acceptance — see risks).
- Optional: WASAPI exclusive mode toggle, per-device oversampling quality tiers.

---

## 6. Device porting inventory

| Category | Devices | Source of truth |
|---|---|---|
| Instruments (11) | poly, duo, fm, mono, pluck, keys, sampler, ksampler, granular, drum, audiobus | Tone graphs as spec; drum pads from procedural defs |
| Effects — worklet (canonical) | mbcomp + everything in sf-fx.js | Direct JS→Rust port |
| Effects — dynamics | comp, opto, duck | Spec + GR-meter parity |
| Effects — EQ/filter | eq, eq7, filter, autofilt | Biquad/SVF, match curves |
| Effects — time/mod | delay, pingpong, reverb, plate, chorus, phaser, trem, autopan, vib, widen | Match by golden render |
| Effects — nonlinear | dist, crush, cheby | Port + real polyphase oversampling |
| Effects — hard DSP | shift, autotune | Hilbert; YIN + PSOLA/phase-vocoder |
| MIDI FX (5) | scale, chord, arp, velo, rand | Tick-domain, port into scheduler |

---

## 7. Crate choices

| Concern | Crate | Notes |
|---|---|---|
| Audio I/O | `cpal` | CoreAudio + WASAPI now; ASIO feature later |
| RT queues | `rtrb` | SPSC lock-free rings |
| RT-safe drop | `basedrop` | old graphs collected off-thread |
| Decode | `symphonia` | wav/flac/ogg/mp3/aac |
| Encode | `hound` (wav), `vorbis_rs` / `mp3lame-encoder` | export formats |
| Resampling | `rubato` | sample import + ksampler repitch |
| FFT | `realfft` | autotune, analysis |
| MIDI | `midir` | hardware MIDI in |
| Shell | `tauri` v2 | plus tauri-plugin-updater |
| Serialization | `serde`/`serde_json` | project JSON contract |
| Tests | `assert_no_alloc`, `criterion` | RT discipline + benchmarks |

Deliberately **not** picking a framework like fundsp for the core: our devices need exact parity with existing sounds, so we own every sample loop.

---

## 8. Parity harness (how we know it's right)

This is the backbone of the whole migration:

1. A set of fixture projects checked into the repo, each exercising one device or feature (every synth, every fx, automation shapes, swing/humanize, sends, freeze).
2. The **browser** engine renders each fixture to WAV (once, via the existing export path) — these are the golden files.
3. `cargo test` renders the same fixtures through `sf-render` and compares: null-test RMS difference under a per-device threshold (tight for worklet ports and samplers; looser, spectrally-judged for reverbs/chorus where exact-sample match is not the goal).
4. Every device lands with its fixture + threshold. Regressions in either engine show up as a failed diff.

Plus: `assert_no_alloc` graph tests, xrun soak test (30-min playback at 64 samples under CPU load), and criterion benchmarks per device so the perf story is measured, not vibes.

### 8a. Generating the goldens — read this before trying

Browser renders are **not** headless-friendly; these were all learned the hard way:

- The render path (`src/audio/render.ts`) runs in a page, and `AudioContext` needs a user gesture first — a synthetic pointer event does **not** count. In an automated tab, click a real element (e.g. `.session-scroll`) via the browser tooling's click action before rendering.
- A **hidden/backgrounded tab** throttles `setTimeout` and pauses `requestAnimationFrame` — renders that take seconds in a visible tab take minutes hidden, and completion polling misses. Keep the tab fronted, or poll on long intervals.
- The engine is reachable at `window.__engine`; the Yjs doc and render functions are *not* on `window` — drive renders through the app's own export path (or add a small dev-only `window.__renderFixture(projectJson, scope)` hook, which is the recommended move: build it once in Phase 0).
- The preview console buffer persists across reloads — capture errors freshly per render or you'll chase stale ones.
- Concrete recipe: a dev-only fixtures page (`/#fixtures`) that loads each fixture project JSON, renders mixdown WAVs via the existing export path, and offers them as downloads / posts them to a local endpoint. Run it manually in a visible tab once per fixture change; commit the WAVs (or their hashes + a regen script) under `fixtures/golden/`.

---

## 9. Risks and calls made now

- **Autotune + freq shifter are the long poles.** Scheduled last in Phase 4; everything else ships around them. If needed, desktop v1 can ship with these two still webview-rendered — but the goal is full native.
- **ASIO SDK licensing** (Steinberg agreement, can't redistribute SDK) → ASIO is a feature flag built in CI with the SDK fetched under license; WASAPI shared/exclusive is the default Windows story and is already good.
- **Reverb character match** — convolution/plate tails won't null sample-exactly. Call: match by frequency response + decay curve, not null test; state the tolerance in the fixture.
- **WebView differences** (WKWebView vs WebView2) — UI CI runs on both; no Chromium-only APIs in the UI (audit `AudioContext` uses that must move behind the engine facade anyway).
- **Sample library migration** (IndexedDB → files) — one-time importer with verify-before-delete; browser and desktop libraries are separate stores by design.
- **Two engines during migration** — cost is real but bounded by the phase gates; the facade keeps one API so UI code never forks.
- **IPC latency for live playing** — live note-on goes through the RT lane, not Tauri JSON commands; budget < 1 ms webview→callback. Verified in Phase 0 with a latency probe.

---

## 10. Handoff notes (for a fresh session starting this work)

**Environment**
- Web app: Vite + React 19 + TS 6. `npm run dev` (port 5173, launch config in `.claude/launch.json`), `npm run build` = `tsc --noEmit && vite build` — this is the green-check for the web target.
- Desktop: Rust stable + `cargo`, Tauri 2 CLI (`cargo install tauri-cli`). Workspace lives in `crates/` + `src-tauri/` per §3. Green-check: `cargo test && cargo clippy -- -D warnings` plus the web build.
- Deploys: pushes to `main` auto-deploy the web app to synthtagram.vercel.app — the web build must never break, even mid-migration.

**Ground rules**
- Read `CLAUDE.md` at the repo root — it holds the working conventions (schema is frozen, browser engine is reference behavior, parity discipline). This plan is the *what*; CLAUDE.md is the *how we work*.
- Port devices faithfully, then improve. If a device sounds wrong in the browser, fix it there first (both engines), never silently "improve" it in Rust — that breaks the parity harness and project compatibility.
- You cannot verify audio by ear in this harness. Verification is: golden-render null tests (§8), meter/GR snapshot assertions, xrun counters, and `assert_no_alloc`. Build those *before* the DSP they verify.
- Start at Phase 0 and take the exit tests literally. The facade interface extraction and the fixtures hook are the first two deliverables.

## 11. What the UI keeps / loses / gains

- **Keeps:** all views (Session, Arrangement, PianoRoll, DeviceRack, mixer, dialogs), Yjs undo/collab, theory tools, browser/library UX.
- **Changes:** `engine.ts`'s public surface becomes an interface with two implementations — `engine-tone.ts` (web) and `engine-native.ts` (desktop IPC client). Meters/playhead read from the shared snapshot instead of Tone nodes. AudioSettings gains device/buffer/exclusive-mode controls.
- **Gains:** real device selection, small buffers, an honest PerfMonitor (callback load, xruns), faster-than-realtime exports, multicore headroom, and recording latency that's measured instead of guessed.
