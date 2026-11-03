# Synthyy for Windows — Port Plan

Status (2026-11-03): **W0 and W1 implemented, plus the core of W3 (ASIO)
and the MMCSS part of W2** — compiled and unit-tested for Windows and macOS
targets, awaiting the real-hardware checks in `docs/WINDOWS.md` §3. See
section 9 for what changed and what is still open.

Scope decisions (from the owner): keep macOS supported; personal PCs only
(no managed/corporate machines); must work with onboard sound and USB audio
interfaces; include ASIO for low latency; a Windows PC is available for
testing.

## 1. Summary

Synthyy does **not** need to be rebuilt for Windows. Every layer was chosen to be
cross-platform from the start:

| Layer | Tech | Windows status |
|---|---|---|
| UI | React 19 + Vite, served in the Tauri webview | Runs in WebView2 (Chromium) instead of WKWebView. No code changes expected; needs a verification pass (section 6, W4). |
| Shell / IPC | Tauri 2.11 | Cross-platform. The release workflow already has a Windows job (MSI + NSIS). |
| DSP | `crates/sf-dsp` (pure Rust + `libm`, no OS calls) | Portable as-is. |
| Engine / scheduler / render | `sf-engine`, `sf-render`, `sf-project` | Portable as-is. No `cfg(target_os)` anywhere in these crates. |
| Audio I/O | `crates/sf-io` on `cpal` 0.16 | **This is where the Windows work is.** cpal uses WASAPI on Windows, and its WASAPI backend behaves differently from CoreAudio in ways the current code does not handle (section 3). |

So the job is: fix the audio I/O layer for WASAPI, add a low-latency driver
option (ASIO), tune real-time behaviour for Windows, verify the UI in WebView2,
and package for Windows. The project schema, device list, param keys and the
whole DSP stack stay untouched, which keeps projects fully interchangeable
between the Mac and Windows builds.

**Recommendation: one codebase, two platforms.** Do not fork a Windows-only
repo. Every fix below is either Windows-only (behind `#[cfg(windows)]`) or
improves both platforms. Forking would double the maintenance of a 25k-line
Rust engine for no benefit.

## 2. What already works for Windows (no change needed)

- **Denormal protection.** `sf-io/src/lib.rs` `set_flush_denormals()` already
  sets FTZ+DAZ in MXCSR on x86_64. This matters more on Windows than on the
  Mac: Intel/AMD CPUs slow down dramatically (often 10–100x per operation) when
  reverb/filter tails decay into denormal numbers, while Apple Silicon barely
  notices. It is set once from inside the audio callback, which is correct.
- **Allocation-free callback.** Proven by `assert_no_alloc` tests. The callback
  also handles variable callback lengths (chunked through `SCRATCH_FRAMES`),
  which WASAPI produces (see 3.3).
- **Recording clock.** cpal's WASAPI backend timestamps both input and output
  from the same QPC clock, so the latency-compensation maths in `capture.rs`
  is valid on Windows.
- **Golden fixtures are tolerance-based** (RMS null + spectral similarity), and
  DSP math is mostly `libm` (229 call sites vs 22 `std` float calls), so tiny
  CRT math differences between macOS and MSVC will not fail `cargo test`.
- **Keyboard shortcuts** already accept `ctrlKey` alongside `metaKey`
  (`App.tsx`, `PianoRoll.tsx`, `ArrangementView.tsx`).
- `main.rs` already sets `windows_subsystem = "windows"` (no console window).

## 3. Windows problems found (confirmed by reading cpal 0.16 source)

These were verified against `cpal-0.16.0/src/host/wasapi/{device,stream}.rs`,
the exact version locked in `Cargo.lock`.

### 3.1 Output and input fail on devices that are not 2-channel — BLOCKER

`open_output()` hard-codes `channels: 2`. cpal's WASAPI backend opens streams in
**shared mode without format auto-conversion**, and its own comment says only the
device's mix-format channel count is accepted. Any device whose Windows mix
format is not stereo will refuse to open:

