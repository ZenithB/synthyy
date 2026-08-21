//! Engine facade: command ring in, graph handover, transport + scheduler
//! drive, block rendering. `process()` is the audio-callback entry — after the
//! graph is built it allocates nothing: commands are fixed-size `Cmd`s over an
//! `rtrb` ring, graphs arrive pre-built through a second ring wrapped in
//! `basedrop::Owned` (dropping the old graph defers deallocation to the
//! off-thread Collector), and every buffer was sized at build time.
//!
//! The facade is single-object for the offline driver (sf-render pulls it
//! directly); the IPC layer splits usage across threads by keeping
//! `push_cmd`/`load_project`/`snapshot` on the control side and `process` on
//! the callback — the internals are already ring-based for that split.
//!
//! HOST OBLIGATION — `service()`: modulation routed at a MIDI-fx param
//! (dest "midi") needs the playing clip re-expanded when the modulated value
//! crosses a step. That re-expansion allocates, so the audio thread only
//! flags the track ([`crate::midimod::MidiModShared`]); the host must call
//! [`Engine::service`] / [`EngineControl::service`] periodically (~50 Hz is
//! plenty — rebuilds are throttled to 20 Hz per track) to perform the
//! pending re-expansions off-thread and queue the new patterns. The offline
//! renderer calls it between pull chunks; the IPC layer should drive it from
//! its control thread (a timer or the snapshot poll). Projects without a
//! dest-"midi" route make `service()` a no-op.

use std::sync::Arc;

use basedrop::{Collector, Owned};
use rtrb::{Consumer, Producer, RingBuffer};
use sf_dsp::MAX_BLOCK;
use sf_project::project::ProjectJSON;
use sf_project::ticks_to_seconds;

use crate::command::{Cmd, CommandResolver};
use crate::graph::{build_graph_full, EngineGraph, SampleBank};
use crate::meters::{EngineSnapshot, MeterBank};
use crate::metro::Metronome;
use crate::midimod::{MidiModHost, PatternSwap, PATTERN_CAPACITY};
use crate::midifx::expand_midi;
use crate::scheduler::{ExpandMidi, XorShift};
use crate::transport::{Mode, Transport};

const CMD_CAPACITY: usize = 1024;
const GRAPH_CAPACITY: usize = 4;
const RNG_SEED: u64 = 0x5F3F_9A1B_2C4D_6E7F;

/// One fired note (kept on the stack while distributing duck triggers).
#[derive(Clone, Copy)]
struct Fired {
    track: usize,
    pitch: u8,
}

pub struct Engine {
    sample_rate: f32,
    cmd_tx: Producer<Cmd>,
    cmd_rx: Consumer<Cmd>,
    graph_tx: Producer<Owned<EngineGraph>>,
    graph_rx: Consumer<Owned<EngineGraph>>,
    graph: Option<Owned<EngineGraph>>,
    collector: Collector,
    transport: Transport,
    metro: Metronome,
    metro_on: bool,
    next_metro_tick: f64,
    rng: XorShift,
    resolver: CommandResolver,
    meters: Arc<MeterBank>,
    expand: ExpandMidi,
    /// Offline render: force the arr loop region off regardless of meta.
    pub loop_override_off: bool,
    pattern_tx: Producer<PatternSwap>,
    pattern_rx: Consumer<PatternSwap>,
    /// Control-side MIDI-modulation host for the latest built graph.
    midi_host: Option<MidiModHost>,
    graph_gen: u64,
}

