//! The track/bus/master graph built from a `ProjectJSON`, mirroring
//! engine-tone.ts buildTrack/wireFx/rewireBuses/applyMuteSolo semantics:
//!   instrument → inst out gain → [fx → out gain…] → pan (Web-Audio stereo
//!   pan law) → fader → master + A/B/per-bus send taps; buses render after
//!   tracks in topological order, cycles cut at the back edge except the
//!   Feedback bus's self-send (0.09 s delay + loop gain). Master: sum →
//!   master fx chain → limiter (Tone.Limiter(−1) semantics: the comp kernel
//!   at threshold −1 dB, ratio 20, attack 3 ms, release 10 ms — not a brickwall).
//!
//! Everything is pre-allocated at build time; `render_chunk` performs no
//! allocation (RUST_ARCH.md §4).

use std::collections::HashMap;
use std::sync::Arc;

use sf_dsp::sample::SampleBuf;
use sf_dsp::traits::{EffectDsp, InstrumentDsp};
use sf_dsp::util::{db_to_lin, DelayLine, Limiter, Smoother};
use sf_dsp::{make_effect, make_instrument, MAX_BLOCK};
use sf_project::project::{AudioClipData, ProjectJSON, SendBus, TrackJSON, TrackKind};
use sf_project::registry::{fx_params, inst_params, ParamSpec};
use sf_project::{ticks_to_seconds, TimeSig, PPQ};

use crate::command::{
    CmdAddr, CommandResolver, MIX_BUS_SEND0, MIX_GAIN, MIX_PAN, MIX_SEND_A, MIX_SEND_B, PARAM_OUT,
    SLOT_FX0, SLOT_INST, SLOT_MIXER, TARGET_MASTER,
};
use crate::meters::MeterBank;
use crate::midimod::{MidiModHost, MidiModShared};
use crate::scheduler::{
    swing_subdiv_ticks, AudioSlot, ClipPattern, ExpandMidi, SchedParams, TrackSched,
};
use crate::transport::Mode;

/// The native sample store: decoded PCM keyed by sample id, owned by the
/// control side (filled by IPC `sample_put`) and read at graph-build time —
/// buffers are injected into instruments (`set_sample`) and baked into
/// audio-clip voices. Persists across `project_load` pushes; a rebuild
/// re-injects.
pub type SampleBank = HashMap<String, Arc<SampleBuf>>;

const FB_DELAY_SEC: f32 = 0.09;
const FB_DEFAULT_SEND: f32 = 0.3;
pub(crate) const MAX_BUS_SENDS: usize = 16;

pub(crate) struct FxSlot {
    pub dev: Box<dyn EffectDsp>,
    pub out: Smoother, // linear gain from FxJSON.out (dB)
    pub params: &'static [ParamSpec],
    pub gr_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BusOut {
    Master,
    Bus(usize),
    /// Back edge routed through a short delay line (index into
    /// `EngineGraph::back_edges`) so the cycle REGENERATES instead of being
    /// dropped. This is what makes a Feedback Chunk (two buses whose outputs
    /// feed each other) loop, and mirrors how the built-in Feedback bus makes
    /// its self-send a legal, audible cycle.
    Delayed(usize),
}

/// A bus→bus edge that closes a cycle. The target renders BEFORE the source,
/// so the source writes into the delay each block and the target reads what
/// was written `delay_samples` ago — one fixed delay per lap, independent of
/// the audio buffer size.
pub(crate) struct BackEdge {
    pub target: usize,
    dl: DelayLine,
    dr: DelayLine,
    delay_samples: usize,
}

impl BackEdge {
    fn new(target: usize, sample_rate: f32) -> Self {
        let delay_samples = (sample_rate * FB_DELAY_SEC).max(64.0) as usize;
        Self {
            target,
            dl: DelayLine::new(delay_samples + 2),
            dr: DelayLine::new(delay_samples + 2),
            delay_samples,
        }
    }

    /// Target side: add what the source wrote a lap ago into `l`/`r`.
    /// Mirrors the Feedback idiom — the write pointer is still where the
    /// previous block left it, so the read walks forward with `k`.
    #[inline]
    fn read_into(&self, l: &mut [f32], r: &mut [f32]) {
        let d = self.delay_samples;
        for k in 0..l.len() {
            l[k] += self.dl.read((d - k).max(1));
            r[k] += self.dr.read((d - k).max(1));
        }
    }

    /// Source side: push this block's post-fader output into the delay.
    #[inline]
    fn write(&mut self, l: &[f32], r: &[f32]) {
        for k in 0..l.len() {
            self.dl.write(l[k]);
            self.dr.write(r[k]);
        }
    }
}

pub(crate) struct Feedback {
    dl: DelayLine,
    dr: DelayLine,
    pub gain: Smoother,
    delay_samples: usize,
}

/// One playable audio clip, derived at graph build exactly like
/// audioclip.ts `clipAudioBuffer`: the crop [offset, offset+dur] with the
/// loop crossfade baked into the head (equal-power sin/cos blend over `xfade`
/// seconds), reversed when `rev`. Pitch+cents fold into `step`
/// (`audioRate() · src_sr / engine_sr`); fades and gain apply at render.
pub(crate) struct DerivedClip {
    l: Vec<f32>,
    r: Vec<f32>, // empty = mono (read l for both sides)
    step: f64,   // source frames per engine sample
    looped: bool,
    gain: f32,
    fade_in: f64,  // engine samples
    fade_out: f64, // engine samples
}

impl DerivedClip {
    #[inline]
    fn frames(&self) -> f64 {
        self.l.len() as f64
    }

    /// Linear-interp read; looped clips wrap the tail back to the head.
    #[inline]
    fn read(&self, pos: f64) -> (f32, f32) {
        let n = self.l.len();
        if n == 0 || pos < 0.0 {
            return (0.0, 0.0);
        }
        let i = pos as usize;
        if i >= n {
            return (0.0, 0.0);
        }
        let frac = (pos - i as f64) as f32;
        let i2 = if i + 1 < n {
            i + 1
        } else if self.looped {
            0
        } else {
            i
        };
        let l = self.l[i] + (self.l[i2] - self.l[i]) * frac;
        let r = if self.r.is_empty() {
            l
        } else {
            self.r[i] + (self.r[i2] - self.r[i]) * frac
        };
        (l, r)
    }

