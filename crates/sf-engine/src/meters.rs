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

pub struct MeterBank {
    track_peak: Vec<AtomicU32>,
    track_rms: Vec<AtomicU32>,
    master_peak: AtomicU32,
    master_rms: AtomicU32,
    /// Per-fx gain reduction in dB (≤ 0), flat-indexed across all fx chains.
    fx_gr: Vec<AtomicU32>,
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
}
