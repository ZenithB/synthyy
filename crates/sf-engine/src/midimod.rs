//! dest "midi" modulation (LFO/macro/automation → a MIDI-fx param): port of
//! engine-tone's `midiFxMod`/`midiDirty`/`refreshPart` trio.
//!
//! The browser regenerates the playing Part's notes whenever a modulated
//! MIDI-fx value crosses a quantized step. Here patterns are pre-expanded
//! `Vec<NoteEv>`s, so regeneration allocates — and must never run on the
//! audio thread. The work is split in two:
//!
//! - **audio side** ([`MidiModShared`], written from `apply_modulation`):
//!   per-override live value + active flag, per-track dirty flag, the
//!   launched scene per track and a sample clock. All atomics, no alloc.
//! - **control side** ([`MidiModHost::service`], called from
//!   `Engine::service`/`EngineControl::service`): for every dirty track,
//!   fold the active overrides into a clone of the track's MIDI-fx params
//!   (the `applyMidiFx` override fold), re-expand the launched clip through
//!   the `ExpandMidi` seam, wrap the new pattern in `basedrop::Owned` and
//!   push a [`PatternSwap`] down an `rtrb` ring. The audio thread swaps the
//!   Vec in place (a pointer move) and drops the old one into the collector.
//!
//! Throttle: ≤ 20 Hz per track (`MIDI_REBUILD_SEC`), measured on the audio
//! sample clock so offline renders throttle identically to live playback.
//! A dirty flag persists until a rebuild actually fires, like the browser.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use basedrop::{Handle, Owned};
use rtrb::Producer;
use sf_project::project::{clip_key, Note, ProjectJSON, TrackJSON};
use sf_project::registry::midifx_params;

use crate::scheduler::{ClipPattern, ExpandMidi};

/// Minimum spacing between two rebuilds of one track (browser: 0.05 s).
pub const MIDI_REBUILD_SEC: f64 = 0.05;

/// Pattern ring depth (one swap per dirty track per service call; the host
/// keeps the track dirty and retries when the ring is full).
pub const PATTERN_CAPACITY: usize = 64;

pub(crate) const NO_SCENE: u32 = u32::MAX;

/// A note clip's source notes + loop length (re-expansion input).
type ClipNotes = Option<(Vec<Note>, f64)>;

/// One resolved "midi|fxId|pkey" target (built off-thread, read by both
/// sides; immutable after build).
#[derive(Debug, Clone)]
pub(crate) struct MidiOvDesc {
    pub track: usize,
    /// Index into `TrackJSON.midifx` (the FULL chain, disabled devices
    /// included — the browser looks the device up by id regardless of `on`).
    pub fx: usize,
    pub key: &'static str,
}

/// Audio-thread-written modulation state shared with the control side.
#[derive(Debug)]
pub struct MidiModShared {
    /// Live override value per override (f32 bits).
    vals: Vec<AtomicU32>,
    active: Vec<AtomicBool>,
    /// Per track: a rebuild is pending.
    dirty: Vec<AtomicBool>,
    /// Per track: launched session scene (NO_SCENE when none).
    launched: Vec<AtomicU32>,
    /// Audio sample clock (rebuild throttle reference).
    samples: AtomicU64,
}

impl MidiModShared {
    pub(crate) fn new(n_ov: usize, n_tracks: usize) -> Self {
        Self {
            vals: (0..n_ov).map(|_| AtomicU32::new(0)).collect(),
            active: (0..n_ov).map(|_| AtomicBool::new(false)).collect(),
            dirty: (0..n_tracks).map(|_| AtomicBool::new(false)).collect(),
            launched: (0..n_tracks).map(|_| AtomicU32::new(NO_SCENE)).collect(),
            samples: AtomicU64::new(0),
        }
    }

    pub fn override_count(&self) -> usize {
        self.vals.len()
    }