    /// audioclip.ts semantics, verbatim (crop, loop crossfade, reverse).
    fn build(a: &AudioClipData, buf: &SampleBuf, engine_sr: f32, bpm: f64) -> Option<Self> {
        let raw_len = buf.frames();
        if raw_len == 0 || buf.sample_rate <= 0.0 || engine_sr <= 0.0 {
            return None;
        }
        let sr = buf.sample_rate as f64;
        let offset = a.offset.unwrap_or(0.0).max(0.0);
        let dur = a.dur.unwrap_or(0.0);
        let looped = a.r#loop != 0.0;
        let xfade = a.xfade.unwrap_or(0.0);
        let s0 = ((offset * sr) as usize).min(raw_len - 1);
        let max_n = raw_len - s0;
        let n = if dur > 0.0 {
            ((dur * sr) as usize).clamp(1, max_n)
        } else {
            max_n
        };
        let xf = if looped && xfade > 0.0 {
            ((xfade * sr) as usize).min(n / 2)
        } else {
            0
        };
        let out_len = if xf > 0 { n - xf } else { n }.max(1);

        let derive = |src: &[f32]| -> Vec<f32> {
            let mut dst = vec![0.0f32; out_len];
            for (i, d) in dst.iter_mut().enumerate() {
                *d = if xf > 0 && i < xf {
                    // crossfade the region tail (out) into its head (in)
                    let t = i as f32 / xf as f32;
                    let fin = (t * core::f32::consts::FRAC_PI_2).sin();
                    let fout = (t * core::f32::consts::FRAC_PI_2).cos();
                    src[s0 + i] * fin + src[s0 + (n - xf) + i] * fout
                } else {
                    src.get(s0 + i).copied().unwrap_or(0.0)
                };
            }
            if a.rev != 0.0 {
                dst.reverse();
            }
            dst
        };
        let l = derive(&buf.l);
        let r = if buf.r.is_empty() { Vec::new() } else { derive(&buf.r) };

        let rate = 2f64.powf((a.pitch + a.cents.unwrap_or(0.0) / 100.0) / 12.0);
        let spt = engine_sr as f64; // samples per second
        Some(Self {
            l,
            r,
            step: rate * sr / engine_sr as f64,
            looped,
            gain: db_to_lin(a.gain_db as f32),
            fade_in: ticks_to_seconds(a.fade_in.max(0.0), bpm) * spt,
            fade_out: ticks_to_seconds(a.fade_out.max(0.0), bpm) * spt,
        })
    }
}

/// A bounded arrangement audio clip: start/stop in absolute ticks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AudioArrEv {
    tick: f64,
    stop_tick: f64,
    clip: usize,
}

/// One playing (or armed) audio-clip voice. Session voices re-derive their
/// position from the launch anchor; arrangement voices are activated by the
/// per-chunk cursor. All state is Copy — no allocation on the audio thread.
#[derive(Debug, Clone, Copy)]
struct AudioVoice {
    active: bool,
    clip: usize,
    pos: f64,       // fractional frame in the derived buffer
    played: f64,    // engine samples since the current pass began (fade-in)
    begin_tick: f64, // current pass start (session one-shot: last fire)
    stop_tick: f64, // begin the fade-out here (∞ = run forever)
    next_fire: f64, // session one-shot: next re-fire tick (∞ otherwise)
    fire_period: f64,
    anchor: f64, // session identity: the launch anchor this voice derives from
}

impl AudioVoice {
    fn idle() -> Self {
        Self {
            active: false,
            clip: 0,
            pos: 0.0,
            played: 0.0,
            begin_tick: 0.0,
            stop_tick: f64::INFINITY,
            next_fire: f64::INFINITY,
            fire_period: 0.0,
            anchor: 0.0,
        }
    }
}

/// Session voice slot; arrangement voices fill the rest.
const AUDIO_SESSION_VOICE: usize = 0;
const AUDIO_VOICES: usize = 6;

pub(crate) struct TrackNode {
    pub is_bus: bool,
    pub inst: Box<dyn InstrumentDsp>,
    pub inst_params: &'static [ParamSpec],
    pub inst_out: Smoother,
    pub fx: Vec<FxSlot>,
    pub pan: Smoother,
    pub fader: Smoother,
    pub mute_gain: Smoother,
    pub send_a: Smoother,
    pub send_b: Smoother,
    /// (target track index of a bus, level smoother); forward edges only.
    pub bus_sends: Vec<(usize, Smoother)>,
    pub bus_out: BusOut,
    pub fb: Option<Feedback>,
    pub sched: TrackSched,
    // audio-clip playback (audiobus tracks): derived buffers + voice pool
    audio_clips: Vec<DerivedClip>,
    audio_arr: Vec<AudioArrEv>,
    audio_voices: [AudioVoice; AUDIO_VOICES],
    audio_arr_idx: usize,
    /// Expected chunk-start tick; a mismatch (seek, loop wrap, launch while
    /// stopped) triggers a resync of every audio voice. NaN forces resync.
    audio_expected: f64,
    audio_mode: Mode,
    // work buffer: accumulation input for buses, scratch for legacy returns
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
}

