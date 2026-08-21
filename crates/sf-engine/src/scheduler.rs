//! Event scheduler: pre-expanded note lists (session clip patterns looping
//! from a launch anchor; bounded arrangement events) fired sample-accurately —
//! the render loop splits blocks at the next fire time. Swing mirrors Tone's
//! Transport swing math; humanize mirrors engine-tone's makePart jitter, both
//! applied at schedule time exactly as render.ts does offline.

use sf_project::project::{Note, TrackJSON};
use sf_project::PPQ;

use crate::transport::Mode;

/// One expanded note event. `tick` is clip-relative (session pattern) or
/// absolute (arrangement).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteEv {
    pub tick: f64,
    pub pitch: u8,
    pub dur_ticks: f64,
    pub vel: f32,
    pub pr: f32,
}

#[derive(Debug, Clone, Default)]
pub struct ClipPattern {
    pub loop_len: f64,
    pub events: Vec<NoteEv>, // sorted by tick
}

/// MIDI-fx expansion seam: clip notes → scheduled events for one loop pass,
/// through the track's `midifx` chain (see [`crate::midifx::expand_midi`],
/// the default). `root`/`scale` are the project key (meta.root/meta.scale).
/// The identity hook remains for tests that want raw notes.
pub type ExpandMidi = fn(&TrackJSON, &[Note], f64, f64, &str) -> Vec<NoteEv>;

pub fn identity_expand(
    _track: &TrackJSON,
    notes: &[Note],
    _loop_len: f64,
    _root: f64,
    _scale: &str,
) -> Vec<NoteEv> {
    let mut evs: Vec<NoteEv> = notes
        .iter()
        .map(|n| NoteEv {
            tick: n.s,
            pitch: n.p.clamp(0.0, 127.0) as u8,
            dur_ticks: n.d.max(0.0),
            vel: n.v as f32,
            pr: n.pr as f32,
        })
        .collect();
    evs.sort_by(|a, b| a.tick.total_cmp(&b.tick));
    evs
}

/// Deterministic xorshift64* RNG — humanize/probability rolls must be
/// reproducible per run for the fixture tests (browser parity not required).
#[derive(Debug, Clone)]
pub struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Tone Transport swing: events on odd `swingSubdivision` positions are pushed
/// late by `sin(progress·π) · swing · (subdiv·2/3)` ticks; downbeats (quarter
/// notes) and even subdivisions stay put. `swing_ticks` = subdivision in ticks
/// (PPQ 96 ⇒ "16n" = 24, "8n" = 48).
pub fn swing_offset_ticks(abs_tick: f64, swing: f64, swing_ticks: f64) -> f64 {
    if swing <= 0.0 || swing_ticks <= 0.0 {
        return 0.0;
    }
    let t = abs_tick.round();
    let period = swing_ticks * 2.0;
    if (t % PPQ as f64).abs() < 1e-9 || (t % period).abs() < 1e-9 {
        return 0.0;
    }
    let progress = (t % period) / period;
    (progress * std::f64::consts::PI).sin() * swing * (period / 3.0)
}

