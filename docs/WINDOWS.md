# Synthyy on Windows — setup and hardware tests

Companion to `docs/WINDOWS_PORT_PLAN.md`. This page is the hands-on part:
setting up a Windows PC to build Synthyy, and the checks that need real audio
hardware (CI cannot do these).

Target: Windows 10/11 x64 personal PCs; onboard sound and USB audio interfaces
via WASAPI, plus ASIO for low latency.

## 1. One-time setup

Install, in this order (accept defaults unless noted):

1. **Git** — https://git-scm.com/download/win
2. **Visual Studio 2022 Build Tools** — https://visualstudio.microsoft.com/downloads/
   (under "Tools for Visual Studio"). In the installer tick
   **Desktop development with C++**. Rust on Windows uses Microsoft's linker
   and C runtime from this package.
3. **Rust** — https://rustup.rs → run `rustup-init.exe`, choose the default
   (`x86_64-pc-windows-msvc`).
4. **Node.js 22 LTS** — https://nodejs.org
5. **LLVM** (only needed for ASIO builds) — in PowerShell:
   ```powershell
   winget install LLVM.LLVM
   ```
   ASIO's C++ headers are turned into Rust bindings at build time by
   `bindgen`, which needs `libclang` from LLVM. If the ASIO build later says
   it cannot find libclang, set
   `$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"` and retry.
6. **WebView2** is part of Windows 11 and current Windows 10 — nothing to do.

Then, in a new PowerShell window:

```powershell
git clone https://github.com/ZenithB/synthyy.git
```

```powershell
cd synthyy
```

```powershell
npm ci
```

## 2. Run the app

Standard build (WASAPI only):

```powershell
npm run tauri dev
```

With ASIO support:

```powershell
npm run tauri dev -- --features asio
```

The first build compiles the whole Rust workspace (several minutes); later
runs are quick. The first ASIO build also downloads the Steinberg ASIO SDK
into a temp folder (no account needed). The SDK is never committed to the
repo. Synthyy is for personal use; check Steinberg's licence terms before
giving ASIO builds to anyone else.

In the app: **Audio settings (top bar) → native section**:

- **Driver**: WASAPI (works with every device) or ASIO (low latency, needs
  the interface's ASIO driver installed — Focusrite, MOTU, RME, Behringer
  etc. ship one). For onboard sound, WASAPI is the right choice.
- **Native output**: the device. The engine now uses this choice (it used to
  always use the system default). Remembered between launches.
- **Native buffer**: honoured by ASIO and CoreAudio. On WASAPI, Windows sets
  the period itself (about 10 ms) — the stats line shows what is really
  happening.
- **Outputs** (shown for interfaces with more than two outputs): which
  pair carries the mix — e.g. 3–4 for a headphone output. Remembered.
- **Apply to playback** re-opens the engine with these settings (closing the
  dialog does the same).

On an audio track's header, next to the input device there is now a small
**input channel** picker for multi-input interfaces: `In 1+2` (stereo pair)
or a single input such as `In 2` (records a mono take of just that input).
Each track remembers its own choice, so a vocal track can use input 1 and a
guitar track input 2.

**ASIO and recording**: ASIO runs one driver at a time, so the recording
input must be on the same interface as the output (the app says so if not).
Starting playback output on ASIO re-initialises the driver, which closes the
recording input; it reopens automatically on the next record.

To build an installer instead of running from source, see `docs/RELEASE.md`
§4 (`scripts\release-windows.ps1`).

The stats line shows the callback size actually seen, xruns, deadline misses,
peak load and — on WASAPI — the audio thread priority. It should read
**real-time (MMCSS)**. If it says MMCSS registration failed, report it.

## 3. Hardware tests (W6 acceptance)

### 3.0 The quick way: Diagnostics (no tools needed)