- Onboard audio configured as 5.1 / 7.1 speakers
- Multi-output interfaces (Focusrite, MOTU, RME, etc.) exposed as 4–18 channels
- **Dante Virtual Soundcard** WDM endpoints (typically 8–64 channels) — very
  likely at MCEC

`resolve_input()` has the same issue: it clamps input to 1–2 channels before
opening, so a 4-channel interface input will fail too (the fold-down code that
follows never gets a chance to run).

Fix: open the stream at the device's native channel count; in the output
callback write the engine's stereo to channels 1–2 (selectable pair later) and
zero the rest; in the input callback keep the existing fold-down from `dev_ch`
to the kept channels. All pre-allocated, so still RT-safe.

### 3.2 The audio thread is NOT running at real-time priority — BLOCKER for performance

cpal's WASAPI thread, without the optional `audio_thread_priority` feature,
calls `SetThreadPriority(HANDLE(GetCurrentThreadId()), TIME_CRITICAL)`. A thread
**ID** is not a thread **handle**, so the call fails and the error is ignored.
The audio callback therefore runs at normal priority with no MMCSS registration.

Why this matters: on macOS, CoreAudio's IO thread is real-time by default, so
the engine's "0 xruns at 64 frames" result came for free. On Windows, a normal
priority thread competes with the UI, WebView2, antivirus scans and every other
app, so it will miss deadlines under load.

Fix: register the callback thread with MMCSS ("Pro Audio" task,
`AVRT_PRIORITY_HIGH`) once from inside the callback, in the same place FTZ is
set. This uses `AvSetMmThreadCharacteristicsW` from the `windows` crate, which is
already in the dependency tree. Do the same for the input callback thread.
(Enabling cpal's `audio_thread_priority` feature is an alternative, but owning
the call in `sf-io` gives us control of the task name and lets us report
success/failure in the stats.)

### 3.3 The buffer-size picker does nothing on WASAPI, and the app reports the wrong latency

In WASAPI shared mode, the audio engine wakes the app once per **engine
period**, normally 10 ms (480 frames at 48 kHz), regardless of the buffer size
requested. cpal passes the requested size to `IAudioClient::Initialize`, which
silently raises it to the minimum. cpal reports the supported range as
`0..u32::MAX`, so `open_output()` always takes the `Fixed` branch and
`StreamInfo.buffer_frames` reports e.g. 64 — which is not what is happening.

Consequences:
- The 64–512 picker in Audio Settings is cosmetic on Windows.
- `estimate_ms_locked()` (the recording-latency fallback) underestimates.
- Expected real-world output latency through cpal WASAPI shared: roughly
  20 ms. That is fine for playback and mixing, not for playing a soft synth
  live from a MIDI keyboard or monitoring a mic. See W3 for the fix (ASIO).

Fix (honesty first): record the actual callback frame counts (min/max) in
`Stats` and report those as the achieved buffer; label the WASAPI buffer
control as "managed by Windows" when the driver ignores it.

### 3.4 Unplugging or switching the audio device silently kills the engine

When a WASAPI device is removed, its format changes, or Windows switches it
(Bluetooth headsets do this when a mic is opened — A2DP to hands-free mode),
cpal reports `DeviceNotAvailable` through the error callback and its stream
thread **exits**. `sf-io` only counts this as an xrun. Result: playback goes
silent, the UI still shows "playing", no message.

Also, when the user picks "default device", the stream does not follow a change
of the Windows default device.

Fix: distinguish "device lost" from "xrun" in the error callback (atomic flag);
the manager thread restarts the stream (same device if it returns, otherwise
default), re-pushes the project via the existing `repush_stored()` path in
`src-tauri/src/lib.rs`, and the UI shows a notice. Add default-device following
via a small poll on the manager thread (simpler than COM
`IMMNotificationClient`, adequate at 1–2 s granularity).

### 3.5 Sample format assumption