impl TrackNode {
    /// Mix the track's audio-clip voices into l/r (pre inst-out gain — the
    /// browser players connect into `inst.out`). Sample-accurate via
    /// per-sample tick gating; a chunk-start tick that isn't the expected
    /// continuation (seek, loop wrap, transport start, mode switch) resyncs
    /// every voice by re-deriving positions from the launch anchor / event
    /// starts, mirroring Tone's transport-synced players.
    fn render_audio(
        &mut self,
        l: &mut [f32],
        r: &mut [f32],
        pos: f64,
        tps: f64,
        mode: Mode,
        playing: bool,
    ) {
        if self.audio_clips.is_empty() {
            return;
        }
        if !playing || tps <= 0.0 {
            self.audio_expected = f64::NAN;
            return;
        }
        let n = l.len();
        let end = pos + n as f64 * tps;
        let resync = !self.audio_expected.is_finite()
            || (pos - self.audio_expected).abs() > tps * 0.5
            || mode != self.audio_mode;
        self.audio_mode = mode;
        self.audio_expected = end;
        if resync {
            for v in &mut self.audio_voices {
                v.active = false;
            }
            if mode == Mode::Arr {
                self.audio_arr_idx = self.audio_arr.partition_point(|e| e.tick < pos - 1e-9);
                // clips already begun and still audible restart mid-flight
                for ei in 0..self.audio_arr_idx {
                    let ev = self.audio_arr[ei];
                    let clip = &self.audio_clips[ev.clip];
                    if ev.stop_tick + clip.fade_out * tps <= pos {
                        continue;
                    }
                    let elapsed = (pos - ev.tick) / tps; // engine samples
                    let mut fpos = elapsed * clip.step;
                    if clip.looped {
                        fpos %= clip.frames().max(1.0);
                    } else if fpos >= clip.frames() {
                        continue;
                    }
                    self.activate_arr(ev, fpos, elapsed);
                }
            }
        }
        match mode {
            Mode::Session => match self.sched.audio_slot() {
                None => self.audio_voices[AUDIO_SESSION_VOICE].active = false,
                Some(s) => {
                    let anchor = self.sched.anchor();
                    let v = &self.audio_voices[AUDIO_SESSION_VOICE];
                    if !v.active || v.clip != s.clip || v.anchor != anchor {
                        self.init_session_voice(s, anchor, pos, tps);
                    }
                }
            },
            Mode::Arr => {
                while self.audio_arr_idx < self.audio_arr.len()
                    && self.audio_arr[self.audio_arr_idx].tick < end
                {
                    let ev = self.audio_arr[self.audio_arr_idx];
                    self.audio_arr_idx += 1;
                    self.activate_arr(ev, 0.0, 0.0);
                }
            }
        }
        for vi in 0..AUDIO_VOICES {
            let v = &mut self.audio_voices[vi];
            if !v.active {
                continue;
            }
            let Some(clip) = self.audio_clips.get(v.clip) else {
                v.active = false;
                continue;
            };
            let len_f = clip.frames();
            for (k, (ol, or)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
                let tick = pos + k as f64 * tps;
                if tick >= v.next_fire {
                    // session one-shot: re-fire from the head each clip length
                    v.pos = 0.0;
                    v.played = 0.0;
                    v.begin_tick = v.next_fire;
                    v.next_fire += v.fire_period.max(tps);
                }
                if tick < v.begin_tick {
                    continue;
                }
                if !clip.looped && v.pos >= len_f {
                    if v.next_fire.is_finite() {
                        continue; // armed one-shot: silent until the next fire
                    }
                    v.active = false;
                    break;
                }
                let mut g = clip.gain;
                if tick >= v.stop_tick {
                    if clip.fade_out <= 0.0 {
                        v.active = false;
                        break;
                    }
                    // Tone OneShotSource: the fade-out BEGINS at the stop time
                    let t = (tick - v.stop_tick) / tps;
                    if t >= clip.fade_out {
                        v.active = false;
                        break;
                    }
                    g *= (1.0 - t / clip.fade_out) as f32;
                }
                if clip.fade_in > 0.0 && v.played < clip.fade_in {
                    g *= (v.played / clip.fade_in) as f32;
                }
                let (sl, sr) = clip.read(v.pos);
                *ol += sl * g;
                *or += sr * g;
                v.pos += clip.step;
                if clip.looped && v.pos >= len_f {
                    v.pos -= len_f;
                }
                v.played += 1.0;
            }
        }
    }

    /// (Re)derive the session voice from the launch anchor at time `now` —
    /// mid-launch, seek and resume all land at the position the browser's
    /// transport-synced player would be at.
    fn init_session_voice(&mut self, s: AudioSlot, anchor: f64, now: f64, tps: f64) {
        let clip = &self.audio_clips[s.clip];
        let len_f = clip.frames().max(1.0);
        let mut v = AudioVoice {
            active: true,
            clip: s.clip,
            anchor,
            ..AudioVoice::idle()
        };
        if s.looped {
            v.begin_tick = anchor;
            if now > anchor {
                let elapsed = (now - anchor) / tps;
                v.pos = (elapsed * clip.step) % len_f;
                v.played = elapsed;
            }
        } else {
            let period = s.loop_len.max(1.0);
            let last = if now > anchor {
                anchor + ((now - anchor) / period).floor() * period
            } else {
                anchor
            };
            v.begin_tick = last;
            v.next_fire = last + period;
            v.fire_period = period;
            if now > last {
                let elapsed = (now - last) / tps;
                v.pos = elapsed * clip.step; // past the end ⇒ waits, armed
                v.played = elapsed;
            }
        }
        self.audio_voices[AUDIO_SESSION_VOICE] = v;
    }

    /// Start an arrangement voice (free slot, else steal the earliest start).
    fn activate_arr(&mut self, ev: AudioArrEv, pos: f64, played: f64) {
        let mut vi = AUDIO_SESSION_VOICE + 1;
        let mut oldest = f64::INFINITY;
        let mut free = None;
        for (i, v) in self
            .audio_voices
            .iter()
            .enumerate()
            .skip(AUDIO_SESSION_VOICE + 1)
        {
            if !v.active {
                free = Some(i);
                break;
            }
            if v.begin_tick < oldest {
                oldest = v.begin_tick;
                vi = i;
            }
        }
        self.audio_voices[free.unwrap_or(vi)] = AudioVoice {
            active: true,
            clip: ev.clip,
            pos,
            played,
            begin_tick: ev.tick,
            stop_tick: ev.stop_tick,
            next_fire: f64::INFINITY,
            fire_period: 0.0,
            anchor: 0.0,
        };
    }
}

pub(crate) struct ReturnNode {
    dev: Box<dyn EffectDsp>,
    gain: Smoother,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuckTarget {
    Track { track: usize, fx: usize },
    Master { fx: usize },
}

#[derive(Debug, Clone, Copy)]
pub struct DuckRoute {
    pub src: usize,
    pub target: DuckTarget,
    /// −1 = any note; 0–7 = drum-pad match on `pitch % 8`.
    pub pitch: i32,
}

pub struct EngineGraph {
    pub(crate) tracks: Vec<TrackNode>,
    /// Delay lines closing bus cycles (see [`BusOut::Delayed`]).
    back_edges: Vec<BackEdge>,
    /// Render order: non-bus tracks in doc order, then buses topologically.
    order: Vec<usize>,
    send_a_bus: Option<usize>,
    send_b_bus: Option<usize>,
    legacy_returns: Vec<ReturnNode>,
    master_gain: Smoother,
    pub(crate) master_fx: Vec<FxSlot>,
    limiter: Limiter,
    pub duck_map: Vec<DuckRoute>,
    mix_l: Vec<f32>,
    mix_r: Vec<f32>,
    tmp_l: Vec<f32>,
    tmp_r: Vec<f32>,
    pub meters: Arc<MeterBank>,
    /// Pre-resolved modulation/automation state (modulation.rs).
    pub(crate) modul: crate::modulation::ModState,
    /// dest-"midi" override slots shared with the control-side host.
    pub(crate) midi_shared: Arc<MidiModShared>,
    /// Build generation: pattern swaps from an older graph are discarded.
    pub(crate) gen: u64,
    /// Engine rate at build (audio-clip voices convert ticks ↔ samples).
    sample_rate: f32,

    // meta snapshot
    pub sched_params: SchedParams,
    pub bpm: f64,
    pub launch_q: f64,
    pub time_sig: TimeSig,
    pub loop_on: bool,
    pub loop_start: f64,
    pub loop_end: f64,
}

/// Web-Audio StereoPannerNode stereo pan law (what the live engine's raw
/// StereoPannerNode applies): pan 0 is passthrough; panning blends the far
/// channel into the near one with equal-power gains.
#[inline]
fn stereo_pan(l: f32, r: f32, pan: f32) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    if p == 0.0 {
        return (l, r);
    }
    let x = if p <= 0.0 { p + 1.0 } else { p };
    let (gl, gr) = (
        (x * core::f32::consts::FRAC_PI_2).cos(),
        (x * core::f32::consts::FRAC_PI_2).sin(),
    );
    if p <= 0.0 {
        (l + r * gl, r * gr)
    } else {
        (l * gl, r + l * gr)
    }
}