/// meta.swingSubdivision ("8n" | "16n", default "16n" like engine-tone).
pub fn swing_subdiv_ticks(subdiv: Option<&str>) -> f64 {
    match subdiv {
        Some("8n") => (PPQ / 2) as f64,
        _ => (PPQ / 4) as f64,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Pending {
    pub fire_tick: f64,
    pub pitch: u8,
    pub dur_ticks: f64,
    pub vel: f32,
    /// False for the synthetic placeholder used to bound peek() work when a
    /// pathological pattern (all-zero probability) never yields a firing note.
    pub audible: bool,
}

/// A session slot holding an AUDIO clip: index into the track's derived-clip
/// list (graph.rs) plus the loop metadata the renderer needs. Launch/stop/
/// anchor bookkeeping is shared with note clips.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioSlot {
    pub clip: usize,
    pub loop_len: f64,
    pub looped: bool,
}

/// Per-track scheduling state. Patterns/arr events are built with the graph;
/// the runtime cursors mutate on the audio thread with no allocation.
#[derive(Debug, Default)]
pub struct TrackSched {
    /// Session clip pattern per scene index (None = empty slot / audio clip).
    pub session: Vec<Option<ClipPattern>>,
    /// Session AUDIO clip per scene index (None = empty / note clip). Rendered
    /// by the graph's audio-clip voices; launch anchoring is shared.
    pub audio_session: Vec<Option<AudioSlot>>,
    /// Arrangement events at absolute ticks, sorted.
    pub arr: Vec<NoteEv>,

    launched: Option<usize>,
    anchor: f64,
    loop_iter: f64,
    next_idx: usize,
    arr_idx: usize,
    pending: Option<Pending>,
}

/// Scheduling knobs snapshotted from meta at graph build.
#[derive(Debug, Clone, Copy)]
pub struct SchedParams {
    pub swing: f64,
    pub swing_ticks: f64,
    pub humanize: f64,
}

impl TrackSched {
    /// Launch the clip of `scene` looping from `anchor` ticks (session mode).
    /// Accepts note AND audio slots — audio clips share the anchor/launched
    /// state but render through the graph's audio voices.
    pub fn launch(&mut self, scene: usize, anchor: f64) {
        let has_notes = self.session.get(scene).map(|s| s.is_some()) == Some(true);
        let has_audio = self.audio_session.get(scene).map(|s| s.is_some()) == Some(true);
        if has_notes || has_audio {
            self.launched = Some(scene);
            self.anchor = anchor;
            self.loop_iter = 0.0;
            self.next_idx = 0;
            self.pending = None;
        }
    }

    /// The launched scene's audio slot, if it holds an audio clip.
    pub fn audio_slot(&self) -> Option<AudioSlot> {
        self.launched
            .and_then(|s| self.audio_session.get(s).copied().flatten())
    }

    pub fn stop_clip(&mut self) {
        self.launched = None;
        self.pending = None;
    }

    pub fn launched_scene(&self) -> Option<usize> {
        self.launched
    }

    /// Launch anchor in ticks (clip-envelope position = now − anchor mod len).
    pub fn anchor(&self) -> f64 {
        self.anchor
    }

    /// Reset cursors after a position jump (play-from, loop wrap).
    pub fn relocate(&mut self, mode: Mode, now: f64) {
        self.pending = None;
        match mode {
            Mode::Session => {
                if let Some(p) = self.launched.and_then(|s| self.session[s].as_ref()) {
                    let len = p.loop_len.max(1.0);
                    let rel = now - self.anchor;
                    let li = (rel / len).floor().max(0.0);
                    let pos = rel - li * len;
                    self.loop_iter = li;
                    self.next_idx = p.events.partition_point(|e| e.tick < pos - 1e-9);
                }
            }
            Mode::Arr => {
                self.arr_idx = self.arr.partition_point(|e| e.tick < now - 1e-9);
            }
        }
    }

    /// Ensure `pending` holds the next event at/after `now` and return its
    /// fire tick. Probability (`pr`) and humanize are rolled here, once per
    /// firing — mirroring the Tone.Part callback.
    pub fn peek(
        &mut self,
        mode: Mode,
        now: f64,
        p: &SchedParams,
        rng: &mut XorShift,
    ) -> Option<f64> {
        if let Some(pd) = &self.pending {
            return Some(pd.fire_tick);
        }
        // bound the work per call: a pattern whose notes all fail their
        // probability roll must not spin the audio thread
        for _ in 0..4096 {
            let (ev, base_tick) = match mode {
                Mode::Session => {
                    let scene = self.launched?;
                    let pat = self.session[scene].as_ref()?;
                    if pat.events.is_empty() {
                        return None;
                    }
                    let len = pat.loop_len.max(1.0);
                    if self.next_idx >= pat.events.len() {
                        self.next_idx = 0;
                        self.loop_iter += 1.0;
                    }
                    let ev = pat.events[self.next_idx];
                    self.next_idx += 1;
                    (ev, self.anchor + self.loop_iter * len + ev.tick)
                }
                Mode::Arr => {
                    if self.arr_idx >= self.arr.len() {
                        return None;
                    }
                    let ev = self.arr[self.arr_idx];
                    self.arr_idx += 1;
                    (ev, ev.tick)
                }
            };
            if ev.pr < 1.0 && rng.next_f64() > ev.pr as f64 {
                continue;
            }
            let mut fire = base_tick + swing_offset_ticks(base_tick, p.swing, p.swing_ticks);
            let mut vel = ev.vel;
            if p.humanize > 0.0 {
                // ±10% of a beat of timing, ±10% of velocity, scaled by knob
                fire += (rng.next_f64() * 2.0 - 1.0) * 0.1 * PPQ as f64 * p.humanize;
                vel = (vel + ((rng.next_f64() * 2.0 - 1.0) * 0.1 * p.humanize) as f32)
                    .clamp(0.02, 1.0);
            }
            if fire < now {
                fire = now; // a negative jitter never schedules into the past
            }
            self.pending = Some(Pending {
                fire_tick: fire,
                pitch: ev.pitch,
                dur_ticks: ev.dur_ticks,
                vel,
                audible: true,
            });
            return Some(fire);
        }
        // pathological pattern: park a silent marker one loop ahead
        let hold = now + 4.0 * 384.0;
        self.pending = Some(Pending {
            fire_tick: hold,
            pitch: 0,
            dur_ticks: 0.0,
            vel: 0.0,
            audible: false,
        });
        Some(hold)
    }

    /// Take the pending event if it is due at `now` (within `eps` ticks).
    pub fn take_due(&mut self, now: f64, eps: f64) -> Option<Pending> {
        if self.pending.map(|p| p.fire_tick <= now + eps) == Some(true) {
            self.pending.take()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(len: f64, ticks: &[f64]) -> ClipPattern {
        ClipPattern {
            loop_len: len,
            events: ticks
                .iter()
                .map(|&t| NoteEv {
                    tick: t,
                    pitch: 60,
                    dur_ticks: 48.0,
                    vel: 0.8,
                    pr: 1.0,
                })
                .collect(),
        }
    }

    fn params() -> SchedParams {
        SchedParams {
            swing: 0.0,
            swing_ticks: 24.0,
            humanize: 0.0,
        }
    }

    #[test]
    fn session_pattern_loops_forever() {
        let mut s = TrackSched {
            session: vec![Some(pattern(384.0, &[0.0, 96.0, 192.0, 288.0]))],
            ..Default::default()
        };
        s.launch(0, 0.0);
        let mut rng = XorShift::new(1);
        let p = params();
        let mut fired = vec![];
        let mut now = 0.0;
        while now < 384.0 * 3.0 {
            let next = s.peek(Mode::Session, now, &p, &mut rng).unwrap();
            now = next;
            let ev = s.take_due(now, 1e-9).unwrap();
            fired.push(ev.fire_tick);
        }
        assert_eq!(fired.len(), 13); // 3 full loops + the downbeat of loop 4
        assert_eq!(fired[4], 384.0);
        assert_eq!(fired[12], 1152.0);
    }

    #[test]
    fn arr_events_are_bounded() {
        let mut s = TrackSched {
            arr: vec![
                NoteEv { tick: 100.0, pitch: 60, dur_ticks: 10.0, vel: 1.0, pr: 1.0 },
                NoteEv { tick: 500.0, pitch: 61, dur_ticks: 10.0, vel: 1.0, pr: 1.0 },
            ],
            ..Default::default()
        };
        s.relocate(Mode::Arr, 0.0);
        let mut rng = XorShift::new(1);
        let p = params();
        assert_eq!(s.peek(Mode::Arr, 0.0, &p, &mut rng), Some(100.0));
        s.take_due(100.0, 1e-9).unwrap();
        assert_eq!(s.peek(Mode::Arr, 100.0, &p, &mut rng), Some(500.0));
        s.take_due(500.0, 1e-9).unwrap();
        assert_eq!(s.peek(Mode::Arr, 500.0, &p, &mut rng), None);
    }

    #[test]
    fn swing_matches_tone_formula() {
        // 16n swing at PPQ 96: tick 24 (odd 16th) swings, 0/48/96 don't
        assert_eq!(swing_offset_ticks(0.0, 0.5, 24.0), 0.0);
        assert_eq!(swing_offset_ticks(48.0, 0.5, 24.0), 0.0);
        assert_eq!(swing_offset_ticks(96.0, 0.5, 24.0), 0.0);
        let off = swing_offset_ticks(24.0, 0.5, 24.0);
        // sin(0.5π)·0.5·(48/3) = 8 ticks
        assert!((off - 8.0).abs() < 1e-9, "{off}");
        // 8n subdivision: tick 48 swings
        assert!(swing_offset_ticks(48.0, 0.5, 48.0) > 0.0);
    }

    #[test]
    fn zero_probability_never_spins() {
        let mut s = TrackSched {
            session: vec![Some(ClipPattern {
                loop_len: 96.0,
                events: vec![NoteEv { tick: 0.0, pitch: 60, dur_ticks: 10.0, vel: 1.0, pr: 0.0 }],
            })],
            ..Default::default()
        };
        s.launch(0, 0.0);
        let mut rng = XorShift::new(7);
        let p = params();
        let t = s.peek(Mode::Session, 0.0, &p, &mut rng).unwrap();
        assert!(t > 0.0);
        assert!(!s.take_due(t, 1e-9).map(|e| e.audible).unwrap_or(true));
    }

    #[test]
    fn humanize_is_deterministic_per_seed() {
        let run = |seed: u64| {
            let mut s = TrackSched {
                session: vec![Some(pattern(384.0, &[0.0, 96.0]))],
                ..Default::default()
            };
            s.launch(0, 0.0);
            let mut rng = XorShift::new(seed);
            let p = SchedParams { swing: 0.0, swing_ticks: 24.0, humanize: 1.0 };
            let mut out = vec![];
            let mut now = 0.0;
            for _ in 0..6 {
                let t = s.peek(Mode::Session, now, &p, &mut rng).unwrap();
                now = t;
                out.push((t, s.take_due(t, 1e-9).unwrap().vel));
            }
            out
        };
        assert_eq!(run(42), run(42));
        assert_ne!(run(42), run(43));
    }
}