The callbacks are typed `&mut [f32]`. Shared-mode mix formats are float32 on
almost every Windows device, but cpal will refuse the stream if a driver reports
16/24-bit integer. Fix: check `default_output_config().sample_format()` and use
cpal's sample conversion for non-f32 devices. (Low probability, cheap to fix.)

### 3.6 Windows microphone privacy switch

If Settings > Privacy & security > Microphone > "Let desktop apps access your
microphone" is off, the input stream opens but delivers silence. Fix: if the
input peak stays at exactly 0.0 for ~2 s after arming, show "Check Windows
microphone privacy settings".

### 3.7 Smaller items

- `docs/DESKTOP.md` says "CoreAudio" only; `RELEASE.md` says the WASAPI leg is
  untested on real hardware. Both need updating when this lands.
- Release names still say "Synthtagram" (`desktop-release.yml`, artifact names).
- `Topbar.tsx` shows a hard-coded "⌘K" label; on Windows it should read Ctrl+K.
- The Windows CI job builds installers but runs **no tests**.
- `webviewInstallMode: downloadBootstrapper` needs internet during install;
  this fails on locked-down or offline machines.

## 4. Real-time processing on Windows — what "efficient" means here

The audio callback must finish each buffer before its deadline or you hear a
click. On Windows the main threats are, in order:

1. **Thread priority** (3.2) — the single biggest win. Must be fixed first.
2. **Denormals** — already handled.
3. **CPU power management.** Windows 11 "Efficiency mode" (EcoQoS) and
   laptop power plans can park cores and drop clocks when the window is in the
   background, which raises callback time unpredictably. Fix: opt the process
   out of power throttling (`SetProcessInformation` with
   `ProcessPowerThrottling`, Windows-only, a few lines in `src-tauri`), and
   document "use the Best performance power mode" for show machines.
