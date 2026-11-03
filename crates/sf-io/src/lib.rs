//! cpal glue: driver-type ("host") selection, device enumeration, stream
//! config, and the real-time callback host. Hosts: CoreAudio on macOS,
//! WASAPI on Windows, plus ASIO on Windows when built with the `asio`
//! feature (docs/WINDOWS_PORT_PLAN.md W1/W3).
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
//! Windows port (W1): streams open at the device's NATIVE channel count and
//! sample format (WASAPI shared mode and ASIO accept nothing else). The engine
//! always renders stereo planar; the callback writes it to channels 1–2 (mono
//! devices get the downmix, extra channels get silence) converting to the
//! device's sample type. Device loss / stalled callbacks / a changed system
//! default are reported by [`IoHandle::check_health`] so the host app can
//! restart the stream. On WASAPI the callback thread registers with MMCSS
//! ("Pro Audio") — cpal 0.16's own priority boost on WASAPI is a no-op.
//!
//! cpal's `Stream` is !Send, so a dedicated manager thread owns it; callers
//! hold an [`IoHandle`] (Send + Sync) and talk over a channel. The audio
//! callback itself touches only pre-built state and atomics — no allocation,
//! no locks (RT rules, §4).

pub mod capture;

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};

use capture::{Anchor, ClockOrigin, Take, Tap, RING_SECONDS};

/// One selectable output device.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub name: String,
    pub is_default: bool,
    /// Channel count of the device's default config in this direction
    /// (0 if it could not be read) — drives the channel pickers.
    pub channels: u16,
}

/// One audio driver type available in this build ("CoreAudio", "WASAPI",
/// "ASIO", "ALSA", …).
#[derive(Debug, Clone)]
pub struct HostInfo {
    pub name: String,
    /// The platform default (what `None` resolves to).
    pub is_default: bool,
}

/// What the opened stream actually negotiated (may differ from the request).
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub device: String,
    /// Driver type the stream runs on (cpal host name).
    pub host: String,
    pub sample_rate: u32,
    /// Achieved buffer size in frames, if the backend honours a fixed size.
    /// None = backend-managed (always None on WASAPI shared mode, which runs
    /// on the Windows audio engine's period whatever is requested); the
    /// callback sizes actually seen are in [`StatsSnapshot`].
    pub buffer_frames: Option<u32>,
    /// Device channel count the stream was opened with.
    pub channels: u16,
    /// Device sample format ("f32", "i32", "i24", "i16", …).
    pub sample_format: String,
}

/// Real-time priority state of the audio callback thread.
pub const RT_HOST_MANAGED: u8 = 0;
/// MMCSS "Pro Audio" registration succeeded (Windows / WASAPI).
pub const RT_MMCSS_OK: u8 = 1;
/// MMCSS registration was attempted and failed.
pub const RT_MMCSS_FAILED: u8 = 2;

/// Live callback health, updated from the audio thread with atomics only.
#[derive(Default)]
pub struct Stats {
    /// Callbacks served since the stream started.
    pub callbacks: AtomicU64,
    /// Stream error callbacks other than device loss (cpal reports
    /// overruns/underruns here).
    pub xruns: AtomicU32,
    /// Worst observed callback duration, nanoseconds.
    pub max_callback_nanos: AtomicU64,
    /// Most recent callback duration, nanoseconds.
    pub last_callback_nanos: AtomicU64,
    /// Nanosecond budget per callback (frames / sample_rate); 0 until known.
    pub budget_nanos: AtomicU64,
    /// Callbacks that exceeded their budget (deadline misses = audible risk).
    pub over_budget: AtomicU64,
    /// Smallest / largest callback seen, frames (min is u32::MAX until the
    /// first callback). The honest buffer size on backends that ignore a
    /// requested size.
    pub min_frames: AtomicU32,
    pub max_frames: AtomicU32,
    /// The output device went away (cpal `DeviceNotAvailable`).
    pub device_lost: AtomicBool,
    /// RT_* state of the output callback thread.
    pub rt_priority: AtomicU8,
    /// Input stream callbacks served.
    pub input_callbacks: AtomicU64,
    /// Input stream error callbacks other than device loss (overruns).
    pub input_xruns: AtomicU32,
    /// Input peak (f32 bits) over the most recent input callback.
    pub input_peak_bits: AtomicU32,
    /// Largest input callback seen, frames.
    pub input_max_frames: AtomicU32,
    /// Frames delivered by the input stream since it opened.
    pub input_frames: AtomicU64,
    /// Any non-zero input sample seen since the input stream opened. Stays
    /// false on Windows when microphone privacy blocks desktop apps (the
    /// stream runs but delivers exact digital silence).
    pub input_nonzero: AtomicBool,
    /// The input device went away.
    pub input_device_lost: AtomicBool,
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
    /// Callback size range actually seen (0 = no callback yet).
    pub min_callback_frames: u32,
    pub max_callback_frames: u32,
    pub device_lost: bool,
    /// RT_* constant.
    pub rt_priority: u8,
    pub input_callbacks: u64,
    pub input_xruns: u32,
    /// Peak |sample| in the latest input callback (0 when no input stream).
    pub input_peak: f32,
    pub input_max_frames: u32,
    pub input_frames: u64,
    pub input_nonzero: bool,
    pub input_device_lost: bool,
}

impl Stats {
    pub fn snapshot(&self) -> StatsSnapshot {
        let budget = self.budget_nanos.load(Ordering::Relaxed);
        let max = self.max_callback_nanos.load(Ordering::Relaxed);
        let min_frames = self.min_frames.load(Ordering::Relaxed);
        StatsSnapshot {
            callbacks: self.callbacks.load(Ordering::Relaxed),
            xruns: self.xruns.load(Ordering::Relaxed),
            max_callback_us: max as f64 / 1e3,
            last_callback_us: self.last_callback_nanos.load(Ordering::Relaxed) as f64 / 1e3,
            budget_us: budget as f64 / 1e3,
            peak_load: if budget > 0 { max as f64 / budget as f64 } else { 0.0 },
            over_budget: self.over_budget.load(Ordering::Relaxed),
            min_callback_frames: if min_frames == u32::MAX { 0 } else { min_frames },
            max_callback_frames: self.max_frames.load(Ordering::Relaxed),
            device_lost: self.device_lost.load(Ordering::Relaxed),
            rt_priority: self.rt_priority.load(Ordering::Relaxed),
            input_callbacks: self.input_callbacks.load(Ordering::Relaxed),
            input_xruns: self.input_xruns.load(Ordering::Relaxed),
            input_peak: f32::from_bits(self.input_peak_bits.load(Ordering::Relaxed)),
            input_max_frames: self.input_max_frames.load(Ordering::Relaxed),
            input_frames: self.input_frames.load(Ordering::Relaxed),
            input_nonzero: self.input_nonzero.load(Ordering::Relaxed),
            input_device_lost: self.input_device_lost.load(Ordering::Relaxed),
        }
    }
}

/// Driver types compiled into this build, platform default flagged.
pub fn hosts() -> Vec<HostInfo> {
    let default = cpal::default_host().id();
    cpal::available_hosts()
        .into_iter()
        .map(|id| HostInfo { name: id.name().to_string(), is_default: id == default })
        .collect()
}

/// Resolve a driver type by name (case-insensitive); None = platform default.
fn host_named(name: Option<&str>) -> Result<cpal::Host, String> {
    let Some(want) = name else { return Ok(cpal::default_host()) };
    let id = cpal::available_hosts()
        .into_iter()
        .find(|id| id.name().eq_ignore_ascii_case(want))
        .ok_or_else(|| format!("audio driver type \"{want}\" is not available in this build"))?;
    cpal::host_from_id(id).map_err(|e| e.to_string())
}

/// True for WASAPI: shared mode ignores a requested buffer size and its
/// callback thread needs MMCSS registration.
fn is_wasapi(host: &cpal::Host) -> bool {
    host.id().name().eq_ignore_ascii_case("WASAPI")
}

/// True for ASIO: its driver owns the callback thread (never re-prioritise
/// it) and only the loaded driver enumerates, so default-following is off.
fn is_asio(host_name: &str) -> bool {
    host_name.eq_ignore_ascii_case("ASIO")
}

/// List output devices on the platform default host.
pub fn output_devices() -> Vec<DeviceInfo> {
    output_devices_on(None)
}

