//! cpal glue: device enumeration, stream config, and the real-time callback
//! host (CoreAudio on macOS, WASAPI on Windows; ASIO later behind a feature).
//!
//! Phase 0 (RUST_REWRITE_PLAN.md §5): open an output stream at a requested
//! buffer size, play a test tone, and measure what actually happened —
//! callback load (time spent ÷ callback budget), xruns, achieved buffer size.
//! These stats are the data source the PerfMonitor UI will read.
//!
//! Phase 6 adds recording: a separate INPUT stream (duplex-in-one-stream is
//! not portable) whose callback pushes frames into an `rtrb` ring drained by
//! a writer thread into takes ([`capture`]), a post-limiter master tap in the
//! output callback (desktop "Record Output"), and loopback latency
//! calibration (a chirp mixed into the output, found again on the input by
//! cross-correlation). Input monitoring (hearing yourself through the engine)
//! is NOT part of this pass.
//!
//! cpal's `Stream` is !Send, so a dedicated manager thread owns it; callers
//! hold an [`IoHandle`] (Send + Sync) and talk over a channel. The audio
//! callback itself touches only pre-built state and atomics — no allocation,
//! no locks (RT rules, §4).

pub mod capture;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use capture::{Anchor, ClockOrigin, Take, Tap, RING_SECONDS};

/// One selectable output device.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
}

/// What the opened stream actually negotiated (may differ from the request).
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub device: String,
    pub sample_rate: u32,
    /// Achieved buffer size in frames, if the backend reports one.
    pub buffer_frames: Option<u32>,
}

/// Live callback health, updated from the audio thread with atomics only.
#[derive(Default)]
pub struct Stats {
    /// Callbacks served since the stream started.
    pub callbacks: AtomicU64,
    /// Stream error callbacks (cpal reports overruns/underruns here).
    pub xruns: AtomicU32,
    /// Worst observed callback duration, nanoseconds.
    pub max_callback_nanos: AtomicU64,
    /// Most recent callback duration, nanoseconds.
    pub last_callback_nanos: AtomicU64,
    /// Nanosecond budget per callback (frames / sample_rate); 0 until known.
    pub budget_nanos: AtomicU64,
    /// Callbacks that exceeded their budget (deadline misses = audible risk).
    pub over_budget: AtomicU64,
    /// Input stream callbacks served.
    pub input_callbacks: AtomicU64,
    /// Input stream error callbacks (overruns).
    pub input_xruns: AtomicU32,
    /// Input peak (f32 bits) over the most recent input callback.
    pub input_peak_bits: AtomicU32,
}

/// Point-in-time copy of [`Stats`] for display / IPC.
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub callbacks: u64,
    pub xruns: u32,
    pub max_callback_us: f64,
    pub last_callback_us: f64,
    pub budget_us: f64,
    /// max_callback / budget — >1.0 means a deadline was missed.
    pub peak_load: f64,
    pub over_budget: u64,
    pub input_callbacks: u64,
    pub input_xruns: u32,
    /// Peak |sample| in the latest input callback (0 when no input stream).
    pub input_peak: f32,
}

impl Stats {
    pub fn snapshot(&self) -> StatsSnapshot {
        let budget = self.budget_nanos.load(Ordering::Relaxed);
        let max = self.max_callback_nanos.load(Ordering::Relaxed);
        StatsSnapshot {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            xruns: self.xruns.load(Ordering::Relaxed),
            max_callback_us: max as f64 / 1e3,
            last_callback_us: self.last_callback_nanos.load(Ordering::Relaxed) as f64 / 1e3,
            budget_us: budget as f64 / 1e3,
            peak_load: if budget > 0 { max as f64 / budget as f64 } else { 0.0 },
            over_budget: self.over_budget.load(Ordering::Relaxed),
            input_callbacks: self.input_callbacks.load(Ordering::Relaxed),
            input_xruns: self.input_xruns.load(Ordering::Relaxed),
            input_peak: f32::from_bits(self.input_peak_bits.load(Ordering::Relaxed)),
        }
    }
}

/// List output devices on the default host.
pub fn output_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.name().ok());
    let mut out = Vec::new();
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                let is_default = Some(&name) == default_name.as_ref();
                out.push(DeviceInfo { name, is_default });
            }
        }
    }
    out
}

/// List input devices on the default host.
pub fn input_devices() -> Vec<DeviceInfo> {
    let host = cpal::default_host();
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let mut out = Vec::new();
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                let is_default = Some(&name) == default_name.as_ref();
                out.push(DeviceInfo { name, is_default });
            }
        }
    }
    out
}

