//! Recording plumbing shared by the input stream (takes) and the output
//! stream (master capture): a frame-indexed tap that an audio callback feeds
//! through an `rtrb` ring and a writer thread drains into growable buffers —
//! but ONLY between `arm()` and `disarm()`, and frame-exactly: arm/disarm
//! snapshot the stream's running frame counter, so a take contains exactly the
//! frames `[start, end)` regardless of when the writer thread gets scheduled.
//!
//! Nothing here runs on the audio thread except [`Tap::note_callback`] (three
//! atomic stores) and the ring push, which the callback owners do themselves.
//!
//! Also home to the pure DSP for loopback latency calibration: the test chirp,
//! the cross-correlation peak finder, and the compensation shift.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cpal::StreamInstant;

/// Seconds of audio the callback→writer ring can absorb before dropping.
pub const RING_SECONDS: usize = 2;

/// Clock origin shared by every stream so their `StreamInstant`s can be put
/// on one signed nanosecond axis (cpal's instants are comparable across
/// streams on one host — CoreAudio/WASAPI derive them from the host clock —
/// but only expose relative `duration_since`).
#[derive(Default)]
pub struct ClockOrigin(std::sync::OnceLock<StreamInstant>);

impl ClockOrigin {
    /// Nanoseconds of `t` relative to the origin (set lazily by the first
    /// caller). RT-safe after the first call: a OnceLock get is one load.
    pub fn nanos(&self, t: StreamInstant) -> i64 {
        let o = self.0.get_or_init(|| t);
        if let Some(d) = t.duration_since(o) {
            d.as_nanos() as i64
        } else if let Some(d) = o.duration_since(&t) {
            -(d.as_nanos() as i64)
        } else {
            0
        }
    }
}

/// Timestamp anchor: the stream frame index of a callback's first frame and
/// the instant that frame hits (output) or left (input) the converter.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Anchor {
    pub frame: u64,
    pub nanos: i64,
}

impl Anchor {
    /// Instant (nanos on the shared axis) of an arbitrary frame index,
    /// extrapolated at `sample_rate`.
    pub fn instant_of(&self, frame: u64, sample_rate: u32) -> f64 {
        self.nanos as f64 + (frame as f64 - self.frame as f64) * 1e9 / sample_rate as f64
    }
}

struct TapInner {
    armed: bool,
    /// First frame index to keep (set by `arm`).
    start: u64,
    /// Exclusive end (set by `disarm`); `None` while still recording.
    end: Option<u64>,
    /// Frame index of the next frame the writer will see.
    cursor: u64,
    bufs: Vec<Vec<f32>>,
    /// Writer reached `end` (or the arm was abandoned) — buffers are final.
    complete: bool,
    /// Anchor snapshot taken at arm time (None: no callback had run yet).
    start_anchor: Option<Anchor>,
    start_frame_other: u64,
}

/// A frame-indexed capture point on one stream.
pub struct Tap {
    channels: usize,
    sample_rate: u32,
    /// Frames the callback has produced so far (index of the next frame).
    total: AtomicU64,
    anchor_frame: AtomicU64,
    anchor_nanos: AtomicI64,
    /// Ring overflow events (writer fell behind by > RING_SECONDS).
    pub drops: AtomicU64,
    inner: Mutex<TapInner>,
}

/// A finished capture.
#[derive(Debug, Clone, PartialEq)]
pub struct Take {
    pub sample_rate: u32,
    /// Planar channels, all equal length.
    pub channels: Vec<Vec<f32>>,
    /// Stream frame index of `channels[*][0]`.
    pub start_frame: u64,
    /// Timestamp anchor snapshotted at arm time (None: stream had not run).
    pub start_anchor: Option<Anchor>,
}

