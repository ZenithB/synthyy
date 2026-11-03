//! Tauri shell: hosts the existing React UI and exposes the Rust audio core
//! over IPC (docs/RUST_ARCH.md §IPC). Phase 0 surface (device enumeration,
//! test tone, callback stats) stays; Phase 1 adds the engine command lane:
//! `engine_start` opens a stream hosting an sf-engine Engine, `project_load`
//! pushes the full doc, `param_set` is the no-rebuild fast path, transport /
//! clip / preview commands map 1:1 onto `sf_engine::Cmd`, `engine_snapshot`
//! is the ~30 Hz meter/playhead poll, and `export_mixdown` runs the offline
//! sf-render driver on a blocking task. Phase 6 adds the recording surface:
//! `input_*` (native input stream), `record_take_*` (latency-compensated
//! takes), `master_capture_*` (desktop "Record Output") and `latency_*`
//! (loopback calibration). The input stream is owned by sf-io and is
//! independent of project pushes / engine restarts.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;
use tauri::{Manager, State};

use sf_engine::{Cmd, EngineControl, Mode};
use sf_project::project::{ProjectJSON, TrackJSON};
use sf_render::{with_defaults, RenderScope};

struct AudioState(Mutex<sf_io::IoHandle>);

/// Everything the IPC layer keeps between calls: the control half of the
/// split engine (the Engine itself lives in the audio callback), the last
/// loaded project (export + stream-restart re-push), the native sample store
/// (webview-decoded PCM, persists across project pushes AND stream restarts —
/// every graph rebuild re-injects it), and the id maps that let the webview
/// address meters/GR by track/fx id.
#[derive(Default)]
struct EngineInner {
    ctl: Option<EngineControl>,
    project: Option<ProjectJSON>,
    bank: sf_engine::SampleBank,
    track_ids: Vec<String>,
    gr_index: Vec<GrEntry>,
    playing: bool,
    arr_mode: bool,
    /// Last automatic audio-device event (supervisor restart), for a UI
    /// toast; `notice_seq` bumps each time the message changes.
    notice_seq: u64,
    notice: Option<String>,
    /// What the engine stream negotiated at its last (re)start — for the
    /// Diagnostics report.
    stream: Option<StreamDto>,
}

/// Rebuild the current graph from the stored project + sample bank (the
/// post-`sample_put` pickup path and any bank-driven refresh).
fn repush_stored(e: &mut EngineInner) {
    if let (Some(ctl), Some(p)) = (e.ctl.as_mut(), e.project.as_ref()) {
        ctl.load_project_with_bank(p, &e.bank);
    }
}

struct EngineState(Mutex<EngineInner>);

#[derive(Serialize)]
struct DeviceDto {
    name: String,
    is_default: bool,
    /// Channel count in the listed direction (0 = unknown).
    channels: u16,
}

#[derive(Serialize)]
struct HostDto {
    name: String,
    is_default: bool,
}

#[derive(Serialize, Clone)]
struct StreamDto {
    device: String,
    sample_rate: u32,
    /// None = backend-managed (always on WASAPI); see StatsDto callback frames.
    buffer_frames: Option<u32>,
    /// Driver type (cpal host name: CoreAudio / WASAPI / ASIO).
    host: String,
    channels: u16,
    sample_format: String,
}

fn stream_dto(info: sf_io::StreamInfo) -> StreamDto {
    StreamDto {
        device: info.device,
        sample_rate: info.sample_rate,
        buffer_frames: info.buffer_frames,
        host: info.host,
        channels: info.channels,
        sample_format: info.sample_format,
    }
}

fn rt_label(code: u8) -> &'static str {
    match code {
        sf_io::RT_MMCSS_OK => "mmcss",
        sf_io::RT_MMCSS_FAILED => "mmcss-failed",
        _ => "host",
    }
}

#[derive(Serialize)]
struct StatsDto {
    callbacks: u64,
    xruns: u32,
    over_budget: u64,
    last_callback_us: f64,
    max_callback_us: f64,
    budget_us: f64,
    peak_load: f64,
    /// Callback sizes actually seen (0 = none yet) — the honest buffer size.
    min_callback_frames: u32,
    max_callback_frames: u32,
    /// "host" (driver-managed) | "mmcss" | "mmcss-failed".
    rt_priority: &'static str,
    device_lost: bool,
}

#[derive(Serialize, Clone)]
struct GrEntry {
    track_id: String,
    fx_id: String,
}

#[derive(Serialize)]
struct ProjectLoadDto {
    /// Track ids in engine order (meter index = position in this list).
    tracks: Vec<String>,
    /// Flat GR-meter order: `engine_snapshot().gr[i]` belongs to `gr[i]`.
    gr: Vec<GrEntry>,
}

#[derive(Serialize)]
struct TrackMeterDto {
    id: String,
    peak: f32,
    rms: f32,
    /// Instrument output level (linear, decaying peak-hold) — the device
    /// rack's instrument meter.
    inst: f32,
}

#[derive(Serialize)]
struct MeterDto {
    peak: f32,
    rms: f32,
}

#[derive(Serialize)]
struct SnapshotDto {
    playing: bool,
    mode: String,
    playhead_ticks: f64,
    master: MeterDto,
    tracks: Vec<TrackMeterDto>,
    /// Per-fx gain reduction in dB (≤ 0), ordered like ProjectLoadDto.gr.
    gr: Vec<f32>,
    /// Per-fx output level (linear, decaying peak-hold), same order as `gr`.
    fx_level: Vec<f32>,
    /// Per-fx, per-band gain reduction in dB (≤ 0), same order as `gr`.
    fx_bands: Vec<[f32; 3]>,
    xruns: u32,
    callbacks: u64,
    over_budget: u64,
    peak_load: f64,
    max_callback_us: f64,
    budget_us: f64,
    /// Automatic device-change notice (see EngineInner::notice).
    notice_seq: u64,
    notice: Option<String>,
    /// The input stream has delivered only exact digital silence for 2 s
    /// (Windows microphone privacy) — the UI shows a hint.
    input_silent: bool,
}

fn track_id(t: &TrackJSON) -> String {
    t.id.clone().unwrap_or_else(|| t.name.clone())
}