/// Hooks the ENGINE output callback exposes to the recording layer: the
/// post-limiter master tap and the calibration-chirp trigger. Built per
/// engine stream; replaced when the stream is.
struct OutputHooks {
    master: Arc<Tap>,
    capture_on: AtomicBool,
    /// Calibration chirp: u32::MAX = idle; 0 = requested (callback starts it
    /// at its next buffer); otherwise = samples already emitted.
    impulse_pos: AtomicU32,
    /// Output-stream frame index where the last chirp began.
    impulse_frame: AtomicU64,
    buffer_frames: Option<u32>,
}

const IMPULSE_IDLE: u32 = u32::MAX;

/// Input stream bookkeeping held by the IoHandle (the stream itself lives
/// on the manager thread).
struct InputHooks {
    tap: Arc<Tap>,
    info: StreamInfo,
}

/// What the recording layer keeps between calls.
#[derive(Default)]
struct RecState {
    out: Option<Arc<OutputHooks>>,
    input: Option<InputHooks>,
    /// Loopback-measured extra round-trip (DAC→ADC beyond what the stream
    /// timestamps account for), in ms. None = unmeasured (treated as 0).
    roundtrip_ms: Option<f64>,
    /// Manual override of the whole compensation, ms (latency_set).
    override_ms: Option<f64>,
    take_counter: u64,
}

/// A delivered recording: planar channels at the input rate, already
/// shifted left by the latency compensation.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
    /// Compensation that was removed from the front, in frames.
    pub compensation_frames: i64,
}

/// Current latency-compensation figures (all ms).
#[derive(Debug, Clone, Copy, Default)]
pub struct LatencyInfo {
    /// Compensation that the next take would get.
    pub compensation_ms: f64,
    /// Loopback-measured round-trip component (None = unmeasured).
    pub measured_roundtrip_ms: Option<f64>,
    /// Manual override (None = automatic).
    pub override_ms: Option<f64>,
    /// Buffer-based estimate (output buffer + input buffer) — the fallback.
    pub estimate_ms: f64,
}

type EngineStart = Result<(StreamInfo, sf_engine::EngineControl, Arc<OutputHooks>), String>;

enum Cmd {
    StartInput {
        device: Option<String>,
        buffer_frames: Option<u32>,
        tap: Arc<Tap>,
        reply: mpsc::Sender<Result<(StreamInfo, Arc<Tap>), String>>,
    },
    StopInput,
    StartTone {
        device: Option<String>,
        buffer_frames: Option<u32>,
        freq_hz: f32,
        gain: f32,
        reply: mpsc::Sender<Result<StreamInfo, String>>,
    },
    StartEngine {
        device: Option<String>,
        buffer_frames: Option<u32>,
        reply: mpsc::Sender<EngineStart>,
    },
    Stop,
    Quit,
}

/// Set FTZ/DAZ on the CALLING thread so device DSP never hits denormal
/// slow paths (RUST_ARCH.md "Block model" — devices may assume it). Called
/// once from inside the audio callback, i.e. on the actual audio thread.
fn set_flush_denormals() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let mut fpcr: u64;
        core::arch::asm!("mrs {fpcr}, fpcr", fpcr = out(reg) fpcr);
        fpcr |= 1 << 24; // FZ: flush denormal results/inputs to zero
        core::arch::asm!("msr fpcr, {fpcr}", fpcr = in(reg) fpcr);
    }
    #[cfg(target_arch = "x86_64")]
    unsafe {
        use core::arch::x86_64::{_mm_getcsr, _mm_setcsr};
        _mm_setcsr(_mm_getcsr() | 0x8040); // FTZ | DAZ in MXCSR
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {}
}

/// Handle to the manager thread that owns the (!Send) cpal stream.
pub struct IoHandle {
    tx: mpsc::Sender<Cmd>,
    stats: Arc<Stats>,
    rec: Mutex<RecState>,
}