Install from the latest installer (the `windows-installer` run in the
repository's Actions tab builds one for every app change), then:

1. Pick the driver and device in **Audio settings → Native output**, press
   **Apply to playback**.
2. Launch a scene with several tracks so it is playing.
3. **Audio settings → Diagnostics → Run checks** (about 25 s; keep it
   playing). It records drivers and devices with channel counts, the live
   stream (rate, format, buffer, routing), dropouts and deadline misses
   during the check, the audio thread's priority, the recording input, the
   latency compensation, and the CPU headroom test, and ends with
   `RESULT: no problems found` or a `PROBLEMS:` list.
4. **Save to Downloads** and send `synthyy-diagnostics.txt`.

Run it once per driver (WASAPI, then ASIO) and per interface. The manual
checks below (unplugging, drag and drop, display scaling, recording) still
need a person; the command-line probes are for deeper digging and need the
section 1 setup.

### 3.0a Command-line probes (optional)

Command-line probes run without the GUI. `SF_HOST` picks the driver type,
`SF_DEVICE` an output device by exact name (the probes print the names).
In PowerShell, set them like this, then run the command; remove them with
`Remove-Item Env:SF_HOST` / `Remove-Item Env:SF_DEVICE`.

```powershell
$env:SF_HOST = "WASAPI"
```

### 3.1 Test tone on each device

Plays 2 s of quiet 440 Hz sine and prints what was negotiated:

```powershell
cargo run -p sf-io --example tone 128
```

Expect: `stream open: <device> [WASAPI] @ 48000 Hz, <N> ch f32 …`, a
callback size near 480 frames on WASAPI, `rt priority: MMCSS Pro Audio`,
`xruns: 0`. Repeat with `SF_DEVICE` set to each device: onboard speakers or
headphones, a USB headset, the USB interface. Devices with more than two
channels must now open (they used to fail); sound comes out of outputs 1–2.

ASIO (needs the ASIO build — add `--features asio`):

```powershell
$env:SF_HOST = "ASIO"
```

```powershell
cargo run -p sf-io --features asio --example tone 128
```

Expect `[ASIO]`, a callback size of 128 (or the driver's own size if it
does not accept 128), `rt priority: host-managed` (ASIO's driver owns the
thread), and the interface's real sample format (often `i32`).

### 3.2 Demo project through the engine

```powershell
cargo run -p sf-io --example play-project -- src-tauri/assets/demo-project.json 8
```

Listen for clean playback; the output prints xruns (expect 0).

### 3.3 Recording input

```powershell
cargo run -p sf-io --example record-probe
```

Speak or play into the input. Expect a level well above -90 dBFS and 0 input
xruns. If it prints the "exact digital silence" warning: open **Settings >
Privacy & security > Microphone** and turn on **Let desktop apps access your
microphone**, then retry.

### 3.4 Stress (deadline) test — 10 minutes

```powershell
cargo run --release -p sf-io --example soak -- src-tauri/assets/demo-project.json 10 128 --load 4
```

Plays the demo project (master at -40 dB) with four CPU-burning threads
competing. Pass = **0 deadline misses**. Run it once on WASAPI and once on
ASIO (add `--features asio` and set `SF_HOST`). Note the peak load figure —
it is the input for the W2 decision on multi-core rendering.

### 3.5 Unplug / replug (in the app)

1. Pick the USB interface as **Native output**, press Apply, play something.
2. Unplug the interface. Within about 2 s a toast should say it was
   disconnected and that audio switched to another device; playback stops.
3. Plug it back in and press **Apply to playback** — it returns to the
   interface.
4. With **System default** selected, change the default device in the
   Windows sound settings while the app is open: a toast should report the
   change and audio follows it.

### 3.6a ASIO recording (W3)

With the ASIO build, driver set to ASIO and your interface selected:

1. Play the demo song, then on an audio track pick the interface and
   `In 1`, arm and record a few bars while playing into input 1. Playback
   must continue without a dropout or crash while recording starts and stops.
2. Repeat with `In 2` and with `In 1+2`; the clip should be mono for single
   inputs and stereo for the pair, with the signal on the right input.
3. Set **Outputs** to 3–4 (if your interface has them) and confirm the mix
   moves to that output pair.
4. Change the buffer size in the interface's own control panel while the
   app plays: within a few seconds a toast reports the restart and audio
   resumes at the new size (the stats line shows it).

### 3.6 Latency measurement

Audio settings → Native input → pick the interface input → connect an output
to an input with a cable → **Measure latency**. Expect roughly 15–30 ms on
WASAPI and well under 10 ms on ASIO at 128 frames.

### 3.7 CPU benchmark (W2 — decides whether multi-core rendering is needed)

No audio device involved: this measures the PC's processor running the
engine. Use `--release` (debug numbers are meaningless).

```powershell
cargo run --release -p sf-render --example bench -- src-tauri/assets/demo-project.json
```

It runs 1, 2, 4, 8… copies of the demo song inside every 128-frame block
and prints the share of the 2.67 ms deadline used. The last line reads
`RESULT: Nx this project fits…`. How to read N:

| N | Meaning |
|---|---|
| 4 or more | One core has plenty of headroom — multi-core rendering not needed. |
| 2 | Borderline for big projects at 64–128 frames; use 256 frames or report it. |
| 1 or 0 | Multi-core rendering (plan Phase 8) is worth building — report it. |

For reference, a cloud server core gave N = 4. Also run the all-fixtures
mode (no arguments) and note the `TOTAL … x realtime` figure:

```powershell
cargo run --release -p sf-render --example bench
```

Copy the `TABLE |` line printed at the end of the soak test (3.4) and the
two benchmark results into `docs/PARITY.md` (per-device CPU table).

### 3.8 UI checks in the Windows webview (W4)

Run the app (section 2) and check:

1. **Drag and drop**: drag a clip between scene slots, drag a sample from
   the browser onto a track, drag a device into a track, and drop a `.wav`
   file from File Explorer onto the window. (These were all dead on Windows
   before this change — Tauri's native drop handler swallowed them.)
2. **Display scaling**: Settings > System > Display > Scale at **125 %**
   and **150 %**. The piano roll, waveform editor and the session analyser
   should be sharp, not blurry, and must not flicker during playback.
3. **Browser keys do nothing**: Ctrl+R, F5, Ctrl+P, Ctrl+F, Alt+Left, the
   mouse back button, and right-click on empty space (no browser menu).
   App keys still work: Ctrl+Z / Ctrl+Y, Ctrl+K, Ctrl+D, Ctrl+A, Space, Tab.
   Right-click in a text box still offers copy/paste.
4. **Exports**: Export a WAV mixdown, then stems. Files appear in
   `Downloads` with a toast per file and no "allow multiple downloads"
   prompt. Save project file the same way.
5. **Long sample**: import a 5-minute WAV and add it to a track. The UI
   should stay responsive while it loads (samples now cross to the engine
   as raw bytes, not a giant text string).
6. The command palette hint reads **Ctrl+K** (not ⌘K).

## 4. Troubleshooting

| Symptom | Likely cause |
|---|---|
| Device fails to open, "device in use" | Another app holds it in **exclusive mode**. Sound settings → device → Properties → Advanced → untick "Allow applications to take exclusive control", or close the other app. |
| ASIO: not listed as a driver | Built without `--features asio`. |
| ASIO: no devices listed | The interface's ASIO driver is not installed, or another ASIO app (DAW) has it open — most ASIO drivers allow one app at a time. |
| ASIO: "runs one driver at a time" when recording | The track's input is on a different interface than the output. Pick the output's interface for the input, or use WASAPI. |
| ASIO: no "open control panel" button | The ASIO library used here does not expose it; use the interface maker's settings app (Focusrite Control, MOTU, RME TotalMix, etc.). |
| Crackles on WASAPI under load | Check the stats line says real-time (MMCSS); set Windows power mode to Best performance; try the Stable latency preset. |
| Recording is silent | Microphone privacy setting (see 3.3). |