fn fx_id(f: &sf_project::project::FxJSON) -> String {
    f.id.clone().unwrap_or_else(|| f.r#type.clone())
}

/// Mirror of sf-engine build_graph's flat GR indexing: every BUILT fx slot
/// (enabled + known type) gets one index — track chains in doc order, then
/// the master chain.
fn gr_entries(p: &ProjectJSON) -> Vec<GrEntry> {
    let mut out = Vec::new();
    let built = |f: &sf_project::project::FxJSON| f.on && sf_dsp::make_effect(&f.r#type).is_some();
    for t in &p.tracks {
        let tid = track_id(t);
        for f in t.fx.iter().filter(|f| built(f)) {
            out.push(GrEntry { track_id: tid.clone(), fx_id: fx_id(f) });
        }
    }
    for f in p.master_fx.iter().flatten().filter(|f| built(f)) {
        out.push(GrEntry { track_id: "master".into(), fx_id: fx_id(f) });
    }
    out
}

// ---- plain helpers (shared by the #[tauri::command] wrappers and the
// SF_SMOKE self-test, which has no invoke context) ----

fn do_engine_start(
    io: &AudioState,
    eng: &EngineState,
    device: Option<String>,
    buffer_frames: Option<u32>,
) -> Result<StreamDto, String> {
    let (info, mut ctl) = io
        .0
        .lock()
        .map_err(|_| "audio state poisoned")?
        .start_engine(device, buffer_frames)?;
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    if let Some(p) = &e.project {
        // stream restart: the previous control handle died with its stream —
        // re-push the last project (with the surviving sample bank) into the
        // fresh engine
        ctl.load_project_with_bank(p, &e.bank);
    }
    e.playing = false;
    e.ctl = Some(ctl);
    let dto = stream_dto(info);
    e.stream = Some(dto.clone());
    Ok(dto)
}

fn do_project_load(eng: &EngineState, project: Value) -> Result<ProjectLoadDto, String> {
    let raw: ProjectJSON =
        serde_json::from_value(project).map_err(|e| format!("bad ProjectJSON: {e}"))?;
    let p = with_defaults(&raw);
    let track_ids: Vec<String> = p.tracks.iter().map(track_id).collect();
    let gr = gr_entries(&p);
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    let inner = &mut *e;
    if let Some(ctl) = inner.ctl.as_mut() {
        ctl.load_project_with_bank(&p, &inner.bank);
    }
    e.project = Some(p);
    e.track_ids = track_ids.clone();
    e.gr_index = gr.clone();
    Ok(ProjectLoadDto { tracks: track_ids, gr })
}

/// Launch a BATCH of session clips atomically. Every ClipLaunch is queued
/// before the transport starts: the engine anchors launches that arrive
/// while stopped at the play position, so the whole batch starts together.
/// `start_from` = Some(ticks): start the transport there if it is stopped
/// (the UI's "launching from stopped starts playback"); None: never start it
/// (the caller issues transport_play itself, e.g. the re-push path).
///
/// Bug fixed here: the old per-clip path started the transport right after
/// the FIRST launch, so launches 2..N arrived while playing and were
/// quantised to the next bar — "track 1 on time, the others a bar late",
/// and those stale anchors made every later Play repeat it.
fn do_clips_launch(eng: &EngineState, launches: &[(&str, &str)], start_from: Option<f64>) -> Result<(), String> {
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    let was_playing = e.playing;
    let Some(ctl) = e.ctl.as_mut() else { return Ok(()) };
    for (track_id, scene_id) in launches {
        // unknown id (doc ahead of the last push) is skipped — repush fixes it
        if let (Some(track), Some(clip)) = (ctl.resolver().track(track_id), ctl.resolver().scene(scene_id)) {
            ctl.push_cmd(Cmd::ClipLaunch { track, clip });
        }
    }
    if let (Some(from), false) = (start_from, was_playing) {
        ctl.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: from });
        e.playing = true;
        e.arr_mode = false;
    }
    Ok(())
}

/// One clip (a click on a slot): starts the transport from bar 1 if stopped.
fn do_clip_launch(eng: &EngineState, track_id: &str, scene_id: &str) -> Result<(), String> {
    do_clips_launch(eng, &[(track_id, scene_id)], Some(0.0))
}

// ---- Phase 0 commands (unchanged surface) ----

/// Driver types available in this build (CoreAudio; WASAPI, plus ASIO when
/// built with the `asio` feature).
#[tauri::command]
fn audio_hosts() -> Vec<HostDto> {
    sf_io::hosts().into_iter().map(|h| HostDto { name: h.name, is_default: h.is_default }).collect()
}

/// Resolve an optional driver-type argument: None = the one in use.
fn host_or_current(io: &AudioState, host: Option<String>) -> Option<String> {
    host.or_else(|| io.0.lock().ok().and_then(|g| g.host()))
}

/// List selectable output devices on `host` (None = the driver type in use).
#[tauri::command]
fn audio_devices(io: State<'_, AudioState>, host: Option<String>) -> Vec<DeviceDto> {
    sf_io::output_devices_on(host_or_current(&io, host).as_deref())
        .into_iter()
        .map(|d| DeviceDto { name: d.name, is_default: d.is_default, channels: d.channels })
        .collect()
}

/// Open an output stream (default device when `device` is null) at
/// `buffer_frames` and play the Phase 0 test tone. Returns what the backend
/// actually negotiated.
#[tauri::command]
fn audio_start_tone(
    state: State<'_, AudioState>,
    host: Option<String>,
    device: Option<String>,
    buffer_frames: Option<u32>,
    out_first: Option<u16>,
) -> Result<StreamDto, String> {
    let io = state.0.lock().map_err(|_| "audio state poisoned")?;
    if host.is_some() {
        io.set_host(host);
    }
    if let Some(f) = out_first {
        io.set_output_channel(f);
    }
    let info = io.start_tone(device, buffer_frames, 440.0, 0.1)?;
    Ok(stream_dto(info))
}

/// Stop the current stream.
#[tauri::command]
fn audio_stop(state: State<'_, AudioState>) -> Result<(), String> {
    state.0.lock().map_err(|_| "audio state poisoned")?.stop();
    Ok(())
}

/// Live callback health (poll from the UI at ~1 Hz).
#[tauri::command]
fn audio_stats(state: State<'_, AudioState>) -> Result<StatsDto, String> {
    let s = state.0.lock().map_err(|_| "audio state poisoned")?.stats();
    Ok(StatsDto {
        callbacks: s.callbacks,
        xruns: s.xruns,
        over_budget: s.over_budget,
        last_callback_us: s.last_callback_us,
        max_callback_us: s.max_callback_us,
        budget_us: s.budget_us,
        peak_load: s.peak_load,
        min_callback_frames: s.min_callback_frames,
        max_callback_frames: s.max_callback_frames,
        rt_priority: rt_label(s.rt_priority),
        device_lost: s.device_lost,
    })
}

// ---- Phase 1: engine hosting ----

/// Open the output stream hosting a fresh Engine at the device rate.
/// `host` (driver type) is optional: None keeps the one in use; it also
/// applies to the input stream opened afterwards (ASIO runs both on one
/// driver).
#[tauri::command]
fn engine_start(
    io: State<'_, AudioState>,
    eng: State<'_, EngineState>,
    host: Option<String>,
    device: Option<String>,
    buffer_frames: Option<u32>,
    out_first: Option<u16>,
) -> Result<StreamDto, String> {
    {
        let g = io.0.lock().map_err(|_| "audio state poisoned")?;
        if host.is_some() {
            g.set_host(host);
        }
        // which device outputs carry the stereo mix (0 = 1–2); kept for
        // automatic restarts
        if let Some(f) = out_first {
            g.set_output_channel(f);
        }
    }
    do_engine_start(&io, &eng, device, buffer_frames)
}