/// List output devices on `host` (None = platform default). An unknown
/// host yields an empty list.
pub fn output_devices_on(host: Option<&str>) -> Vec<DeviceInfo> {
    let Ok(host) = host_named(host) else { return Vec::new() };
    let default_name = host.default_output_device().and_then(|d| d.name().ok());
    let mut out = Vec::new();
    if let Ok(devs) = host.output_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                let is_default = Some(&name) == default_name.as_ref();
                let channels = d.default_output_config().map(|c| c.channels()).unwrap_or(0);
                out.push(DeviceInfo { name, is_default, channels });
            }
        }
    }
    out
}

/// List input devices on the platform default host.
pub fn input_devices() -> Vec<DeviceInfo> {
    input_devices_on(None)
}

/// List input devices on `host` (None = platform default).
pub fn input_devices_on(host: Option<&str>) -> Vec<DeviceInfo> {
    let Ok(host) = host_named(host) else { return Vec::new() };
    let default_name = host.default_input_device().and_then(|d| d.name().ok());
    let mut out = Vec::new();
    if let Ok(devs) = host.input_devices() {
        for d in devs {
            if let Ok(name) = d.name() {
                let is_default = Some(&name) == default_name.as_ref();
                let channels = d.default_input_config().map(|c| c.channels()).unwrap_or(0);
                out.push(DeviceInfo { name, is_default, channels });
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


/// Engine start reply: the stream, its control handle, the recording hooks,
/// and whether the INPUT stream had to be closed to open it (see
/// [`input_closes_for_output`]).
type EngineStart = Result<(StreamInfo, sf_engine::EngineControl, Arc<OutputHooks>, bool), String>;

enum Cmd {
    StartInput {
        host: Option<String>,
        device: Option<String>,
        buffer_frames: Option<u32>,
        reply: mpsc::Sender<Result<(StreamInfo, Arc<Tap>), String>>,
    },
    StopInput,
    StartTone {
        host: Option<String>,
        out_first: u16,
        device: Option<String>,
        buffer_frames: Option<u32>,
        freq_hz: f32,
        gain: f32,
        reply: mpsc::Sender<Result<(StreamInfo, bool), String>>,
    },
    StartEngine {
        host: Option<String>,
        out_first: u16,
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
/// x86 matters most: Intel/AMD take a large per-op penalty on denormals.
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
        // _mm_getcsr/_mm_setcsr are deprecated (and fail clippy -D warnings
        // on x86 targets); the documented replacement is stmxcsr/ldmxcsr.
        let mut csr: u32 = 0;
        core::arch::asm!("stmxcsr [{p}]", p = in(reg) &mut csr as *mut u32, options(nostack, preserves_flags));
        csr |= 0x8040; // FTZ | DAZ
        core::arch::asm!("ldmxcsr [{p}]", p = in(reg) &csr as *const u32, options(nostack, preserves_flags, readonly));
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {}
}

/// Register the CALLING thread with MMCSS as "Pro Audio" (Windows only).
/// Without this the WASAPI callback runs at normal priority: cpal 0.16
/// passes a thread ID where SetThreadPriority wants a handle, so its own
/// boost silently fails. Returns an RT_* code. The registration ends when
/// the thread exits (cpal ends it with the stream), so no revert is needed.
fn promote_rt(mmcss: bool) -> u8 {
    if !mmcss {
        return RT_HOST_MANAGED;
    }
    #[cfg(windows)]
    unsafe {
        use windows::core::w;
        use windows::Win32::System::Threading::{
            AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, AVRT_PRIORITY_HIGH,
        };
        let mut task_index = 0u32;
        match AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) {
            Ok(h) => {
                let _ = AvSetMmThreadPriority(h, AVRT_PRIORITY_HIGH);
                RT_MMCSS_OK
            }
            Err(_) => RT_MMCSS_FAILED,
        }
    }
    #[cfg(not(windows))]
    {
        RT_HOST_MANAGED
    }
}

/// Opt the whole process out of Windows power throttling ("Efficiency mode"
/// / EcoQoS). Windows 11 may otherwise run a window that is in the
/// background or minimised on efficiency cores at reduced clocks, which
/// raises audio-callback time unpredictably. Call once at startup. No-op on
/// other platforms. Returns false if Windows refused (older Windows 10
/// builds without the API — harmless).
pub fn prefer_performance() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Threading::{
            GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
            PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_STATE,
        };
        // ControlMask = "I am deciding execution-speed throttling",
        // StateMask = 0 = "...and the answer is never throttle".
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
        .is_ok()
    }
    #[cfg(not(windows))]
    {
        true
    }
}

/// How long a running stream may go without a callback before it is
/// considered dead (WASAPI ends its stream thread on ANY error, so a stall
/// is the portable signal), and the grace period after opening.
const STALL_AFTER: Duration = Duration::from_millis(2000);
const STALL_GRACE: Duration = Duration::from_millis(3000);
/// How often to compare the system default device against the open one.
const DEFAULT_POLL: Duration = Duration::from_millis(2000);
/// A reported problem is re-reported this long after, so a failed restart
/// is retried (e.g. the interface is plugged back in).
const REPORT_RETRY: Duration = Duration::from_millis(5000);

/// What the host app asked for, kept so a restart can repeat the request.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamRequest {
    pub host: Option<String>,
    /// None = follow the system default device.
    pub device: Option<String>,
    pub buffer_frames: Option<u32>,
}

/// Per-stream watchdog state (owned by [`IoHandle`], advanced by
/// [`IoHandle::check_health`]).
#[derive(Debug, Clone)]
struct Watch {
    req: StreamRequest,
    /// Device name actually opened.
    device: String,
    host_name: String,
    since: Instant,
    last_cb: u64,
    last_progress: Instant,
    last_default_check: Instant,
    reported_at: Option<Instant>,
}

impl Watch {
    fn new(req: StreamRequest, info: &StreamInfo, now: Instant) -> Self {
        Self {
            req,
            device: info.device.clone(),
            host_name: info.host.clone(),
            since: now,
            last_cb: 0,
            last_progress: now,
            last_default_check: now,
            reported_at: None,
        }
    }

    /// Watch for a stream that FAILED to open: already "reported", so the
    /// next report (a retry) comes after REPORT_RETRY — the host app keeps
    /// trying until the device is back.
    fn failed(req: StreamRequest, now: Instant) -> Self {
        let past = |d: Duration| now.checked_sub(d).unwrap_or(now);
        Self {
            device: req.device.clone().unwrap_or_else(|| "the default device".into()),
            host_name: req.host.clone().unwrap_or_default(),
            req,
            since: past(STALL_GRACE + Duration::from_millis(1)),
            last_cb: u64::MAX, // first assess() sees "progress" once, then a stall
            last_progress: past(STALL_AFTER + Duration::from_millis(1)),
            last_default_check: now,
            reported_at: Some(now),
        }
    }

    /// Advance with the current callback count / loss flag / system default
    /// name (None = not checked this time). Returns a human-readable reason
    /// when the stream needs restarting.
    fn assess(&mut self, callbacks: u64, lost: bool, default_now: Option<&str>, now: Instant) -> Option<String> {
        if callbacks != self.last_cb {
            self.last_cb = callbacks;
            self.last_progress = now;
        }
        if let Some(at) = self.reported_at {
            if now.duration_since(at) < REPORT_RETRY {
                return None;
            }
        }
        let reason = if lost {
            Some(format!("\"{}\" was disconnected", self.device))
        } else if now.duration_since(self.since) > STALL_GRACE && now.duration_since(self.last_progress) > STALL_AFTER {
            Some(format!("\"{}\" stopped responding", self.device))
        } else {
            match default_now {
                Some(d) if self.req.device.is_none() && d != self.device => {
                    Some(format!("System default device changed to \"{d}\""))
                }
                _ => None,
            }
        };
        if reason.is_some() {
            self.reported_at = Some(now);
        }
        reason
    }

    /// Whether it is time to look up the system default (and remember that
    /// we did).
    fn due_default_check(&mut self, now: Instant) -> bool {
        if self.req.device.is_some() || is_asio(&self.host_name) {
            return false;
        }
        if now.duration_since(self.last_default_check) < DEFAULT_POLL {
            return false;
        }
        self.last_default_check = now;
        true
    }
}

#[derive(Default)]
struct Watches {
    /// Only ENGINE output streams are watched (the test tone is transient
    /// and owned by the settings UI).
    engine: Option<Watch>,
    input: Option<Watch>,
}

/// Streams that need restarting, with the reason (for a user notice).
#[derive(Debug, Clone, Default)]
pub struct Health {
    pub output: Option<String>,
    pub input: Option<String>,
}

/// Handle to the manager thread that owns the (!Send) cpal stream.
pub struct IoHandle {
    tx: mpsc::Sender<Cmd>,
    stats: Arc<Stats>,
    rec: Mutex<RecState>,
    /// Driver type used when a start call does not name one (None =
    /// platform default). Set by every start that does.
    host: Mutex<Option<String>>,
    /// Device output channel (0-based) that receives the engine's LEFT
    /// signal; right goes to the next one. 0 = outputs 1–2.
    out_first: AtomicU32,
    watches: Mutex<Watches>,
}

impl IoHandle {
    /// Spawn the stream-owner thread. One per app.
    pub fn spawn() -> Self {
        let (tx, rx) = mpsc::channel::<Cmd>();
        let stats = Arc::new(Stats::default());
        stats.min_frames.store(u32::MAX, Ordering::Relaxed);
        let clock = Arc::new(ClockOrigin::default());
        let thread_stats = stats.clone();
        let thread_clock = clock.clone();
        std::thread::Builder::new()
            .name("sf-io-manager".into())
            .spawn(move || manager_loop(rx, thread_stats, thread_clock))
            .expect("spawn sf-io manager");
        Self {
            tx,
            stats,
            rec: Mutex::new(RecState::default()),
            host: Mutex::new(None),
            out_first: AtomicU32::new(0),
            watches: Mutex::new(Watches::default()),
        }
    }

    fn rec(&self) -> std::sync::MutexGuard<'_, RecState> {
        self.rec.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn watches(&self) -> std::sync::MutexGuard<'_, Watches> {
        self.watches.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Current driver type (None = platform default).
    pub fn host(&self) -> Option<String> {
        self.host.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Route the engine's stereo out to device outputs `first+1`/`first+2`
    /// (0-based `first`; clamped to the device when a stream opens). Applies
    /// to the next output stream start; automatic restarts keep it.
    pub fn set_output_channel(&self, first: u16) {
        self.out_first.store(first as u32, Ordering::Relaxed);
    }

    pub fn output_channel(&self) -> u16 {
        self.out_first.load(Ordering::Relaxed) as u16
    }

    /// Choose the driver type for subsequent starts that don't name one
    /// (None = platform default). Does not touch running streams.
    pub fn set_host(&self, host: Option<String>) {
        *self.host.lock().unwrap_or_else(|e| e.into_inner()) = host;
    }

    /// Open an output stream on `device` (default when None) at
    /// `buffer_frames` (backend default when None) and play a sine test tone.
    /// Replaces any running stream. Uses the current driver type.
    pub fn start_tone(
        &self,
        device: Option<String>,
        buffer_frames: Option<u32>,
        freq_hz: f32,
        gain: f32,
    ) -> Result<StreamInfo, String> {
        let host = self.host();
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::StartTone { host, out_first: self.output_channel(), device, buffer_frames, freq_hz, gain, reply })
            .map_err(|_| "sf-io manager thread is gone".to_string())?;
        // the tone replaces (and so ends) any engine stream
        self.watches().engine = None;
        let (info, input_closed) = rx.recv().map_err(|_| "sf-io manager dropped the reply".to_string())??;
        if input_closed {
            self.forget_input();
        }
        Ok(info)
    }

    /// The manager closed the input stream as a side effect (ASIO session
    /// restart, or the driver type changed): drop our view of it. The next
    /// record call reopens it (src/ui/actions.tsx checks `input_info`).
    fn forget_input(&self) {
        self.rec().input = None;
        self.watches().input = None;
    }

    /// Open an output stream hosting a fresh [`sf_engine::Engine`] built at
    /// the device's rate. The Engine itself is moved into the audio callback;
    /// the returned [`sf_engine::EngineControl`] is the Send-safe handle for
    /// commands / project loads / meter snapshots. Replaces any running
    /// stream (a previous control handle goes dead: its pushes are dropped).
    /// Uses the current driver type.
    pub fn start_engine(
        &self,
        device: Option<String>,
        buffer_frames: Option<u32>,
    ) -> Result<(StreamInfo, sf_engine::EngineControl), String> {
        let host = self.host();
        let req = StreamRequest { host: host.clone(), device: device.clone(), buffer_frames };
        self.watches().engine = None;
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::StartEngine { host, out_first: self.output_channel(), device, buffer_frames, reply })
            .map_err(|_| "sf-io manager thread is gone".to_string())?;
        let started = rx.recv().map_err(|_| "sf-io manager dropped the reply".to_string())?;
        let (info, ctl, hooks, input_closed) = match started {
            Ok(v) => v,
            Err(e) => {
                // keep watching so check_health() asks for a retry later
                self.watches().engine = Some(Watch::failed(req, Instant::now()));
                return Err(e);
            }
        };
        if input_closed {
            self.forget_input();
        }
        self.rec().out = Some(hooks);
        self.watches().engine = Some(Watch::new(req, &info, Instant::now()));
        Ok((info, ctl))
    }

    /// After a restart sequence that failed completely: watch `req` as a
    /// failed stream so [`IoHandle::check_health`] asks for another attempt
    /// at THAT request later (not at whichever fallback was tried last).
    pub fn retry_engine_later(&self, req: StreamRequest) {
        self.watches().engine = Some(Watch::failed(req, Instant::now()));
    }

    /// What the running (or last failed-over) engine stream was asked for.
    pub fn engine_request(&self) -> Option<StreamRequest> {
        self.watches().engine.as_ref().map(|w| w.req.clone())
    }

    /// Stop and drop the current OUTPUT stream (if any). The input stream
    /// (if any) keeps running.
    pub fn stop(&self) {
        self.rec().out = None;
        self.watches().engine = None;
        let _ = self.tx.send(Cmd::Stop);
    }

    // ---- recording (Phase 6) ----

    /// Open an INPUT stream on `device` (default when None) at the device's
    /// native rate/channel count (the first two channels are kept). Frames
    /// flow callback → ring → writer thread, and are kept only while a take
    /// is armed. Replaces any running input stream (an armed take is
    /// abandoned). Uses the current driver type.
    pub fn start_input(
        &self,
        device: Option<String>,
        buffer_frames: Option<u32>,
    ) -> Result<StreamInfo, String> {
        let host = self.host();
        let req = StreamRequest { host: host.clone(), device: device.clone(), buffer_frames };
        self.watches().input = None;
        let (reply, rx) = mpsc::channel();
        self.tx
            .send(Cmd::StartInput { host, device, buffer_frames, reply })
            .map_err(|_| "sf-io manager thread is gone".to_string())?;
        let (info, tap) = rx.recv().map_err(|_| "sf-io manager dropped the reply".to_string())??;
        self.rec().input = Some(InputHooks { tap, info: info.clone() });
        self.watches().input = Some(Watch::new(req, &info, Instant::now()));
        Ok(info)
    }

    /// What the running input stream was asked for.
    pub fn input_request(&self) -> Option<StreamRequest> {
        self.watches().input.as_ref().map(|w| w.req.clone())
    }

    /// Close the input stream.
    pub fn stop_input(&self) {
        self.rec().input = None;
        self.watches().input = None;
        let _ = self.tx.send(Cmd::StopInput);
    }

    /// The running input stream, if any.
    pub fn input_info(&self) -> Option<StreamInfo> {
        self.rec().input.as_ref().map(|i| i.info.clone())
    }

    /// True when the input stream has delivered at least 2 s of EXACT
    /// digital silence — on Windows the signature of microphone privacy
    /// blocking desktop apps (a real input always carries some noise).
    pub fn input_silent(&self) -> bool {
        let r = self.rec();
        let Some(i) = r.input.as_ref() else { return false };
        let need = i.info.sample_rate as u64 * 2;
        need > 0
            && self.stats.input_frames.load(Ordering::Relaxed) >= need
            && !self.stats.input_nonzero.load(Ordering::Relaxed)
    }

    /// Watchdog tick — call every ~0.5 s from a non-audio thread. Reports
    /// (once, then again every few seconds while unresolved) the engine
    /// output and/or input streams that need restarting: device lost,
    /// callbacks stalled, or — when following the system default — the
    /// default device changed. The caller restarts them (the engine restart
    /// needs a project re-push, which lives in the host app).
    pub fn check_health(&self) -> Health {
        let now = Instant::now();
        let mut w = self.watches();
        let mut health = Health::default();
        if let Some(out) = w.engine.as_mut() {
            let default_now = if out.due_default_check(now) {
                host_named(out.req.host.as_deref())
                    .ok()
                    .and_then(|h| h.default_output_device())
                    .and_then(|d| d.name().ok())
            } else {
                None
            };
            health.output = out.assess(
                self.stats.callbacks.load(Ordering::Relaxed),
                self.stats.device_lost.load(Ordering::Relaxed),
                default_now.as_deref(),
                now,
            );
        }
        if let Some(inp) = w.input.as_mut() {
            let default_now = if inp.due_default_check(now) {
                host_named(inp.req.host.as_deref())
                    .ok()
                    .and_then(|h| h.default_input_device())
                    .and_then(|d| d.name().ok())
            } else {
                None
            };
            health.input = inp.assess(
                self.stats.input_callbacks.load(Ordering::Relaxed),
                self.stats.input_device_lost.load(Ordering::Relaxed),
                default_now.as_deref(),
                now,
            );
        }
        health
    }

    /// Arm the input: frames from NOW are kept. Returns the take id.
    pub fn start_take(&self) -> Result<u64, String> {
        self.start_take_channels(0, 2)
    }

    /// Arm the input keeping device input channels `first..first+count`
    /// (0-based; clamped to what the device has). `(0, 2)` = inputs 1–2,
    /// `(1, 1)` = input 2 as a mono take.
    pub fn start_take_channels(&self, first: usize, count: usize) -> Result<u64, String> {
        let mut r = self.rec();
        let out_frame = r.out.as_ref().map(|o| o.master.total_frames()).unwrap_or(0);
        let input = r.input.as_ref().ok_or("no input stream — call start_input first")?;
        input.tap.arm_channels(out_frame, first, count);
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
        // the loopback can land on any input: correlate the channel with the
        // most energy (averaging up to 8 channels would bury the chirp)
        let energy = |c: &Vec<f32>| c.iter().map(|v| v * v).sum::<f32>();
        let mono: Vec<f32> = take
            .channels
            .iter()
            .max_by(|a, b| energy(a).total_cmp(&energy(b)))
            .cloned()
            .unwrap_or_default();
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

    /// Buffer-based fallback: output buffer + input buffer, in ms. Prefers
    /// the callback sizes actually SEEN (WASAPI shared mode ignores the
    /// requested size, so the request would underestimate) and falls back to
    /// the negotiated size before the first callback.
    fn estimate_ms_locked(&self, r: &RecState) -> f64 {
        let seen_out = self.stats.max_frames.load(Ordering::Relaxed);
        let seen_in = self.stats.input_max_frames.load(Ordering::Relaxed);
        let out_ms = r
            .out
            .as_ref()
            .and_then(|o| {
                let frames = if seen_out > 0 { Some(seen_out) } else { o.buffer_frames };
                frames.map(|b| b as f64 / o.master.sample_rate() as f64 * 1000.0)
            })
            .unwrap_or(0.0);
        let in_ms = r
            .input
            .as_ref()
            .and_then(|i| {
                let frames = if seen_in > 0 { Some(seen_in) } else { i.info.buffer_frames };
                frames.map(|b| b as f64 / i.tap.sample_rate() as f64 * 1000.0)
            })
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

// ---- ASIO session (docs/WINDOWS_PORT_PLAN.md W3) ----
//
// ASIO has ONE buffer set per driver: every ASIOCreateBuffers call frees the
// previous one. cpal 0.16 tracks that set per `Device` OBJECT, and each
// enumeration makes a new object. Opening output and input from two
// separately-enumerated objects therefore re-created the buffers for input
// only, leaving the output callback reading freed buffers (silence at best,
// a crash at worst). cpal's own design expects both directions on one object:
// `prepare_input_stream` carries the existing output buffers into the new
// set. So the manager keeps one `AsioSession` and hands its Device to both.
//
// cpal also sizes each stream's scratch buffer once at build time, so every
// stream on the session must share one buffer size. And cpal ignores the
// driver's reset requests (sent after control-panel changes), so a restart
// must re-initialise the driver. Hence the rules below.

/// The ASIO driver in use and the Device every stream on it must share.
struct AsioSession {
    driver: String,
    device: cpal::Device,
    /// Buffer request all streams on this session were built with.
    buffer: Option<u32>,
}

/// Opening an OUTPUT stream: must the open input stream close first?
/// - On ASIO, yes: output always starts a fresh session (driver re-init —
///   also how a driver reset or a new buffer size is picked up), which
///   frees the input's buffers.
/// - When the driver type changes, yes: input and output must share a
///   clock for latency compensation (it reopens on the next record).
fn input_closes_for_output(out_host_asio: bool, input_open: bool, input_host: Option<&str>, out_host: Option<&str>) -> bool {
    input_open && (out_host_asio || input_host != out_host)
}

/// How an ASIO INPUT stream gets its device.
#[derive(Debug, PartialEq)]
enum AsioInput {
    /// Join the running output's session with its buffer size.
    Join { buffer: Option<u32> },
    /// No output running: start a new session for the input.
    Fresh,
}

/// `session` = (driver, buffer) of the running ASIO session, if any.
fn plan_asio_input(session: Option<(&str, Option<u32>)>, output_open: bool, want: Option<&str>) -> Result<AsioInput, String> {
    match session {
        Some((driver, buffer)) if output_open => {
            if want.is_none() || want == Some(driver) {
                Ok(AsioInput::Join { buffer })
            } else {
                Err(format!(
                    "ASIO runs one driver at a time: choose an input on \"{driver}\" (the output's interface), or use WASAPI"
                ))
            }
        }
        _ => Ok(AsioInput::Fresh),
    }
}

fn manager_loop(rx: mpsc::Receiver<Cmd>, stats: Arc<Stats>, clock: Arc<ClockOrigin>) {
    // The Streams live HERE, on this thread, for their whole life. They are
    // never read — they're held purely so Drop (assigning None / a
    // replacement) stops the hardware stream — hence the underscore names.
    let mut _stream: Option<cpal::Stream> = None;
    let mut _input: Option<cpal::Stream> = None;
    let mut input_host: Option<String> = None;
    let mut asio: Option<AsioSession> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::StartInput { host, device, buffer_frames, reply } => {
                _input = None;
                if _stream.is_none() {
                    asio = None; // input-only session is being replaced
                }
                let res = open_input_stream(host.as_deref(), device, buffer_frames, &stats, &clock, &mut asio, _stream.is_some());
                match res {
                    Ok((s, info, tap)) => {
                        let _ = reply.send(Ok((info, tap)));
                        _input = Some(s);
                        input_host = host;
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::StopInput => {
                _input = None;
                if _stream.is_none() {
                    asio = None; // last stream gone: release the driver
                }
            }
            Cmd::StartTone { host, out_first, device, buffer_frames, freq_hz, gain, reply } => {
                let input_closed = prepare_output_open(&mut _stream, &mut _input, &mut asio, input_host.as_deref(), host.as_deref());
                let req = OutputRequest { host: host.as_deref(), device, buffer_frames, out_first };
                let res = open_tone_stream(req, freq_hz, gain, &stats, &mut asio);
                match res {
                    Ok((s, info)) => {
                        let _ = reply.send(Ok((info, input_closed)));
                        _stream = Some(s);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::StartEngine { host, out_first, device, buffer_frames, reply } => {
                let input_closed = prepare_output_open(&mut _stream, &mut _input, &mut asio, input_host.as_deref(), host.as_deref());
                let req = OutputRequest { host: host.as_deref(), device, buffer_frames, out_first };
                let res = open_engine_stream(req, &stats, &clock, &mut asio);
                match res {
                    Ok((s, info, ctl, hooks)) => {
                        let _ = reply.send(Ok((info, ctl, hooks, input_closed)));
                        _stream = Some(s);
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
            Cmd::Stop => {
                _stream = None;
                if _input.is_none() {
                    asio = None;
                }
            }
            Cmd::Quit => break,
        }
    }
}

/// Before opening an output stream: drop the old one, close the input if
/// required ([`input_closes_for_output`]) and, on ASIO, end the session so
/// the driver is re-initialised. Order matters: streams before the session,
/// so the last driver handle goes and asio-sys really tears the driver down.
/// Returns whether the input was closed.
fn prepare_output_open(
    stream: &mut Option<cpal::Stream>,
    input: &mut Option<cpal::Stream>,
    asio: &mut Option<AsioSession>,
    input_host: Option<&str>,
    out_host: Option<&str>,
) -> bool {
    *stream = None;
    let out_asio = out_host.is_some_and(is_asio);
    let close = input_closes_for_output(out_asio, input.is_some(), input_host, out_host);
    if close {
        *input = None;
    }
    if out_asio || input.is_none() {
        *asio = None;
    }
    close
}

fn reset_stats(stats: &Stats) {
    stats.callbacks.store(0, Ordering::Relaxed);
    stats.xruns.store(0, Ordering::Relaxed);
    stats.max_callback_nanos.store(0, Ordering::Relaxed);
    stats.last_callback_nanos.store(0, Ordering::Relaxed);
    stats.budget_nanos.store(0, Ordering::Relaxed);
    stats.over_budget.store(0, Ordering::Relaxed);
    stats.min_frames.store(u32::MAX, Ordering::Relaxed);
    stats.max_frames.store(0, Ordering::Relaxed);
    stats.device_lost.store(false, Ordering::Relaxed);
    stats.rt_priority.store(RT_HOST_MANAGED, Ordering::Relaxed);
}

fn reset_input_stats(stats: &Stats) {
    stats.input_callbacks.store(0, Ordering::Relaxed);
    stats.input_xruns.store(0, Ordering::Relaxed);
    stats.input_peak_bits.store(0, Ordering::Relaxed);
    stats.input_max_frames.store(0, Ordering::Relaxed);
    stats.input_frames.store(0, Ordering::Relaxed);
    stats.input_nonzero.store(false, Ordering::Relaxed);
    stats.input_device_lost.store(false, Ordering::Relaxed);
}

/// Error callback body shared by every stream: device loss is a separate
/// signal (the watchdog restarts the stream); anything else is an xrun.
fn note_stream_error(lost: &AtomicBool, xruns: &AtomicU32, e: cpal::StreamError) {
    match e {
        cpal::StreamError::DeviceNotAvailable => lost.store(true, Ordering::Relaxed),
        _ => {
            xruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// What an output-stream open asks for.
struct OutputRequest<'a> {
    host: Option<&'a str>,
    device: Option<String>,
    buffer_frames: Option<u32>,
    /// Requested first output channel (0-based); clamped by [`first_pair`].
    out_first: u16,
}

/// Clamp a requested first output channel so the stereo pair fits the
/// device (mono devices: 0, the downmix).
fn first_pair(requested: u16, channels: u16) -> usize {
    if channels < 2 { 0 } else { (requested as usize).min(channels as usize - 2) }
}

/// A resolved output device + the config it will be opened with.
struct OutputTarget {
    device: cpal::Device,
    name: String,
    host_name: String,
    config: cpal::StreamConfig,
    format: SampleFormat,
    wasapi: bool,
    /// First device channel of the stereo pair (already clamped).
    first: usize,
}

impl OutputTarget {
    fn info(&self, requested: Option<u32>) -> StreamInfo {
        StreamInfo {
            device: self.name.clone(),
            host: self.host_name.clone(),
            sample_rate: self.config.sample_rate.0,
            buffer_frames: achieved_frames(self.wasapi, requested, &self.config),
            channels: self.config.channels,
            sample_format: self.format.to_string(),
        }
    }
}

/// A requested fixed size is only reported as achieved when the backend
/// honours it — never on WASAPI shared mode.
fn achieved_frames(wasapi: bool, requested: Option<u32>, config: &cpal::StreamConfig) -> Option<u32> {
    if wasapi {
        return None;
    }
    requested.filter(|_| matches!(config.buffer_size, cpal::BufferSize::Fixed(_)))
}

/// Request `frames` only inside the backend's supported range; Default
/// otherwise (Unknown range included).
fn apply_buffer_request(config: &mut cpal::StreamConfig, supported: &cpal::SupportedBufferSize, frames: Option<u32>) {
    if let (Some(frames), cpal::SupportedBufferSize::Range { min, max }) = (frames, supported) {
        if frames >= *min && frames <= *max {
            config.buffer_size = cpal::BufferSize::Fixed(frames);
        }
    }
}

/// Resolve the output device and its NATIVE config (channel count, rate,
/// sample format — WASAPI shared mode and ASIO accept nothing else),
/// honoring `buffer_frames` when the backend supports it.
fn open_output(req: &OutputRequest<'_>, asio: &mut Option<AsioSession>) -> Result<OutputTarget, String> {
    let (device_name, buffer_frames) = (&req.device, req.buffer_frames);
    let host = host_named(req.host)?;
    let wasapi = is_wasapi(&host);
    let host_name = host.id().name().to_string();
    let device = match device_name {
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
    let channels = default_cfg.channels();
    if channels == 0 {
        return Err(format!("output device \"{name}\" reports 0 channels"));
    }
    let mut config = cpal::StreamConfig {
        channels,
        sample_rate: default_cfg.sample_rate(),
        buffer_size: cpal::BufferSize::Default,
    };
    apply_buffer_request(&mut config, default_cfg.buffer_size(), buffer_frames);
    if is_asio(&host_name) {
        // the session every later stream on this driver must share
        *asio = Some(AsioSession {
            driver: name.clone(),
            device: device.clone(),
            buffer: achieved_frames(false, buffer_frames, &config),
        });
    }
    let first = first_pair(req.out_first, channels);
    Ok(OutputTarget { device, name, host_name, config, format: default_cfg.sample_format(), wasapi, first })
}

/// Largest chunk the output callback renders at once. Bigger callbacks
/// are processed in SCRATCH_FRAMES slices (Engine::process accepts any
/// length), so no callback size can force an allocation.
const SCRATCH_FRAMES: usize = 4096;

/// Something that renders stereo planar audio inside the output callback.
trait OutputSource: Send + 'static {
    /// Once per callback, before the first `render`, with the callback's
    /// total frame count.
    fn begin(&mut self, _frames: usize, _info: &cpal::OutputCallbackInfo) {}
    /// Fill `l`/`r` (equal length, ≤ SCRATCH_FRAMES). RT-safe.
    fn render(&mut self, l: &mut [f32], r: &mut [f32]);
}

struct ToneSource {
    tone: sf_dsp::TestTone,
    /// interleaved stereo scratch, 2 × SCRATCH_FRAMES
    buf: Vec<f32>,
}

impl OutputSource for ToneSource {
    fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        let buf = &mut self.buf[..2 * n];
        self.tone.process_interleaved_stereo(buf);
        for i in 0..n {
            l[i] = buf[2 * i];
            r[i] = buf[2 * i + 1];
        }
    }
}

struct EngineSource {
    engine: sf_engine::Engine,
    hooks: Arc<OutputHooks>,
    clock: Arc<ClockOrigin>,
    chirp: Vec<f32>,
    master_prod: rtrb::Producer<f32>,
    /// interleaved stereo scratch for the master tap, 2 × SCRATCH_FRAMES
    tap_buf: Vec<f32>,
    /// stream frame index of the next frame `render` produces
    frame: u64,
}

impl OutputSource for EngineSource {
    fn begin(&mut self, frames: usize, info: &cpal::OutputCallbackInfo) {
        self.frame = self.hooks.master.note_callback(frames, self.clock.nanos(info.timestamp().playback));
    }

    fn render(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        self.engine.process(l, r);
        // calibration chirp, mixed post-limiter so its timing is the true
        // DAC timing (the limiter's lookahead would otherwise shift it)
        let mut ip = self.hooks.impulse_pos.load(Ordering::Acquire);
        if ip != IMPULSE_IDLE {
            if ip == 0 {
                self.hooks.impulse_frame.store(self.frame, Ordering::Release);
            }
            for i in 0..n {
                if (ip as usize) >= self.chirp.len() {
                    break;
                }
                let v = self.chirp[ip as usize];
                l[i] += v;
                r[i] += v;
                ip += 1;
            }
            self.hooks.impulse_pos.store(
                if (ip as usize) >= self.chirp.len() { IMPULSE_IDLE } else { ip },
                Ordering::Release,
            );
        }
        if self.hooks.capture_on.load(Ordering::Acquire) {
            let buf = &mut self.tap_buf[..2 * n];
            for i in 0..n {
                buf[2 * i] = l[i];
                buf[2 * i + 1] = r[i];
            }
            self.hooks.master.push_ring(&mut self.master_prod, buf);
        }
        self.frame += n as u64;
    }
}

/// Largest positive sample handed to a device: one 24-bit step below 1.0.
/// dasp converts +1.0 to I24 as 8_388_608 — one past the 24-bit maximum —
/// which wraps to full-scale NEGATIVE when packed into 3 bytes (a loud click
/// on every 0 dBFS peak through a 24-bit ASIO driver). i16/i32 saturate
/// anyway; for float devices the difference is -0.000001 dB.
const CLIP_HI: f32 = 1.0 - 1.0 / 8_388_608.0;

#[inline]
fn clip(x: f32) -> f32 {
    x.clamp(-1.0, CLIP_HI)
}

/// Stereo planar → `ch`-channel interleaved device samples. Channels
/// `first`/`first+1` get L/R (0 = outputs 1–2; the caller clamps it with
/// [`first_pair`]), every other channel silence, a mono device the L+R
/// average. Clipped first (see [`CLIP_HI`]): integer formats (ASIO is
/// usually i32/i24) would otherwise wrap instead of clip.
#[inline]
fn write_frames<T: Sample + FromSample<f32>>(out: &mut [T], ch: usize, first: usize, l: &[f32], r: &[f32]) {
    if ch == 1 {
        for (o, (a, b)) in out.iter_mut().zip(l.iter().zip(r)) {
            *o = T::from_sample(clip((a + b) * 0.5));
        }
        return;
    }
    for (i, frame) in out.chunks_exact_mut(ch).enumerate() {
        for s in frame.iter_mut() {
            *s = T::EQUILIBRIUM;
        }
        frame[first] = T::from_sample(clip(l[i]));
        frame[first + 1] = T::from_sample(clip(r[i]));
    }
}

/// Device input samples (`dev_ch` interleaved) → the first `kept` channels
/// as f32 interleaved.
#[inline]
fn fold_input<T: Sample>(data: &[T], dev_ch: usize, out: &mut [f32], kept: usize)
where
    f32: FromSample<T>,
{
    for (f, frame) in data.chunks_exact(dev_ch).enumerate() {
        for c in 0..kept {
            out[f * kept + c] = f32::from_sample(frame[c]);
        }
    }
}

fn record_output_stats(s: &Stats, frames: usize, dt: u64, ns_per_frame: f64) {
    let budget = (frames as f64 * ns_per_frame) as u64;
    s.budget_nanos.store(budget, Ordering::Relaxed);
    s.last_callback_nanos.store(dt, Ordering::Relaxed);
    s.max_callback_nanos.fetch_max(dt, Ordering::Relaxed);
    s.callbacks.fetch_add(1, Ordering::Relaxed);
    if frames > 0 {
        s.min_frames.fetch_min(frames as u32, Ordering::Relaxed);
        s.max_frames.fetch_max(frames as u32, Ordering::Relaxed);
    }
    if dt > budget {
        s.over_budget.fetch_add(1, Ordering::Relaxed);
    }
}

/// Build the output stream in the device's sample format.
fn build_output<S: OutputSource>(t: &OutputTarget, source: S, stats: &Arc<Stats>) -> Result<cpal::Stream, String> {
    match t.format {
        SampleFormat::F32 => build_output_typed::<f32, S>(t, source, stats),
        SampleFormat::F64 => build_output_typed::<f64, S>(t, source, stats),
        SampleFormat::I32 => build_output_typed::<i32, S>(t, source, stats),
        SampleFormat::I24 => build_output_typed::<cpal::I24, S>(t, source, stats),
        SampleFormat::I16 => build_output_typed::<i16, S>(t, source, stats),
        SampleFormat::U16 => build_output_typed::<u16, S>(t, source, stats),
        other => Err(format!("output device \"{}\" uses unsupported sample format {other}", t.name)),
    }
}

fn build_output_typed<T, S>(t: &OutputTarget, mut source: S, stats: &Arc<Stats>) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32> + Send + 'static,
    S: OutputSource,
{
    let ch = t.config.channels as usize;
    let first = t.first;
    // planar scratch, pre-allocated: the callback renders into these and
    // writes the device format — no allocation on the audio thread.
    let mut l = vec![0.0f32; SCRATCH_FRAMES];
    let mut r = vec![0.0f32; SCRATCH_FRAMES];
    let mmcss = t.wasapi;
    let mut thread_ready = false;
    let cb_stats = stats.clone();
    let err_stats = stats.clone();
    let ns_per_frame = 1e9 / t.config.sample_rate.0 as f64;
    t.device
        .build_output_stream(
            &t.config,
            move |out: &mut [T], info: &cpal::OutputCallbackInfo| {
                if !thread_ready {
                    set_flush_denormals();
                    cb_stats.rt_priority.store(promote_rt(mmcss), Ordering::Relaxed);
                    thread_ready = true;
                }
                let t0 = Instant::now();
                let total = out.len() / ch;
                source.begin(total, info);
                let mut done = 0usize;
                while done < total {
                    let n = (total - done).min(SCRATCH_FRAMES);
                    source.render(&mut l[..n], &mut r[..n]);
                    write_frames(&mut out[done * ch..(done + n) * ch], ch, first, &l[..n], &r[..n]);
                    done += n;
                }
                for s in &mut out[total * ch..] {
                    *s = T::EQUILIBRIUM; // partial trailing frame (never expected)
                }
                record_output_stats(&cb_stats, total, t0.elapsed().as_nanos() as u64, ns_per_frame);
            },
            move |e| note_stream_error(&err_stats.device_lost, &err_stats.xruns, e),
            None,
        )
        .map_err(|e| e.to_string())
}

fn open_tone_stream(
    req: OutputRequest<'_>,
    freq_hz: f32,
    gain: f32,
    stats: &Arc<Stats>,
    asio: &mut Option<AsioSession>,
) -> Result<(cpal::Stream, StreamInfo), String> {
    let buffer_frames = req.buffer_frames;
    let t = open_output(&req, asio)?;
    reset_stats(stats);
    let source = ToneSource {
        tone: sf_dsp::TestTone::new(freq_hz, t.config.sample_rate.0 as f32, gain),
        buf: vec![0.0f32; 2 * SCRATCH_FRAMES],
    };
    let stream = build_output(&t, source, stats)?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, t.info(buffer_frames)))
}

fn open_engine_stream(
    req: OutputRequest<'_>,
    stats: &Arc<Stats>,
    clock: &Arc<ClockOrigin>,
    asio: &mut Option<AsioSession>,
) -> Result<(cpal::Stream, StreamInfo, sf_engine::EngineControl, Arc<OutputHooks>), String> {
    let buffer_frames = req.buffer_frames;
    let t = open_output(&req, asio)?;
    let info = t.info(buffer_frames);
    let sample_rate = t.config.sample_rate.0;

    reset_stats(stats);
    let mut engine = sf_engine::Engine::new(sample_rate as f32);
    let ctl = engine.split_control();

    // recording hooks: master tap ring + writer thread, calibration chirp
    let master = Tap::new(2, sample_rate);
    let (master_prod, master_cons) = rtrb::RingBuffer::<f32>::new(sample_rate as usize * 2 * RING_SECONDS);
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
        buffer_frames: info.buffer_frames,
    });
    let source = EngineSource {
        engine,
        hooks: hooks.clone(),
        clock: clock.clone(),
        chirp: capture::chirp(sample_rate, 0.5),
        master_prod,
        tap_buf: vec![0.0f32; 2 * SCRATCH_FRAMES],
        frame: 0,
    };
    let stream = build_output(&t, source, stats)?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, info, ctl, hooks))
}

/// Device input channels carried from the callback to the writer thread.
/// Each take keeps only the channels its track asked for
/// ([`IoHandle::start_take_channels`]); 8 covers common USB interfaces and
/// keeps the ring small (8 ch × 2 s ≈ 3 MB at 48 kHz).
const MAX_INPUT_CHANNELS: usize = 8;

/// A resolved input device + its native config. Rate, channel count and
/// sample format are the device's; only the buffer size is negotiable. The
/// first `kept_channels` (≤ [`MAX_INPUT_CHANNELS`]) reach the tap.
struct InputTarget {
    device: cpal::Device,
    name: String,
    host_name: String,
    config: cpal::StreamConfig,
    format: SampleFormat,
    kept_channels: usize,
    wasapi: bool,
    /// Buffer request actually used (an ASIO input adopts the session's).
    requested: Option<u32>,
}

fn resolve_input(
    host: Option<&str>,
    device_name: Option<String>,
    mut buffer_frames: Option<u32>,
    asio: &mut Option<AsioSession>,
    output_open: bool,
) -> Result<InputTarget, String> {
    let host = host_named(host)?;
    let wasapi = is_wasapi(&host);
    let host_name = host.id().name().to_string();
    let asio_plan = if is_asio(&host_name) {
        Some(plan_asio_input(
            asio.as_ref().map(|s| (s.driver.as_str(), s.buffer)),
            output_open,
            device_name.as_deref(),
        )?)
    } else {
        None
    };
    let joined = match (&asio_plan, asio.as_ref()) {
        (Some(AsioInput::Join { buffer }), Some(s)) => {
            // same Device object as the output, same buffer size
            buffer_frames = *buffer;
            Some(s.device.clone())
        }
        _ => None,
    };
    let device = match (joined, &device_name) {
        (Some(d), _) => d,
        (None, None) => host.default_input_device().ok_or("no default input device")?,
        (None, Some(want)) => host
            .input_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|n| &n == want).unwrap_or(false))
            .ok_or_else(|| format!("input device \"{want}\" not found"))?,
    };
    let name = device.name().map_err(|e| e.to_string())?;
    let default_cfg = device.default_input_config().map_err(|e| e.to_string())?;
    let mut config: cpal::StreamConfig = default_cfg.config();
    if config.channels == 0 {
        return Err(format!("input device \"{name}\" reports 0 channels"));
    }
    apply_buffer_request(&mut config, default_cfg.buffer_size(), buffer_frames);
    if matches!(asio_plan, Some(AsioInput::Fresh)) {
        *asio = Some(AsioSession {
            driver: name.clone(),
            device: device.clone(),
            buffer: achieved_frames(false, buffer_frames, &config),
        });
    }
    let kept_channels = (config.channels as usize).min(MAX_INPUT_CHANNELS);
    Ok(InputTarget {
        device,
        name,
        host_name,
        config,
        format: default_cfg.sample_format(),
        kept_channels,
        wasapi,
        requested: buffer_frames,
    })
}

/// Open the input stream: callback → ring (no alloc) → writer thread → `tap`.
fn open_input_stream(
    host: Option<&str>,
    device_name: Option<String>,
    buffer_frames: Option<u32>,
    stats: &Arc<Stats>,
    clock: &Arc<ClockOrigin>,
    asio: &mut Option<AsioSession>,
    output_open: bool,
) -> Result<(cpal::Stream, StreamInfo, Arc<Tap>), String> {
    let t = resolve_input(host, device_name, buffer_frames, asio, output_open)?;
    let tap = Tap::new(t.kept_channels, t.config.sample_rate.0);
    let (prod, cons) =
        rtrb::RingBuffer::<f32>::new(t.config.sample_rate.0 as usize * t.kept_channels * RING_SECONDS);
    {
        let tap = tap.clone();
        std::thread::Builder::new()
            .name("sf-io-input-writer".into())
            .spawn(move || Tap::writer_loop(tap, cons))
            .map_err(|e| e.to_string())?;
    }
    reset_input_stats(stats);
    let stream = match t.format {
        SampleFormat::F32 => build_input_typed::<f32>(&t, tap.clone(), prod, stats, clock),
        SampleFormat::F64 => build_input_typed::<f64>(&t, tap.clone(), prod, stats, clock),
        SampleFormat::I32 => build_input_typed::<i32>(&t, tap.clone(), prod, stats, clock),
        SampleFormat::I24 => build_input_typed::<cpal::I24>(&t, tap.clone(), prod, stats, clock),
        SampleFormat::I16 => build_input_typed::<i16>(&t, tap.clone(), prod, stats, clock),
        SampleFormat::U16 => build_input_typed::<u16>(&t, tap.clone(), prod, stats, clock),
        other => Err(format!("input device \"{}\" uses unsupported sample format {other}", t.name)),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((
        stream,
        StreamInfo {
            device: t.name.clone(),
            host: t.host_name.clone(),
            sample_rate: t.config.sample_rate.0,
            buffer_frames: achieved_frames(t.wasapi, t.requested, &t.config),
            channels: t.config.channels,
            sample_format: t.format.to_string(),
        },
        tap,
    ))
}

fn build_input_typed<T>(
    t: &InputTarget,
    tap: Arc<Tap>,
    mut prod: rtrb::Producer<f32>,
    stats: &Arc<Stats>,
    clock: &Arc<ClockOrigin>,
) -> Result<cpal::Stream, String>
where
    T: SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    let dev_ch = t.config.channels as usize;
    let kept = t.kept_channels;
    let mut scratch = vec![0.0f32; SCRATCH_FRAMES * kept];
    let mmcss = t.wasapi;
    let mut thread_ready = false;
    let cb_stats = stats.clone();
    let err_stats = stats.clone();
    let cb_clock = clock.clone();
    t.device
        .build_input_stream(
            &t.config,
            move |data: &[T], info: &cpal::InputCallbackInfo| {
                if !thread_ready {
                    set_flush_denormals();
                    let _ = promote_rt(mmcss);
                    thread_ready = true;
                }
                let frames = data.len() / dev_ch;
                tap.note_callback(frames, cb_clock.nanos(info.timestamp().capture));
                let mut peak = 0.0f32;
                let mut done = 0usize;
                while done < frames {
                    let n = (frames - done).min(SCRATCH_FRAMES);
                    let buf = &mut scratch[..n * kept];
                    fold_input(&data[done * dev_ch..(done + n) * dev_ch], dev_ch, buf, kept);
                    for &v in buf.iter() {
                        peak = peak.max(v.abs());
                    }
                    tap.push_ring(&mut prod, buf);
                    done += n;
                }
                cb_stats.input_peak_bits.store(peak.to_bits(), Ordering::Relaxed);
                cb_stats.input_callbacks.fetch_add(1, Ordering::Relaxed);
                cb_stats.input_frames.fetch_add(frames as u64, Ordering::Relaxed);
                cb_stats.input_max_frames.fetch_max(frames as u32, Ordering::Relaxed);
                if peak > 0.0 {
                    cb_stats.input_nonzero.store(true, Ordering::Relaxed);
                }
            },
            move |e| note_stream_error(&err_stats.input_device_lost, &err_stats.input_xruns, e),
            None,
        )
        .map_err(|e| e.to_string())
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

#[cfg(test)]
mod port_tests {
    //! Windows-port logic (docs/WINDOWS_PORT_PLAN.md W1) that needs no
    //! audio hardware: channel mapping, sample conversion, watchdog rules.
    use super::*;

    #[test]
    fn stereo_to_multichannel_fills_pair_and_silences_rest() {
        let l = [0.5f32, -0.25];
        let r = [0.1f32, 0.2];
        let mut out = [9.0f32; 2 * 4];
        write_frames(&mut out, 4, 0, &l, &r);
        assert_eq!(out, [0.5, 0.1, 0.0, 0.0, -0.25, 0.2, 0.0, 0.0]);
    }

    #[test]
    fn output_pair_routes_to_chosen_channels() {
        // 6-channel interface, headphones on outputs 3–4
        let mut out = [9.0f32; 6];
        write_frames(&mut out, 6, first_pair(2, 6), &[0.5], &[-0.5]);
        assert_eq!(out, [0.0, 0.0, 0.5, -0.5, 0.0, 0.0]);
        // requests past the end clamp to the last pair; mono ignores it
        assert_eq!(first_pair(7, 6), 4);
        assert_eq!(first_pair(3, 2), 0);
        assert_eq!(first_pair(5, 1), 0);
    }

    #[test]
    fn mono_device_gets_downmix() {
        let mut out = [9.0f32; 2];
        write_frames(&mut out, 1, 0, &[1.0, 0.0], &[0.0, -0.5]);
        assert_eq!(out, [0.5, -0.25]);
    }

    #[test]
    fn integer_formats_clip_instead_of_wrapping() {
        // the calibration chirp is mixed post-limiter and can exceed 1.0
        let mut out = [0i32; 4];
        write_frames(&mut out, 2, 0, &[1.5, -2.0], &[0.5, 0.0]);
        // positive ceiling is CLIP_HI (one 24-bit step below full scale)
        assert_eq!(out[0], i32::MAX - 255);
        assert_eq!(out[2], i32::MIN);
        assert!((out[1] as f64 / i32::MAX as f64 - 0.5).abs() < 1e-6);
        // exactly 0 dBFS and beyond must land on the 24-bit extremes
        let mut out24 = [cpal::I24::EQUILIBRIUM; 4];
        write_frames(&mut out24, 2, 0, &[1.0, 3.0], &[-1.0, -3.0]);
        assert_eq!(out24[0].inner(), (1 << 23) - 1);
        assert_eq!(out24[1].inner(), -(1 << 23));
        assert_eq!(out24[2].inner(), (1 << 23) - 1);
        assert_eq!(out24[3].inner(), -(1 << 23));
        let mut out16 = [0u16; 2];
        write_frames(&mut out16, 2, 0, &[0.0], &[0.0]);
        assert_eq!(out16[0], u16::EQUILIBRIUM, "unsigned silence is mid-scale");
    }

    #[test]
    fn input_fold_keeps_first_channels_and_converts() {
        // 4-channel i16 device, keep 2
        let data: [i16; 8] = [16384, -16384, 7, 7, 0, 32767, 7, 7];
        let mut out = [0.0f32; 4];
        fold_input(&data, 4, &mut out, 2);
        assert_eq!(out[0], 0.5);
        assert_eq!(out[1], -0.5);
        assert_eq!(out[2], 0.0);
        assert!(out[3] > 0.999);
    }

    fn info(device: &str, host: &str) -> StreamInfo {
        StreamInfo {
            device: device.into(),
            host: host.into(),
            sample_rate: 48_000,
            buffer_frames: None,
            channels: 2,
            sample_format: "f32".into(),
        }
    }

    fn req(device: Option<&str>) -> StreamRequest {
        StreamRequest { host: None, device: device.map(Into::into), buffer_frames: None }
    }

    #[test]
    fn watchdog_reports_loss_once_then_retries() {
        let t0 = Instant::now();
        let mut w = Watch::new(req(Some("USB")), &info("USB", "WASAPI"), t0);
        assert_eq!(w.assess(10, false, None, t0 + Duration::from_millis(500)), None);
        let r = w.assess(11, true, None, t0 + Duration::from_millis(1000));
        assert!(r.unwrap().contains("disconnected"));
        // suppressed while a restart is in flight…
        assert_eq!(w.assess(11, true, None, t0 + Duration::from_millis(2000)), None);
        // …re-reported later so a failed restart is retried
        assert!(w.assess(11, true, None, t0 + Duration::from_millis(6500)).is_some());
    }

    #[test]
    fn asio_input_joins_the_output_session() {
        // output running on the Focusrite at 128: input joins with 128
        assert_eq!(
            plan_asio_input(Some(("Focusrite USB ASIO", Some(128))), true, None),
            Ok(AsioInput::Join { buffer: Some(128) })
        );
        assert_eq!(
            plan_asio_input(Some(("Focusrite USB ASIO", None)), true, Some("Focusrite USB ASIO")),
            Ok(AsioInput::Join { buffer: None })
        );
        // a different ASIO driver while output runs: refused, never re-buffered
        let err = plan_asio_input(Some(("Focusrite USB ASIO", Some(128))), true, Some("MOTU ASIO")).unwrap_err();
        assert!(err.contains("one driver at a time"), "{err}");
        // no output running: the input starts its own session
        assert_eq!(plan_asio_input(None, false, Some("MOTU ASIO")), Ok(AsioInput::Fresh));
        assert_eq!(plan_asio_input(Some(("MOTU ASIO", None)), false, Some("Other")), Ok(AsioInput::Fresh));
    }

    #[test]
    fn output_open_closes_input_only_when_it_must() {
        // ASIO output always re-initialises the driver
        assert!(input_closes_for_output(true, true, Some("ASIO"), Some("ASIO")));
        // driver type changed
        assert!(input_closes_for_output(false, true, Some("ASIO"), Some("WASAPI")));
        assert!(input_closes_for_output(false, true, None, Some("WASAPI")));
        // same non-ASIO driver: input untouched (WASAPI/CoreAudio streams are independent)
        assert!(!input_closes_for_output(false, true, Some("WASAPI"), Some("WASAPI")));
        assert!(!input_closes_for_output(false, true, None, None));
        // nothing to close
        assert!(!input_closes_for_output(true, false, None, Some("ASIO")));
    }

    #[test]
    fn failed_start_is_retried_after_backoff() {
        let t0 = Instant::now() + Duration::from_secs(10); // keep checked_sub in range
        let mut w = Watch::failed(req(Some("USB")), t0);
        // frozen callback counter; first look is inside the retry back-off
        assert_eq!(w.assess(7, false, None, t0 + Duration::from_millis(1000)), None);
        assert_eq!(w.assess(7, false, None, t0 + Duration::from_millis(4000)), None);
        let r = w.assess(7, false, None, t0 + Duration::from_millis(5500));
        assert!(r.unwrap().contains("\"USB\""));
    }

    #[test]
    fn watchdog_detects_stall_only_after_grace() {
        let t0 = Instant::now();
        let mut w = Watch::new(req(None), &info("Speakers", "WASAPI"), t0);
        // no callbacks yet, but still inside the grace period
        assert_eq!(w.assess(0, false, None, t0 + Duration::from_millis(2500)), None);
        // callbacks flowing
        assert_eq!(w.assess(100, false, None, t0 + Duration::from_millis(3500)), None);
        // count frozen for > STALL_AFTER
        let r = w.assess(100, false, None, t0 + Duration::from_millis(6000));
        assert!(r.unwrap().contains("stopped responding"));
    }

    #[test]
    fn watchdog_follows_default_only_when_unpinned() {
        let t0 = Instant::now();
        let mut follow = Watch::new(req(None), &info("Speakers", "WASAPI"), t0);
        assert!(!follow.due_default_check(t0 + Duration::from_millis(500)));
        assert!(follow.due_default_check(t0 + Duration::from_millis(2500)));
        assert_eq!(follow.assess(5, false, Some("Speakers"), t0 + Duration::from_millis(2500)), None);
        let r = follow.assess(6, false, Some("Headphones"), t0 + Duration::from_millis(2600));
        assert!(r.unwrap().contains("Headphones"));

        // pinned device: the system default is irrelevant
        let mut pinned = Watch::new(req(Some("USB")), &info("USB", "WASAPI"), t0);
        assert!(!pinned.due_default_check(t0 + Duration::from_secs(10)));
        assert_eq!(pinned.assess(5, false, Some("Headphones"), t0 + Duration::from_millis(500)), None);

        // ASIO: never polled (only the loaded driver enumerates)
        let mut asio = Watch::new(req(None), &info("Focusrite USB ASIO", "ASIO"), t0);
        assert!(!asio.due_default_check(t0 + Duration::from_secs(10)));
    }

    #[test]
    fn hosts_list_has_one_default_and_names_resolve() {
        let hs = hosts();
        assert_eq!(hs.iter().filter(|h| h.is_default).count(), 1);
        for h in &hs {
            assert!(host_named(Some(&h.name.to_lowercase())).is_ok(), "{} should resolve", h.name);
        }
        let err = host_named(Some("NoSuchDriver")).err().expect("unknown driver must fail");
        assert!(err.contains("not available"));
        assert!(host_named(None).is_ok());
    }

    #[test]
    fn wasapi_never_claims_a_fixed_buffer() {
        let cfg = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(48_000),
            buffer_size: cpal::BufferSize::Fixed(64),
        };
        assert_eq!(achieved_frames(true, Some(64), &cfg), None);
        assert_eq!(achieved_frames(false, Some(64), &cfg), Some(64));
        let mut c = cfg.clone();
        c.buffer_size = cpal::BufferSize::Default;
        apply_buffer_request(&mut c, &cpal::SupportedBufferSize::Range { min: 32, max: 1024 }, Some(16));
        assert_eq!(c.buffer_size, cpal::BufferSize::Default, "out of range stays Default");
        apply_buffer_request(&mut c, &cpal::SupportedBufferSize::Unknown, Some(128));
        assert_eq!(c.buffer_size, cpal::BufferSize::Default);
        apply_buffer_request(&mut c, &cpal::SupportedBufferSize::Range { min: 32, max: 1024 }, Some(128));
        assert_eq!(c.buffer_size, cpal::BufferSize::Fixed(128));
    }
}
