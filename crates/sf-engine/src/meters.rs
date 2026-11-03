//! Meter/snapshot plumbing: a flat bank of atomics the audio thread writes and
//! the UI polls (RUST_ARCH.md "Meters"). Rebuilt with the graph; the engine
//! keeps an `Arc` to the latest bank so `snapshot()` never touches the RT side.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

fn store_f32(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}

fn load_f32(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

/// Device-meter release time. The rack polls at ~30 Hz, but a block is only
/// ≤128 samples (~3 ms) — reading a raw block peak would sample a random 3 ms
/// window and miss most transients. A decaying peak-hold keeps the loudest
/// recent level with a smooth fall, like the browser's smoothed Tone.Meter.
pub const DEVICE_METER_RELEASE_S: f32 = 0.3;

/// Per-block decay factor for [`DEVICE_METER_RELEASE_S`] (computed once per
/// block by the graph, shared by every device meter in that block).
#[inline]
pub fn meter_decay(frames: usize, sample_rate: f32) -> f32 {
    (-(frames as f32) / (sample_rate.max(1.0) * DEVICE_METER_RELEASE_S)).exp()
}

/// Peak |sample| across a stereo block.
#[inline]
pub fn block_peak(l: &[f32], r: &[f32]) -> f32 {
    let mut p = 0.0f32;
    for (a, b) in l.iter().zip(r) {
        p = p.max(a.abs()).max(b.abs());
    }
    p
}

pub struct MeterBank {
    track_peak: Vec<AtomicU32>,
    track_rms: Vec<AtomicU32>,
    master_peak: AtomicU32,
    master_rms: AtomicU32,
    /// Per-fx gain reduction in dB (≤ 0), flat-indexed across all fx chains.
    fx_gr: Vec<AtomicU32>,
    /// Per-track instrument output level (linear, decaying peak-hold) — the
    /// device rack's instrument meter, measured AFTER the instrument out knob.
    inst_level: Vec<AtomicU32>,
    /// Per-fx output level (linear, decaying peak-hold), same flat index as
    /// `fx_gr` — measured AFTER each device's out knob, like the browser.
    fx_level: Vec<AtomicU32>,
    /// Per-fx, per-band gain reduction in dB (≤ 0): 3 slots per fx, same flat
    /// index. Only the multiband compressor reports non-zero values.
    fx_bands: Vec<AtomicU32>,
    playhead_ticks: AtomicU64,
    playing: AtomicBool,
}

impl MeterBank {
    pub fn new(tracks: usize, fx: usize) -> Self {
        let z = || AtomicU32::new(0.0f32.to_bits());
        Self {
            track_peak: (0..tracks).map(|_| z()).collect(),
            track_rms: (0..tracks).map(|_| z()).collect(),
            master_peak: z(),
            master_rms: z(),
            fx_gr: (0..fx).map(|_| z()).collect(),
            inst_level: (0..tracks).map(|_| z()).collect(),
            fx_level: (0..fx).map(|_| z()).collect(),
            fx_bands: (0..fx * 3).map(|_| z()).collect(),
            playhead_ticks: AtomicU64::new(0.0f64.to_bits()),
            playing: AtomicBool::new(false),
        }
    }

    #[inline]
    pub fn set_track(&self, idx: usize, peak: f32, rms: f32) {
        if let (Some(p), Some(r)) = (self.track_peak.get(idx), self.track_rms.get(idx)) {
            store_f32(p, peak);
            store_f32(r, rms);
        }
    }

    #[inline]
    pub fn set_master(&self, peak: f32, rms: f32) {
        store_f32(&self.master_peak, peak);
        store_f32(&self.master_rms, rms);
    }

    #[inline]
    pub fn set_gr(&self, idx: usize, gr_db: f32) {
        if let Some(a) = self.fx_gr.get(idx) {
            store_f32(a, gr_db);
        }
    }

    /// AUDIO THREAD: fold a block peak into a decaying peak-hold.
    #[inline]
    fn hold(slot: &AtomicU32, block_peak: f32, decay: f32) {
        let held = load_f32(slot) * decay;
        store_f32(slot, if block_peak > held { block_peak } else { held });
    }

    #[inline]
    pub fn set_inst_level(&self, track: usize, block_peak: f32, decay: f32) {
        if let Some(a) = self.inst_level.get(track) {
            Self::hold(a, block_peak, decay);
        }
    }

    #[inline]
    pub fn set_fx_level(&self, idx: usize, block_peak: f32, decay: f32) {
        if let Some(a) = self.fx_level.get(idx) {
            Self::hold(a, block_peak, decay);
        }
    }

    #[inline]
    pub fn set_fx_bands(&self, idx: usize, bands_db: [f32; 3]) {
        for (b, v) in bands_db.iter().enumerate() {
            if let Some(a) = self.fx_bands.get(idx * 3 + b) {
                store_f32(a, *v);
            }
        }
    }

    #[inline]
    pub fn set_playhead(&self, ticks: f64, playing: bool) {
        self.playhead_ticks.store(ticks.to_bits(), Ordering::Relaxed);
        self.playing.store(playing, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot {
            playhead_ticks: f64::from_bits(self.playhead_ticks.load(Ordering::Relaxed)),
            playing: self.playing.load(Ordering::Relaxed),
            tracks: self
                .track_peak
                .iter()
                .zip(&self.track_rms)
                .map(|(p, r)| TrackMeter {
                    peak: load_f32(p),
                    rms: load_f32(r),
                })
                .collect(),
            master: TrackMeter {
                peak: load_f32(&self.master_peak),
                rms: load_f32(&self.master_rms),
            },
            fx_gr_db: self.fx_gr.iter().map(load_f32).collect(),
            inst_level: self.inst_level.iter().map(load_f32).collect(),
            fx_level: self.fx_level.iter().map(load_f32).collect(),
            fx_bands_db: self
                .fx_bands
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| [load_f32(&c[0]), load_f32(&c[1]), load_f32(&c[2])])
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TrackMeter {
    pub peak: f32,
    pub rms: f32,
}

/// Plain-data snapshot for the UI poll (`engine_snapshot()` in the IPC layer).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EngineSnapshot {
    pub playhead_ticks: f64,
    pub playing: bool,
    pub tracks: Vec<TrackMeter>,
    pub master: TrackMeter,
    pub fx_gr_db: Vec<f32>,
    /// Per-track instrument output level (linear), in track order.
    pub inst_level: Vec<f32>,
    /// Per-fx output level (linear), same flat index as `fx_gr_db`.
    pub fx_level: Vec<f32>,
    /// Per-fx band gain reduction in dB (≤ 0), same flat index as `fx_gr_db`.
    pub fx_bands_db: Vec<[f32; 3]>,
}