/// Full-doc push: parse ProjectJSON, fill schema defaults, build + swap the
/// graph off the audio thread, rebuild the command resolver.
#[tauri::command]
fn project_load(eng: State<'_, EngineState>, project: Value) -> Result<ProjectLoadDto, String> {
    do_project_load(&eng, project)
}

/// Fast path: resolve a string param address and push Cmd::SetParam.
/// Unresolvable (stale id, disabled fx) is Ok(false), never an error.
#[tauri::command]
fn param_set(
    eng: State<'_, EngineState>,
    scope: String,
    track_id: String,
    device_id: String,
    key: String,
    value: f64,
) -> Result<bool, String> {
    let addr_key = if scope == "master" {
        match device_id.as_str() {
            "mixer" => {
                if key != "gain" {
                    return Ok(false);
                }
                "master|gain".to_string()
            }
            _ => format!("master|{device_id}|{key}"),
        }
    } else {
        match device_id.as_str() {
            "inst" => format!("{track_id}|inst|{key}"),
            "mixer" => format!("{track_id}|mix|{key}"),
            "send" => format!("{track_id}|send|{key}"),
            _ => format!("{track_id}|{device_id}|{key}"),
        }
    };
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    let Some(ctl) = e.ctl.as_mut() else { return Ok(false) };
    match ctl.resolver().param(&addr_key) {
        Some(a) => Ok(ctl.push_cmd(Cmd::SetParam {
            target: a.target,
            slot: a.slot,
            param: a.param,
            value: value as f32,
        })),
        None => Ok(false),
    }
}

#[tauri::command]
fn transport_play(eng: State<'_, EngineState>, mode: String, from_ticks: f64) -> Result<(), String> {
    let m = if mode == "arr" { Mode::Arr } else { Mode::Session };
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    e.playing = true;
    e.arr_mode = m == Mode::Arr;
    if let Some(ctl) = e.ctl.as_mut() {
        ctl.push_cmd(Cmd::TransportPlay { mode: m, from_ticks });
    }
    Ok(())
}

#[tauri::command]
fn transport_stop(eng: State<'_, EngineState>) -> Result<(), String> {
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    e.playing = false;
    if let Some(ctl) = e.ctl.as_mut() {
        ctl.push_cmd(Cmd::TransportStop);
    }
    Ok(())
}

