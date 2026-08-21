# Running Synthtagram Desktop

The desktop app is the existing React UI in a Tauri 2 shell with playback served
by the native Rust engine (CoreAudio; sub-10 ms buffers). The browser app is
unchanged and keeps deploying from the same repo.

## Prereqs (already set up on this machine)

- Rust stable via rustup (`~/.rustup/...`; brew's rustup shim is on PATH as `rustup`)
- Node deps: `npm install` (includes the Tauri CLI)

## Run it

```bash
npm run tauri dev
```

First launch compiles the Rust workspace (~a minute), then opens the app window.
Click anywhere / press Play — the native engine starts on the default output
device. Audio Settings (top bar) has the desktop-only section: output-device
picker, native buffer size (64–512), test tone, and live xrun / deadline-miss /
load readouts. PerfMonitor shows real callback numbers.

What runs natively: all 11 instruments (samplers stream the webview's decoded
samples over IPC), all 23 effects, MIDI fx, clip/scene launching, arrangement
playback, automation + LFOs + macros, sends/buses (incl. the Feedback bus),
master fx + limiter, per-fx GR meters. `export_mixdown` renders offline through
the same graph, faster than real time.

Known no-ops on desktop (documented in `src/audio/engine-native.ts`): browser
audition previews, track freeze (unnecessary — native headroom), spectrum/
waveform/tuner analysers.

## Packaging / release

```bash
scripts/release-macos.sh --unsigned   # local unsigned .app + .dmg (universal)
scripts/release-macos.sh              # signed + notarized; needs APPLE_* env vars
```

Signing, notarization, CI (`.github/workflows/desktop-release.yml`) and the
Windows leg are documented in `docs/RELEASE.md`.

## Self-tests

```bash
# audio E2E without the GUI: demo project through real hardware, 4 s
cargo run -p sf-io --example play-project -- src-tauri/assets/demo-project.json 4
```

```bash
# full engine test suite + parity harness (37 golden fixtures)
cargo test --workspace
```

```bash
# the Tauri binary's built-in smoke (starts engine, loads demo, plays 3 s)
SF_SMOKE=1 cargo run -p synthyy
```

```bash
# xrun soak: N minutes at 64 frames under 4 busy-loop threads (master at -40 dB)
cargo run --release -p sf-io --example soak -- src-tauri/assets/demo-project.json 30 64 --load 4
```

```bash
# input recording probe: default mic for 2 s, prints level + input xruns
cargo run -p sf-io --example record-probe
```

Recording on desktop: arm a track's input (Track Input panel) and record — takes
come from the native input stream, latency-compensated. Audio Settings → native
section has the input picker and **Measure latency** (needs a loopback path:
mic picking up the speakers, or a cable). "Record Output" captures the
post-limiter master natively.

Parity status per device: `docs/PARITY.md`. Architecture contract:
`docs/RUST_ARCH.md`. Plan + phase status: `docs/RUST_REWRITE_PLAN.md`.