4. **Single-threaded engine.** The whole graph renders on the one callback
   thread (the plan's Phase 8 worker pool is not built yet). Apple Silicon has
   very strong single-core performance; a typical corporate Intel laptop has
   less. Decision: **measure before building**. Run the existing `soak` example
   on the actual Windows machines; only start the parallel track renderer if
   peak load exceeds ~70% of budget with realistic projects.
5. **Compiler target.** Ship the default `x86-64` baseline (runs on any 64-bit
   PC). Do not use `target-cpu=native` in release builds — it can produce a
   binary that crashes on older CPUs. If profiling later shows a hot DSP
   kernel, use per-function runtime dispatch (`is_x86_feature_detected!`) for
   AVX2, not a global flag.
6. **Keep the webview off the audio path.** Already true: the UI talks to the
   engine over IPC and a 30 Hz snapshot poll. Large sample transfers
   (`sample_put`, base64 over IPC) happen off the audio thread, but are worth
   timing on WebView2 (see W4).

## 5. Low-latency drivers: WASAPI vs ASIO

| Option | Latency | Effort | Notes |
|---|---|---|---|
| WASAPI shared (current, via cpal) | ~20 ms out | Fix 3.1–3.5 | Works with every device, coexists with other apps' audio. Default. |
| **ASIO** via cpal `asio` feature | 1.5–6 ms at 64–256 frames | Medium | What pro interfaces and Dante Virtual Soundcard are built for. Buffer size is set in the driver's control panel. One ASIO driver at a time; input and output share it. Build needs the Steinberg ASIO SDK and LLVM/clang on the build machine. Check Steinberg's current licence terms before distributing installers outside the team. |
| WASAPI exclusive / IAudioClient3 low-latency shared | 3–10 ms | High | Not supported by cpal 0.16; would need our own backend. Not recommended unless ASIO is ruled out. |

Recommendation: WASAPI as the default, ASIO as an opt-in "Driver type" choice
in Audio Settings. Build ASIO into the Windows binary from CI (SDK fetched at
build time, never committed).

## 6. Phased plan

Each phase has an exit test. Phases W0–W2 can mostly be written from a Mac (the
code is `#[cfg(windows)]`, CI compiles and tests it on `windows-latest`), but
**exit tests for W1–W3 and W6 need a real Windows PC with real audio hardware**.
Virtual machines (Parallels, UTM) have unrepresentative audio timing and are
only good for UI checks.

### W0 — Windows dev environment and CI (small)

- Windows PC: Rust (rustup, `x86_64-pc-windows-msvc`), Visual Studio Build
  Tools (C++ workload), Node 22, WebView2 (preinstalled on Win 10/11).
- New CI workflow on every push: `windows-latest` and `macos-latest` running
  `npm ci && npm run build`, `cargo test --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`.
- Exit: `npm run tauri dev` opens the app on the Windows PC; CI green on both OSes.

### W1 — WASAPI correctness (medium) — `crates/sf-io`

- 3.1 native channel count + stereo pair routing (output and input)
- 3.5 sample-format handling
- 3.3 measured callback size in `Stats`; honest `StreamInfo.buffer_frames`
- 3.4 device-lost detection, stream restart, project re-push, UI notice;
  default-device following
- 3.6 mic-silence privacy hint
- Exit: `tone`, `play-project`, `record-probe`, `record-master` examples run on
  (a) onboard audio, (b) USB headset, (c) a multichannel interface or Dante
  VSC; unplugging a USB device mid-playback recovers within 2 s.

### W2 — Real-time tuning (small–medium)

- MMCSS "Pro Audio" registration in output and input callbacks; expose
  "RT priority: ok/failed" in `audio_stats` and the PerfMonitor.
- Power-throttling opt-out in `src-tauri`.
- Run `soak` for 30 min with `--load 4` on each target PC; record results in a
  per-device CPU table in `docs/PARITY.md` (already a listed "Remaining" item).
- Decide on the Phase 8 worker pool from the numbers.
- Exit: 0 deadline misses over a 30 min soak on the target show/office PC.

### W3 — ASIO (medium)

- `sf-io` feature `asio`; host selection (WASAPI / ASIO) through the existing
  device-list and engine-start IPC (additive fields only).
- Audio Settings: driver-type picker; "Open ASIO control panel" button; show
  the driver's buffer size and rate.
- CI: install LLVM, fetch SDK, set `CPAL_ASIO_DIR`, build with `--features asio`.
- Exit: 128-frame playback and recording with 0 xruns over 30 min on an ASIO
  interface; measured round-trip latency via the existing **Measure latency**.

### W4 — UI / WebView2 parity pass (small)

Check, fix only what breaks:
- Shortcut labels (⌘ vs Ctrl); right-click menus; mouse-wheel zoom with Ctrl.
- Display scaling at 125% / 150% (canvas sharpness in piano roll, arrangement,
  meters).
- Windows classic scrollbars take layout width (macOS overlays them).
- Exports: browser-path `download()` in `encode.ts` uses `<a download>`;
  confirm WebView2 saves it (Downloads folder) and that native
  `export_mixdown` output is findable (it writes to the app cache dir —
  same behaviour as Mac, but worth deciding whether to add a save dialog).
- Audio-file import (drag-drop into the window), sample loading, autosave
  (IndexedDB lives under `%LOCALAPPDATA%\app.synthyy.desktop\EBWebView`).
- Time a large `sample_put` (e.g. a 5-minute stereo audio clip) on WebView2.
- Collaboration (WebRTC via nostr/torrent trackers) on the MCEC network — may
  be blocked by corporate firewall regardless of OS.
- Exit: the FEATURES.md list exercised by hand on Windows with no difference
  from the Mac build.

### W5 — Packaging and distribution (small)

- NSIS `installMode: "currentUser"` so it installs without admin rights.
- `webviewInstallMode: embedBootstrapper` (or `offlineInstaller`) for machines
  without internet during install.
- Rename "Synthtagram" strings in the workflow and artifacts.
- Code signing: optional, but unsigned installers trigger SmartScreen
  warnings and may be blocked by corporate policy (AppLocker / Intune).
- Optional later: Windows on ARM (`aarch64-pc-windows-msvc`); the FTZ code
  already covers aarch64.
- Exit: installer runs on a clean Windows 11 machine without admin, launches,
  plays the demo project.

### W6 — Acceptance on real hardware (small, but do not skip)

Device matrix: onboard Realtek, USB headset, Bluetooth headphones, HDMI monitor
audio, a pro USB interface (WASAPI and ASIO), Dante Virtual Soundcard.
For each: play, record a take, Measure latency, unplug/replug, export mixdown.
Plus: export the same project on Mac and Windows and null-test the two WAVs
(expect better than -90 dB difference).

## 7. Suggested order

W0 → W1 (3.1 and 3.2 first: one is a hard failure, the other is the biggest
performance risk) → W2 → W4 → W5 → W3 → W6. ASIO comes after a shippable
WASAPI build because WASAPI covers every device; ASIO only improves latency.

## 8. Answered questions

1. Mac stays supported — one codebase, CI on both.
2. Personal PCs only — no Intune/AppLocker/proxy constraints; per-user
   install and unsigned builds are acceptable (SmartScreen "More info → Run
   anyway" on first launch).
3. Hardware: onboard sound and USB interfaces (WASAPI), ASIO for low latency.
4. Live playing/monitoring matters → ASIO is in scope now, not later.
5. A Windows PC is available for the hardware checks.

## 9. Implementation log

### Done (W0, W1, W3 core, W2 MMCSS)

- **CI** (`.github/workflows/ci.yml`): Windows + macOS on every push — web
  build, `cargo clippy -D warnings`, `cargo test --workspace`, and an ASIO
  clippy pass on Windows. The release workflow builds Windows installers with
  `--features asio`.
- **Driver types** (`sf-io`): `hosts()`, `IoHandle::set_host`, device lists
  per driver; ASIO behind the `asio` cargo feature (`sf-io`, `synthyy`).
- **3.1** streams open at the device's native channel count; stereo goes to
  outputs 1–2, mono devices get a downmix; inputs keep channels 1–2.
- **3.2** WASAPI callback threads register with MMCSS "Pro Audio"; result
  shown in the UI and the `tone` probe. ASIO threads are left alone.
- **3.3** WASAPI never claims a fixed buffer; actual callback sizes are
  measured and used for the latency estimate and the UI.
- **3.4** watchdog (`IoHandle::check_health`) + Tauri supervisor thread:
  device loss, stalled callbacks and default-device changes restart the
  engine (same device → driver default → platform default driver), re-push
  the project, and toast the user; failed restarts are retried.
- **3.5** every cpal sample format handled (f32/f64/i32/i24/i16/u16).
- **3.6** exact-digital-silence detection on the input → privacy hint toast.
- FTZ/DAZ on x86 moved to `stmxcsr`/`ldmxcsr` (the intrinsics are deprecated
  and fail clippy on x86 targets — this would have broken Windows CI and
  Intel Mac builds).

### Found during implementation

- **The engine ignored the output picker.** `engine-native.ts` always opened
  the system default device; the Audio-settings picker only routed the test
  tone. Fine on a Mac (switch the system output), a blocker on Windows where
  a USB interface is rarely the default and ASIO has no default. The choice
  (driver, device, buffer) is now remembered per machine and used by the
  engine, with a fallback to the system default if the device is missing.
- **24-bit clipping bug.** The sample converter maps +1.0 to 8 388 608 for
  24-bit — one past the maximum — which wraps to full-scale negative when
  packed: a loud click on every 0 dBFS peak through a 24-bit ASIO driver.
  Output is now clipped one 24-bit step below 1.0 (unit-tested).

### Still open

- Hardware checks in `docs/WINDOWS.md` §3 (all W1/W3 exit tests).
- W2: power-throttling opt-out, soak numbers per PC, multi-core decision.
- W3: an "open ASIO control panel" button (cpal does not expose it); choosing
  an output pair other than 1–2 and input channels other than 1–2.
- W4 (WebView2 UI pass), W5 (packaging: `currentUser` NSIS, offline WebView2,
  "Synthtagram" release names), W6.