#[tauri::command]
fn clip_launch(eng: State<'_, EngineState>, track_id: String, scene_id: String) -> Result<(), String> {
    do_clip_launch(&eng, &track_id, &scene_id)
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LaunchDto {
    track_id: String,
    scene_id: String,
}

/// Atomic batch launch (scene launch, resume after pause, re-push): see
/// [`do_clips_launch`]. `start_from` null = leave the transport alone.
#[tauri::command]
fn clips_launch(eng: State<'_, EngineState>, launches: Vec<LaunchDto>, start_from: Option<f64>) -> Result<(), String> {
    let pairs: Vec<(&str, &str)> = launches.iter().map(|l| (l.track_id.as_str(), l.scene_id.as_str())).collect();
    do_clips_launch(&eng, &pairs, start_from)
}

#[tauri::command]
fn clip_stop(eng: State<'_, EngineState>, track_id: String) -> Result<(), String> {
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    let Some(ctl) = e.ctl.as_mut() else { return Ok(()) };
    if let Some(track) = ctl.resolver().track(&track_id) {
        ctl.push_cmd(Cmd::ClipStop { track });
    }
    Ok(())
}

#[tauri::command]
fn note_preview(
    eng: State<'_, EngineState>,
    track_id: String,
    pitch: u8,
    vel: f64,
    on: bool,
) -> Result<(), String> {
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    let Some(ctl) = e.ctl.as_mut() else { return Ok(()) };
    let Some(track) = ctl.resolver().track(&track_id) else { return Ok(()) };
    if on {
        ctl.push_cmd(Cmd::NoteOn { track, pitch, vel: vel as f32 });
    } else {
        ctl.push_cmd(Cmd::NoteOff { track, pitch });
    }
    Ok(())
}

#[tauri::command]
fn metronome(eng: State<'_, EngineState>, on: bool) -> Result<(), String> {
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    if let Some(ctl) = e.ctl.as_mut() {
        ctl.push_cmd(if on { Cmd::MetronomeOn } else { Cmd::MetronomeOff });
    }
    Ok(())
}

#[tauri::command]
fn set_tempo(eng: State<'_, EngineState>, bpm: f64) -> Result<(), String> {
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    if let Some(ctl) = e.ctl.as_mut() {
        ctl.push_cmd(Cmd::SetTempo { bpm });
    }
    Ok(())
}

fn decode_f32le_b64(b64: &str) -> Result<Vec<f32>, String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| format!("bad base64 PCM: {e}"))?;
    if bytes.len() % 4 != 0 {
        return Err("PCM byte length not a multiple of 4".into());
    }
    Ok(bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// Webview-decoded PCM into the native sample store: base64 f32le channels at
/// the source rate (`r` absent = mono). Decoding runs on a blocking task off
/// the IPC thread (payloads reach tens of MB). The CURRENT graph picks the
/// sample up immediately: the stored project is re-pushed through the normal
/// load path (cheap full rebuild; the webview sends samples one at a time).
#[tauri::command]
async fn sample_put(
    eng: State<'_, EngineState>,
    id: String,
    sample_rate: u32,
    l_b64: String,
    r_b64: Option<String>,
) -> Result<(), String> {
    if sample_rate == 0 {
        return Err("sample_rate must be > 0".into());
    }
    let (l, r) = tauri::async_runtime::spawn_blocking(move || -> Result<_, String> {
        let l = decode_f32le_b64(&l_b64)?;
        let r = match &r_b64 {
            Some(b) => decode_f32le_b64(b)?,
            None => Vec::new(),
        };
        Ok((l, r))
    })
    .await
    .map_err(|e| e.to_string())??;
    let buf = std::sync::Arc::new(sf_dsp::sample::SampleBuf {
        sample_rate: sample_rate as f32,
        l,
        r,
    });
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    e.bank.insert(id, buf);
    repush_stored(&mut e);
    Ok(())
}

// ---- raw-binary IPC (docs/WINDOWS_PORT_PLAN.md W4) ----
// Large payloads travel as the raw request body (Uint8Array on the JS side)
// with metadata in headers, instead of base64 inside JSON: no 33 % size
// overhead and, more importantly, no multi-hundred-MB string building on the
// webview's UI thread (which froze the UI while a long sample loaded).

/// Percent-decode a header value (the webview sends `encodeURIComponent`
/// output, since HTTP header values must be ASCII).
fn percent_decode(v: &str) -> String {
    let b = v.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn header(req: &tauri::ipc::Request<'_>, name: &str) -> Result<String, String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(percent_decode)
        .ok_or_else(|| format!("missing header {name}"))
}

fn raw_body<'a>(req: &'a tauri::ipc::Request<'_>) -> Result<&'a [u8], String> {
    match req.body() {
        tauri::ipc::InvokeBody::Raw(b) => Ok(b),
        _ => Err("expected a raw binary body".into()),
    }
}

/// f32 little-endian bytes → samples (byte length must be a multiple of 4).
fn f32le(bytes: &[u8]) -> Vec<f32> {
    bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// [`sample_put`] over raw IPC. Body: f32le PCM, the left channel then (when
/// `x-sf-channels` is 2) the right, each `frames` long. Headers: `x-sf-id`
/// (percent-encoded), `x-sf-rate`, `x-sf-channels`.
#[tauri::command]
async fn sample_put_raw(eng: State<'_, EngineState>, request: tauri::ipc::Request<'_>) -> Result<(), String> {
    let id = header(&request, "x-sf-id")?;
    let sample_rate: u32 = header(&request, "x-sf-rate")?.parse().map_err(|_| "bad x-sf-rate")?;
    let channels: usize = header(&request, "x-sf-channels")?.parse().map_err(|_| "bad x-sf-channels")?;
    if sample_rate == 0 || !(1..=2).contains(&channels) {
        return Err("sample_rate must be > 0 and channels 1 or 2".into());
    }
    let body = raw_body(&request)?;
    if body.len() % (4 * channels) != 0 {
        return Err("PCM byte length does not match the channel count".into());
    }
    let half = body.len() / channels;
    let l = f32le(&body[..half]);
    let r = if channels == 2 { f32le(&body[half..]) } else { Vec::new() };
    let buf = std::sync::Arc::new(sf_dsp::sample::SampleBuf { sample_rate: sample_rate as f32, l, r });
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    e.bank.insert(id, buf);
    repush_stored(&mut e);
    Ok(())
}

/// Make a user-supplied file name safe on every OS: no path separators,
/// no Windows-reserved characters or trailing dots/spaces, never empty.
fn safe_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.') { "export".into() } else { trimmed }
}

/// `dir/name`, or `dir/name (2).ext`, `(3)`… if taken.
fn unique_path(dir: &std::path::Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..10_000)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

/// Desktop exports (mixdown / stems / live recording / project JSON) go
/// straight to the user's Downloads folder. Replaces the browser `<a
/// download>` path, which on Windows shows Edge's download UI and asks to
/// "allow multiple downloads" for stems, and behaves differently on macOS.
/// Body: the file bytes; header `x-sf-name` (percent-encoded). Returns the
/// saved path.
#[tauri::command]
async fn save_download(app: tauri::AppHandle, request: tauri::ipc::Request<'_>) -> Result<String, String> {
    let name = safe_file_name(&header(&request, "x-sf-name")?);
    let dir = app.path().download_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = unique_path(&dir, &name);
    std::fs::write(&path, raw_body(&request)?).map_err(|e| format!("could not save {}: {e}", path.display()))?;
    Ok(path.to_string_lossy().into_owned())
}

/// Ids currently held in the native sample store.
#[tauri::command]
fn sample_list(eng: State<'_, EngineState>) -> Result<Vec<String>, String> {
    let e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    Ok(e.bank.keys().cloned().collect())
}

/// Meter/playhead/health poll (~30 Hz from the webview). Reads atomics and
/// builds a few small vecs — no locks shared with the audio thread.
#[tauri::command]
fn engine_snapshot(
    io: State<'_, AudioState>,
    eng: State<'_, EngineState>,
) -> Result<SnapshotDto, String> {
    let (stats, input_silent) = {
        let io = io.0.lock().map_err(|_| "audio state poisoned")?;
        (io.stats(), io.input_silent())
    };
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    // Host obligation (sf-engine midimod): the ~30 Hz poll doubles as the
    // control-side service tick — pending MIDI-fx re-expansions for modulated
    // midifx params run here, off the audio thread, and basedrop collection
    // of retired graphs/patterns happens inside service() too.
    if let Some(ctl) = e.ctl.as_mut() {
        if ctl.needs_service() {
            ctl.service();
        }
    }
    let snap = e.ctl.as_ref().map(|c| c.snapshot()).unwrap_or_default();
    Ok(SnapshotDto {
        playing: snap.playing,
        mode: if e.arr_mode { "arr".into() } else { "session".into() },
        playhead_ticks: snap.playhead_ticks,
        master: MeterDto { peak: snap.master.peak, rms: snap.master.rms },
        tracks: e
            .track_ids
            .iter()
            .zip(snap.tracks.iter())
            .enumerate()
            .map(|(i, (id, m))| TrackMeterDto {
                id: id.clone(),
                peak: m.peak,
                rms: m.rms,
                inst: snap.inst_level.get(i).copied().unwrap_or(0.0),
            })
            .collect(),
        gr: snap.fx_gr_db,
        fx_level: snap.fx_level,
        fx_bands: snap.fx_bands_db,
        xruns: stats.xruns,
        callbacks: stats.callbacks,
        over_budget: stats.over_budget,
        peak_load: stats.peak_load,
        max_callback_us: stats.max_callback_us,
        budget_us: stats.budget_us,
        notice_seq: e.notice_seq,
        notice: e.notice.clone(),
        input_silent,
    })
}

/// Offline mixdown of the CURRENT loaded project (44.1 kHz 16-bit WAV) to
/// `out_path` or a temp file in the app cache dir. Runs on a blocking task.
#[tauri::command]
async fn export_mixdown(
    app: tauri::AppHandle,
    eng: State<'_, EngineState>,
    scope: Value,
    out_path: Option<String>,
) -> Result<String, String> {
    let scope: RenderScope =
        serde_json::from_value(scope).map_err(|e| format!("bad render scope: {e}"))?;
    let (project, bank) = {
        let e = eng.0.lock().map_err(|_| "engine state poisoned")?;
        (
            e.project.clone().ok_or("no project loaded")?,
            e.bank.clone(), // Arc clones — cheap
        )
    };
    let path = match out_path {
        Some(p) => PathBuf::from(p),
        None => {
            let dir = app.path().app_cache_dir().map_err(|e| e.to_string())?;
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            dir.join(format!("mixdown-{stamp}.wav"))
        }
    };
    tauri::async_runtime::spawn_blocking(move || {
        sf_render::render_to_wav_with_bank(&project, &scope, sf_render::OUT_SR as f32, &path, &bank)
            .map(|_| path.to_string_lossy().into_owned())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---- Phase 6: recording ----

#[derive(Serialize)]
struct PcmDto {
    sample_rate: u32,
    channels: u32,
    /// Frames per channel after compensation.
    frames: usize,
    /// base64 f32le (sample_put convention); `r_b64` absent for mono.
    l_b64: String,
    r_b64: Option<String>,
    compensation_frames: i64,
}

#[derive(Serialize)]
struct TakeStartDto {
    take_id: u64,
    /// Engine playhead (ticks) when the input was armed — anchor the clip here.
    playhead_ticks: f64,
    playing: bool,
}

#[derive(Serialize)]
struct LatencyDto {
    compensation_ms: f64,
    measured_roundtrip_ms: Option<f64>,
    override_ms: Option<f64>,
    estimate_ms: f64,
}

fn encode_f32le_b64(v: &[f32]) -> String {
    use base64::Engine as _;
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn pcm_dto(rec: sf_io::Recorded) -> PcmDto {
    let frames = rec.channels.first().map(|c| c.len()).unwrap_or(0);
    let mut it = rec.channels.into_iter();
    let l = it.next().unwrap_or_default();
    let r = it.next();
    PcmDto {
        sample_rate: rec.sample_rate,
        channels: if r.is_some() { 2 } else { 1 },
        frames,
        l_b64: encode_f32le_b64(&l),
        r_b64: r.as_deref().map(encode_f32le_b64),
        compensation_frames: rec.compensation_frames,
    }
}

fn latency_dto(l: sf_io::LatencyInfo) -> LatencyDto {
    LatencyDto {
        compensation_ms: l.compensation_ms,
        measured_roundtrip_ms: l.measured_roundtrip_ms,
        override_ms: l.override_ms,
        estimate_ms: l.estimate_ms,
    }
}

/// List selectable input devices on `host` (None = the driver type in use).
#[tauri::command]
fn input_devices(io: State<'_, AudioState>, host: Option<String>) -> Vec<DeviceDto> {
    sf_io::input_devices_on(host_or_current(&io, host).as_deref())
        .into_iter()
        .map(|d| DeviceDto { name: d.name, is_default: d.is_default, channels: d.channels })
        .collect()
}

/// Open the native input stream (default device when null). Independent of
/// the output stream; survives project pushes and engine restarts.
#[tauri::command]
fn input_start(
    io: State<'_, AudioState>,
    device: Option<String>,
    buffer_frames: Option<u32>,
) -> Result<StreamDto, String> {
    let info = io.0.lock().map_err(|_| "audio state poisoned")?.start_input(device, buffer_frames)?;
    Ok(stream_dto(info))
}

#[tauri::command]
fn input_stop(io: State<'_, AudioState>) -> Result<(), String> {
    io.0.lock().map_err(|_| "audio state poisoned")?.stop_input();
    Ok(())
}

/// The running input stream, if any.
#[tauri::command]
fn input_info(io: State<'_, AudioState>) -> Result<Option<StreamDto>, String> {
    Ok(io
        .0
        .lock()
        .map_err(|_| "audio state poisoned")?
        .input_info()
        .map(stream_dto))
}

/// Arm the input. Returns the take id and the playhead at arm time (the
/// compensation delivered with the take is relative to THIS tick).
/// `channel_first` / `channel_count` pick the device inputs the take keeps
/// (0-based; default inputs 1–2) — a track's "Input 2" is (1, 1).
#[tauri::command]
fn record_take_start(
    io: State<'_, AudioState>,
    eng: State<'_, EngineState>,
    channel_first: Option<usize>,
    channel_count: Option<usize>,
) -> Result<TakeStartDto, String> {
    let e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    let take_id = io
        .0
        .lock()
        .map_err(|_| "audio state poisoned")?
        .start_take_channels(channel_first.unwrap_or(0), channel_count.unwrap_or(2))?;
    let snap = e.ctl.as_ref().map(|c| c.snapshot()).unwrap_or_default();
    Ok(TakeStartDto { take_id, playhead_ticks: snap.playhead_ticks, playing: snap.playing })
}

/// Disarm and deliver the take (already shifted by the latency compensation).
/// Encoding runs on a blocking task (takes reach tens of MB).
#[tauri::command]
async fn record_take_stop(io: State<'_, AudioState>) -> Result<PcmDto, String> {
    let rec = io.0.lock().map_err(|_| "audio state poisoned")?.stop_take()?;
    tauri::async_runtime::spawn_blocking(move || pcm_dto(rec))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn master_capture_start(io: State<'_, AudioState>) -> Result<(), String> {
    io.0.lock().map_err(|_| "audio state poisoned")?.start_master_capture()
}

#[tauri::command]
async fn master_capture_stop(io: State<'_, AudioState>) -> Result<PcmDto, String> {
    let rec = io.0.lock().map_err(|_| "audio state poisoned")?.stop_master_capture()?;
    tauri::async_runtime::spawn_blocking(move || pcm_dto(rec))
        .await
        .map_err(|e| e.to_string())
}

/// Loopback calibration (~1 s, blocks on a worker task). The AudioState
/// mutex is held for the duration — other IPC calls on it wait.
#[tauri::command]
async fn latency_measure(io: State<'_, AudioState>) -> Result<f64, String> {
    io.0.lock().map_err(|_| "audio state poisoned")?.measure_roundtrip_ms()
}

// ---- Diagnostics (docs/WINDOWS_PORT_PLAN.md W6) ----
// One button in Audio settings that gathers what the hardware acceptance
// checks need from the user's PC, as a plain-text report they can send:
// drivers/devices, the live stream, a timed health window while they play,
// input state, latency, and the CPU headroom benchmark.

fn rt_text(code: &str) -> &'static str {
    match code {
        "mmcss" => "real-time (MMCSS Pro Audio)",
        "mmcss-failed" => "NORMAL - MMCSS registration failed",
        _ => "managed by the driver",
    }
}

fn diagnostics_report<R: tauri::Runtime>(app: &tauri::AppHandle<R>, seconds: u64) -> String {
    use std::fmt::Write as _;
    let io = app.state::<AudioState>();
    let eng = app.state::<EngineState>();
    let mut r = String::new();
    let mut problems: Vec<String> = Vec::new();

    let _ = writeln!(r, "Synthyy {} diagnostics", app.package_info().version);
    let _ = writeln!(
        r,
        "System: {} {} - {} logical CPUs",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0)
    );

    // drivers and devices
    let host = io.0.lock().ok().and_then(|g| g.host());
    let hosts: Vec<String> = sf_io::hosts()
        .into_iter()
        .map(|h| if h.is_default { format!("{} (default)", h.name) } else { h.name })
        .collect();
    let _ = writeln!(r, "Drivers available: {}", hosts.join(", "));
    let _ = writeln!(r, "Driver in use: {}", host.as_deref().unwrap_or("platform default"));
    let list = |devs: Vec<sf_io::DeviceInfo>| -> String {
        if devs.is_empty() {
            return "  (none)".into();
        }
        devs.iter()
            .map(|d| format!("  - {} [{} ch]{}", d.name, d.channels, if d.is_default { " (default)" } else { "" }))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let _ = writeln!(r, "Output devices:\n{}", list(sf_io::output_devices_on(host.as_deref())));
    let _ = writeln!(r, "Input devices:\n{}", list(sf_io::input_devices_on(host.as_deref())));

    // engine stream
    let (stream, playing) = match eng.0.lock() {
        Ok(e) => (e.stream.clone(), e.playing),
        Err(_) => (None, false),
    };
    let out_first = io.0.lock().map(|g| g.output_channel()).unwrap_or(0);
    let rate = match &stream {
        Some(s) => {
            let _ = writeln!(
                r,
                "Engine stream: {} [{}] {} Hz, {} ch {}, outputs {}-{}, buffer {}",
                s.device,
                s.host,
                s.sample_rate,
                s.channels,
                s.sample_format,
                out_first + 1,
                out_first + 2,
                s.buffer_frames.map_or("driver-managed".to_string(), |b| format!("{b} frames"))
            );
            s.sample_rate as f32
        }
        None => {
            let _ = writeln!(r, "Engine stream: not started (press Play once, then run again)");
            problems.push("The audio engine was not running.".into());
            48_000.0
        }
    };

    // live health window
    let before = io.0.lock().map(|g| g.stats()).ok();
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    let after = io.0.lock().map(|g| g.stats()).ok();
    if let (Some(b), Some(a)) = (before, after) {
        let cbs = a.callbacks.saturating_sub(b.callbacks);
        let xr = a.xruns.saturating_sub(b.xruns);
        let miss = a.over_budget.saturating_sub(b.over_budget);
        let _ = writeln!(
            r,
            "Live check ({seconds} s{}): {cbs} callbacks, {xr} xruns, {miss} deadline misses",
            if playing { ", transport playing" } else { ", transport stopped" }
        );
        let _ = writeln!(
            r,
            "  callback size {}-{} frames, peak load since start {:.0}%, priority {}",
            a.min_callback_frames,
            a.max_callback_frames,
            a.peak_load * 100.0,
            rt_text(match a.rt_priority {
                sf_io::RT_MMCSS_OK => "mmcss",
                sf_io::RT_MMCSS_FAILED => "mmcss-failed",
                _ => "host",
            })
        );
        if stream.is_some() && cbs == 0 {
            problems.push("No audio callbacks during the check: the output is stalled.".into());
        }
        if xr > 0 || miss > 0 {
            problems.push(format!("Dropouts during the check ({xr} xruns, {miss} deadline misses)."));
        }
        if a.rt_priority == sf_io::RT_MMCSS_FAILED {
            problems.push("The audio thread could not get real-time priority (MMCSS).".into());
        }
        if a.device_lost {
            problems.push("The output device reported itself lost.".into());
        }
    }

    // input + latency
    if let Ok(g) = io.0.lock() {
        match g.input_info() {
            Some(i) => {
                let s = g.stats();
                let _ = writeln!(
                    r,
                    "Input: {} [{}] {} Hz, {} ch {}; peak {:.1} dBFS; input xruns {}",
                    i.device,
                    i.host,
                    i.sample_rate,
                    i.channels,
                    i.sample_format,
                    20.0 * (s.input_peak.max(1e-9) as f64).log10(),
                    s.input_xruns
                );
                if g.input_silent() {
                    problems.push(
                        "The input delivers exact silence: check Windows microphone privacy settings.".into(),
                    );
                }
            }
            None => {
                let _ = writeln!(r, "Input: not open (opens on the first recording)");
            }
        }
        let l = g.latency();
        let _ = writeln!(
            r,
            "Record compensation: {:.1} ms ({})",
            l.compensation_ms,
            l.measured_roundtrip_ms.map_or("unmeasured".to_string(), |m| format!("measured round trip {m:.1} ms"))
        );
    }

    // CPU headroom on the demo song
    let _ = writeln!(r, "CPU headroom (demo song, 128-frame blocks @ {rate} Hz, one core):");
    let demo: Result<ProjectJSON, _> = serde_json::from_str(include_str!("../assets/demo-project.json"));
    match demo {
        Ok(p) => {
            let scope = p
                .scenes
                .first()
                .map(|s| RenderScope::Scene { scene_id: s.id.clone().unwrap_or_else(|| s.name.clone()) })
                .unwrap_or(RenderScope::Arr);
            match sf_render::headroom(&p, &scope, 128, rate, 2.0, 32) {
                Ok(steps) => {
                    for s in &steps {
                        let _ = writeln!(r, "  {:>2}x: mean {:.0}%  p99 {:.0}%  max {:.0}%", s.k, s.mean_pct, s.p99_pct, s.max_pct);
                    }
                    let fit = sf_render::headroom_fit(&steps);
                    let _ = writeln!(r, "  RESULT: {fit}x the demo song fits in 70% of the deadline");
                    if fit < 2 {
                        problems.push(format!(
                            "Low CPU headroom ({fit}x the demo song): use a larger buffer or report this."
                        ));
                    }
                }
                Err(e) => {
                    let _ = writeln!(r, "  failed: {e}");
                }
            }
        }
        Err(e) => {
            let _ = writeln!(r, "  demo project unreadable: {e}");
        }
    }

    let _ = writeln!(r);
    if problems.is_empty() {
        let _ = writeln!(r, "RESULT: no problems found");
    } else {
        let _ = writeln!(r, "PROBLEMS:");
        for p in &problems {
            let _ = writeln!(r, "  - {p}");
        }
    }
    r
}

/// Run the Diagnostics (about `seconds` + 10 s) on a background thread and
/// return the report text.
#[tauri::command]
async fn audio_diagnostics(app: tauri::AppHandle, seconds: Option<u64>) -> Result<String, String> {
    let secs = seconds.unwrap_or(15).clamp(3, 120);
    tauri::async_runtime::spawn_blocking(move || diagnostics_report(&app, secs))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn latency_get(io: State<'_, AudioState>) -> Result<LatencyDto, String> {
    Ok(latency_dto(io.0.lock().map_err(|_| "audio state poisoned")?.latency()))
}

/// Manual compensation override in ms (null = back to automatic).
#[tauri::command]
fn latency_set(io: State<'_, AudioState>, ms: Option<f64>) -> Result<LatencyDto, String> {
    let io = io.0.lock().map_err(|_| "audio state poisoned")?;
    io.set_latency_override(ms);
    Ok(latency_dto(io.latency()))
}

// ---- audio-device supervisor (docs/WINDOWS_PORT_PLAN.md 3.4) ----

/// Publish an automatic-restart message for the UI (toasted once per new
/// text, so a retry loop while a device stays unplugged doesn't spam).
fn post_notice(eng: &EngineState, msg: String) {
    if let Ok(mut e) = eng.0.lock() {
        if e.notice.as_deref() != Some(msg.as_str()) {
            e.notice = Some(msg);
            e.notice_seq += 1;
        }
    }
}

/// Re-open the engine stream after a device loss / stall / default change:
/// the same request first (the device may be back), then the driver's
/// default device, then the PLATFORM default driver's default device (an
/// unplugged ASIO interface can leave no ASIO driver at all) — so playback
/// is never left silent. The user's saved choice is untouched: the next
/// launch / Apply tries it again.
fn restart_engine(io: &AudioState, eng: &EngineState, req: sf_io::StreamRequest, reason: &str) -> String {
    let switched = |info: StreamDto| format!("{reason}. Switched to \"{}\" (playback stopped).", info.device);
    let first = match do_engine_start(io, eng, req.device.clone(), req.buffer_frames) {
        Ok(info) => return format!("{reason}. Audio restarted on \"{}\" (playback stopped).", info.device),
        Err(e) => e,
    };
    if req.device.is_some() {
        if let Ok(info) = do_engine_start(io, eng, None, req.buffer_frames) {
            return switched(info);
        }
    }
    if req.host.is_some() {
        if let Ok(g) = io.0.lock() {
            g.set_host(None);
        }
        if let Ok(info) = do_engine_start(io, eng, None, req.buffer_frames) {
            return switched(info);
        }
        // nothing worked: retry the user's driver later, not the fallback
        if let Ok(g) = io.0.lock() {
            g.set_host(req.host.clone());
        }
    }
    if let Ok(g) = io.0.lock() {
        g.retry_engine_later(req);
    }
    format!("{reason}. Audio could not restart: {first}")
}

/// One watchdog tick: ask sf-io which streams need restarting and do it.
fn supervise(app: &tauri::AppHandle) {
    let io = app.state::<AudioState>();
    let eng = app.state::<EngineState>();
    let (health, out_req, in_req) = match io.0.lock() {
        Ok(g) => (g.check_health(), g.engine_request(), g.input_request()),
        Err(_) => return,
    };
    if let (Some(reason), Some(req)) = (health.output, out_req) {
        let msg = restart_engine(&io, &eng, req, &reason);
        post_notice(&eng, msg);
    }
    if let (Some(reason), Some(req)) = (health.input, in_req) {
        let res = {
            let Ok(g) = io.0.lock() else { return };
            g.start_input(req.device.clone(), req.buffer_frames).or_else(|e| {
                if req.device.is_some() {
                    g.start_input(None, req.buffer_frames)
                } else {
                    Err(e)
                }
            })
        };
        let msg = match res {
            Ok(info) => format!("{reason}. Recording input is now \"{}\".", info.device),
            Err(e) => format!("{reason}. Recording input could not restart: {e}"),
        };
        post_notice(&eng, msg);
    }
}

/// SF_SMOKE=1: headless self-test — start the engine on the default device,
/// load the bundled demo song, launch scene 0, play 3 s, print a verdict and
/// exit. Lets CI/dev validate the native audio path without clicking the UI.
fn run_smoke(app: tauri::AppHandle) {
    let verdict = (|| -> Result<String, String> {
        let io = app.state::<AudioState>();
        let eng = app.state::<EngineState>();
        let info = do_engine_start(&io, &eng, None, None)?;
        println!(
            "SMOKE stream: {} @ {} Hz (buffer {:?})",
            info.device, info.sample_rate, info.buffer_frames
        );
        let demo: Value = serde_json::from_str(include_str!("../assets/demo-project.json"))
            .map_err(|e| e.to_string())?;
        let loaded = do_project_load(&eng, demo)?;
        let scene_id = {
            let e = eng.0.lock().map_err(|_| "engine state poisoned")?;
            e.project
                .as_ref()
                .and_then(|p| p.scenes.first())
                .map(|s| s.id.clone().unwrap_or_else(|| s.name.clone()))
                .ok_or("demo project has no scenes")?
        };
        let batch: Vec<(&str, &str)> = loaded.tracks.iter().map(|t| (t.as_str(), scene_id.as_str())).collect();
        do_clips_launch(&eng, &batch, Some(0.0))?;
        let mut max_peak = 0.0f32;
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            let e = eng.0.lock().map_err(|_| "engine state poisoned")?;
            if let Some(ctl) = e.ctl.as_ref() {
                max_peak = max_peak.max(ctl.snapshot().master.peak);
            }
        }
        let xruns = io.0.lock().map_err(|_| "audio state poisoned")?.stats().xruns;
        let peak_db = 20.0 * max_peak.max(1e-6).log10();
        if peak_db > -40.0 && xruns == 0 {
            Ok(format!("SMOKE ok: peak={peak_db:+.1} dBFS xruns={xruns}"))
        } else {
            Err(format!("SMOKE FAIL: peak={peak_db:+.1} dBFS xruns={xruns}"))
        }
    })();
    match verdict {
        Ok(msg) => {
            println!("{msg}");
            std::process::exit(0);
        }
        Err(msg) => {
            println!("{msg}");
            std::process::exit(1);
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .manage(AudioState(Mutex::new(sf_io::IoHandle::spawn())))
        .manage(EngineState(Mutex::new(EngineInner::default())))
        .setup(|app| {
            // Windows: keep the audio process out of "Efficiency mode"
            // (no-op elsewhere; see sf_io::prefer_performance)
            sf_io::prefer_performance();
            if std::env::var("SF_SMOKE").ok().as_deref() == Some("1") {
                let handle = app.handle().clone();
                std::thread::spawn(move || run_smoke(handle));
            }
            // device watchdog: restarts streams after unplug / driver reset /
            // default-device change (WASAPI ends a stream on any error)
            let handle = app.handle().clone();
            std::thread::Builder::new()
                .name("sf-audio-supervisor".into())
                .spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                    supervise(&handle);
                })
                .map_err(|e| e.to_string())?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            audio_hosts,
            audio_devices,
            audio_start_tone,
            audio_stop,
            audio_stats,
            engine_start,
            project_load,
            param_set,
            transport_play,
            transport_stop,
            clip_launch,
            clips_launch,
            clip_stop,
            note_preview,
            metronome,
            set_tempo,
            engine_snapshot,
            sample_put,
            sample_put_raw,
            save_download,
            sample_list,
            export_mixdown,
            input_devices,
            input_start,
            input_stop,
            input_info,
            record_take_start,
            record_take_stop,
            master_capture_start,
            master_capture_stop,
            latency_measure,
            latency_get,
            audio_diagnostics,
            latency_set
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_roundtrips_encode_uri_component() {
        // encodeURIComponent("Kick 01 (é).wav")
        assert_eq!(percent_decode("Kick%2001%20(%C3%A9).wav"), "Kick 01 (é).wav");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%zz%"), "bad%zz%");
    }

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(safe_file_name("My Song - Bass.wav"), "My Song - Bass.wav");
        assert_eq!(safe_file_name("a/b\\c:d*e?.wav"), "a_b_c_d_e_.wav");
        assert_eq!(safe_file_name("../../etc"), ".._.._etc");
        assert_eq!(safe_file_name("name. . "), "name");
        assert_eq!(safe_file_name("   "), "export");
        assert_eq!(safe_file_name(".."), "export");
    }

    #[test]
    fn unique_path_appends_counter() {
        let dir = std::env::temp_dir().join(format!("sf-unique-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = unique_path(&dir, "mix.wav");
        assert_eq!(a, dir.join("mix.wav"));
        std::fs::write(&a, b"x").unwrap();
        assert_eq!(unique_path(&dir, "mix.wav"), dir.join("mix (2).wav"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// End-to-end through Tauri's real command dispatcher (mock runtime):
    /// a raw f32le body + percent-encoded headers lands in the sample bank.
    #[test]
    fn sample_put_raw_dispatches_through_ipc() {
        use tauri::ipc::{CallbackFn, InvokeBody};
        let app = tauri::test::mock_builder()
            .manage(EngineState(Mutex::new(EngineInner::default())))
            .invoke_handler(tauri::generate_handler![sample_put_raw, sample_list])
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("mock app");
        let webview = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .expect("mock webview");
        let call = |cmd: &str, body: InvokeBody, headers: &[(&'static str, &str)]| {
            let mut map = tauri::http::HeaderMap::new();
            for (k, v) in headers {
                map.insert(*k, v.parse().unwrap());
            }
            tauri::test::get_ipc_response(
                &webview,
                tauri::webview::InvokeRequest {
                    cmd: cmd.into(),
                    callback: CallbackFn(0),
                    error: CallbackFn(1),
                    // the app's own page origin: Tauri's permission check
                    // rejects calls from any other origin, and it differs
                    // on Windows (WebView2 serves the app over http)
                    url: if cfg!(windows) { "http://tauri.localhost" } else { "tauri://localhost" }
                        .parse()
                        .unwrap(),
                    body,
                    headers: map,
                    invoke_key: tauri::test::INVOKE_KEY.into(),
                },
            )
        };
        // stereo: L = [0.25, 0.5], R = [-0.25, -0.5]
        let bytes: Vec<u8> = [0.25f32, 0.5, -0.25, -0.5].iter().flat_map(|x| x.to_le_bytes()).collect();
        let ok = call(
            "sample_put_raw",
            InvokeBody::Raw(bytes),
            &[("x-sf-id", "kick%2001%20%C3%A9"), ("x-sf-rate", "48000"), ("x-sf-channels", "2")],
        );
        assert!(ok.is_ok(), "{ok:?}");
        let ids = call("sample_list", InvokeBody::default(), &[]).unwrap();
        let ids: Vec<String> = ids.deserialize().unwrap();
        assert_eq!(ids, vec!["kick 01 é".to_string()]);
        let state = app.state::<EngineState>();
        let e = state.0.lock().unwrap();
        let s = e.bank.get("kick 01 é").unwrap();
        assert_eq!((s.l.as_slice(), s.r.as_slice(), s.sample_rate), (&[0.25, 0.5][..], &[-0.25, -0.5][..], 48000.0));
        drop(e);
        // odd byte count for 2 channels → rejected, not mis-split
        let bad = call(
            "sample_put_raw",
            InvokeBody::Raw(vec![0; 12]),
            &[("x-sf-id", "x"), ("x-sf-rate", "48000"), ("x-sf-channels", "2")],
        );
        assert!(bad.is_err());
        // JSON body instead of raw → rejected
        let json = call(
            "sample_put_raw",
            InvokeBody::Json(serde_json::json!({})),
            &[("x-sf-id", "x"), ("x-sf-rate", "48000"), ("x-sf-channels", "1")],
        );
        assert!(json.is_err());
    }

    /// The Diagnostics report runs end-to-end (mock runtime, no engine
    /// started, possibly no audio devices) and flags the stopped engine.
    #[test]
    fn diagnostics_report_has_every_section() {
        let app = tauri::test::mock_builder()
            .manage(AudioState(Mutex::new(sf_io::IoHandle::spawn())))
            .manage(EngineState(Mutex::new(EngineInner::default())))
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("mock app");
        let r = diagnostics_report(app.handle(), 0);
        for section in [
            "diagnostics",
            "System:",
            "Drivers available:",
            "Output devices:",
            "Input devices:",
            "Engine stream: not started",
            "Live check (0 s",
            "Input: not open",
            "Record compensation:",
            "CPU headroom",
            "RESULT:",
            "PROBLEMS:",
            "The audio engine was not running.",
        ] {
            assert!(r.contains(section), "missing {section:?} in:\n{r}");
        }
    }

    /// Render the first bar of the demo song with an engine driven only
    /// through the IPC helpers; returns each track's peak level.
    fn first_bar_peaks(launch: impl Fn(&EngineState)) -> Vec<(String, f32)> {
        let mut engine = sf_engine::Engine::new(48_000.0);
        let ctl = engine.split_control();
        let eng = EngineState(Mutex::new(EngineInner { ctl: Some(ctl), ..Default::default() }));
        let demo: Value = serde_json::from_str(include_str!("../assets/demo-project.json")).unwrap();
        let ids = do_project_load(&eng, demo).unwrap().tracks;
        let (mut l, mut r) = (vec![0.0f32; 128], vec![0.0f32; 128]);
        engine.process(&mut l, &mut r); // adopt the new graph
        launch(&eng);
        // 90 % of one bar at 112 bpm
        let frames = (4.0 * 60.0 / 112.0 * 48_000.0 * 0.9) as usize;
        let mut peaks = vec![0.0f32; ids.len()];
        for _ in 0..frames / 128 {
            engine.process(&mut l, &mut r);
            let snap = eng.0.lock().unwrap().ctl.as_ref().unwrap().snapshot();
            for (p, m) in peaks.iter_mut().zip(snap.tracks.iter()) {
                *p = p.max(m.peak);
            }
        }
        ids.into_iter().zip(peaks).collect()
    }

    /// Bug report: "track 1 starts on time, the other tracks lag one bar".
    /// Demo scene s3 has Drums, Bass and Chords clips, each with a note at
    /// tick 0 — launched together from stopped, all three must sound in bar 1.
    #[test]
    fn scene_launch_from_stopped_starts_every_clip_in_bar_one() {
        let scene = [("tDrums", "s3"), ("tBass", "s3"), ("tChords", "s3")];
        let peaks = first_bar_peaks(|eng| do_clips_launch(eng, &scene, Some(0.0)).unwrap());
        for (t, _) in scene {
            let peak = peaks.iter().find(|(id, _)| id == t).unwrap().1;
            assert!(peak > 1e-4, "{t} silent in bar 1 (peak {peak}); all: {peaks:?}");
        }
    }

    /// The re-push path: stop, relaunch as a batch WITHOUT starting, then
    /// play — every clip must still come in at bar 1.
    #[test]
    fn batch_launch_without_start_waits_for_transport_play() {
        let scene = [("tDrums", "s3"), ("tBass", "s3"), ("tChords", "s3")];
        let peaks = first_bar_peaks(|eng| {
            do_clips_launch(eng, &scene, None).unwrap();
            assert!(!eng.0.lock().unwrap().playing, "None must not start the transport");
            let mut e = eng.0.lock().unwrap();
            e.playing = true;
            e.ctl.as_mut().unwrap().push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        });
        for (t, _) in scene {
            assert!(peaks.iter().find(|(id, _)| id == t).unwrap().1 > 1e-4, "{t} silent in bar 1: {peaks:?}");
        }
    }

    #[test]
    fn f32le_decodes_little_endian() {
        let v = [0.5f32, -1.0];
        let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
        assert_eq!(f32le(&bytes), v);
    }
}