fn track_id(t: &TrackJSON) -> &str {
    t.id.as_deref().unwrap_or(&t.name)
}

pub(crate) fn fx_id(f: &sf_project::project::FxJSON) -> &str {
    f.id.as_deref().unwrap_or(&f.r#type)
}

fn snapped(v: f32) -> Smoother {
    let mut s = Smoother::new(v);
    s.snap(v);
    s
}

/// Advance a smoother by `n` samples and return the landed value (per-chunk
/// gain resolution for send taps; faders/pans stay per-sample).
#[inline]
fn advance(s: &mut Smoother, n: usize) -> f32 {
    let mut v = s.current();
    for _ in 0..n {
        v = s.next();
    }
    v
}

struct BuildFx {
    slots: Vec<FxSlot>,
    ducks: Vec<(String, usize, i32)>, // (src track id, fx slot, pitch)
}

fn build_fx_chain(
    fx_json: &[sf_project::project::FxJSON],
    sample_rate: f32,
    gr_counter: &mut usize,
) -> BuildFx {
    let mut slots = Vec::new();
    let mut ducks = Vec::new();
    for f in fx_json {
        if !f.on {
            continue; // honor FxJSON.on
        }
        let Some(mut dev) = make_effect(&f.r#type) else {
            continue; // unknown device type: skip
        };
        dev.prepare(sample_rate);
        for (k, v) in &f.params {
            dev.set_param(k, *v as f32);
        }
        if f.r#type == "duck" {
            let src = f.src_track.clone().unwrap_or_default();
            dev.set_param("srcOn", if src.is_empty() { 0.0 } else { 1.0 });
            if !src.is_empty() {
                ducks.push((
                    src,
                    slots.len(),
                    f.src_pitch.map(|p| p as i32).unwrap_or(-1),
                ));
            }
        }
        slots.push(FxSlot {
            dev,
            out: snapped(db_to_lin(f.out.unwrap_or(0.0) as f32)),
            params: fx_params(&f.r#type),
            gr_idx: {
                let i = *gr_counter;
                *gr_counter += 1;
                i
            },
        });
    }
    BuildFx { slots, ducks }
}

/// Build the graph + its command resolver from a project (off-thread).
/// `bank` supplies decoded samples: instrument buffers are injected via
/// `set_sample` (inst.sampleId → slot 0, drum padSamples → slots 0–7) and
/// audio clips are derived into per-track voices. Missing ids play silent.
pub fn build_graph(
    project: &ProjectJSON,
    sample_rate: f32,
    expand: ExpandMidi,
    bank: &SampleBank,
) -> (EngineGraph, CommandResolver) {
    let (g, r, _) = build_graph_full(project, sample_rate, expand, bank, 0);
    (g, r)
}

/// [`build_graph`] plus the control-side MIDI-modulation host (present only
/// when the project routes something to a MIDI-fx param) tagged with
/// `graph_gen` so its pattern swaps can be matched to this graph.
pub fn build_graph_full(
    project: &ProjectJSON,
    sample_rate: f32,
    expand: ExpandMidi,
    bank: &SampleBank,
    graph_gen: u64,
) -> (EngineGraph, CommandResolver, Option<MidiModHost>) {
    let meta = &project.meta;
    let bpm = if meta.bpm > 0.0 { meta.bpm } else { 120.0 };
    let sched_params = SchedParams {
        swing: meta.swing.max(0.0),
        swing_ticks: swing_subdiv_ticks(meta.swing_subdivision.as_deref()),
        humanize: meta.humanize.unwrap_or(0.0).clamp(0.0, 1.0),
    };
    let time_sig = TimeSig::from_meta(meta.ts_top, meta.ts_bottom);

    let track_ids: Vec<String> = project.tracks.iter().map(|t| track_id(t).to_string()).collect();
    let scene_ids: Vec<String> = project
        .scenes
        .iter()
        .map(|s| s.id.clone().unwrap_or_else(|| s.name.clone()))
        .collect();
    let idx_of = |id: &str| track_ids.iter().position(|t| t == id);

    // ---- bus topology: Kahn over output-target + send edges; leftovers
    // (cycles) append in doc order and their back edges get cut below.
    let bus_idx: Vec<usize> = project
        .tracks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.kind == TrackKind::Bus)
        .map(|(i, _)| i)
        .collect();
    let is_bus = |i: usize| project.tracks[i].kind == TrackKind::Bus;
    let mut edges: Vec<(usize, usize)> = Vec::new(); // u renders before v
    for &u in &bus_idx {
        let t = &project.tracks[u];
        if let Some(out) = t.output.as_deref() {
            if let Some(v) = idx_of(out).filter(|&v| is_bus(v) && v != u) {
                edges.push((u, v));
            }
        }
        if let Some(sends) = &t.sends {
            for bid in sends.keys() {
                if let Some(v) = idx_of(bid).filter(|&v| is_bus(v) && v != u) {
                    edges.push((u, v));
                }
            }
        }
    }
    let mut bus_order: Vec<usize> = Vec::with_capacity(bus_idx.len());
    {
        let mut indeg: Vec<usize> = bus_idx
            .iter()
            .map(|&v| edges.iter().filter(|(_, e)| *e == v).count())
            .collect();
        let mut done = vec![false; bus_idx.len()];
        while let Some(pos) = (0..bus_idx.len()).find(|&p| !done[p] && indeg[p] == 0) {
            done[pos] = true;
            bus_order.push(bus_idx[pos]);
            for &(u, v) in &edges {
                if u == bus_idx[pos] {
                    if let Some(vp) = bus_idx.iter().position(|&b| b == v) {
                        indeg[vp] = indeg[vp].saturating_sub(1);
                    }
                }
            }
        }
        for (p, &b) in bus_idx.iter().enumerate() {
            if !done[p] {
                bus_order.push(b); // cyclic leftovers, doc order
            }
        }
    }
    let bus_pos = |i: usize| bus_order.iter().position(|&b| b == i);
    // delay lines closing bus cycles, filled as back edges are discovered
    let mut back_edges: Vec<BackEdge> = Vec::new();

    let send_a_bus = project
        .tracks
        .iter()
        .position(|t| t.kind == TrackKind::Bus && t.send == Some(SendBus::A));
    let send_b_bus = project
        .tracks
        .iter()
        .position(|t| t.kind == TrackKind::Bus && t.send == Some(SendBus::B));
    let legacy_ab = send_a_bus.is_none() && send_b_bus.is_none();

    let any_solo = project.tracks.iter().any(|t| t.solo);
    let mut resolver = CommandResolver::new();
    let mut gr_counter = 0usize;
    let mut duck_raw: Vec<(String, DuckTarget, i32)> = Vec::new();
    let mut tracks: Vec<TrackNode> = Vec::with_capacity(project.tracks.len());

    for (ti, t) in project.tracks.iter().enumerate() {
        let tid = &track_ids[ti];
        resolver.insert_track(tid.clone(), ti as u16);
        let bus = t.kind == TrackKind::Bus;

        let mut inst = make_instrument(&t.inst.r#type)
            .or_else(|| make_instrument("audiobus"))
            .expect("audiobus is always registered");
        inst.prepare(sample_rate);
        for (k, v) in &t.inst.params {
            inst.set_param(k, *v as f32);
        }
        // sample injection (set_sample is a defaulted no-op elsewhere)
        if let Some(b) = t.inst.sample_id.as_ref().and_then(|sid| bank.get(sid)) {
            inst.set_sample(0, Some(Arc::clone(b)));
        }
        if let Some(ps) = &t.inst.pad_samples {
            for (k, sid) in ps {
                if let (Ok(slot), Some(b)) = (k.parse::<u32>(), bank.get(sid)) {
                    if slot < 8 {
                        inst.set_sample(slot, Some(Arc::clone(b)));
                    }
                }
            }
        }
        let ip = inst_params(&t.inst.r#type);
        for (pi, spec) in ip.iter().enumerate() {
            resolver.insert_param(
                format!("{tid}|inst|{}", spec.key),
                CmdAddr { target: ti as u16, slot: SLOT_INST, param: pi as u16 },
            );
        }

        let built = build_fx_chain(&t.fx, sample_rate, &mut gr_counter);
        for (src, slot, pitch) in built.ducks {
            duck_raw.push((src, DuckTarget::Track { track: ti, fx: slot }, pitch));
        }
        // resolver entries follow the BUILT chain (enabled, known devices)
        {
            let mut slot = 0usize;
            for f in &t.fx {
                if !f.on || make_effect(&f.r#type).is_none() {
                    continue;
                }
                let fid = fx_id(f);
                for (pi, spec) in fx_params(&f.r#type).iter().enumerate() {
                    resolver.insert_param(
                        format!("{tid}|{fid}|{}", spec.key),
                        CmdAddr {
                            target: ti as u16,
                            slot: SLOT_FX0 + slot as u8,
                            param: pi as u16,
                        },
                    );
                }
                resolver.insert_param(
                    format!("{tid}|{fid}|out"),
                    CmdAddr { target: ti as u16, slot: SLOT_FX0 + slot as u8, param: PARAM_OUT },
                );
                slot += 1;
            }
        }
        for (key, param) in [
            ("gain", MIX_GAIN),
            ("pan", MIX_PAN),
            ("sendA", MIX_SEND_A),
            ("sendB", MIX_SEND_B),
        ] {
            resolver.insert_param(
                format!("{tid}|mix|{key}"),
                CmdAddr { target: ti as u16, slot: SLOT_MIXER, param },
            );
        }

        // per-bus sends (forward edges only for bus sources; self handled by fb)
        let mut bus_sends: Vec<(usize, Smoother)> = Vec::new();
        if let Some(sends) = &t.sends {
            for (bid, level) in sends {
                let Some(v) = idx_of(bid) else { continue };
                if v == ti || !is_bus(v) {
                    continue;
                }
                if bus {
                    let (Some(pu), Some(pv)) = (bus_pos(ti), bus_pos(v)) else { continue };
                    if pv < pu {
                        continue; // back edge — cut
                    }
                }
                if bus_sends.len() >= MAX_BUS_SENDS {
                    break;
                }
                resolver.insert_param(
                    format!("{tid}|send|{bid}"),
                    CmdAddr {
                        target: ti as u16,
                        slot: SLOT_MIXER,
                        param: MIX_BUS_SEND0 + bus_sends.len() as u16,
                    },
                );
                bus_sends.push((v, snapped(*level as f32)));
            }
        }

        // bus output routing (rewireBuses), back edges cut
        let bus_out = if !bus {
            BusOut::Master
        } else {
            match t.output.as_deref() {
                None | Some("master") => BusOut::Master,
                Some(out) => match idx_of(out).filter(|&v| is_bus(v) && v != ti) {
                    None => BusOut::Master,
                    Some(v) => {
                        let (pu, pv) = (bus_pos(ti), bus_pos(v));
                        if pv > pu {
                            BusOut::Bus(v)
                        } else {
                            // closes a cycle: route it through a delay so the
                            // loop regenerates (Feedback Chunk) instead of
                            // being dropped
                            back_edges.push(BackEdge::new(v, sample_rate));
                            BusOut::Delayed(back_edges.len() - 1)
                        }
                    }
                },
            }
        };

        // Feedback bus: post-fader → 0.09 s delay → loop gain → own input
        let fb = if t.send == Some(SendBus::F) {
            let delay_samples =
                ((FB_DELAY_SEC * sample_rate) as usize).max(MAX_BLOCK + 1);
            let level = t
                .sends
                .as_ref()
                .and_then(|s| s.get(tid))
                .map(|v| *v as f32)
                .unwrap_or(FB_DEFAULT_SEND);
            resolver.insert_param(
                format!("{tid}|send|{tid}"),
                CmdAddr { target: ti as u16, slot: SLOT_MIXER, param: MIX_BUS_SEND0 + MAX_BUS_SENDS as u16 },
            );
            Some(Feedback {
                dl: DelayLine::new(delay_samples + MAX_BLOCK + 2),
                dr: DelayLine::new(delay_samples + MAX_BLOCK + 2),
                gain: snapped(level),
                delay_samples,
            })
        } else {
            None
        };

        // ---- event patterns (expanded through the track's MIDI-fx chain) ----
        let root = meta.root;
        let scale = meta.scale.as_str();
        let mut sched = TrackSched::default();
        let mut audio_clips: Vec<DerivedClip> = Vec::new();
        let mut audio_arr: Vec<AudioArrEv> = Vec::new();
        if !bus {
            sched.session = scene_ids
                .iter()
                .map(|sid| {
                    let clip = project.clips.get(&sf_project::project::clip_key(tid, sid))?;
                    if clip.audio.is_some() {
                        return None; // audio clips render via audio voices
                    }
                    let len = clip.len.max(1.0);
                    let notes: Vec<_> = clip.notes.values().cloned().collect();
                    let mut events = expand(t, &notes, len, root, scale);
                    events.retain(|e| e.tick < len);
                    Some(ClipPattern { loop_len: len, events })
                })
                .collect();
            // session audio clips → derived buffers + launchable slots
            sched.audio_session = scene_ids
                .iter()
                .map(|sid| {
                    let clip = project.clips.get(&sf_project::project::clip_key(tid, sid))?;
                    let a = clip.audio.as_ref()?;
                    let buf = bank.get(&a.sample_id)?;
                    let derived = DerivedClip::build(a, buf, sample_rate, bpm)?;
                    let idx = audio_clips.len();
                    audio_clips.push(derived);
                    Some(AudioSlot {
                        clip: idx,
                        loop_len: clip.len.max(1.0),
                        looped: a.r#loop != 0.0,
                    })
                })
                .collect();
            let mut arr = Vec::new();
            for a in project.arr.values() {
                if a.track_id != *tid {
                    continue;
                }
                let len = a.clip.len.max(1.0);
                if let Some(ac) = &a.clip.audio {
                    // bounded audio clip: start..start+len (fade-out after)
                    if let Some(derived) = bank
                        .get(&ac.sample_id)
                        .and_then(|buf| DerivedClip::build(ac, buf, sample_rate, bpm))
                    {
                        let idx = audio_clips.len();
                        audio_clips.push(derived);
                        audio_arr.push(AudioArrEv {
                            tick: a.start,
                            stop_tick: a.start + len,
                            clip: idx,
                        });
                    }
                    continue;
                }
                let notes: Vec<_> = a.clip.notes.values().cloned().collect();
                let mut evs = expand(t, &notes, len, root, scale);
                evs.retain(|e| e.tick < len);
                for mut e in evs {
                    e.tick += a.start;
                    arr.push(e);
                }
            }
            arr.sort_by(|a, b| a.tick.total_cmp(&b.tick));
            audio_arr.sort_by(|a, b| a.tick.total_cmp(&b.tick));
            sched.arr = arr;
        }

        let muted = t.mute || (any_solo && !t.solo);
        tracks.push(TrackNode {
            is_bus: bus,
            inst,
            inst_params: ip,
            inst_out: snapped(db_to_lin(t.inst.out.unwrap_or(0.0) as f32)),
            fx: built.slots,
            pan: snapped(t.pan as f32),
            fader: snapped(db_to_lin(t.gain as f32)),
            mute_gain: snapped(if muted { 0.0 } else { 1.0 }),
            send_a: snapped(t.send_a.unwrap_or(0.0) as f32),
            send_b: snapped(t.send_b.unwrap_or(0.0) as f32),
            bus_sends,
            bus_out,
            fb,
            sched,
            audio_clips,
            audio_arr,
            audio_voices: [AudioVoice::idle(); AUDIO_VOICES],
            audio_arr_idx: 0,
            audio_expected: f64::NAN,
            audio_mode: Mode::Session,
            buf_l: vec![0.0; MAX_BLOCK],
            buf_r: vec![0.0; MAX_BLOCK],
        });
    }

    for (si, sid) in scene_ids.iter().enumerate() {
        resolver.insert_scene(sid.clone(), si as u16);
    }

    // legacy return channels only when the project has no A/B send buses
    let legacy_returns: Vec<ReturnNode> = if legacy_ab {
        project
            .returns
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .filter_map(|r| {
                let mut dev = make_effect(&r.fx_type)?;
                dev.prepare(sample_rate);
                for (k, v) in &r.params {
                    dev.set_param(k, *v as f32);
                }
                Some(ReturnNode {
                    dev,
                    gain: snapped(db_to_lin(r.gain as f32)),
                    buf_l: vec![0.0; MAX_BLOCK],
                    buf_r: vec![0.0; MAX_BLOCK],
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    // master fx chain + resolver entries
    let master_built = build_fx_chain(project.master_fx.as_deref().unwrap_or(&[]), sample_rate, &mut gr_counter);
    for (src, slot, pitch) in master_built.ducks {
        duck_raw.push((src, DuckTarget::Master { fx: slot }, pitch));
    }
    // duck source ids → track indices (unresolvable sources are dropped)
    let duck_map: Vec<DuckRoute> = duck_raw
        .iter()
        .filter_map(|(src, target, pitch)| {
            idx_of(src).map(|s| DuckRoute { src: s, target: *target, pitch: *pitch })
        })
        .collect();
    {
        let mut slot = 0usize;
        for f in project.master_fx.as_deref().unwrap_or(&[]) {
            if !f.on || make_effect(&f.r#type).is_none() {
                continue;
            }
            let fid = fx_id(f);
            for (pi, spec) in fx_params(&f.r#type).iter().enumerate() {
                resolver.insert_param(
                    format!("master|{fid}|{}", spec.key),
                    CmdAddr {
                        target: TARGET_MASTER,
                        slot: SLOT_FX0 + slot as u8,
                        param: pi as u16,
                    },
                );
            }
            resolver.insert_param(
                format!("master|{fid}|out"),
                CmdAddr { target: TARGET_MASTER, slot: SLOT_FX0 + slot as u8, param: PARAM_OUT },
            );
            slot += 1;
        }
    }
    resolver.insert_param(
        "master|gain".to_string(),
        CmdAddr { target: TARGET_MASTER, slot: SLOT_MIXER, param: MIX_GAIN },
    );

    // Tone.Limiter(-1): DynamicsCompressor semantics, threshold −1 dBFS.
    let mut limiter = Limiter::new(-1.0);
    limiter.prepare(sample_rate);

    let order: Vec<usize> = (0..tracks.len())
        .filter(|&i| !tracks[i].is_bus)
        .chain(bus_order.iter().copied())
        .collect();

    let meters = Arc::new(MeterBank::new(tracks.len(), gr_counter));
    let modul = crate::modulation::build_mod_state(project, &track_ids, &scene_ids, sample_rate);
    let midi_shared = Arc::new(MidiModShared::new(modul.midi_override_count(), project.tracks.len()));
    let midi_host = if modul.midi_override_count() > 0 {
        Some(MidiModHost::build(
            project,
            &track_ids,
            &scene_ids,
            modul.midi_ovs.clone(),
            Arc::clone(&midi_shared),
            expand,
            (graph_gen, sample_rate),
        ))
    } else {
        None
    };
    let graph = EngineGraph {
        back_edges,
        tracks,
        order,
        send_a_bus,
        send_b_bus,
        legacy_returns,
        master_gain: snapped(db_to_lin(meta.master_gain.unwrap_or(0.0) as f32)),
        master_fx: master_built.slots,
        limiter,
        duck_map,
        mix_l: vec![0.0; MAX_BLOCK],
        mix_r: vec![0.0; MAX_BLOCK],
        tmp_l: vec![0.0; MAX_BLOCK],
        tmp_r: vec![0.0; MAX_BLOCK],
        meters,
        modul,
        midi_shared,
        gen: graph_gen,
        sample_rate,
        sched_params,
        bpm,
        launch_q: meta.launch_q,
        time_sig,
        loop_on: meta.loop_on.unwrap_or(false),
        loop_start: meta.loop_start.unwrap_or(0.0),
        loop_end: meta.loop_end.unwrap_or(4.0 * 384.0),
    };
    (graph, resolver, midi_host)
}

impl EngineGraph {
    /// Next launch boundary in ticks (engine-tone nextBoundaryTicks).
    pub fn next_boundary_ticks(&self, cur_ticks: f64) -> f64 {
        let cur = cur_ticks.round();
        if self.launch_q == 0.0 {
            return cur + 4.0;
        }
        let q_ticks = self.launch_q * self.time_sig.bar_ticks() as f64;
        ((cur + 1.0) / q_ticks).ceil() * q_ticks
    }

    /// A note fired on `src` — kick every duck listening to that track/pad.
    pub fn fire_ducks(&mut self, src: usize, pitch: u8) {
        let pad = (pitch % 8) as i32;
        for di in 0..self.duck_map.len() {
            let r = self.duck_map[di];
            if r.src != src || (r.pitch >= 0 && r.pitch != pad) {
                continue;
            }
            match r.target {
                DuckTarget::Track { track, fx } => {
                    if let Some(slot) = self.tracks[track].fx.get_mut(fx) {
                        slot.dev.trigger();
                    }
                }
                DuckTarget::Master { fx } => {
                    if let Some(slot) = self.master_fx.get_mut(fx) {
                        slot.dev.trigger();
                    }
                }
            }
        }
    }

    /// Apply a resolved SetParam (RT path — key strings are &'static).
    pub fn set_param(&mut self, target: u16, slot: u8, param: u16, value: f32) {
        if target == TARGET_MASTER {
            if slot == SLOT_MIXER {
                if param == MIX_GAIN {
                    self.master_gain.set_target(db_to_lin(value));
                }
            } else if slot >= SLOT_FX0 {
                Self::set_fx_param(&mut self.master_fx, (slot - SLOT_FX0) as usize, param, value);
            }
            return;
        }
        let Some(tr) = self.tracks.get_mut(target as usize) else {
            return;
        };
        if slot == SLOT_MIXER {
            match param {
                MIX_GAIN => tr.fader.set_target(db_to_lin(value)),
                MIX_PAN => tr.pan.set_target(value.clamp(-1.0, 1.0)),
                MIX_SEND_A => tr.send_a.set_target(value.max(0.0)),
                MIX_SEND_B => tr.send_b.set_target(value.max(0.0)),
                p if p >= MIX_BUS_SEND0 => {
                    let i = (p - MIX_BUS_SEND0) as usize;
                    if i < tr.bus_sends.len() {
                        tr.bus_sends[i].1.set_target(value.max(0.0));
                    } else if i == MAX_BUS_SENDS {
                        if let Some(fb) = &mut tr.fb {
                            fb.gain.set_target(value.max(0.0));
                        }
                    }
                }
                _ => {}
            }
        } else if slot == SLOT_INST {
            if let Some(spec) = tr.inst_params.get(param as usize) {
                tr.inst.set_param(spec.key, value);
            }
        } else {
            Self::set_fx_param(&mut tr.fx, (slot - SLOT_FX0) as usize, param, value);
        }
    }

    fn set_fx_param(chain: &mut [FxSlot], idx: usize, param: u16, value: f32) {
        let Some(slot) = chain.get_mut(idx) else { return };
        if param == PARAM_OUT {
            slot.out.set_target(db_to_lin(value));
        } else if let Some(spec) = slot.params.get(param as usize) {
            slot.dev.set_param(spec.key, value);
        }
    }

    /// Render one ≤ MAX_BLOCK chunk. `metro` mixes into the master input
    /// (pre master-gain, like the live metronome synth). `mode` drives the
    /// audio-clip voices (session loop/re-fire vs bounded arrangement).
    pub fn render_chunk(
        &mut self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        pos_ticks: f64,
        playing: bool,
        mode: Mode,
        metro: Option<&mut crate::metro::Metronome>,
    ) {
        let n = out_l.len();
        debug_assert!(n <= MAX_BLOCK && out_r.len() == n);
        self.mix_l[..n].fill(0.0);
        self.mix_r[..n].fill(0.0);
        for tr in &mut self.tracks {
            tr.buf_l[..n].fill(0.0);
            tr.buf_r[..n].fill(0.0);
        }
        for rn in &mut self.legacy_returns {
            rn.buf_l[..n].fill(0.0);
            rn.buf_r[..n].fill(0.0);
        }

        let bpm = self.bpm;
        // device-meter release, shared by every meter in this block
        let meter_decay = crate::meters::meter_decay(n, self.sample_rate);
        for oi in 0..self.order.len() {
            let i = self.order[oi];
            // process into tmp (disjoint field borrows: tracks[i] + tmp)
            let mut sends: [(usize, f32); MAX_BUS_SENDS] = [(0, 0.0); MAX_BUS_SENDS];
            let mut n_sends = 0usize;
            let (ga, gb): (f32, f32);
            let (is_bus, bus_out, has_fb);
            {
                let tr = &mut self.tracks[i];
                is_bus = tr.is_bus;
                bus_out = tr.bus_out;
                has_fb = tr.fb.is_some();
                if is_bus {
                    self.tmp_l[..n].copy_from_slice(&tr.buf_l[..n]);
                    self.tmp_r[..n].copy_from_slice(&tr.buf_r[..n]);
                    // returning signal from a bus cycle (Feedback Chunk)
                    for e in &self.back_edges {
                        if e.target == i {
                            e.read_into(&mut self.tmp_l[..n], &mut self.tmp_r[..n]);
                        }
                    }
                    // Feedback loop: delayed own output re-enters the input
                    if let Some(fb) = &mut tr.fb {
                        let d = fb.delay_samples;
                        for k in 0..n {
                            let g = fb.gain.next();
                            self.tmp_l[k] += fb.dl.read((d - k).max(1)) * g;
                            self.tmp_r[k] += fb.dr.read((d - k).max(1)) * g;
                        }
                    }
                } else {
                    self.tmp_l[..n].fill(0.0);
                    self.tmp_r[..n].fill(0.0);
                    tr.inst.process(&mut self.tmp_l[..n], &mut self.tmp_r[..n]);
                    let tps = bpm / 60.0 * PPQ as f64 / self.sample_rate.max(1.0) as f64;
                    tr.render_audio(
                        &mut self.tmp_l[..n],
                        &mut self.tmp_r[..n],
                        pos_ticks,
                        tps,
                        mode,
                        playing,
                    );
                }
                let og = advance(&mut tr.inst_out, n);
                if og != 1.0 {
                    for k in 0..n {
                        self.tmp_l[k] *= og;
                        self.tmp_r[k] *= og;
                    }
                }
                // instrument meter: after the instrument's out knob (buses have
                // no instrument, but their input level is still worth showing)
                self.meters.set_inst_level(
                    i,
                    crate::meters::block_peak(&self.tmp_l[..n], &self.tmp_r[..n]),
                    meter_decay,
                );
                for slot in &mut tr.fx {
                    slot.dev.tick(pos_ticks, playing, bpm);
                    slot.dev.process(&mut self.tmp_l[..n], &mut self.tmp_r[..n]);
                    let og = advance(&mut slot.out, n);
                    if og != 1.0 {
                        for k in 0..n {
                            self.tmp_l[k] *= og;
                            self.tmp_r[k] *= og;
                        }
                    }
                    self.meters.set_gr(slot.gr_idx, slot.dev.gr_db());
                    self.meters.set_fx_bands(slot.gr_idx, slot.dev.gr_bands());
                    self.meters.set_fx_level(
                        slot.gr_idx,
                        crate::meters::block_peak(&self.tmp_l[..n], &self.tmp_r[..n]),
                        meter_decay,
                    );
                }
                // pan → fader → mute, per-sample smoothing
                let mut peak = 0.0f32;
                let mut sq = 0.0f32;
                for k in 0..n {
                    let (pl, pr) = stereo_pan(self.tmp_l[k], self.tmp_r[k], tr.pan.next());
                    let g = tr.fader.next() * tr.mute_gain.next();
                    let (l, r) = (pl * g, pr * g);
                    self.tmp_l[k] = l;
                    self.tmp_r[k] = r;
                    peak = peak.max(l.abs()).max(r.abs());
                    sq += 0.5 * (l * l + r * r);
                }
                self.meters.set_track(i, peak, (sq / n.max(1) as f32).sqrt());
                // buses have no A/B sends (engine-tone wireSends skips them)
                if is_bus {
                    (ga, gb) = (0.0, 0.0);
                } else {
                    ga = advance(&mut tr.send_a, n);
                    gb = advance(&mut tr.send_b, n);
                }
                for (bi, sm) in tr.bus_sends.iter_mut() {
                    sends[n_sends] = (*bi, advance(sm, n));
                    n_sends += 1;
                }
            }

            // distribute tmp (post-fader) — tmp immutable, targets mutable
            let route_master = match bus_out {
                BusOut::Master => true,
                BusOut::Delayed(e) => {
                    // cycle back edge: hand this block to the delay; the
                    // target bus picks it up a lap later
                    self.back_edges[e].write(&self.tmp_l[..n], &self.tmp_r[..n]);
                    false
                }
                BusOut::Bus(j) => {
                    let dst = &mut self.tracks[j];
                    for k in 0..n {
                        dst.buf_l[k] += self.tmp_l[k];
                        dst.buf_r[k] += self.tmp_r[k];
                    }
                    false
                }
            };
            if route_master {
                for k in 0..n {
                    self.mix_l[k] += self.tmp_l[k];
                    self.mix_r[k] += self.tmp_r[k];
                }
            }
            for &(j, g) in sends.iter().take(n_sends) {
                if g <= 0.0 {
                    continue;
                }
                let dst = &mut self.tracks[j];
                for k in 0..n {
                    dst.buf_l[k] += self.tmp_l[k] * g;
                    dst.buf_r[k] += self.tmp_r[k] * g;
                }
            }
            if !is_bus {
                if ga > 0.0 {
                    if let Some(a) = self.send_a_bus {
                        let dst = &mut self.tracks[a];
                        for k in 0..n {
                            dst.buf_l[k] += self.tmp_l[k] * ga;
                            dst.buf_r[k] += self.tmp_r[k] * ga;
                        }
                    } else if let Some(rn) = self.legacy_returns.get_mut(0) {
                        for k in 0..n {
                            rn.buf_l[k] += self.tmp_l[k] * ga;
                            rn.buf_r[k] += self.tmp_r[k] * ga;
                        }
                    }
                }
                if gb > 0.0 {
                    if let Some(b) = self.send_b_bus {
                        let dst = &mut self.tracks[b];
                        for k in 0..n {
                            dst.buf_l[k] += self.tmp_l[k] * gb;
                            dst.buf_r[k] += self.tmp_r[k] * gb;
                        }
                    } else if let Some(rn) = self.legacy_returns.get_mut(1) {
                        for k in 0..n {
                            rn.buf_l[k] += self.tmp_l[k] * gb;
                            rn.buf_r[k] += self.tmp_r[k] * gb;
                        }
                    }
                }
            }
            if has_fb {
                let tr = &mut self.tracks[i];
                if let Some(fb) = &mut tr.fb {
                    for k in 0..n {
                        fb.dl.write(self.tmp_l[k]);
                        fb.dr.write(self.tmp_r[k]);
                    }
                }
            }
        }

        // legacy return channels → master
        for rn in &mut self.legacy_returns {
            rn.dev.tick(pos_ticks, playing, bpm);
            rn.dev.process(&mut rn.buf_l[..n], &mut rn.buf_r[..n]);
            let g = advance(&mut rn.gain, n);
            for k in 0..n {
                self.mix_l[k] += rn.buf_l[k] * g;
                self.mix_r[k] += rn.buf_r[k] * g;
            }
        }

        // metronome mixes into the master input, pre master gain
        if let Some(m) = metro {
            m.process(&mut self.mix_l[..n], &mut self.mix_r[..n]);
        }

        // master: gain → fx chain → limiter (Tone.Limiter(−1) semantics)
        for k in 0..n {
            let g = self.master_gain.next();
            self.mix_l[k] *= g;
            self.mix_r[k] *= g;
        }
        for slot in &mut self.master_fx {
            slot.dev.tick(pos_ticks, playing, bpm);
            slot.dev.process(&mut self.mix_l[..n], &mut self.mix_r[..n]);
            let og = advance(&mut slot.out, n);
            if og != 1.0 {
                for k in 0..n {
                    self.mix_l[k] *= og;
                    self.mix_r[k] *= og;
                }
            }
            self.meters.set_gr(slot.gr_idx, slot.dev.gr_db());
            self.meters.set_fx_bands(slot.gr_idx, slot.dev.gr_bands());
            self.meters.set_fx_level(
                slot.gr_idx,
                crate::meters::block_peak(&self.mix_l[..n], &self.mix_r[..n]),
                meter_decay,
            );
        }
        self.limiter.process(&mut self.mix_l[..n], &mut self.mix_r[..n]);

        let mut peak = 0.0f32;
        let mut sq = 0.0f32;
        for k in 0..n {
            out_l[k] = self.mix_l[k];
            out_r[k] = self.mix_r[k];
            peak = peak.max(out_l[k].abs()).max(out_r[k].abs());
            sq += 0.5 * (out_l[k] * out_l[k] + out_r[k] * out_r[k]);
        }
        self.meters.set_master(peak, (sq / n.max(1) as f32).sqrt());
    }
}