impl Engine {
    /// Constructible without audio hardware — sf-render drives it directly.
    pub fn new(sample_rate: f32) -> Self {
        let (cmd_tx, cmd_rx) = RingBuffer::new(CMD_CAPACITY);
        let (graph_tx, graph_rx) = RingBuffer::new(GRAPH_CAPACITY);
        let (pattern_tx, pattern_rx) = RingBuffer::new(PATTERN_CAPACITY);
        let mut metro = Metronome::new();
        metro.prepare(sample_rate);
        Self {
            sample_rate,
            cmd_tx,
            cmd_rx,
            graph_tx,
            graph_rx,
            graph: None,
            collector: Collector::new(),
            transport: Transport::new(sample_rate),
            metro,
            metro_on: false,
            next_metro_tick: 0.0,
            rng: XorShift::new(RNG_SEED),
            resolver: CommandResolver::new(),
            meters: Arc::new(MeterBank::new(0, 0)),
            expand: expand_midi,
            loop_override_off: false,
            pattern_tx,
            pattern_rx,
            midi_host: None,
            graph_gen: 0,
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Swap the MIDI-fx expansion hook (takes effect on the next load).
    pub fn set_expand_midi(&mut self, expand: ExpandMidi) {
        self.expand = expand;
    }

    /// Build a graph from the project and queue it for the audio thread
    /// (off-thread work: allocation + string resolution happen here).
    pub fn load_project(&mut self, project: &ProjectJSON) {
        let bank = SampleBank::new();
        self.load_project_with_bank(project, &bank);
    }

    /// Like [`Self::load_project`] with a sample bank: instrument buffers and
    /// audio-clip voices are injected from `bank` at build time.
    pub fn load_project_with_bank(&mut self, project: &ProjectJSON, bank: &SampleBank) {
        self.graph_gen += 1;
        let (graph, resolver, host) =
            build_graph_full(project, self.sample_rate, self.expand, bank, self.graph_gen);
        self.transport.set_bpm(graph.bpm);
        self.meters = Arc::clone(&graph.meters);
        self.resolver = resolver;
        self.midi_host = host;
        let owned = Owned::new(&self.collector.handle(), graph);
        // ring full = a stale pending graph; adopt-drop here (off-thread)
        while self.graph_tx.is_full() {
            let _ = self.graph_rx.pop();
        }
        let _ = self.graph_tx.push(owned);
        self.collector.collect();
    }

    /// Queue a command for the audio thread (control side).
    pub fn push_cmd(&mut self, cmd: Cmd) -> bool {
        self.cmd_tx.push(cmd).is_ok()
    }

    /// String→numeric address resolution for the IPC layer.
    pub fn resolver(&self) -> &CommandResolver {
        &self.resolver
    }

    /// The latest graph's meter bank (atomics; safe to poll off-thread).
    pub fn meters(&self) -> Arc<MeterBank> {
        Arc::clone(&self.meters)
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        self.meters.snapshot()
    }

    pub fn playing(&self) -> bool {
        self.transport.playing()
    }

    pub fn playhead_ticks(&self) -> f64 {
        self.transport.ticks()
    }

    /// Perform pending MIDI-modulation re-expansions (off the audio thread;
    /// see the module docs). Returns the number of patterns rebuilt.
    pub fn service(&mut self) -> usize {
        let n = match self.midi_host.as_mut() {
            Some(h) => h.service(&self.collector.handle(), &mut self.pattern_tx),
            None => 0,
        };
        self.collector.collect();
        n
    }

    /// Re-expansions performed since the last `load_project` (diagnostics).
    pub fn midi_rebuilds(&self) -> u64 {
        self.midi_host.as_ref().map_or(0, |h| h.rebuilds())
    }

    /// Split off the control half for cross-thread hosting (the IPC layer):
    /// the returned [`EngineControl`] owns the command/graph producers, the
    /// resolver, the meter handle and the basedrop collector; this Engine
    /// keeps `process()` and is moved into the audio callback. After the
    /// split, the Engine-side control methods (`push_cmd`/`load_project`)
    /// push into abandoned rings and must not be used. Additive API — the
    /// single-object use (sf-render, tests) is unchanged.
    pub fn split_control(&mut self) -> EngineControl {
        let (dead_cmd_tx, _) = RingBuffer::new(1);
        let (dead_graph_tx, _) = RingBuffer::new(1);
        let (dead_pattern_tx, _) = RingBuffer::new(1);
        EngineControl {
            sample_rate: self.sample_rate,
            cmd_tx: std::mem::replace(&mut self.cmd_tx, dead_cmd_tx),
            graph_tx: std::mem::replace(&mut self.graph_tx, dead_graph_tx),
            pattern_tx: std::mem::replace(&mut self.pattern_tx, dead_pattern_tx),
            collector: std::mem::replace(&mut self.collector, Collector::new()),
            resolver: std::mem::take(&mut self.resolver),
            meters: Arc::clone(&self.meters),
            expand: self.expand,
            midi_host: self.midi_host.take(),
            graph_gen: self.graph_gen,
        }
    }

    /// Adopt re-expanded session patterns from the control side: swap the
    /// Vec into the scheduler slot (pointer move) and drop the swap — the old
    /// events ride the `Owned` into the off-thread collector. Swaps tagged
    /// with an older graph generation are discarded the same way.
    fn adopt_patterns(&mut self) {
        while let Ok(mut sw) = self.pattern_rx.pop() {
            let Some(graph) = self.graph.as_mut() else { continue };
            if sw.graph_gen != graph.gen {
                continue;
            }
            let Some(tr) = graph.tracks.get_mut(sw.track as usize) else { continue };
            let Some(Some(slot)) = tr.sched.session.get_mut(sw.scene as usize) else {
                continue;
            };
            std::mem::swap(slot, &mut *sw.pattern);
            // the cursor follows the position in the new event list (the
            // browser rebuilds the Part at the same transport offset)
            if self.transport.mode() == Mode::Session
                && tr.sched.launched_scene() == Some(sw.scene as usize)
            {
                tr.sched.relocate(Mode::Session, self.transport.ticks());
            }
        }
    }

    fn apply_cmd(&mut self, cmd: Cmd) {
        let Some(graph) = self.graph.as_mut() else {
            // transport-only commands still apply without a graph
            match cmd {
                Cmd::TransportStop => self.transport.stop(),
                Cmd::SetTempo { bpm } => self.transport.set_bpm(bpm),
                Cmd::MetronomeOn => self.metro_on = true,
                Cmd::MetronomeOff => self.metro_on = false,
                _ => {}
            }
            return;
        };
        match cmd {
            Cmd::SetParam { target, slot, param, value } => {
                graph.set_param(target, slot, param, value)
            }
            Cmd::NoteOn { track, pitch, vel } => {
                if let Some(tr) = graph.tracks.get_mut(track as usize) {
                    tr.inst.note_on(pitch, vel);
                }
                graph.fire_ducks(track as usize, pitch);
            }
            Cmd::NoteOff { track, pitch } => {
                if let Some(tr) = graph.tracks.get_mut(track as usize) {
                    tr.inst.note_off(pitch);
                }
            }
            Cmd::ClipLaunch { track, clip } => {
                // While stopped, launches anchor at 0 and wait for
                // TransportPlay — so a scene's batch of launches lands
                // together (live launchClip starts the transport itself; here
                // that is the IPC layer's job, keeping batches atomic).
                let anchor = if self.transport.playing() {
                    graph.next_boundary_ticks(self.transport.ticks())
                } else {
                    0.0
                };
                if let Some(tr) = graph.tracks.get_mut(track as usize) {
                    tr.sched.launch(clip as usize, anchor);
                    // cursors follow the current position; with a future
                    // anchor the first event lands at anchor + its tick
                    tr.sched.relocate(Mode::Session, self.transport.ticks());
                    graph.midi_mod_relaunch(track as usize);
                }
            }
            Cmd::ClipStop { track } => {
                if let Some(tr) = graph.tracks.get_mut(track as usize) {
                    tr.sched.stop_clip();
                }
            }
            Cmd::TransportPlay { mode, from_ticks } => {
                self.rng = XorShift::new(RNG_SEED); // deterministic per run
                self.transport.loop_enabled =
                    !self.loop_override_off && mode == Mode::Arr && graph.loop_on;
                self.transport.loop_start = graph.loop_start;
                self.transport.loop_end =
                    (graph.loop_start + 384.0).max(graph.loop_end);
                self.transport.play(mode, from_ticks);
                let now = self.transport.ticks();
                for tr in graph.tracks.iter_mut() {
                    tr.sched.relocate(mode, now);
                }
                self.next_metro_tick = Self::next_multiple(
                    now,
                    graph.time_sig.beat_ticks() as f64,
                );
            }
            Cmd::TransportStop => {
                self.transport.stop();
                for tr in graph.tracks.iter_mut() {
                    tr.inst.reset();
                }
            }
            Cmd::SetTempo { bpm } => {
                self.transport.set_bpm(bpm);
                graph.bpm = self.transport.bpm();
            }
            Cmd::MetronomeOn => self.metro_on = true,
            Cmd::MetronomeOff => self.metro_on = false,
        }
    }

    /// Smallest multiple of `step` at or after `t` (with float slack).
    fn next_multiple(t: f64, step: f64) -> f64 {
        (t / step - 1e-9).ceil() * step
    }

    /// The audio-callback entry: drain the ring, run the scheduler, render.
    /// Splits into ≤ MAX_BLOCK chunks and additionally at event boundaries so
    /// note timing is sample-accurate.
    pub fn process(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
        debug_assert_eq!(out_l.len(), out_r.len());
        // adopt a freshly built graph; the old one is collected off-thread.
        // The meter bank follows the graph (in the split hosting mode nobody
        // calls load_project on this side). Arc::clone/drop here is a pair of
        // atomic refcount ops — the old bank can't deallocate on this thread
        // because the outgoing graph still holds a reference and its drop is
        // deferred to the off-thread collector.
        while let Ok(g) = self.graph_rx.pop() {
            self.meters = Arc::clone(&g.meters);
            self.graph = Some(g);
        }
        self.adopt_patterns();
        while let Ok(cmd) = self.cmd_rx.pop() {
            self.apply_cmd(cmd);
        }
        out_l.fill(0.0);
        out_r.fill(0.0);
        let Some(graph) = self.graph.as_mut() else {
            return;
        };

        let mut offset = 0usize;
        let total = out_l.len();
        while offset < total {
            let mut n = (total - offset).min(MAX_BLOCK);
            let playing = self.transport.playing();
            let now = self.transport.ticks();
            if playing {
                let tps = self.transport.ticks_per_sample();
                let eps = tps * 0.5;
                let mode = self.transport.mode();
                let sp = graph.sched_params;
                let bpm = graph.bpm;

                // fire everything due now, collecting duck kicks
                let mut fired: [Fired; 64] = [Fired { track: 0, pitch: 0 }; 64];
                let mut n_fired = 0usize;
                let mut nearest = f64::INFINITY;
                for ti in 0..graph.tracks.len() {
                    loop {
                        let tr = &mut graph.tracks[ti];
                        let Some(t) = tr.sched.peek(mode, now, &sp, &mut self.rng) else {
                            break;
                        };
                        if let Some(ev) = tr.sched.take_due(now, eps) {
                            if ev.audible {
                                let dur = ticks_to_seconds(ev.dur_ticks, bpm)
                                    .max(0.02) as f32;
                                tr.inst.trigger(ev.pitch, dur, ev.vel);
                                if n_fired < fired.len() {
                                    fired[n_fired] = Fired { track: ti, pitch: ev.pitch };
                                    n_fired += 1;
                                }
                            }
                        } else {
                            nearest = nearest.min(t);
                            break;
                        }
                    }
                }
                for f in fired.iter().take(n_fired) {
                    graph.fire_ducks(f.track, f.pitch);
                }

                // metronome beat boundary
                if self.metro_on {
                    if self.next_metro_tick <= now + eps {
                        let bt = graph.time_sig.beat_ticks() as f64;
                        let bar = graph.time_sig.bar_ticks() as f64;
                        let beat = (self.next_metro_tick / bt).round();
                        let accent = ((beat * bt) % bar).abs() < 1e-6;
                        self.metro.trigger(accent);
                        self.next_metro_tick += bt;
                    }
                    nearest = nearest.min(self.next_metro_tick);
                }

                // loop wrap is a boundary too
                if self.transport.loop_enabled && mode == Mode::Arr {
                    nearest = nearest.min(self.transport.loop_end);
                }

                if nearest.is_finite() {
                    let s = self.transport.samples_until(nearest);
                    if s > 0.0 {
                        n = n.min((s.ceil() as usize).max(1));
                    }
                }
            }

            // block-rate modulation: envelopes/automation/LFOs → set_param,
            // evaluated once per chunk before it renders (engine-tone's rAF
            // loop, at audio-block resolution)
            graph.apply_modulation(now, playing, self.transport.mode(), n);
            graph.render_chunk(
                &mut out_l[offset..offset + n],
                &mut out_r[offset..offset + n],
                now,
                playing,
                self.transport.mode(),
                if self.metro_on { Some(&mut self.metro) } else { None },
            );
            let wrapped = self.transport.advance(n);
            if wrapped {
                let now = self.transport.ticks();
                for tr in graph.tracks.iter_mut() {
                    tr.sched.relocate(self.transport.mode(), now);
                }
                self.next_metro_tick =
                    Self::next_multiple(now, graph.time_sig.beat_ticks() as f64);
            }
            offset += n;
        }
        self.meters
            .set_playhead(self.transport.ticks(), self.transport.playing());
    }
}

/// Control half of a split [`Engine`] (see [`Engine::split_control`]).
/// `Send` (not `Sync`): the host serializes access (e.g. behind a `Mutex`)
/// while the Engine itself lives inside the audio callback. All methods run
/// off the audio thread; communication is the same lock-free rings the
/// single-object Engine uses internally.
pub struct EngineControl {
    sample_rate: f32,
    cmd_tx: Producer<Cmd>,
    graph_tx: Producer<Owned<EngineGraph>>,
    pattern_tx: Producer<PatternSwap>,
    collector: Collector,
    resolver: CommandResolver,
    meters: Arc<MeterBank>,
    expand: ExpandMidi,
    midi_host: Option<MidiModHost>,
    graph_gen: u64,
}

impl EngineControl {
    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    /// Build a graph off-thread and queue it for the audio callback. Also
    /// pushes `Cmd::SetTempo` (in the split the transport lives audio-side,
    /// so tempo must travel as a command) and collects graphs the callback
    /// dropped earlier. If the graph ring is full (audio side gone/stalled)
    /// the new graph is dropped; the host re-pushes on the next stream start.
    pub fn load_project(&mut self, project: &ProjectJSON) {
        let bank = SampleBank::new();
        self.load_project_with_bank(project, &bank);
    }

    /// Like [`Self::load_project`] with a sample bank (the IPC layer owns the
    /// bank so it survives stream restarts; every rebuild re-injects).
    pub fn load_project_with_bank(&mut self, project: &ProjectJSON, bank: &SampleBank) {
        self.graph_gen += 1;
        let (graph, resolver, host) =
            build_graph_full(project, self.sample_rate, self.expand, bank, self.graph_gen);
        let bpm = graph.bpm;
        self.meters = Arc::clone(&graph.meters);
        self.resolver = resolver;
        self.midi_host = host;
        let owned = Owned::new(&self.collector.handle(), graph);
        let _ = self.graph_tx.push(owned);
        let _ = self.cmd_tx.push(Cmd::SetTempo { bpm });
        self.collector.collect();
    }

    /// Queue a command for the audio callback; `false` when the ring is full.
    pub fn push_cmd(&mut self, cmd: Cmd) -> bool {
        self.cmd_tx.push(cmd).is_ok()
    }

    /// String→numeric address resolution for the latest loaded graph.
    pub fn resolver(&self) -> &CommandResolver {
        &self.resolver
    }

    /// The latest graph's meter bank (atomics; safe to poll from any thread).
    pub fn meters(&self) -> Arc<MeterBank> {
        Arc::clone(&self.meters)
    }

    pub fn snapshot(&self) -> EngineSnapshot {
        self.meters.snapshot()
    }

    /// Perform pending MIDI-modulation re-expansions and collect graphs /
    /// patterns the callback dropped. The host must call this periodically
    /// (~50 Hz) while a project with dest-"midi" routes is loaded — see the
    /// module docs. Returns the number of patterns rebuilt.
    pub fn service(&mut self) -> usize {
        let n = match self.midi_host.as_mut() {
            Some(h) => h.service(&self.collector.handle(), &mut self.pattern_tx),
            None => 0,
        };
        self.collector.collect();
        n
    }

    /// Whether the loaded project routes modulation at a MIDI-fx param (i.e.
    /// whether `service()` has anything to do).
    pub fn needs_service(&self) -> bool {
        self.midi_host.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sf_project::project::*;
    use std::collections::BTreeMap;

    fn note(p: f64, s: f64, d: f64, v: f64) -> Note {
        Note { p, s, d, v, pr: 1.0 }
    }

    fn minimal_project() -> ProjectJSON {
        let mut notes = BTreeMap::new();
        notes.insert("n1".into(), note(48.0, 0.0, 84.0, 0.9));
        notes.insert("n2".into(), note(55.0, 96.0, 84.0, 0.8));
        let mut clips = BTreeMap::new();
        clips.insert(
            "t1|s1".to_string(),
            ClipJSON {
                name: "c".into(),
                color: 0.0,
                len: 384.0,
                notes,
                audio: None,
                env: None,
                follow: None,
            },
        );
        let mut params = BTreeMap::new();
        params.insert("cutoff".to_string(), 2000.0);
        ProjectJSON {
            meta: MetaJSON {
                title: "t".into(),
                bpm: 120.0,
                swing: 0.0,
                swing_subdivision: Some("16n".into()),
                humanize: Some(0.0),
                root: 9.0,
                scale: "minor".into(),
                launch_q: 1.0,
                master_gain: Some(0.0),
                loop_on: None,
                loop_start: None,
                loop_end: None,
                master_auto_open: None,
                master_auto_param: None,
                ts_top: Some(4.0),
                ts_bottom: Some(4.0),
            },
            tracks: vec![TrackJSON {
                id: Some("t1".into()),
                name: "Mono".into(),
                color: 0.0,
                kind: TrackKind::Synth,
                inst: InstJSON {
                    r#type: "mono".into(),
                    params,
                    sample_id: None,
                    sample_name: None,
                    out: None,
                    pad_samples: None,
                    pad_names: None,
                },
                fx: vec![],
                gain: 0.0,
                pan: 0.0,
                mute: false,
                solo: false,
                send_a: None,
                send_b: None,
                output: None,
                sends: None,
                locked: None,
                send: None,
                lfos: None,
                macros: None,
                midifx: None,
                auto: None,
                auto_open: None,
                auto_param: None,
            }],
            scenes: vec![SceneJSON { id: Some("s1".into()), name: "1".into() }],
            clips,
            arr: BTreeMap::new(),
            returns: None,
            master_fx: None,
            master_auto: None,
        }
    }

    fn render_secs(engine: &mut Engine, secs: f32) -> (Vec<f32>, Vec<f32>) {
        let n = (engine.sample_rate() * secs) as usize;
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        for (cl, cr) in l.chunks_mut(512).zip(r.chunks_mut(512)) {
            engine.process(cl, cr);
        }
        (l, r)
    }

    fn rms(b: &[f32]) -> f32 {
        (b.iter().map(|s| s * s).sum::<f32>() / b.len().max(1) as f32).sqrt()
    }

    #[test]
    fn session_playback_is_audible_and_finite() {
        let mut e = Engine::new(44_100.0);
        e.load_project(&minimal_project());
        e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        let (l, r) = render_secs(&mut e, 2.05);
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
        assert!(rms(&l) > 1e-4, "silent output: rms {}", rms(&l));
        let snap = e.snapshot();
        assert!(snap.playing);
        assert!(snap.playhead_ticks > 380.0, "{}", snap.playhead_ticks);
        assert!(snap.master.peak > 0.0);
        assert_eq!(snap.tracks.len(), 1);
    }

    #[test]
    fn deterministic_across_runs() {
        let run = || {
            let mut e = Engine::new(44_100.0);
            e.load_project(&minimal_project());
            e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
            e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
            render_secs(&mut e, 1.0).0
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn mute_silences_track() {
        let mut p = minimal_project();
        p.tracks[0].mute = true;
        let mut e = Engine::new(44_100.0);
        e.load_project(&p);
        e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        let (l, _) = render_secs(&mut e, 1.0);
        assert!(rms(&l) < 1e-6, "muted track leaked: {}", rms(&l));
    }

    #[test]
    fn solo_on_other_track_mutes_this_one() {
        let mut p = minimal_project();
        let mut t2 = p.tracks[0].clone();
        t2.id = Some("t2".into());
        t2.solo = true;
        p.tracks.push(t2);
        let mut e = Engine::new(44_100.0);
        e.load_project(&p);
        e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 }); // t1 has the clip
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        let (l, _) = render_secs(&mut e, 1.0);
        assert!(rms(&l) < 1e-6, "non-solo track leaked: {}", rms(&l));
    }

    #[test]
    fn resolver_addresses_params() {
        let mut e = Engine::new(44_100.0);
        e.load_project(&minimal_project());
        let addr = e.resolver().param("t1|inst|cutoff").expect("cutoff address");
        assert_eq!(addr.target, 0);
        assert_eq!(addr.slot, crate::command::SLOT_INST);
        assert!(e.resolver().param("t1|mix|gain").is_some());
        assert!(e.resolver().param("master|gain").is_some());
        assert!(e.resolver().param("t1|inst|nope").is_none());
        assert_eq!(e.resolver().track("t1"), Some(0));
        assert_eq!(e.resolver().scene("s1"), Some(0));
    }

    #[test]
    fn arr_mode_plays_bounded_clips() {
        let mut p = minimal_project();
        let clip = p.clips.get("t1|s1").unwrap().clone();
        p.arr.insert(
            "a1".into(),
            ArrClipJSON { clip, track_id: "t1".into(), start: 0.0 },
        );
        let mut e = Engine::new(44_100.0);
        e.load_project(&p);
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Arr, from_ticks: 0.0 });
        // 1 bar at 120 bpm = 2 s; render 4 s: sound early, none late
        let (l, _) = render_secs(&mut e, 4.0);
        let early = rms(&l[..44_100]);
        let late = rms(&l[3 * 44_100..]);
        assert!(early > 1e-4);
        assert!(late < 1e-4, "arr clip not bounded: late rms {late}");
    }

    #[test]
    fn split_control_drives_engine_across_threads() {
        fn assert_send<T: Send>() {}
        assert_send::<Engine>();
        assert_send::<EngineControl>();

        let mut e = Engine::new(44_100.0);
        let mut ctl = e.split_control();
        ctl.load_project(&minimal_project());
        assert!(ctl.resolver().param("t1|inst|cutoff").is_some());
        assert!(ctl.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 }));
        assert!(ctl.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 }));
        // the "audio thread": engine moved into it, control stays here
        let rms_l = std::thread::spawn(move || {
            let (l, _r) = render_secs(&mut e, 1.0);
            rms(&l)
        })
        .join()
        .unwrap();
        assert!(rms_l > 1e-4, "split engine silent: rms {rms_l}");
        let snap = ctl.snapshot();
        assert!(snap.playing);
        assert!(snap.playhead_ticks > 0.0);
        assert!(snap.master.peak > 0.0);
    }

    // ---- audio-clip playback (SampleBank → audio voices) ----

    fn sine_bank(id: &str, sr: f32, freq: f32, secs: f32) -> crate::SampleBank {
        let n = (sr * secs) as usize;
        let l: Vec<f32> = (0..n)
            .map(|i| (core::f32::consts::TAU * freq * i as f32 / sr).sin())
            .collect();
        let mut bank = crate::SampleBank::new();
        bank.insert(
            id.to_string(),
            std::sync::Arc::new(sf_dsp::sample::SampleBuf { sample_rate: sr, l, r: vec![] }),
        );
        bank
    }

    fn audio_clip(looped: f64, xfade: f64) -> AudioClipData {
        AudioClipData {
            sample_id: "s1".into(),
            sample_name: "S".into(),
            gain_db: 0.0,
            pitch: 0.0,
            rev: 0.0,
            r#loop: looped,
            fade_in: 0.0,
            fade_out: 0.0,
            offset: None,
            dur: None,
            cents: None,
            xfade: Some(xfade),
        }
    }

    fn audio_project(audio: AudioClipData) -> ProjectJSON {
        let mut p = minimal_project();
        let t = &mut p.tracks[0];
        t.kind = TrackKind::Audio;
        t.inst.r#type = "audiobus".into();
        t.inst.params.clear();
        let clip = p.clips.get_mut("t1|s1").unwrap();
        clip.notes.clear();
        clip.audio = Some(audio);
        p
    }

    #[test]
    fn audio_clip_loop_with_xfade_is_continuous() {
        // 401 Hz (a non-integer cycle count over the crop) would click at a
        // hard loop seam; the baked equal-power crossfade must keep every
        // sample-to-sample step within a continuous sine's slope budget.
        let sr = 44_100.0f32;
        let mut e = Engine::new(sr);
        let bank = sine_bank("s1", sr, 401.0, 0.35);
        e.load_project_with_bank(&audio_project(audio_clip(1.0, 0.05)), &bank);
        e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        let (l, _) = render_secs(&mut e, 3.0); // several loop passes
        assert!(l.iter().all(|s| s.is_finite()));
        assert!(rms(&l[..44_100]) > 0.05, "looped audio clip silent");
        // skip the (browser-identical) hard onset, which lands ~220 samples in
        // behind the master limiter's lookahead delay; every later step — loop
        // seams included — must stay inside a continuous sine's slope budget
        let max_step = l[400..].windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
        // continuous 401 Hz sine slope ≈ 0.057/sample; a seam click is O(1)
        assert!(max_step < 0.19, "loop seam discontinuity: step {max_step}");
    }

    #[test]
    fn audio_one_shot_refires_each_scene_loop() {
        // 0.3 s one-shot in a 1-bar (2 s) clip: plays at the top of every
        // scene pass, silent in between (engine-tone startSessionPlayer).
        let sr = 44_100.0f32;
        let mut e = Engine::new(sr);
        let mut a = audio_clip(0.0, 0.0);
        a.dur = Some(0.3);
        let bank = sine_bank("s1", sr, 300.0, 1.0);
        e.load_project_with_bank(&audio_project(a), &bank);
        e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        let (l, _) = render_secs(&mut e, 2.4);
        let sec = sr as usize;
        assert!(rms(&l[..(0.25 * sr) as usize]) > 0.05, "first fire silent");
        assert!(rms(&l[sec..(1.9 * sr) as usize]) < 1e-5, "one-shot did not end");
        assert!(rms(&l[(2.05 * sr) as usize..(2.25 * sr) as usize]) > 0.05, "no re-fire");
    }

    #[test]
    fn arr_audio_clip_is_bounded() {
        // a LOOPING clip placed on the timeline must still stop at start+len
        let sr = 44_100.0f32;
        let mut e = Engine::new(sr);
        let p0 = audio_project(audio_clip(1.0, 0.0));
        let mut p = p0.clone();
        let clip = p0.clips.get("t1|s1").unwrap().clone();
        p.clips.clear();
        p.arr.insert(
            "a1".into(),
            ArrClipJSON { clip, track_id: "t1".into(), start: 0.0 },
        );
        let bank = sine_bank("s1", sr, 300.0, 1.0);
        e.load_project_with_bank(&p, &bank);
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Arr, from_ticks: 0.0 });
        let (l, _) = render_secs(&mut e, 4.0); // clip = 1 bar = 2 s at 120 bpm
        assert!(l.iter().all(|s| s.is_finite()));
        assert!(rms(&l[..44_100]) > 0.05, "arr audio silent");
        assert!(rms(&l[3 * 44_100..]) < 1e-5, "arr audio clip not bounded");
    }

    #[test]
    fn missing_sample_is_silent_not_a_panic() {
        let sr = 44_100.0f32;
        let mut e = Engine::new(sr);
        e.load_project(&audio_project(audio_clip(1.0, 0.02))); // empty bank
        e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        let (l, r) = render_secs(&mut e, 1.0);
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.0));
    }

    #[test]
    fn ksampler_track_with_bank_entry_is_audible() {
        let sr = 44_100.0f32;
        let mut p = minimal_project();
        p.tracks[0].inst.r#type = "ksampler".into();
        p.tracks[0].inst.params.clear();
        p.tracks[0].inst.sample_id = Some("s1".into());
        let p = sf_project_defaults(&p);
        let bank = sine_bank("s1", sr, 220.0, 2.0);
        let mut e = Engine::new(sr);
        e.load_project_with_bank(&p, &bank);
        e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        let (l, _) = render_secs(&mut e, 1.0);
        assert!(l.iter().all(|s| s.is_finite()));
        assert!(rms(&l) > 1e-3, "ksampler track silent: rms {}", rms(&l));
    }

    /// Fill schema defaults like sf-render's with_defaults (sf-engine can't
    /// depend on sf-render; devices expect full param maps).
    fn sf_project_defaults(p: &ProjectJSON) -> ProjectJSON {
        let mut p = p.clone();
        for t in &mut p.tracks {
            for s in sf_project::registry::inst_params(&t.inst.r#type) {
                t.inst.params.entry(s.key.to_string()).or_insert(s.def as f64);
            }
        }
        p
    }

    // ---- dest "midi" modulation (midimod.rs) ----

    fn arp_fx(rate: f64) -> MidiFxJSON {
        let mut params = BTreeMap::new();
        for (k, v) in [("rate", rate), ("mode", 0.0), ("pat", 0.0), ("oct", 1.0), ("gate", 0.8), ("sel", 1.0)] {
            params.insert(k.to_string(), v);
        }
        MidiFxJSON { id: Some("arp1".into()), r#type: "arp".into(), on: true, params }
    }

    fn midi_lfo(hz: f64, shape: f64) -> LfoJSON {
        LfoJSON {
            id: Some("a".into()),
            on: 1.0,
            shape,
            sync: 0.0,
            rate: 5.0,
            hz,
            depth: 1.0,
            phase: 0.0,
            dest: "midi".into(),
            fx_id: "arp1".into(),
            pkey: "rate".into(),
            targets: None,
        }
    }

    /// One held whole-bar note through an arp at 1/4 (4 events per bar).
    /// rate is an int spec 0..5; a +1 square LFO at depth 1 lands at
    /// 0 + 5/2 = 2.5 → index 2 = 1/8T (32 ticks) → 12 events per bar.
    fn arp_project(lfos: Option<Vec<LfoJSON>>, env: Option<AutoMap>) -> ProjectJSON {
        let mut p = minimal_project();
        p.tracks[0].midifx = Some(vec![arp_fx(0.0)]);
        p.tracks[0].lfos = lfos;
        let clip = p.clips.get_mut("t1|s1").unwrap();
        clip.notes.clear();
        clip.notes.insert("n1".into(), note(48.0, 0.0, 384.0, 0.9));
        clip.env = env;
        p
    }

    fn session_events(e: &Engine) -> usize {
        e.graph.as_ref().unwrap().tracks[0].sched.session[0].as_ref().unwrap().events.len()
    }

    fn start(e: &mut Engine) {
        e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
    }

    #[test]
    fn midi_override_reexpands_after_service() {
        let mut e = Engine::new(44_100.0);
        e.load_project(&arp_project(Some(vec![midi_lfo(0.001, 4.0)]), None));
        start(&mut e);
        let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
        e.process(&mut l, &mut r);
        // the audio thread only flagged the track — the pattern is untouched
        assert_eq!(session_events(&e), 4);
        assert!(e.graph.as_ref().unwrap().midi_shared.is_dirty(0));
        assert_eq!(e.service(), 1);
        assert_eq!(e.midi_rebuilds(), 1);
        assert_eq!(session_events(&e), 4, "swap must land on the audio side");
        e.process(&mut l, &mut r);
        assert_eq!(session_events(&e), 12);
        // the rebuilt pattern is audible (events keep firing)
        let (out, _) = render_secs(&mut e, 1.0);
        assert!(rms(&out) > 1e-4);
    }

    #[test]
    fn midi_quantization_reexpands_only_on_signature_change() {
        let count = |lfo: LfoJSON| {
            let mut e = Engine::new(44_100.0);
            e.load_project(&arp_project(Some(vec![lfo]), None));
            start(&mut e);
            let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
            for _ in 0..(2 * 44_100 / 512) {
                e.process(&mut l, &mut r);
                e.service();
            }
            e.midi_rebuilds()
        };
        // constant value: one rebuild, however many service() calls
        assert_eq!(count(midi_lfo(0.001, 4.0)), 1);
        // 3 Hz triangle sweeping 0..2.5 on an int param: integer crossings
        // (~4/cycle → ~24 in 2 s) each rebuild, but never above 20 Hz
        let n = count(midi_lfo(3.0, 1.0));
        assert!(n > 3, "sweeping LFO should re-expand on crossings: {n}");
        assert!(n <= 41, "throttle exceeded: {n} rebuilds in 2 s");
    }

    #[test]
    fn midi_override_restores_pattern_when_mapping_disappears() {
        // a session clip envelope (active only while the clip is launched)
        // pins rate at norm 1 → 1/32 → 32 events per bar
        let mut env = BTreeMap::new();
        env.insert("midi|arp1|rate".to_string(), vec![AutoPoint { t: 0.0, v: 1.0 }]);
        let mut e = Engine::new(44_100.0);
        e.load_project(&arp_project(None, Some(env)));
        start(&mut e);
        let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
        e.process(&mut l, &mut r);
        assert_eq!(e.service(), 1);
        e.process(&mut l, &mut r);
        assert_eq!(session_events(&e), 32);
        // clip stops → mapping deactivates → override cleared, one restore
        e.push_cmd(Cmd::ClipStop { track: 0 });
        e.process(&mut l, &mut r);
        assert!(e.graph.as_ref().unwrap().midi_shared.value(0).is_none());
        // throttle window: 0.05 s = 2205 samples since the last rebuild
        for _ in 0..5 {
            e.process(&mut l, &mut r);
        }
        assert_eq!(e.service(), 1);
        e.process(&mut l, &mut r);
        assert_eq!(session_events(&e), 4, "stored params not restored");
        // and nothing further once clean
        for _ in 0..10 {
            e.process(&mut l, &mut r);
            assert_eq!(e.service(), 0);
        }
    }

    #[test]
    fn process_does_not_allocate_with_midi_modulation_active() {
        let mut e = Engine::new(44_100.0);
        e.load_project(&arp_project(Some(vec![midi_lfo(3.0, 1.0)]), None));
        start(&mut e);
        let (mut l, mut r) = (vec![0.0f32; 512], vec![0.0f32; 512]);
        e.process(&mut l, &mut r); // graph adoption
        for _ in 0..200 {
            e.service(); // allocation lives here, off the callback
            assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
        }
        assert!(e.midi_rebuilds() > 1, "modulation never re-expanded");
    }

    /// Graph swaps while processing: `load_project` twice (off-thread build)
    /// with guarded `process` calls in between and after — adoption is a
    /// pointer move, the outgoing graph rides `Owned` into the collector.
    #[test]
    fn process_does_not_allocate_across_graph_swaps() {
        let mut e = Engine::new(44_100.0);
        e.load_project(&minimal_project());
        start(&mut e);
        let (mut l, mut r) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        for _ in 0..20 {
            assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
        }
        for gen in 0..3 {
            let mut p = minimal_project();
            p.meta.bpm = 100.0 + 20.0 * gen as f64;
            let mut t2 = p.tracks[0].clone();
            t2.id = Some("t2".into());
            p.tracks.push(t2);
            e.load_project(&p); // control side: allocates, outside the guard
            if gen == 1 {
                e.load_project(&p); // two pending graphs → only the newest wins
            }
            start(&mut e);
            for _ in 0..40 {
                assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
            }
            e.collector.collect(); // off-thread: the old graph frees here
        }
        assert!(l.iter().all(|s| s.is_finite()));
    }

    /// Pattern swaps: `service()` between chunks with dest-"midi" modulation
    /// sweeping, plus re-launches that re-arm the MIDI-mod host — all of
    /// `adopt_patterns` runs guarded.
    #[test]
    fn process_does_not_allocate_across_pattern_swaps_and_relaunch() {
        let mut e = Engine::new(44_100.0);
        e.load_project(&arp_project(Some(vec![midi_lfo(3.0, 1.0)]), None));
        start(&mut e);
        let (mut l, mut r) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        for i in 0..1500 {
            if i % 300 == 150 {
                e.push_cmd(Cmd::ClipStop { track: 0 });
            }
            if i % 300 == 200 {
                e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
            }
            assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
            e.service();
        }
        assert!(e.midi_rebuilds() > 5, "rebuilds: {}", e.midi_rebuilds());
    }

    /// Every `Cmd` variant in a dense burst between guarded chunks.
    #[test]
    fn process_does_not_allocate_under_command_burst() {
        let mut e = Engine::new(44_100.0);
        e.load_project(&minimal_project());
        start(&mut e);
        let (mut l, mut r) = (vec![0.0f32; 64], vec![0.0f32; 64]);
        assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
        for i in 0..600u32 {
            let pitch = 36 + (i % 48) as u8;
            e.push_cmd(Cmd::NoteOn { track: 0, pitch, vel: 0.8 });
            e.push_cmd(Cmd::SetParam {
                target: 0,
                slot: crate::command::SLOT_INST,
                param: (i % 8) as u16,
                value: (i % 5) as f32 / 5.0,
            });
            e.push_cmd(Cmd::SetParam {
                target: crate::command::TARGET_MASTER,
                slot: crate::command::SLOT_MIXER,
                param: crate::command::MIX_GAIN,
                value: -3.0,
            });
            if i % 3 == 0 {
                e.push_cmd(Cmd::NoteOff { track: 0, pitch });
            }
            match i % 120 {
                10 => {
                    e.push_cmd(Cmd::ClipStop { track: 0 });
                }
                20 => {
                    e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
                }
                40 => {
                    e.push_cmd(Cmd::TransportStop);
                }
                50 => {
                    e.push_cmd(Cmd::SetTempo { bpm: 60.0 + (i % 100) as f64 });
                    e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
                }
                70 => {
                    e.push_cmd(Cmd::MetronomeOn);
                }
                90 => {
                    e.push_cmd(Cmd::MetronomeOff);
                    e.push_cmd(Cmd::TransportPlay { mode: Mode::Arr, from_ticks: 0.0 });
                }
                _ => {}
            }
            assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
        }
        assert!(l.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn metronome_blips_when_enabled() {
        let mut e = Engine::new(44_100.0);
        let mut p = minimal_project();
        p.clips.clear(); // no notes — only the metronome sounds
        e.load_project(&p);
        e.push_cmd(Cmd::MetronomeOn);
        e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        let (l, _) = render_secs(&mut e, 1.5);
        assert!(rms(&l) > 1e-5, "metronome silent");
        // beat period at 120 bpm = 0.5 s: expect energy at 0 and 0.5 s
        assert!(rms(&l[..2_000]) > 1e-4);
        assert!(rms(&l[22_050..24_050]) > 1e-4);
        assert!(rms(&l[11_000..21_000]) < 1e-4, "blip rings too long");
    }
}
