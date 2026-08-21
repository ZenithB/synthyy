# Rust engine architecture contract

Binding contract for every crate/agent working on the desktop engine. Read
`docs/RUST_REWRITE_PLAN.md` and `CLAUDE.md` first. When this doc and code
disagree, fix the code.

## Block model

- Audio is processed in **planar stereo blocks**: `(&mut [f32], &mut [f32])`,
  length ≤ `MAX_BLOCK = 128` frames. The callback splits its buffer into
  ≤ MAX_BLOCK chunks and additionally splits at scheduled-event boundaries, so
  note timing is sample-accurate without per-sample event checks.
- Sample rate is fixed at `prepare()` time; a rate change rebuilds the graph.
- Denormals: FTZ/DAZ is set once per audio thread (sf-io); devices may assume it.

## Device traits (sf-dsp)

One file per device: `crates/sf-dsp/src/inst/<type>.rs`, `crates/sf-dsp/src/fx/<type>.rs`.
The registry (`inst/mod.rs`, `fx/mod.rs`) is pre-wired with all type strings —
**replace your stub file only; never edit shared files** (mod.rs, registry, Cargo.toml).

```rust
pub trait EffectDsp: Send {
    fn prepare(&mut self, sample_rate: f32);
    /// Param keys are the FROZEN schema keys (src/audio/schema.ts). Values are
    /// raw (unnormalized) spec-range values. Called at block boundaries only.
    fn set_param(&mut self, key: &str, value: f32);
    fn process(&mut self, l: &mut [f32], r: &mut [f32]);
    /// Tempo-synced devices (duck, autofilt, trem…): transport position in ticks.
    fn tick(&mut self, _pos_ticks: f64, _playing: bool, _bpm: f64) {}
    /// Sidechain trigger (duck): a source note fired.
    fn trigger(&mut self) {}
    /// Gain reduction in dB (≤ 0) for GR meters; per-band variant for mbcomp.
    fn gr_db(&self) -> f32 { 0.0 }
    fn gr_bands(&self) -> [f32; 3] { [0.0; 3] }
    fn reset(&mut self) {}
}

pub trait InstrumentDsp: Send {
    fn prepare(&mut self, sample_rate: f32);
    fn set_param(&mut self, key: &str, value: f32);
    fn note_on(&mut self, pitch: u8, vel: f32);
    fn note_off(&mut self, pitch: u8);
    /// Fixed-length trigger (scheduler path): duration in seconds.
    fn trigger(&mut self, pitch: u8, dur_sec: f32, vel: f32);
    /// ADDITIVE render: mix into l/r (the track buffer may carry other voices).
    fn process(&mut self, l: &mut [f32], r: &mut [f32]);
    /// Load/clear a sample (control path). Slot 0 = main sample; drum pads use
    /// slots 0–7. Defaulted no-op for non-sampled instruments.
    fn set_sample(&mut self, _slot: u32, _buf: Option<std::sync::Arc<SampleBuf>>) {}
    fn reset(&mut self);
}
```

Rules for every device:
- **No allocation in `process()`/`note_on()`/`trigger()`** — pre-allocate in
  `new()`/`prepare()`. No locks, no syscalls, no panics (`debug_assert!` only).
- Every audible param goes through a **one-pole smoother** (~15 ms) unless the
  param is inherently stepped. `sf_dsp::util::Smoother` is provided.
- Polyphonic instruments use a **fixed voice pool** (max 12 unless the browser
  device implies otherwise) with oldest-voice stealing.
- Use `sf_dsp::util` building blocks (PolyBLEP osc, SVF, ADSR, DelayLine, LFO,
  OnePole) rather than re-deriving them.
- Nonlinear devices (dist, crush, cheby) oversample 4× via `util::Oversampler`.

## Source of truth per device

| Kind | Port from |
|---|---|
| `mbcomp` | `public/sf-mbdyn.js` (line-for-line; LR4 crossover + 3-band comp/expander + GR) |
| `shift`, `autotune` | `public/sf-fx.js` (worklet code is canonical) |
| all other fx | Tone composite described in `src/audio/devices.ts` `makeEffect()` — port the *behavior*; param mapping from `src/audio/schema.ts` |
| synths | `src/audio/devices.ts` `make<Type>()` graphs as spec (envelope/filter/osc topology + param mapping) |
| `drum` | procedural pad definitions in `src/audio/devices.ts` (synthesized, stays synthesized) |
| MIDI fx | `src/audio/midifx.ts` (tick-domain, pure note-list transforms — port exactly) |

## Parity gates (this pass)