impl Tap {
    pub fn new(channels: usize, sample_rate: u32) -> Arc<Self> {
        Arc::new(Self {
            channels: channels.max(1),
            sample_rate,
            total: AtomicU64::new(0),
            anchor_frame: AtomicU64::new(0),
            anchor_nanos: AtomicI64::new(i64::MIN),
            drops: AtomicU64::new(0),
            inner: Mutex::new(TapInner {
                armed: false,
                start: 0,
                end: None,
                cursor: 0,
                bufs: Vec::new(),
                complete: true,
                start_anchor: None,
                start_frame_other: 0,
            }),
        })
    }

    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    /// Frames produced by the stream so far.
    pub fn total_frames(&self) -> u64 {
        self.total.load(Ordering::Acquire)
    }

    /// AUDIO THREAD: record this callback's anchor, then account `frames`.
    /// Returns the stream frame index of the callback's first frame.
    #[inline]
    pub fn note_callback(&self, frames: usize, instant_nanos: i64) -> u64 {
        let first = self.total.load(Ordering::Relaxed);
        self.anchor_frame.store(first, Ordering::Relaxed);
        self.anchor_nanos.store(instant_nanos, Ordering::Relaxed);
        self.total.store(first + frames as u64, Ordering::Release);
        first
    }

    /// Latest timestamp anchor (None until the first callback ran).
    pub fn anchor(&self) -> Option<Anchor> {
        let nanos = self.anchor_nanos.load(Ordering::Relaxed);
        if nanos == i64::MIN {
            return None;
        }
        Some(Anchor { frame: self.anchor_frame.load(Ordering::Relaxed), nanos })
    }

    pub fn is_armed(&self) -> bool {
        self.inner.lock().map(|i| i.armed).unwrap_or(false)
    }

    /// Start keeping frames from the stream's CURRENT frame index. Returns the
    /// start frame. `other_frame` is any companion counter to snapshot at the
    /// same instant (the output stream's frame count for an input take).
    pub fn arm(&self, other_frame: u64) -> u64 {
        self.arm_inner(other_frame, false)
    }

    /// Arm a tap whose producer is GATED — it pushes only while recording.
    ///
    /// `total` counts every frame the stream produced; `cursor` counts only
    /// frames handed to the tap. An ungated producer keeps them in lockstep,
    /// which is what [`arm`] assumes. The master tap pushes only while
    /// recording, so `cursor` lags `total` by the ENTIRE idle period, and
    /// `ingest` then discards every captured frame as "before start" — the
    /// take comes back empty and Record Output silently produced nothing.
    ///
    /// Snapping the cursor to the stream clock in the same critical section
    /// as the arm fixes that. Valid only while the producer is idle and the
    /// ring drained (i.e. before the gate is opened), which is exactly when
    /// this is called.
    pub fn arm_gated(&self, other_frame: u64) -> u64 {
        self.arm_inner(other_frame, true)
    }

    fn arm_inner(&self, other_frame: u64, resync_cursor: bool) -> u64 {
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // read the counter UNDER the lock: the writer drains under the same
        // lock, so its cursor is ≤ total here → frames ≥ start are all still
        // in the ring (or not produced yet) — nothing to recover
        let start = self.total.load(Ordering::Acquire);
        if resync_cursor {
            i.cursor = start;
        }
        i.armed = true;
        i.start = start;
        i.end = None;
        i.complete = false;
        i.bufs = (0..self.channels).map(|_| Vec::new()).collect();
        i.start_anchor = self.anchor();
        i.start_frame_other = other_frame;
        start
    }

