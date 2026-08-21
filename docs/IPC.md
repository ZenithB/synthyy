# Desktop IPC surface (v1)

Concrete wire reference for RUST_ARCH.md §IPC. Server: `src-tauri/src/lib.rs`;
client: `src/audio/engine-native.ts`. All calls are Tauri `invoke`; JS args are
camelCase, response fields snake_case.

| Command | Args | Returns |
|---|---|---|
| `engine_start` | `device?`, `bufferFrames?` | `{ device, sample_rate, buffer_frames }` — opens the cpal stream hosting a fresh `sf_engine::Engine` at the device rate (FTZ/DAZ set on the audio thread). Restart re-pushes the last project. |
| `project_load` | `project` (ProjectJSON) | `{ tracks: [id…], gr: [{track_id, fx_id}…] }` — full-doc push: `with_defaults` → `build_graph` off-thread → ring handover. `tracks` is engine/meter order; `gr` is the flat GR-meter order (`track_id` is `"master"` for master fx). |
| `param_set` | `scope` ("track"\|"master"), `trackId`, `deviceId` ("inst"\|fxId\|"mixer"\|"send"), `key`, `value` | `bool` — resolved → `Cmd::SetParam`; unresolvable = `false`, never an error (client re-pushes). `mixer` keys: gain/pan/sendA/sendB (master: gain). `send`: key = bus id. |
| `transport_play` | `mode` ("session"\|"arr"), `fromTicks` | — |
| `transport_stop` | | — (session clips stay armed in the schedulers) |
| `clip_launch` | `trackId`, `sceneId` | — auto-starts the transport (session, from 0) when stopped, so a scene's batch lands together |
| `clip_stop` | `trackId` | — |
| `note_preview` | `trackId`, `pitch`, `vel`, `on` | — |
| `metronome` | `on` | — |
| `set_tempo` | `bpm` | — (also the bpm fast path for doc edits) |
| `engine_snapshot` | | `{ playing, mode, playhead_ticks, master: {peak, rms}, tracks: [{id, peak, rms}…], gr: [db…], xruns, callbacks, over_budget, peak_load, max_callback_us, budget_us }` — atomics only; polled ~30 Hz |
| `export_mixdown` | `scope` (RenderScope JSON: `{kind: "scene", sceneId}` \| `{kind:"arr"}` \| `{kind:"loop"}`), `outPath?` | WAV path — sf-render on the LAST loaded project, 44.1 kHz 16-bit, blocking task |
| `audio_devices` / `audio_start_tone` / `audio_stop` / `audio_stats` | | Phase 0 surface, unchanged |

Client sync model (engine-native.ts): pure param edits → `param_set`; bpm →
`set_tempo`; everything else → debounced (150 ms) full `project_load` re-push,
resuming transport/clips at the current position afterwards.

Smoke paths: `SF_SMOKE=1` on the desktop binary (loads
`src-tauri/assets/demo-project.json`, scene 0, 3 s, prints `SMOKE ok: …`,
exits), and `cargo run -p sf-io --example play-project -- <path.json> [secs]`.
Regenerate the demo asset with `node scripts/dump-demo-project.mjs`.

Not yet over IPC: `sample_put` (audio clips/samplers are silent on desktop),
LFO/macro/automation modulation, master spectrum/waveform taps, per-band GR.