    #[inline]
    pub(crate) fn set_value(&self, ov: usize, v: f32) {
        self.vals[ov].store(v.to_bits(), Ordering::Relaxed);
        self.active[ov].store(true, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn clear_value(&self, ov: usize) {
        self.active[ov].store(false, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn mark_dirty(&self, track: usize) {
        self.dirty[track].store(true, Ordering::Release);
    }

    #[inline]
    pub(crate) fn set_launched(&self, track: usize, scene: Option<usize>) {
        let v = scene.map(|s| s as u32).unwrap_or(NO_SCENE);
        self.launched[track].store(v, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn advance(&self, frames: usize) {
        self.samples.fetch_add(frames as u64, Ordering::Relaxed);
    }

    pub fn is_dirty(&self, track: usize) -> bool {
        self.dirty.get(track).map(|d| d.load(Ordering::Acquire)) == Some(true)
    }

    /// Live override value (None when the override is inactive).
    pub fn value(&self, ov: usize) -> Option<f32> {
        if self.active[ov].load(Ordering::Relaxed) {
            Some(f32::from_bits(self.vals[ov].load(Ordering::Relaxed)))
        } else {
            None
        }
    }
}

/// A re-expanded session pattern travelling control → audio. The audio side
/// swaps the Vec into the scheduler slot and drops this (old events inside),
/// which `basedrop` defers to the off-thread collector.
pub struct PatternSwap {
    pub graph_gen: u64,
    pub track: u16,
    pub scene: u16,
    pub pattern: Owned<ClipPattern>,
}

/// Control-side half: everything `service()` needs to re-expand a track's
/// launched clip with the live overrides folded in.
pub struct MidiModHost {
    shared: Arc<MidiModShared>,
    ovs: Vec<MidiOvDesc>,
    /// Per track: a clone of the TrackJSON (only tracks that own an override).
    tracks: Vec<Option<TrackJSON>>,
    /// Per track, per scene: the clip's notes + loop length (note clips only).
    clips: Vec<Vec<ClipNotes>>,
    root: f64,
    scale: String,
    expand: ExpandMidi,
    graph_gen: u64,
    throttle_samples: u64,
    /// Per track: sample clock of the last rebuild (None = never).
    last_rebuild: Vec<Option<u64>>,
    /// Per track, per scene: the slot currently holds an override-folded
    /// pattern and must be restored once the overrides go away.
    modded: Vec<Vec<bool>>,
    rebuilds: u64,
}

impl MidiModHost {
    /// Build alongside the graph (off-thread). `ovs` are the routes the
    /// modulation builder resolved; the host snapshots only what those
    /// tracks need.
    pub(crate) fn build(
        project: &ProjectJSON,
        track_ids: &[String],
        scene_ids: &[String],
        ovs: Vec<MidiOvDesc>,
        shared: Arc<MidiModShared>,
        expand: ExpandMidi,
        (graph_gen, sample_rate): (u64, f32),
    ) -> Self {
        let n = project.tracks.len();
        let mut tracks: Vec<Option<TrackJSON>> = vec![None; n];
        let mut clips: Vec<Vec<ClipNotes>> = vec![Vec::new(); n];
        for ov in &ovs {
            let ti = ov.track;
            if tracks[ti].is_some() {
                continue;
            }
            tracks[ti] = Some(project.tracks[ti].clone());
            clips[ti] = scene_ids
                .iter()
                .map(|sid| {
                    let clip = project.clips.get(&clip_key(&track_ids[ti], sid))?;
                    if clip.audio.is_some() {
                        return None;
                    }
                    let notes: Vec<Note> = clip.notes.values().cloned().collect();
                    Some((notes, clip.len.max(1.0)))
                })
                .collect();
        }
        let modded = clips.iter().map(|c| vec![false; c.len()]).collect();
        Self {
            shared,
            ovs,
            tracks,
            clips,
            root: project.meta.root,
            scale: project.meta.scale.clone(),
            expand,
            graph_gen,
            throttle_samples: (MIDI_REBUILD_SEC * sample_rate.max(1.0) as f64) as u64,
            last_rebuild: vec![None; n],
            modded,
            rebuilds: 0,
        }
    }

    pub fn shared(&self) -> &Arc<MidiModShared> {
        &self.shared
    }

    /// Number of re-expansions performed so far (tests / diagnostics).
    pub fn rebuilds(&self) -> u64 {
        self.rebuilds
    }

    /// Re-expand every dirty track whose throttle window has elapsed and
    /// queue the new patterns: the launched scene with the live overrides
    /// folded in, plus any other scene still holding a folded pattern
    /// restored to its stored params. Returns the number of rebuilds.
    pub(crate) fn service(&mut self, handle: &Handle, tx: &mut Producer<PatternSwap>) -> usize {
        let mut n = 0usize;
        let now = self.shared.samples.load(Ordering::Relaxed);
        for ti in 0..self.tracks.len() {
            if !self.shared.dirty[ti].load(Ordering::Acquire) {
                continue;
            }
            if let Some(last) = self.last_rebuild[ti] {
                if now.saturating_sub(last) < self.throttle_samples {
                    continue; // stays dirty; retried next service()
                }
            }
            let Some(track) = self.tracks[ti].as_ref() else {
                self.shared.dirty[ti].store(false, Ordering::Release);
                continue;
            };
            // clear BEFORE reading the values: a change landing during the
            // expansion re-dirties the track instead of getting lost
            self.shared.dirty[ti].store(false, Ordering::Release);
            let launched = self.shared.launched[ti].load(Ordering::Relaxed) as usize;

            // applyMidiFx's override fold: live values replace stored params
            let mut folded = track.clone();
            let mut any = false;
            if let Some(chain) = folded.midifx.as_mut() {
                for (oi, ov) in self.ovs.iter().enumerate() {
                    if ov.track != ti {
                        continue;
                    }
                    if let Some(v) = self.shared.value(oi) {
                        if let Some(d) = chain.get_mut(ov.fx) {
                            d.params.insert(ov.key.to_string(), v as f64);
                            any = true;
                        }
                    }
                }
            }

            let mut rebuilt = false;
            for scene in 0..self.clips[ti].len() {
                let is_launched = scene == launched;
                if !is_launched && !self.modded[ti][scene] {
                    continue;
                }
                let Some((notes, len)) = self.clips[ti][scene].as_ref() else { continue };
                if tx.is_full() {
                    self.shared.dirty[ti].store(true, Ordering::Release);
                    return n;
                }
                let src = if is_launched { &folded } else { track };
                let mut events = (self.expand)(src, notes, *len, self.root, &self.scale);
                events.retain(|e| e.tick < *len);
                let pattern = Owned::new(handle, ClipPattern { loop_len: *len, events });
                let swap = PatternSwap {
                    graph_gen: self.graph_gen,
                    track: ti as u16,
                    scene: scene as u16,
                    pattern,
                };
                if tx.push(swap).is_err() {
                    self.shared.dirty[ti].store(true, Ordering::Release);
                    return n;
                }
                self.modded[ti][scene] = is_launched && any;
                rebuilt = true;
                n += 1;
            }
            if rebuilt {
                self.last_rebuild[ti] = Some(now);
                self.rebuilds += 1;
            }
        }
        n
    }
}

/// Browser `quantOf`: int/stepped params rebuild on integer crossings,
/// continuous params on ~40 steps of their range.
#[inline]
pub(crate) fn quantize(v: f32, min: f32, max: f32, stepped: bool) -> i32 {
    if stepped {
        v.round() as i32
    } else {
        let range = if max - min != 0.0 { max - min } else { 1.0 };
        ((v - min) / range * 40.0).round() as i32
    }
}

/// Resolve "midi|fxId|pkey" on a track: (chain index, spec) — None mirrors
/// the browser's null (unknown device id / key, non-numeric base).
pub(crate) fn resolve_midi_target(
    t: &TrackJSON,
    fxid: &str,
    pkey: &str,
) -> Option<(usize, &'static sf_project::registry::ParamSpec, f32)> {
    let chain = t.midifx.as_deref()?;
    let fi = chain.iter().position(|d| d.id.as_deref() == Some(fxid))?;
    let d = &chain[fi];
    let spec = midifx_params(&d.r#type).iter().find(|s| s.key == pkey)?;
    let base = *d.params.get(pkey)? as f32;
    Some((fi, spec, base))
}