    /// Stop at the stream's current frame index and wait (≤ `timeout`) for the
    /// writer to deliver every frame up to it. Returns the take plus the
    /// companion counter snapshotted at arm.
    pub fn disarm(&self, timeout: Duration) -> Option<(Take, u64)> {
        {
            let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if !i.armed {
                return None;
            }
            let end = self.total.load(Ordering::Acquire).max(i.start);
            i.end = Some(end);
            if i.cursor >= end {
                i.complete = true;
                i.armed = false;
            }
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            {
                let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
                if i.complete || std::time::Instant::now() >= deadline {
                    i.armed = false;
                    i.complete = true;
                    let bufs = std::mem::take(&mut i.bufs);
                    return Some((
                        Take {
                            sample_rate: self.sample_rate,
                            channels: bufs,
                            start_frame: i.start,
                            start_anchor: i.start_anchor,
                        },
                        i.start_frame_other,
                    ));
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// WRITER THREAD: consume `interleaved` (whole frames, `channels` wide) in
    /// stream order. Frames inside `[start, end)` are appended to the take.
    pub fn ingest(&self, interleaved: &[f32]) {
        let ch = self.channels;
        let frames = interleaved.len() / ch;
        let mut i = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let first = i.cursor;
        i.cursor = first + frames as u64;
        if !i.armed || i.complete {
            return;
        }
        let end = i.end.unwrap_or(u64::MAX);
        let lo = i.start.max(first);
        let hi = end.min(first + frames as u64);
        if lo < hi {
            let a = (lo - first) as usize;
            let b = (hi - first) as usize;
            for c in 0..ch {
                let buf = &mut i.bufs[c];
                buf.extend(interleaved[a * ch..b * ch].iter().skip(c).step_by(ch));
            }
        }
        if i.cursor >= end {
            i.complete = true;
        }
    }

    /// Writer-thread body: drain `consumer` into `tap` until the producer
    /// (owned by the audio callback) is dropped with its stream.
    pub fn writer_loop(tap: Arc<Tap>, mut consumer: rtrb::Consumer<f32>) {
        let ch = tap.channels;
        let mut scratch = vec![0.0f32; 4096 * ch];
        loop {
            let avail = consumer.slots() / ch * ch;
            if avail > 0 {
                let n = avail.min(scratch.len());
                if let Ok(chunk) = consumer.read_chunk(n) {
                    let (a, b) = chunk.as_slices();
                    scratch[..a.len()].copy_from_slice(a);
                    scratch[a.len()..a.len() + b.len()].copy_from_slice(b);
                    chunk.commit_all();
                    tap.ingest(&scratch[..n]);
                    continue; // keep draining while there's backlog
                }
            }
            if consumer.is_abandoned() {
                // stream gone: whatever is armed can never complete
                let mut i = tap.inner.lock().unwrap_or_else(|e| e.into_inner());
                i.complete = true;
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// AUDIO THREAD: push whole frames into the ring; counts (never blocks
    /// on) overflow.
    #[inline]
    pub fn push_ring(&self, producer: &mut rtrb::Producer<f32>, interleaved: &[f32]) {
        let ch = self.channels;
        let room = producer.slots() / ch * ch;
        let n = interleaved.len().min(room);
        if n < interleaved.len() {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
        if n == 0 {
            return;
        }
        if let Ok(chunk) = producer.write_chunk_uninit(n) {
            chunk.fill_from_iter(interleaved[..n].iter().copied());
        }
    }
}

/// Drop the first `frames` of every channel (record-latency compensation:
/// a take's sample 0 was captured `frames` before the sound of the playhead
/// it was anchored to could arrive). Negative/zero = untouched.
pub fn shift_left(channels: &mut [Vec<f32>], frames: i64) {
    if frames <= 0 {
        return;
    }
    for c in channels.iter_mut() {
        let n = (frames as usize).min(c.len());
        c.drain(..n);
    }
}

// ---- loopback calibration DSP ----

/// Calibration signal: 20 ms linear chirp 400 Hz → 6 kHz, Hann-windowed, at
/// `gain`. A chirp's autocorrelation is a sharp spike (unlike a click it is
/// robust to speaker/mic band limits and to room noise).
pub fn chirp(sample_rate: u32, gain: f32) -> Vec<f32> {
    let n = (sample_rate as f32 * 0.020) as usize;
    let (f0, f1) = (400.0f32, 6000.0f32);
    let dur = n as f32 / sample_rate as f32;
    (0..n)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            let phase = 2.0 * std::f32::consts::PI * (f0 * t + (f1 - f0) * t * t / (2.0 * dur));
            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos();
            gain * w * phase.sin()
        })
        .collect()
}

/// Locate `template` inside `signal` by normalized cross-correlation. Returns
/// the sample offset of the best match when its correlation coefficient
/// clears `min_corr` (0..1), else None (no loopback / mic pickup).
pub fn find_delay(signal: &[f32], template: &[f32], min_corr: f32) -> Option<usize> {
    let m = template.len();
    if m == 0 || signal.len() < m {
        return None;
    }
    let t_energy: f64 = template.iter().map(|&v| (v as f64) * (v as f64)).sum();
    if t_energy <= 0.0 {
        return None;
    }
    // sliding window energy, maintained incrementally
    let mut w_energy: f64 = signal[..m].iter().map(|&v| (v as f64) * (v as f64)).sum();
    let mut best = (0usize, 0.0f64);
    for lag in 0..=signal.len() - m {
        if lag > 0 {
            let out = signal[lag - 1] as f64;
            let inn = signal[lag + m - 1] as f64;
            w_energy += inn * inn - out * out;
        }
        if w_energy > 1e-12 {
            let dot: f64 = signal[lag..lag + m]
                .iter()
                .zip(template)
                .map(|(&a, &b)| a as f64 * b as f64)
                .sum();
            let r = dot / (w_energy.max(0.0) * t_energy).sqrt();
            if r > best.1 {
                best = (lag, r);
            }
        }
    }
    (best.1 >= min_corr as f64).then_some(best.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// REGRESSION: "Record Output" captured nothing.
    ///
    /// The engine output callback notes EVERY frame but only pushes while
    /// recording, so by the time the user armed, `cursor` (frames delivered)
    /// lagged `total` (frames produced) by the whole idle period. `arm()`
    /// starts at `total`, so `ingest` discarded every captured frame as
    /// "before start" and the take came back empty — silently.
    #[test]
    fn gated_producer_captures_audio_after_a_long_idle_period() {
        let tap = Tap::new(2, 48_000);
        // stream runs for 10 s WITHOUT recording: noted, never pushed
        for _ in 0..(10 * 48_000 / 128) {
            tap.note_callback(128, 0);
        }
        tap.arm_gated(0);
        // now recording: the callback notes AND pushes each block
        let block = [0.25f32; 128 * 2];
        for _ in 0..8 {
            tap.note_callback(128, 0);
            tap.ingest(&block); // stands in for ring push + writer drain
        }
        let (take, _) = tap.disarm(Duration::from_millis(50)).expect("armed take");
        assert_eq!(take.channels.len(), 2);
        assert_eq!(take.channels[0].len(), 8 * 128, "every recorded frame is kept");
        assert!(take.channels[0].iter().all(|&v| v == 0.25), "audio, not silence");
    }

    /// The ungated path (input stream: notes and pushes every frame) must be
    /// unaffected by the gated variant's cursor re-sync.
    #[test]
    fn ungated_arm_still_starts_at_the_stream_frame() {
        let tap = Tap::new(1, 48_000);
        tap.note_callback(300, 0);
        tap.ingest(&[0.0; 300]);
        assert_eq!(tap.arm(0), 300);
    }

    #[test]
    fn take_is_frame_exact_regardless_of_writer_timing() {
        let tap = Tap::new(2, 48_000);
        // callback produced 300 frames before anyone armed
        tap.note_callback(300, 0);
        let start = tap.arm(7);
        assert_eq!(start, 300);
        // writer is LATE: nothing drained yet; callback keeps producing
        tap.note_callback(200, 0);
        // stream-order frames 0..500 arrive at the writer in odd chunk sizes
        let frames: Vec<f32> = (0..500).flat_map(|f| [f as f32, -(f as f32)]).collect();
        tap.ingest(&frames[..2 * 123]);
        tap.ingest(&frames[2 * 123..2 * 400]);
        // disarm at frame 500 (current total) while 100 frames are still undrained
        let tap2 = tap.clone();
        let h = std::thread::spawn(move || tap2.disarm(Duration::from_secs(2)));
        std::thread::sleep(Duration::from_millis(20));
        tap.ingest(&frames[2 * 400..]);
        let (take, other) = h.join().unwrap().unwrap();
        assert_eq!(other, 7);
        assert_eq!(take.start_frame, 300);
        assert_eq!(take.channels.len(), 2);
        assert_eq!(take.channels[0].len(), 200);
        assert_eq!(take.channels[0][0], 300.0);
        assert_eq!(take.channels[1][0], -300.0);
        assert_eq!(*take.channels[0].last().unwrap(), 499.0);
        // frames after disarm are ignored
        tap.ingest(&[9.0, 9.0]);
        assert!(!tap.is_armed());
    }

    #[test]
    fn disarm_without_arm_is_none() {
        let tap = Tap::new(1, 44_100);
        assert!(tap.disarm(Duration::from_millis(10)).is_none());
    }

    #[test]
    fn ring_to_writer_round_trip() {
        let tap = Tap::new(2, 48_000);
        let (mut prod, cons) = rtrb::RingBuffer::<f32>::new(1024);
        let t2 = tap.clone();
        let writer = std::thread::spawn(move || Tap::writer_loop(t2, cons));
        tap.arm(0);
        let mut expect = Vec::new();
        for blk in 0..40 {
            let frames: Vec<f32> = (0..64).flat_map(|i| {
                let v = (blk * 64 + i) as f32;
                [v, v * 0.5]
            }).collect();
            expect.extend(frames.iter().step_by(2));
            tap.push_ring(&mut prod, &frames);
            tap.note_callback(64, 0);
            std::thread::sleep(Duration::from_millis(1));
        }
        let (take, _) = tap.disarm(Duration::from_secs(2)).unwrap();
        drop(prod);
        writer.join().unwrap();
        assert_eq!(take.channels[0], expect);
        assert_eq!(take.channels[1][3], 1.5);
        assert_eq!(tap.drops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn xcorr_finds_exact_delay() {
        let sr = 48_000;
        let t = chirp(sr, 0.5);
        let delay = 1234usize;
        let mut sig = vec![0.0f32; delay + t.len() + 5000];
        // attenuated + noisy copy, like a mic hearing a speaker
        let mut seed = 12345u32;
        for (i, v) in sig.iter_mut().enumerate() {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = ((seed >> 9) as f32 / (1 << 23) as f32 - 1.0) * 0.02;
            *v = noise + if i >= delay && i < delay + t.len() { 0.1 * t[i - delay] } else { 0.0 };
        }
        assert_eq!(find_delay(&sig, &t, 0.3), Some(delay));
    }

    #[test]
    fn xcorr_rejects_silence_and_noise() {
        let t = chirp(48_000, 0.5);
        assert_eq!(find_delay(&vec![0.0; 20_000], &t, 0.3), None);
        let mut seed = 7u32;
        let noise: Vec<f32> = (0..20_000)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 9) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect();
        assert_eq!(find_delay(&noise, &t, 0.3), None);
    }

    #[test]
    fn compensation_shift() {
        let mut ch = vec![vec![0.0, 1.0, 2.0, 3.0], vec![4.0, 5.0, 6.0, 7.0]];
        shift_left(&mut ch, 0);
        assert_eq!(ch[0].len(), 4);
        shift_left(&mut ch, -5);
        assert_eq!(ch[0].len(), 4);
        shift_left(&mut ch, 3);
        assert_eq!(ch, vec![vec![3.0], vec![7.0]]);
        shift_left(&mut ch, 10);
        assert!(ch[0].is_empty());
    }

    #[test]
    fn anchor_extrapolation() {
        let a = Anchor { frame: 1000, nanos: 5_000_000 };
        assert_eq!(a.instant_of(1048, 48_000), 6_000_000.0);
        assert_eq!(a.instant_of(952, 48_000), 4_000_000.0);
    }
}