impl IoHandle {
    /// Spawn the stream-owner thread. One per app.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let stats = Arc::new(Stats::default());
        let clock = Arc::new(ClockOrigin::default());
        let thread_stats = stats.clone();
        let thread_clock = clock.clone();
        std::thread::Builder::new()
            .name("sf-io-manager".into())
            .spawn(move || manager_loop(rx, thread_stats, thread_clock))
            .expect("spawn sf-io manager");
        Self { tx, stats, rec: Mutex::new(RecState::default()) }
    }

    fn rec(&self) -> std::sync::MutexGuard<'_, RecState> {
        self.rec.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Open an output stream on `device` (default when None) at
    /// `buffer_frames` (backend default when None) and play a sine test tone.
    /// Replaces any running stream.
    pub fn start_tone(
        &self,
        device: Option<String>,
        buffer_frames: Option<u32>,
        freq_hz: f32,
        gain: f32,
    ) -> Result<StreamInfo, String> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::StartTone { device, buffer_frames, freq_hz, gain, reply })
            .map_err(|_| "sf-io manager thread is gone".to_string())?;
        rx.recv().map_err(|_| "sf-io manager dropped the reply".to_string())?
    }

    /// Open an output stream hosting a fresh [`sf_engine::Engine`] built at
    /// the device's rate. The Engine itself is moved into the audio callback;
    /// the returned [`sf_engine::EngineControl`] is the Send-safe handle for
    /// commands / project loads / meter snapshots. Replaces any running
    /// stream (a previous control handle goes dead: its pushes are dropped).
    pub fn start_engine(
        &self,
        device: Option<String>,
        buffer_frames: Option<u32>,
    ) -> Result<(StreamInfo, sf_engine::EngineControl), String> {
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::StartEngine { device, buffer_frames, reply })
            .map_err(|_| "sf-io manager thread is gone".to_string())?;
        let (info, ctl, hooks) =
            rx.recv().map_err(|_| "sf-io manager dropped the reply".to_string())??;
        self.rec().out = Some(hooks);
        Ok((info, ctl))
    }

    /// Stop and drop the current OUTPUT stream (if any). The input stream
    /// (if any) keeps running.
    pub fn stop(&self) {
        self.rec().out = None;
        let _ = self.tx.send(Cmd::Stop);
    }

    // ---- recording (Phase 6) ----

    /// Open an INPUT stream on `device` (default when None) at the device's
    /// default rate/channel count. Frames flow callback → ring → writer
    /// thread, and are kept only while a take is armed. Replaces any running
    /// input stream (an armed take is abandoned).
    pub fn start_input(
        &self,
        device: Option<String>,
        buffer_frames: Option<u32>,
    ) -> Result<StreamInfo, String> {
        let (dev_name, cfg, _dev) = resolve_input(device.clone(), buffer_frames)?;
        let tap = Tap::new(cfg.channels as usize, cfg.sample_rate.0);
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::StartInput { device, buffer_frames, tap, reply })
            .map_err(|_| "sf-io manager thread is gone".to_string())?;
        let (info, tap) = rx.recv().map_err(|_| "sf-io manager dropped the reply".to_string())??;
        debug_assert_eq!(info.device, dev_name);
        self.rec().input = Some(InputHooks { tap, info: info.clone() });
        Ok(info)
    }

    /// Close the input stream.
    pub fn stop_input(&self) {
        self.rec().input = None;
        let _ = self.tx.send(Cmd::StopInput);
    }

    /// The running input stream, if any.
    pub fn input_info(&self) -> Option<StreamInfo> {
        self.rec().input.as_ref().map(|i| i.info.clone())
    }

    /// Arm the input: frames from NOW are kept. Returns the take id.
    pub fn start_take(&self) -> Result<u64, String> {
        let mut r = self.rec();
        let out_frame = r.out.as_ref().map(|o| o.master.total_frames()).unwrap_or(0);
        let input = r.input.as_ref().ok_or("no input stream — call start_input first")?;
        input.tap.arm(out_frame);
        r.take_counter += 1;
        Ok(r.take_counter)
    }

    /// Disarm and deliver the take, shifted left by the compensation.
    pub fn stop_take(&self) -> Result<Recorded, String> {
        let (take, out_frame_at_arm, out, roundtrip, override_ms, est) = {
            let r = self.rec();
            let input = r.input.as_ref().ok_or("no input stream")?;
            let (take, out_frame) = input
                .tap
                .disarm(Duration::from_secs(2))
                .ok_or("no take armed")?;
            (
                take,
                out_frame,
                r.out.clone(),
                r.roundtrip_ms.unwrap_or(0.0),
                r.override_ms,
                self.estimate_ms_locked(&r),
            )
        };
        let comp_ms = match override_ms {
            Some(ms) => ms,
            None => {
                let out_anchor = out.as_ref().and_then(|o| o.master.anchor());
                let out_sr = out.as_ref().map(|o| o.master.sample_rate()).unwrap_or(0);
                timestamp_latency_ms(&take, out_frame_at_arm, out_anchor, out_sr).unwrap_or(est)
                    + roundtrip
            }
        };
        let comp_ms = comp_ms.clamp(0.0, 500.0);
        let comp_frames = (comp_ms / 1000.0 * take.sample_rate as f64).round() as i64;
        let mut channels = take.channels;
        capture::shift_left(&mut channels, comp_frames);
        Ok(Recorded { sample_rate: take.sample_rate, channels, compensation_frames: comp_frames })
    }

    /// Begin tapping the post-limiter master (desktop "Record Output").
    pub fn start_master_capture(&self) -> Result<(), String> {
        let r = self.rec();
        let out = r.out.as_ref().ok_or("engine stream not running")?;
        // GATED producer: the callback only pushes master frames while
        // capture_on — arm_gated re-syncs the delivery cursor so the frames
        // that follow aren't all discarded as "before start" (empty take).
        out.master.arm_gated(0);
        out.capture_on.store(true, Ordering::Release);
        Ok(())
    }

    /// Stop the master tap and deliver what was captured (no compensation:
    /// it IS the output).
    pub fn stop_master_capture(&self) -> Result<Recorded, String> {
        let out = self.rec().out.clone().ok_or("engine stream not running")?;
        // disarm BEFORE closing the gate: `end` is the stream frame counter,
        // which already includes frames this callback noted but has not pushed
        // yet. Closing the gate first strands those, so the wait could only
        // ever end by timing out (a 2 s hang on every stop).
        let (take, _) = out.master.disarm(Duration::from_secs(2)).ok_or("master capture not running")?;
        out.capture_on.store(false, Ordering::Release);
        Ok(Recorded { sample_rate: take.sample_rate, channels: take.channels, compensation_frames: 0 })
    }

    /// Loopback calibration: play a chirp through the engine output, record
    /// the input, cross-correlate. Needs both streams running and some path
    /// from output to input (cable, or speakers→mic). Stores the result for
    /// subsequent takes and returns the measured round-trip in ms; Err when
    /// no correlation peak is found (nothing came back).
    pub fn measure_roundtrip_ms(&self) -> Result<f64, String> {
        let (out, in_tap) = {
            let r = self.rec();
            (
                r.out.clone().ok_or("engine stream not running")?,
                r.input.as_ref().map(|i| i.tap.clone()).ok_or("no input stream")?,
            )
        };
        if in_tap.is_armed() {
            return Err("a take is in progress".into());
        }
        let out_sr = out.master.sample_rate();
        let in_sr = in_tap.sample_rate();
        let out_frame_at_arm = out.master.total_frames();
        in_tap.arm(out_frame_at_arm);
        // settle, fire, then leave room for up to ~600 ms of round trip
        std::thread::sleep(Duration::from_millis(150));
        out.impulse_pos.store(0, Ordering::Release);
        std::thread::sleep(Duration::from_millis(700));
        let (take, _) = in_tap.disarm(Duration::from_secs(2)).ok_or("input stream stopped")?;
        if out.impulse_pos.load(Ordering::Acquire) != IMPULSE_IDLE {
            return Err("chirp was not played (output callback not running?)".into());
        }
        let impulse_frame = out.impulse_frame.load(Ordering::Acquire);
        // template at the INPUT rate (streams may run at different rates)
        let template = capture::chirp(in_sr, 1.0);
        let mono: Vec<f32> = if take.channels.len() == 1 {
            take.channels[0].clone()
        } else {
            let n = take.channels[0].len();
            (0..n).map(|i| take.channels.iter().map(|c| c[i]).sum::<f32>() / take.channels.len() as f32).collect()
        };
        let pos = capture::find_delay(&mono, &template, 0.3)
            .ok_or("no correlation peak — is the output reaching the input (loopback cable / mic)?")?;
        // instants on the shared clock axis
        let out_anchor = out.master.anchor().ok_or("output stream has no timestamps yet")?;
        let in_anchor = take.start_anchor.ok_or("input stream has no timestamps yet")?;
        let played = out_anchor.instant_of(impulse_frame, out_sr);
        let captured = in_anchor.instant_of(take.start_frame + pos as u64, in_sr);
        // With perfect stream timestamps this is the pure DAC→ADC path (≈0
        // for a cable); any driver latency the backend under-reports shows
        // up here too — which is exactly the part takes need on top of the
        // timestamp-derived base (see stop_take).
        let extra_ms = (captured - played) / 1e6;
        if !(0.0..=1000.0).contains(&extra_ms) {
            return Err(format!("implausible round-trip {extra_ms:.1} ms"));
        }
        self.rec().roundtrip_ms = Some(extra_ms);
        Ok(extra_ms)
    }

    /// Current compensation figures.
    pub fn latency(&self) -> LatencyInfo {
        let r = self.rec();
        let est = self.estimate_ms_locked(&r);
        let auto = {
            let in_anchor = r.input.as_ref().and_then(|i| i.tap.anchor());
            let out = r.out.as_ref();
            let base = match (in_anchor, out.and_then(|o| o.master.anchor())) {
                (Some(ia), Some(oa)) => {
                    let in_sr = r.input.as_ref().map(|i| i.tap.sample_rate()).unwrap_or(1);
                    let out_sr = out.map(|o| o.master.sample_rate()).unwrap_or(1);
                    let in_frame = r.input.as_ref().map(|i| i.tap.total_frames()).unwrap_or(0);
                    let out_frame = out.map(|o| o.master.total_frames()).unwrap_or(0);
                    let d = (oa.instant_of(out_frame, out_sr) - ia.instant_of(in_frame, in_sr)) / 1e6;
                    if d.is_finite() && (0.0..=500.0).contains(&d) { d } else { est }
                }
                _ => est,
            };
            base + r.roundtrip_ms.unwrap_or(0.0)
        };
        LatencyInfo {
            compensation_ms: r.override_ms.unwrap_or(auto).clamp(0.0, 500.0),
            measured_roundtrip_ms: r.roundtrip_ms,
            override_ms: r.override_ms,
            estimate_ms: est,
        }
    }

    /// Force the compensation (ms); None returns to automatic.
    pub fn set_latency_override(&self, ms: Option<f64>) {
        self.rec().override_ms = ms.map(|m| m.clamp(0.0, 500.0));
    }

    /// Buffer-based fallback: output buffer + input buffer, in ms.
    fn estimate_ms_locked(&self, r: &RecState) -> f64 {
        let out_ms = r
            .out
            .as_ref()
            .and_then(|o| o.buffer_frames.map(|b| b as f64 / o.master.sample_rate() as f64 * 1000.0))
            .unwrap_or(0.0);
        let in_ms = r
            .input
            .as_ref()
            .and_then(|i| i.info.buffer_frames.map(|b| b as f64 / i.tap.sample_rate() as f64 * 1000.0))
            .unwrap_or(0.0);
        out_ms + in_ms
    }

    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }
}