Hard gates (CI-blocking, every device):
1. `cargo test -p sf-dsp` unit tests in the device file: non-silence on input,
   no NaN/inf output, param sweeps don't blow up, and at least one
   device-specific analytic check (filter attenuates in the stop band, comp
   reduces gain above threshold, delay echoes at the set time, osc pitch is
   correct via zero-crossing count, etc.).
2. `cargo clippy --workspace --all-targets -- -D warnings`.
3. RT discipline: no alloc in process (enforced by review + later assert_no_alloc pass).

Advisory (non-blocking, reported): golden-WAV spectral comparison via the
sf-render fixture harness (`fixtures/projects/*.json` vs `fixtures/golden/*.wav`).
Exact null tests are expected to FAIL against Tone composites — record the
similarity score; do not chase it this pass. Worklet ports (mbcomp/shift/autotune)
should get close; note deviations in `docs/PARITY.md` (one line per device).

## sf-engine (single-threaded v1)

- Rendering happens on the audio callback thread, one track at a time
  (worker pool is Phase 8). Track chain: instrument → fx (skip bypassed) →
  pan (equal-power) → fader gain → master + A/B/bus send taps. Buses render
  after tracks (topological: track sends → bus fx → bus output target).
  Master: sum → master fx chain → limiter (−1 dBFS lookahead) → out.
- Transport: `f64` sample position → ticks (`pos_ticks = samples / sr * bpm / 60 * 96`).
  Loop regions in ticks. Swing (`swingSubdivision`) and humanize applied when
  events are scheduled, mirroring `render.ts` semantics.
- Event scheduler: session mode = per-track looping clip (anchor + loop len in
  ticks); arrangement mode = bounded clips on the timeline. Events are
  pre-expanded per loop pass through the track's MIDI-fx chain (tick domain).
- Command lane: `rtrb` SPSC ring, fixed-size `Cmd` enum, **numeric addressing**
  (`track_idx: u16, slot: u8, param_idx: u16`) resolved from strings off-thread.
  No `String` crosses into the callback (dropping one there = dealloc).
- Graph swaps: new graph built off-thread, swapped in via `triple_buffer` or
  atomic pointer; old graph dropped off-thread (`basedrop`).
- Meters: per-track peak/RMS + master, written as `AtomicU32` (f32 bits) into a
  shared `MeterBank`; UI polls. Playhead ticks likewise (AtomicU64 bits of f64).

## sf-project

- Serde structs mirroring `ProjectJSON` in `src/state/doc.ts` **exactly**
  (field names, optionality). The schema is FROZEN — no "improvements".
- Param registry: `params_for(kind, type) -> &[ParamSpec]` (key, min, max, def,
  curve) generated from `src/audio/schema.ts`; a test validates the registry
  against `fixtures/schema-dump.json` (regenerated by `node scripts/dump-schema.mjs`).

## sf-render

- Offline driver: same graph, pulled in MAX_BLOCK chunks; scope semantics match
  `render.ts` (`scene` = longest clip ×2 from 0; `arr` = to arrangement end;
  `loop` = loop region). Output: 44.1 kHz stereo via `rubato` resample when the
  engine rate differs, WAV via `hound` (16-bit PCM to match `encodeWav`).
- Fixture harness: for each `fixtures/projects/*.json`, render mixdown and (if a
  golden exists) report per-fixture RMS-null and spectral similarity to
  `target/parity-report.json`. Hard-fail only on: render panics, NaN, or all-zero
  output when the golden is non-silent.

## IPC (Phase-1 protocol, v1)

- `project_load(json: ProjectJSON)` — full doc push; rebuilds the graph.
  Structural edits re-push the full doc (cheap enough; diffs later).
- `param_set(path: {track|master, id/slot, key}, value)` — fast path, no rebuild.
- `sample_put(id, sample_rate, l: base64 f32le, r?: …)` — webview-decoded PCM
  into the native sample store.
- Transport: `transport_play(mode, from_ticks)`, `transport_stop`, `clip_launch/stop`,
  `note_preview(track, pitch, vel, on)`.
- Polling: `engine_snapshot()` → { playhead_ticks, playing, meters: […], master, xruns, load }.
- All commands are Tauri invoke; the webview side lives in `src/audio/engine-native.ts`
  implementing `EngineApi` (see `src/audio/engine-api.ts`).

## Workspace hygiene

- Never edit another device's file, shared mod.rs/registry files, or Cargo.toml
  (deps you need are already declared; ask-by-failing is not allowed — if a dep
  is missing, note it in your report instead).
- `cargo test -p <your crate> && cargo clippy -p <crate> --all-targets -- -D warnings`
  must pass before you finish. cargo is at
  `$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin/cargo`.
- Comments explain constraints, not narration. Match existing style.
