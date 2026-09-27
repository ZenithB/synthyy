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
}

#[derive(Serialize, Clone)]
struct StreamDto {
    device: String,
    sample_rate: u32,
    buffer_frames: Option<u32>,
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
    Ok(StreamDto {
        device: info.device,
        sample_rate: info.sample_rate,
        buffer_frames: info.buffer_frames,
    })
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

fn do_clip_launch(eng: &EngineState, track_id: &str, scene_id: &str) -> Result<(), String> {
    let mut e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    let was_playing = e.playing;
    let Some(ctl) = e.ctl.as_mut() else { return Ok(()) };
    let (Some(track), Some(clip)) =
        (ctl.resolver().track(track_id), ctl.resolver().scene(scene_id))
    else {
        return Ok(()); // unknown id (doc ahead of last push) — repush will fix
    };
    ctl.push_cmd(Cmd::ClipLaunch { track, clip });
    // engine-tone launchClip starts the transport itself; in the split that's
    // this layer's job (Cmd docs: stopped launches anchor at 0 and wait)
    if !was_playing {
        ctl.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        e.playing = true;
        e.arr_mode = false;
    }
    Ok(())
}

// ---- Phase 0 commands (unchanged surface) ----

/// List selectable output devices.
#[tauri::command]
fn audio_devices() -> Vec<DeviceDto> {
    sf_io::output_devices()
        .into_iter()
        .map(|d| DeviceDto { name: d.name, is_default: d.is_default })
        .collect()
}

/// Open an output stream (default device when `device` is null) at
/// `buffer_frames` and play the Phase 0 test tone. Returns what the backend
/// actually negotiated.
#[tauri::command]
fn audio_start_tone(
    state: State<'_, AudioState>,
    device: Option<String>,
    buffer_frames: Option<u32>,
) -> Result<StreamDto, String> {
    let io = state.0.lock().map_err(|_| "audio state poisoned")?;
    let info = io.start_tone(device, buffer_frames, 440.0, 0.1)?;
    Ok(StreamDto {
        device: info.device,
        sample_rate: info.sample_rate,
        buffer_frames: info.buffer_frames,
    })
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
    })
}

// ---- Phase 1: engine hosting ----

/// Open the output stream hosting a fresh Engine at the device rate.
#[tauri::command]
fn engine_start(
    io: State<'_, AudioState>,
    eng: State<'_, EngineState>,
    device: Option<String>,
    buffer_frames: Option<u32>,
) -> Result<StreamDto, String> {
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
    Ok(bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect())
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
    let stats = io.0.lock().map_err(|_| "audio state poisoned")?.stats();
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

/// List selectable input devices.
#[tauri::command]
fn input_devices() -> Vec<DeviceDto> {
    sf_io::input_devices()
        .into_iter()
        .map(|d| DeviceDto { name: d.name, is_default: d.is_default })
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
    Ok(StreamDto { device: info.device, sample_rate: info.sample_rate, buffer_frames: info.buffer_frames })
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
        .map(|i| StreamDto { device: i.device, sample_rate: i.sample_rate, buffer_frames: i.buffer_frames }))
}

/// Arm the input. Returns the take id and the playhead at arm time (the
/// compensation delivered with the take is relative to THIS tick).
#[tauri::command]
fn record_take_start(io: State<'_, AudioState>, eng: State<'_, EngineState>) -> Result<TakeStartDto, String> {
    let e = eng.0.lock().map_err(|_| "engine state poisoned")?;
    let take_id = io.0.lock().map_err(|_| "audio state poisoned")?.start_take()?;
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
        for tid in &loaded.tracks {
            do_clip_launch(&eng, tid, &scene_id)?;
        }
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
            if std::env::var("SF_SMOKE").ok().as_deref() == Some("1") {
                let handle = app.handle().clone();
                std::thread::spawn(move || run_smoke(handle));
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
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
            clip_stop,
            note_preview,
            metronome,
            set_tempo,
            engine_snapshot,
            sample_put,
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
            latency_set
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