impl Drop for IoHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Quit);
    }
}

/// Timestamp-derived part of the record compensation: how long after the
/// take's first frame was captured does the sound of the output frame that
/// was current at arm time actually leave the DAC. None when either stream
/// lacks timestamps or the result is implausible.
fn timestamp_latency_ms(
    take: &Take,
    out_frame_at_arm: u64,
    out_anchor: Option<Anchor>,
    out_sr: u32,
) -> Option<f64> {
    let oa = out_anchor?;
    let ia = take.start_anchor?;
    if out_sr == 0 {
        return None;
    }
    let played = oa.instant_of(out_frame_at_arm, out_sr);
    let captured = ia.instant_of(take.start_frame, take.sample_rate);
    let ms = (played - captured) / 1e6;
    (ms.is_finite() && (-50.0..=500.0).contains(&ms)).then_some(ms.max(0.0))
}

fn manager_loop(rx: mpsc::Receiver<Cmd>, stats: Arc<Stats>, clock: Arc<ClockOrigin>) {
    // The Stream lives HERE, on this thread, for its whole life. It is never
    // read — it's held purely so Drop (assigning None / a replacement) stops
    // the hardware stream — hence the underscore name.
    let mut _stream: Option<cpal::Stream> = None;
    let mut _input: Option<cpal::Stream> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::StartInput { device, buffer_frames, tap, reply } => {
                _input = None;
                match open_input_stream(device, buffer_frames, tap, &stats, &clock) {
                    Ok((s, info, tap)) => {
                        let _ = reply.send(Ok((info, tap)));
                        _input = Some(s);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::StopInput => _input = None,
            Cmd::StartTone { device, buffer_frames, freq_hz, gain, reply } => {
                _stream = None; // drop any previous stream first
                let res = open_tone_stream(device, buffer_frames, freq_hz, gain, &stats);
                match res {
                    Ok((s, info)) => {
                        let _ = reply.send(Ok(info));
                        _stream = Some(s);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::StartEngine { device, buffer_frames, reply } => {
                _stream = None;
                let res = open_engine_stream(device, buffer_frames, &stats, &clock);
                match res {
                    Ok((s, info, ctl, hooks)) => {
                        let _ = reply.send(Ok((info, ctl, hooks)));
                        _stream = Some(s);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::Stop => _stream = None,
            Cmd::Quit => break,
        }
    }
}

fn reset_stats(stats: &Stats) {
    stats.callbacks.store(0, Ordering::Relaxed);
    stats.xruns.store(0, Ordering::Relaxed);
    stats.max_callback_nanos.store(0, Ordering::Relaxed);
    stats.last_callback_nanos.store(0, Ordering::Relaxed);
    stats.budget_nanos.store(0, Ordering::Relaxed);
    stats.over_budget.store(0, Ordering::Relaxed);
}

/// Resolve the output device + a 2-channel config at its default rate,
/// honoring `buffer_frames` when the backend supports it.
fn open_output(
    device_name: Option<String>,
    buffer_frames: Option<u32>,
) -> Result<(cpal::Device, String, cpal::StreamConfig), String> {
    let host = cpal::default_host();
    let device = match &device_name {
        None => host
            .default_output_device()
            .ok_or("no default output device")?,
        Some(want) => host
            .output_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|n| &n == want).unwrap_or(false))
            .ok_or_else(|| format!("output device \"{want}\" not found"))?,
    };
    let name = device.name().map_err(|e| e.to_string())?;

    let default_cfg = device.default_output_config().map_err(|e| e.to_string())?;
    let sample_rate = default_cfg.sample_rate();
    let mut config = cpal::StreamConfig {
        channels: 2,
        sample_rate,
        buffer_size: cpal::BufferSize::Default,
    };
    if let Some(frames) = buffer_frames {
        // honor the backend's supported range; fall back to Default outside it
        if let cpal::SupportedBufferSize::Range { min, max } = default_cfg.buffer_size() {
            if frames >= *min && frames <= *max {
                config.buffer_size = cpal::BufferSize::Fixed(frames);
            }
        }
    }
    Ok((device, name, config))
}

fn open_tone_stream(
    device_name: Option<String>,
    buffer_frames: Option<u32>,
    freq_hz: f32,
    gain: f32,
    stats: &Arc<Stats>,
) -> Result<(cpal::Stream, StreamInfo), String> {
    let (device, name, config) = open_output(device_name, buffer_frames)?;
    let sample_rate = config.sample_rate;

    reset_stats(stats);
    let sr = sample_rate.0 as f32;
    let mut tone = sf_dsp::TestTone::new(freq_hz, sr, gain);
    let cb_stats = stats.clone();
    let err_stats = stats.clone();
    // budget is per-frame-count and varies per callback; computed inside.
    let ns_per_frame = 1e9 / sr as f64;

    let stream = device
        .build_output_stream(
            &config,
            move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
                let t0 = Instant::now();
                tone.process_interleaved_stereo(out);
                let dt = t0.elapsed().as_nanos() as u64;
                let frames = (out.len() / 2) as f64;
                let budget = (frames * ns_per_frame) as u64;
                cb_stats.budget_nanos.store(budget, Ordering::Relaxed);
                cb_stats.last_callback_nanos.store(dt, Ordering::Relaxed);
                cb_stats.max_callback_nanos.fetch_max(dt, Ordering::Relaxed);
                cb_stats.callbacks.fetch_add(1, Ordering::Relaxed);
                if dt > budget {
                    cb_stats.over_budget.fetch_add(1, Ordering::Relaxed);
                }
            },
            move |_e| {
                err_stats.xruns.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;

    Ok((
        stream,
        StreamInfo {
            device: name,
            sample_rate: sample_rate.0,
            buffer_frames: buffer_frames.filter(|_| matches!(config.buffer_size, cpal::BufferSize::Fixed(_))),
        },
    ))
}

/// Largest chunk the engine callback deinterleaves at once. Bigger callbacks
/// are processed in SCRATCH_FRAMES slices (Engine::process accepts any
/// length), so no callback size can force an allocation.
const SCRATCH_FRAMES: usize = 4096;

fn open_engine_stream(
    device_name: Option<String>,
    buffer_frames: Option<u32>,
    stats: &Arc<Stats>,
    clock: &Arc<ClockOrigin>,
) -> Result<(cpal::Stream, StreamInfo, sf_engine::EngineControl, Arc<OutputHooks>), String> {
    let (device, name, config) = open_output(device_name, buffer_frames)?;
    let sample_rate = config.sample_rate;
    let achieved = buffer_frames.filter(|_| matches!(config.buffer_size, cpal::BufferSize::Fixed(_)));

    reset_stats(stats);
    let sr = sample_rate.0 as f32;
    let mut engine = sf_engine::Engine::new(sr);
    let ctl = engine.split_control();

    // recording hooks: master tap ring + writer thread, calibration chirp
    let master = Tap::new(2, sample_rate.0);
    let (mut master_prod, master_cons) = rtrb::RingBuffer::<f32>::new(sample_rate.0 as usize * 2 * RING_SECONDS);
    {
        let tap = master.clone();
        std::thread::Builder::new()
            .name("sf-io-master-writer".into())
            .spawn(move || Tap::writer_loop(tap, master_cons))
            .map_err(|e| e.to_string())?;
    }
    let hooks = Arc::new(OutputHooks {
        master,
        capture_on: AtomicBool::new(false),
        impulse_pos: AtomicU32::new(IMPULSE_IDLE),
        impulse_frame: AtomicU64::new(0),
        buffer_frames: achieved,
    });
    let cb_hooks = hooks.clone();
    let chirp = capture::chirp(sample_rate.0, 0.5);
    let cb_clock = clock.clone();

    // planar scratch, pre-allocated: the callback deinterleaves into these,
    // renders, and interleaves back — no allocation on the audio thread.
    let mut scratch_l = vec![0.0f32; SCRATCH_FRAMES];
    let mut scratch_r = vec![0.0f32; SCRATCH_FRAMES];
    let mut ftz_set = false;

    let cb_stats = stats.clone();
    let err_stats = stats.clone();
    let ns_per_frame = 1e9 / sr as f64;

    let stream = device
        .build_output_stream(
            &config,
            move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
                if !ftz_set {
                    set_flush_denormals();
                    ftz_set = true;
                }
                let t0 = Instant::now();
                let total = out.len() / 2;
                let first_frame = cb_hooks
                    .master
                    .note_callback(total, cb_clock.nanos(info.timestamp().playback));
                let mut done = 0usize;
                while done < total {
                    let n = (total - done).min(SCRATCH_FRAMES);
                    engine.process(&mut scratch_l[..n], &mut scratch_r[..n]);
                    let base = done * 2;
                    for i in 0..n {
                        out[base + 2 * i] = scratch_l[i];
                        out[base + 2 * i + 1] = scratch_r[i];
                    }
                    done += n;
                }
                // calibration chirp, mixed post-limiter so its timing is the
                // true DAC timing (the limiter's lookahead would otherwise
                // shift it)
                let mut ip = cb_hooks.impulse_pos.load(Ordering::Acquire);
                if ip != IMPULSE_IDLE {
                    if ip == 0 {
                        cb_hooks.impulse_frame.store(first_frame, Ordering::Release);
                    }
                    for frame in out.chunks_exact_mut(2) {
                        if (ip as usize) >= chirp.len() {
                            break;
                        }
                        let v = chirp[ip as usize];
                        frame[0] += v;
                        frame[1] += v;
                        ip += 1;
                    }
                    cb_hooks.impulse_pos.store(
                        if (ip as usize) >= chirp.len() { IMPULSE_IDLE } else { ip },
                        Ordering::Release,
                    );
                }
                if cb_hooks.capture_on.load(Ordering::Acquire) {
                    cb_hooks.master.push_ring(&mut master_prod, out);
                }
                let dt = t0.elapsed().as_nanos() as u64;
                let budget = (total as f64 * ns_per_frame) as u64;
                cb_stats.budget_nanos.store(budget, Ordering::Relaxed);
                cb_stats.last_callback_nanos.store(dt, Ordering::Relaxed);
                cb_stats.max_callback_nanos.fetch_max(dt, Ordering::Relaxed);
                cb_stats.callbacks.fetch_add(1, Ordering::Relaxed);
                if dt > budget {
                    cb_stats.over_budget.fetch_add(1, Ordering::Relaxed);
                }
            },
            move |_e| {
                err_stats.xruns.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;

    Ok((
        stream,
        StreamInfo { device: name, sample_rate: sample_rate.0, buffer_frames: achieved },
        ctl,
        hooks,
    ))
}

/// Resolve the input device + its default config (rate + channel count are
/// the device's; only the buffer size is negotiable).
fn resolve_input(
    device_name: Option<String>,
    buffer_frames: Option<u32>,
) -> Result<(String, cpal::StreamConfig, cpal::Device), String> {
    let host = cpal::default_host();
    let device = match &device_name {
        None => host.default_input_device().ok_or("no default input device")?,
        Some(want) => host
            .input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|n| &n == want).unwrap_or(false))
            .ok_or_else(|| format!("input device \"{want}\" not found"))?,
    };
    let name = device.name().map_err(|e| e.to_string())?;
    let default_cfg = device.default_input_config().map_err(|e| e.to_string())?;
    let mut config: cpal::StreamConfig = default_cfg.config();
    config.channels = default_cfg.channels().clamp(1, 2);
    if let Some(frames) = buffer_frames {
        if let cpal::SupportedBufferSize::Range { min, max } = default_cfg.buffer_size() {
            if frames >= *min && frames <= *max {
                config.buffer_size = cpal::BufferSize::Fixed(frames);
            }
        }
    }
    Ok((name, config, device))
}

/// Open the input stream: callback → ring (no alloc) → writer thread → `tap`.
fn open_input_stream(
    device_name: Option<String>,
    buffer_frames: Option<u32>,
    tap: Arc<Tap>,
    stats: &Arc<Stats>,
    clock: &Arc<ClockOrigin>,
) -> Result<(cpal::Stream, StreamInfo, Arc<Tap>), String> {
    let (name, config, device) = resolve_input(device_name, buffer_frames)?;
    let ch = config.channels as usize;
    debug_assert_eq!(ch, tap.channels());
    let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(config.sample_rate.0 as usize * ch * RING_SECONDS);
    {
        let tap = tap.clone();
        std::thread::Builder::new()
            .name("sf-io-input-writer".into())
            .spawn(move || Tap::writer_loop(tap, cons))
            .map_err(|e| e.to_string())?;
    }
    stats.input_callbacks.store(0, Ordering::Relaxed);
    stats.input_xruns.store(0, Ordering::Relaxed);
    stats.input_peak_bits.store(0, Ordering::Relaxed);
    let cb_stats = stats.clone();
    let err_stats = stats.clone();
    let cb_tap = tap.clone();
    let cb_clock = clock.clone();
    // the device may deliver more channels than we keep: fold into scratch
    let dev_ch = device.default_input_config().map(|c| c.channels() as usize).unwrap_or(ch).max(ch);
    let mut scratch = vec![0.0f32; 8192 * ch];
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], info: &cpal::InputCallbackInfo| {
                let frames = data.len() / dev_ch.max(1);
                cb_tap.note_callback(frames, cb_clock.nanos(info.timestamp().capture));
                let mut peak = 0.0f32;
                let kept = if dev_ch == ch {
                    data
                } else {
                    let n = frames.min(scratch.len() / ch);
                    for f in 0..n {
                        for c in 0..ch {
                            scratch[f * ch + c] = data[f * dev_ch + c];
                        }
                    }
                    &scratch[..n * ch]
                };
                for &v in kept {
                    peak = peak.max(v.abs());
                }
                cb_tap.push_ring(&mut prod, kept);
                cb_stats.input_peak_bits.store(peak.to_bits(), Ordering::Relaxed);
                cb_stats.input_callbacks.fetch_add(1, Ordering::Relaxed);
            },
            move |_e| {
                err_stats.input_xruns.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((
        stream,
        StreamInfo {
            device: name,
            sample_rate: config.sample_rate.0,
            buffer_frames: buffer_frames.filter(|_| matches!(config.buffer_size, cpal::BufferSize::Fixed(_))),
        },
        tap,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Device enumeration must not panic anywhere (CI machines may be headless
    // with zero devices — an empty list is fine).
    #[test]
    fn enumerate_devices() {
        let _ = output_devices();
        let _ = input_devices();
    }

    #[test]
    fn timestamp_latency_uses_both_anchors() {
        // input frame 0 captured at t=0; output frame 0 plays at t=10 ms
        let take = Take {
            sample_rate: 48_000,
            channels: vec![vec![0.0; 10]],
            start_frame: 480, // captured at 10 ms
            start_anchor: Some(Anchor { frame: 0, nanos: 0 }),
        };
        let oa = Anchor { frame: 0, nanos: 10_000_000 };
        // arm happened when out frame 960 (20 ms) was current → plays at 30 ms
        assert_eq!(timestamp_latency_ms(&take, 960, Some(oa), 48_000), Some(20.0));
        assert_eq!(timestamp_latency_ms(&take, 960, None, 48_000), None);
    }
}
