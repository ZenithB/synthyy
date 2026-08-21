//! Block-rate modulation/automation — port of the engine-tone.ts modulation
//! loop (`applyModulation`/`resolveTarget`/`envValueAt`) evaluated once per
//! rendered chunk (≤ MAX_BLOCK frames) instead of per rAF frame.
//!
//! Sources (baked into the graph at `load_project`; live edits arrive as a
//! full re-push, per the IPC design):
//!   - session mode: the launched clip's envelopes (`ClipJSON.env`, keys
//!     "dest|fxId|pkey", values NORMALIZED 0..1, looping over the clip length)
//!   - arr mode: track automation lanes (`TrackJSON.auto`, absolute song
//!     ticks), then envelopes of arrangement clips under the playhead
//!     OVERRIDE same-key lanes; plus master automation (`masterAuto`)
//!   - per-track LFOs (`TrackJSON.lfos`, `targets[]` + the legacy single
//!     dest/fxId/pkey folded in): sync = tick-division phase via
//!     [`LFO_DIV_TICKS`], free = Hz phase integration per block
//!
//! Combination rule (applyModulation): automation gives the BASE via
//! `valueFromSpec(spec, norm)` (log curve for exp specs); each LFO adds a
//! bipolar offset `raw · depth · (max−min)/2`; the sum clamps to [min,max].
//! When a mapping deactivates (clip stopped, playhead left the clip,
//! transport stopped) the param snaps back to its stored base value.
//!
//! Application is the command lane's own `EngineGraph::set_param` path:
//! every mapping is pre-resolved at graph build into numeric addresses and
//! flat arrays, so the per-block work is pure arithmetic + set_param calls
//! (zero allocation on the audio thread).
//!
//! Two routes land somewhere other than a device param:
//!   - dest "lfo" (→ another LFO's rate): the combined value becomes the
//!     target LFO's effective Hz for the NEXT block (the browser's one-frame
//!     lag: `lfoRateMod` is read by the LFO loop, written by the combine
//!     step). Only free-running targets track Hz; a synced target keeps its
//!     division. An override not refreshed this block reverts to the stored
//!     Hz ([`LfoState::rated`] is cleared before each combine pass).
//!   - dest "midi" (→ a MIDI-fx param): the value is published to the shared
//!     [`crate::midimod::MidiModShared`] slots and, when its quantized step changes, the
//!     track is flagged dirty; the CONTROL side re-expands the launched clip
//!     (see midimod.rs — allocation never happens here). A deactivated
//!     mapping clears its override and dirties the track once more so the
//!     un-modulated pattern comes back.
//!
//! NOT ported (documented deviations from the browser loop):
//!   - stopped-transport animation: the browser keeps synced LFOs moving on
//!     wall-clock "free ticks" while stopped; here sync phase follows the
//!     transport tick (frozen when stopped — free-running LFOs still run).

use std::collections::HashMap;

use sf_dsp::make_effect;
use sf_dsp::util::lfo_shape_value;
use sf_project::project::{clip_key, AutoPoint, FxJSON, ProjectJSON, SendBus};
use sf_project::registry::{fx_params, inst_params, ParamSpec};
use sf_project::BAR;

use crate::command::{
    MIX_BUS_SEND0, MIX_GAIN, MIX_PAN, MIX_SEND_A, MIX_SEND_B, SLOT_FX0, SLOT_INST,
    SLOT_MIXER, TARGET_MASTER,
};
use crate::graph::{fx_id, EngineGraph, MAX_BUS_SENDS};
use crate::midimod::{quantize, resolve_midi_target, MidiOvDesc};
use crate::transport::Mode;

/// schema.ts `LFO_DIV_TICKS`: synced-LFO cycle length in ticks per rate index
/// (8 bar … 1/16 at PPQ 96).
pub const LFO_DIV_TICKS: [f64; 9] =
    [3072.0, 1536.0, 768.0, 384.0, 192.0, 96.0, 48.0, 32.0, 24.0];

/// Synced-LFO cycle length for a (JS-truncated) rate index; out-of-range
/// indices fall back to one bar, mirroring `LFO_DIV_TICKS[rate] || 384`.
pub fn lfo_div_ticks(rate: f64) -> f64 {
    let idx = rate.trunc() as i64;
    if (0..LFO_DIV_TICKS.len() as i64).contains(&idx) {
        LFO_DIV_TICKS[idx as usize]
    } else {
        384.0
    }
}

#[derive(Debug, Clone, Copy)]
struct Pt {
    t: f64,
    v: f32,
}

/// How a route lands on the graph: one resolved param address, or a macro
/// fanning out to its own pre-resolved sub-targets.
#[derive(Debug, Clone, Copy)]
enum Apply {
    Param { target: u16, slot: u8, param: u16 },
    Macro { sub_start: u32, sub_len: u32 },
    /// Another LFO's effective Hz (track-local LFO index, pre-resolved).
    LfoRate { track: u16, lfo: u16 },
    /// MIDI-fx param override slot in [`crate::midimod::MidiModShared`]; `stepped` picks the
    /// integer-crossing quantizer (int/steps specs) over the 40-step one.
    Midi { ov: u32, stepped: bool },
}

/// One modulation target: numeric address + the spec fields the combine rule
/// needs (min/max/log curve) + the stored base value to snap back to.
#[derive(Debug, Clone, Copy)]
struct Route {
    apply: Apply,
    min: f32,
    max: f32,
    log: bool,
    base: f32,
}

/// Macro fan-out entry: `min + (max−min)·v`, clamped — the browser's linear
/// fan (no curve), applied through set_param like everything else.
#[derive(Debug, Clone, Copy)]
struct MacroSub {
    target: u16,
    slot: u8,
    param: u16,
    min: f32,
    max: f32,
}

/// Session clip envelope lane (active while its scene's clip is launched).
#[derive(Debug)]
struct EnvLane {
    route: u32,
    scene: u16,
    loop_len: f64,
    pts: Vec<Pt>,
}

/// Arrangement clip envelope lane (active while the playhead is inside).
#[derive(Debug)]
struct ArrLane {
    route: u32,
    start: f64,
    len: f64,
    loop_len: f64,
    pts: Vec<Pt>,
}

/// Track automation / master automation lane (absolute song ticks).
#[derive(Debug)]
struct AutoLane {
    route: u32,
    pts: Vec<Pt>,
}

#[derive(Debug)]
struct LfoState {
    on: bool,
    shape: u32,
    sync: bool,
    div_ticks: f64,
    hz: f64,
    depth: f32,
    phase: f64,
    free_phase: f64,
    /// Rate override written by a dest-"lfo" route in the previous block's
    /// combine pass; `rated` is cleared before every combine so a stale
    /// override reverts to `hz` (browser `lfoRateMod` / `lfoRatedThisFrame`).
    eff_hz: f64,
    rated: bool,
    targets: Vec<u32>, // route indices
}

#[derive(Debug, Default)]
struct TrackMod {
    env: Vec<EnvLane>,
    arr_env: Vec<ArrLane>,
    auto: Vec<AutoLane>,
    lfos: Vec<LfoState>,
}

/// All modulation state, pre-resolved at graph build. The scratch arrays are
/// indexed by route and re-filled each block — no per-block allocation.
#[derive(Debug, Default)]
pub struct ModState {
    routes: Vec<Route>,
    subs: Vec<MacroSub>,
    tracks: Vec<TrackMod>,
    master_auto: Vec<AutoLane>,
    sample_rate: f64,
    // per-block scratch, all sized routes.len()
    auto_norm: Vec<f32>,
    auto_set: Vec<bool>,
    lfo_off: Vec<f32>,
    active: Vec<bool>,
    prev_active: Vec<bool>,
    /// dest-"midi" targets in override-slot order (graph build moves them
    /// into the control-side host); `midi_quant` is the last published
    /// quantized step per slot (None = override inactive).
    pub(crate) midi_ovs: Vec<MidiOvDesc>,
    midi_quant: Vec<Option<i32>>,
}

impl ModState {
    /// Number of dest-"midi" override slots (sizes [`crate::midimod::MidiModShared`]).
    pub(crate) fn midi_override_count(&self) -> usize {
        self.midi_quant.len()
    }
}

/// engine-tone.ts `envValueAt`: linear interpolation between breakpoints,
/// clamped to the first/last values outside their range.
fn env_value_at(pts: &[Pt], pos: f64) -> f32 {
    debug_assert!(!pts.is_empty());
    if pts.len() == 1 || pos <= pts[0].t {
        return pts[0].v;
    }
    let last = pts[pts.len() - 1];
    if pos >= last.t {
        return last.v;
    }
    for i in 0..pts.len() - 1 {
        let (a, b) = (pts[i], pts[i + 1]);
        if pos >= a.t && pos <= b.t {
            let f = (pos - a.t) / (b.t - a.t).max(1.0);
            return a.v + (b.v - a.v) * f as f32;
        }
    }
    last.v
}

/// schema.ts `valueFromSpec` over the route's captured curve fields.
fn value_from(rt: &Route, n: f32) -> f32 {
    let c = n.clamp(0.0, 1.0);
    if rt.log {
        (rt.min.ln() + c * (rt.max.ln() - rt.min.ln())).exp()
    } else {
        rt.min + c * (rt.max - rt.min)
    }
}

/// Automation base + LFO offset, clamped to the spec range (the combine step
/// of applyModulation). `auto` is the normalized lane value when a lane is
/// active this block; otherwise the stored base is the anchor.
fn combined_value(rt: &Route, auto: Option<f32>, lfo_off: f32) -> f32 {
    let base = match auto {
        Some(n) => value_from(rt, n),
        None => rt.base,
    };
    let off = lfo_off * (rt.max - rt.min) * 0.5;
    (base + off).clamp(rt.min, rt.max)
}

// ---------------- build (off-thread, at load_project) ----------------

fn pts_of(points: &[AutoPoint]) -> Vec<Pt> {
    points.iter().map(|p| Pt { t: p.t, v: p.v as f32 }).collect()
}

/// Find `fxId` in a BUILT fx chain (enabled + known devices only, matching
/// build_fx_chain's slot numbering) → (slot index, param specs, json).
fn built_fx<'a>(
    chain: &'a [FxJSON],
    want: &str,
) -> Option<(usize, &'static [ParamSpec], &'a FxJSON)> {
    let mut slot = 0usize;
    for f in chain {
        if !f.on || make_effect(&f.r#type).is_none() {
            continue;
        }
        if fx_id(f) == want {
            return Some((slot, fx_params(&f.r#type), f));
        }
        slot += 1;
    }
    None
}

struct Builder<'a> {
    project: &'a ProjectJSON,
    routes: Vec<Route>,
    subs: Vec<MacroSub>,
    /// "<ti>|dest|fxId|pkey" (or "master|…") → route index; misses cached too.
    cache: HashMap<String, Option<u32>>,
    midi_ovs: Vec<MidiOvDesc>,
}

impl<'a> Builder<'a> {
    /// resolveTarget port for one leaf param (no macro recursion): numeric
    /// address + spec fields + stored base. None mirrors the browser's null.
    fn leaf(
        &self,
        ti: usize,
        dest: &str,
        fxid: &str,
        pkey: &str,
    ) -> Option<(u16, u8, u16, f32, f32, bool, f32)> {
        let t = &self.project.tracks[ti];
        let tid = t.id.as_deref().unwrap_or(&t.name);
        match dest {
            "inst" => {
                let specs = inst_params(&t.inst.r#type);
                let pi = specs.iter().position(|s| s.key == pkey)?;
                let s = &specs[pi];
                let base = *t.inst.params.get(pkey)? as f32;
                Some((ti as u16, SLOT_INST, pi as u16, s.min, s.max, s.is_log(), base))
            }
            "mix" => {
                let (param, min, max, base) = match pkey {
                    "gain" => (MIX_GAIN, -48.0, 6.0, t.gain as f32),
                    "pan" => (MIX_PAN, -1.0, 1.0, t.pan as f32),
                    _ => return None,
                };
                Some((ti as u16, SLOT_MIXER, param, min, max, false, base))
            }
            "send" => match pkey {
                // Feedback bus self-send — only exists on the F bus
                "F" => {
                    if t.send != Some(SendBus::F) {
                        return None;
                    }
                    let base = t
                        .sends
                        .as_ref()
                        .and_then(|s| s.get(tid))
                        .map(|v| *v as f32)
                        .unwrap_or(0.3);
                    Some((
                        ti as u16,
                        SLOT_MIXER,
                        MIX_BUS_SEND0 + MAX_BUS_SENDS as u16,
                        0.0,
                        0.95,
                        false,
                        base,
                    ))
                }
                "B" => {
                    let base = t.send_b.unwrap_or(0.0) as f32;
                    Some((ti as u16, SLOT_MIXER, MIX_SEND_B, 0.0, 1.0, false, base))
                }
                _ => {
                    let base = t.send_a.unwrap_or(0.0) as f32;
                    Some((ti as u16, SLOT_MIXER, MIX_SEND_A, 0.0, 1.0, false, base))
                }
            },
            // anything else falls through to the fx lookup (browser behavior)
            _ => {
                let (slot, specs, f) = built_fx(&t.fx, fxid)?;
                let pi = specs.iter().position(|s| s.key == pkey)?;
                let s = &specs[pi];
                let base = *f.params.get(pkey)? as f32;
                Some((
                    ti as u16,
                    SLOT_FX0 + slot as u8,
                    pi as u16,
                    s.min,
                    s.max,
                    s.is_log(),
                    base,
                ))
            }
        }
    }

    /// Route for a track-scoped "dest|fxId|pkey" key (deduped per track).
    fn track_route(&mut self, ti: usize, key: &str) -> Option<u32> {
        let ck = format!("{ti}|{key}");
        if let Some(hit) = self.cache.get(&ck) {
            return *hit;
        }
        let made = self.make_track_route(ti, key);
        self.cache.insert(ck, made);
        made
    }

    fn make_track_route(&mut self, ti: usize, key: &str) -> Option<u32> {
        let mut parts = key.split('|');
        let dest = parts.next()?;
        let fxid = parts.next()?;
        let pkey = parts.next()?;
        let t = &self.project.tracks[ti];
        let route = match dest {
            // another LFO's rate: fxId = target LFO id; spec = LFO_PARAMS hz
            // (0.01–30, exp); base = the target's stored Hz (`?? 1`)
            "lfo" => {
                let lfos = t.lfos.as_deref()?;
                let li = lfos.iter().position(|l| l.id.as_deref() == Some(fxid))?;
                Route {
                    apply: Apply::LfoRate { track: ti as u16, lfo: li as u16 },
                    min: 0.01,
                    max: 30.0,
                    log: true,
                    base: lfos[li].hz as f32,
                }
            }
            // a MIDI-fx param: fxId = midifx id; lands in an override slot
            "midi" => {
                let (fi, spec, base) = resolve_midi_target(t, fxid, pkey)?;
                let ov = self.midi_ovs.len() as u32;
                self.midi_ovs.push(MidiOvDesc { track: ti, fx: fi, key: spec.key });
                Route {
                    apply: Apply::Midi { ov, stepped: spec.int || spec.steps.is_some() },
                    min: spec.min,
                    max: spec.max,
                    log: spec.is_log(),
                    base,
                }
            }
            "macro" => {
                let idx: usize = pkey.parse().ok()?;
                let m = t.macros.as_ref()?.get(idx)?;
                let sub_start = self.subs.len() as u32;
                let mut n = 0u32;
                for tg in m.targets.clone() {
                    // macro targets are only inst/mix/send/fx — no recursion
                    if matches!(tg.dest.as_str(), "macro" | "midi" | "lfo") {
                        continue;
                    }
                    if let Some((target, slot, param, min, max, _log, _base)) =
                        self.leaf(ti, &tg.dest, &tg.fx_id, &tg.pkey)
                    {
                        self.subs.push(MacroSub { target, slot, param, min, max });
                        n += 1;
                    }
                }
                Route {
                    apply: Apply::Macro { sub_start, sub_len: n },
                    min: 0.0,
                    max: 1.0,
                    log: false,
                    base: m.value as f32,
                }
            }
            _ => {
                let (target, slot, param, min, max, log, base) =
                    self.leaf(ti, dest, fxid, pkey)?;
                Route { apply: Apply::Param { target, slot, param }, min, max, log, base }
            }
        };
        let ri = self.routes.len() as u32;
        self.routes.push(route);
        Some(ri)
    }

    /// resolveMasterTarget port: "mix||gain" or a master-fx param.
    fn master_route(&mut self, key: &str) -> Option<u32> {
        let ck = format!("master|{key}");
        if let Some(hit) = self.cache.get(&ck) {
            return *hit;
        }
        let made = (|| {
            let mut parts = key.split('|');
            let dest = parts.next()?;
            let fxid = parts.next()?;
            let pkey = parts.next()?;
            let route = if dest == "mix" && pkey == "gain" {
                let base = self.project.meta.master_gain.unwrap_or(0.0) as f32;
                Route {
                    apply: Apply::Param { target: TARGET_MASTER, slot: SLOT_MIXER, param: MIX_GAIN },
                    min: -48.0,
                    max: 6.0,
                    log: false,
                    base,
                }
            } else if dest == "fx" {
                let chain = self.project.master_fx.as_deref().unwrap_or(&[]);
                let (slot, specs, f) = built_fx(chain, fxid)?;
                let pi = specs.iter().position(|s| s.key == pkey)?;
                let s = &specs[pi];
                Route {
                    apply: Apply::Param {
                        target: TARGET_MASTER,
                        slot: SLOT_FX0 + slot as u8,
                        param: pi as u16,
                    },
                    min: s.min,
                    max: s.max,
                    log: s.is_log(),
                    base: *f.params.get(pkey)? as f32,
                }
            } else {
                return None;
            };
            let ri = self.routes.len() as u32;
            self.routes.push(route);
            Some(ri)
        })();
        self.cache.insert(ck, made);
        made
    }
}

/// engine-tone.ts `lfoCfgsOf` target list: `targets[]` plus the legacy single
/// dest/fxId/pkey fields, deduped by key, resolved to route indices.
fn lfo_targets(b: &mut Builder, ti: usize, l: &sf_project::project::LfoJSON) -> Vec<u32> {
    let mut keys: Vec<String> = Vec::new();
    let push = |dest: &str, fxid: &str, pkey: &str, keys: &mut Vec<String>| {
        if dest.is_empty() || pkey.is_empty() {
            return;
        }
        let k = format!("{dest}|{fxid}|{pkey}");
        if !keys.contains(&k) {
            keys.push(k);
        }
    };
    for tg in l.targets.iter().flatten() {
        push(&tg.dest, &tg.fx_id, &tg.pkey, &mut keys);
    }
    push(&l.dest, &l.fx_id, &l.pkey, &mut keys);
    keys.iter().filter_map(|k| b.track_route(ti, k)).collect()
}

/// Bake every mapping in the project into pre-resolved modulation state
/// (called from build_graph, off-thread).
pub(crate) fn build_mod_state(
    project: &ProjectJSON,
    track_ids: &[String],
    scene_ids: &[String],
    sample_rate: f32,
) -> ModState {
    let mut b = Builder {
        project,
        routes: Vec::new(),
        subs: Vec::new(),
        cache: HashMap::new(),
        midi_ovs: Vec::new(),
    };
    let mut tracks: Vec<TrackMod> = Vec::with_capacity(project.tracks.len());

    for (ti, t) in project.tracks.iter().enumerate() {
        let tid = &track_ids[ti];
        let mut tm = TrackMod::default();

        // session clip envelopes, one lane per (scene, key)
        for (si, sid) in scene_ids.iter().enumerate() {
            let Some(clip) = project.clips.get(&clip_key(tid, sid)) else { continue };
            let Some(env) = &clip.env else { continue };
            for (key, pts) in env {
                if pts.is_empty() {
                    continue;
                }
                if let Some(route) = b.track_route(ti, key) {
                    tm.env.push(EnvLane {
                        route,
                        scene: si as u16,
                        loop_len: if clip.len > 0.0 { clip.len } else { BAR as f64 },
                        pts: pts_of(pts),
                    });
                }
            }
        }

        // arrangement clip envelopes
        for a in project.arr.values() {
            if a.track_id != *tid {
                continue;
            }
            let Some(env) = &a.clip.env else { continue };
            for (key, pts) in env {
                if pts.is_empty() {
                    continue;
                }
                if let Some(route) = b.track_route(ti, key) {
                    tm.arr_env.push(ArrLane {
                        route,
                        start: a.start,
                        len: a.clip.len,
                        loop_len: if a.clip.len > 0.0 { a.clip.len } else { BAR as f64 },
                        pts: pts_of(pts),
                    });
                }
            }
        }

        // track automation lanes (arr mode, absolute ticks)
        for (key, pts) in t.auto.iter().flatten() {
            if pts.is_empty() {
                continue;
            }
            if let Some(route) = b.track_route(ti, key) {
                tm.auto.push(AutoLane { route, pts: pts_of(pts) });
            }
        }

        // LFOs
        for l in t.lfos.iter().flatten() {
            let targets = lfo_targets(&mut b, ti, l);
            tm.lfos.push(LfoState {
                on: l.on != 0.0,
                shape: l.shape.trunc().max(0.0) as u32,
                sync: l.sync != 0.0,
                div_ticks: lfo_div_ticks(l.rate),
                hz: l.hz,
                depth: l.depth as f32,
                phase: l.phase,
                free_phase: 0.0,
                eff_hz: l.hz,
                rated: false,
                targets,
            });
        }

        tracks.push(tm);
    }

    // master automation (arr mode)
    let mut master_auto = Vec::new();
    for (key, pts) in project.master_auto.iter().flatten() {
        if pts.is_empty() {
            continue;
        }
        if let Some(route) = b.master_route(key) {
            master_auto.push(AutoLane { route, pts: pts_of(pts) });
        }
    }

    let n = b.routes.len();
    ModState {
        routes: b.routes,
        subs: b.subs,
        tracks,
        master_auto,
        sample_rate: sample_rate.max(1.0) as f64,
        auto_norm: vec![0.0; n],
        auto_set: vec![false; n],
        lfo_off: vec![0.0; n],
        active: vec![false; n],
        prev_active: vec![false; n],
        midi_quant: vec![None; b.midi_ovs.len()],
        midi_ovs: b.midi_ovs,
    }
}

// ---------------- apply (audio thread, once per chunk) ----------------

impl EngineGraph {
    fn apply_route(&mut self, rt: &Route, subs: &[MacroSub], v: f32) {
        match rt.apply {
            Apply::Param { target, slot, param } => self.set_param(target, slot, param, v),
            Apply::Macro { sub_start, sub_len } => {
                let (s, n) = (sub_start as usize, sub_len as usize);
                for sub in &subs[s..s + n] {
                    let raw = (sub.min + (sub.max - sub.min) * v).clamp(sub.min, sub.max);
                    self.set_param(sub.target, sub.slot, sub.param, raw);
                }
            }
            // handled in apply_modulation's combine pass (they mutate ModState)
            Apply::LfoRate { .. } | Apply::Midi { .. } => {}
        }
    }

    /// ClipLaunch hook: a track with live MIDI-fx overrides needs its newly
    /// launched pattern re-expanded with them folded in (the browser's
    /// makePart runs applyMidiFx with `midiFxMod` at launch time).
    pub(crate) fn midi_mod_relaunch(&self, track: usize) {
        let shared = &self.midi_shared;
        for (oi, ov) in self.modul.midi_ovs.iter().enumerate() {
            if ov.track == track && shared.value(oi).is_some() {
                shared.set_launched(track, self.tracks[track].sched.launched_scene());
                shared.mark_dirty(track);
                return;
            }
        }
    }

    /// Evaluate every modulation source at `now` and push the combined values
    /// through set_param. Zero allocation: `ModState` is moved out and back
    /// (a Vec move), and all scratch arrays were sized at build.
    pub fn apply_modulation(&mut self, now: f64, playing: bool, mode: Mode, frames: usize) {
        if self.modul.routes.is_empty() {
            return;
        }
        let mut m = std::mem::take(&mut self.modul);
        {
            let ModState {
                routes: _,
                subs: _,
                tracks,
                master_auto,
                sample_rate,
                auto_norm,
                auto_set,
                lfo_off,
                active,
                prev_active: _,
                midi_ovs: _,
                midi_quant,
            } = &mut m;
            auto_set.fill(false);
            lfo_off.fill(0.0);
            active.fill(false);
            let in_session = playing && mode == Mode::Session;
            let in_arr = playing && mode == Mode::Arr;
            let dt = (frames as f64 / *sample_rate).min(0.1);
            let has_midi = !midi_quant.is_empty();
            if has_midi {
                self.midi_shared.advance(frames);
            }

            for (ti, tm) in tracks.iter_mut().enumerate() {
                if has_midi {
                    self.midi_shared
                        .set_launched(ti, self.tracks[ti].sched.launched_scene());
                }
                if in_session {
                    // the launched clip's looping envelopes
                    let sched = &self.tracks[ti].sched;
                    if let Some(scene) = sched.launched_scene() {
                        let anchor = sched.anchor();
                        for lane in &tm.env {
                            if lane.scene as usize != scene {
                                continue;
                            }
                            let pos = (now - anchor).rem_euclid(lane.loop_len.max(1.0));
                            let r = lane.route as usize;
                            auto_norm[r] = env_value_at(&lane.pts, pos);
                            auto_set[r] = true;
                            active[r] = true;
                        }
                    }
                } else if in_arr {
                    // timeline lanes first…
                    for lane in &tm.auto {
                        let r = lane.route as usize;
                        auto_norm[r] = env_value_at(&lane.pts, now);
                        auto_set[r] = true;
                        active[r] = true;
                    }
                    // …then clip envelopes under the playhead override them
                    for lane in &tm.arr_env {
                        if now < lane.start || now >= lane.start + lane.len {
                            continue;
                        }
                        let pos = (now - lane.start).rem_euclid(lane.loop_len);
                        let r = lane.route as usize;
                        auto_norm[r] = env_value_at(&lane.pts, pos);
                        auto_set[r] = true;
                        active[r] = true;
                    }
                }

                // LFOs: bipolar offsets on top; free phase integrates even
                // for off LFOs (mirrors the browser loop)
                for lfo in &mut tm.lfos {
                    let raw = if lfo.sync {
                        lfo_shape_value(lfo.shape, now / lfo.div_ticks + lfo.phase)
                    } else {
                        // free-run: integrate the (possibly overridden) rate
                        let hz = if lfo.rated { lfo.eff_hz } else { lfo.hz };
                        lfo.free_phase += dt * hz;
                        if lfo.free_phase > 1e6 {
                            lfo.free_phase -= 1e6;
                        }
                        lfo_shape_value(lfo.shape, lfo.free_phase + lfo.phase)
                    };
                    if !lfo.on || lfo.targets.is_empty() {
                        continue;
                    }
                    for &r in &lfo.targets {
                        lfo_off[r as usize] += raw * lfo.depth;
                        active[r as usize] = true;
                    }
                }
            }

            // master-bus arrangement automation
            if in_arr {
                for lane in master_auto.iter() {
                    let r = lane.route as usize;
                    auto_norm[r] = env_value_at(&lane.pts, now);
                    auto_set[r] = true;
                    active[r] = true;
                }
            }
        }

        // rate overrides live for exactly one combine pass
        for tm in m.tracks.iter_mut() {
            for lfo in tm.lfos.iter_mut() {
                lfo.rated = false;
            }
        }

        // combine + apply; deactivated mappings snap back to their base
        for ri in 0..m.routes.len() {
            let is_active = m.active[ri];
            let was_active = m.prev_active[ri];
            m.prev_active[ri] = is_active;
            let rt = m.routes[ri];
            match rt.apply {
                Apply::LfoRate { track, lfo } => {
                    // no refresh → `rated` stays false → stored Hz next block
                    if is_active {
                        let auto = if m.auto_set[ri] { Some(m.auto_norm[ri]) } else { None };
                        let v = combined_value(&rt, auto, m.lfo_off[ri]);
                        let l = &mut m.tracks[track as usize].lfos[lfo as usize];
                        l.eff_hz = v as f64;
                        l.rated = true;
                    }
                }
                Apply::Midi { ov, stepped } => {
                    let oi = ov as usize;
                    let track = m.midi_ovs[oi].track;
                    if is_active {
                        let auto = if m.auto_set[ri] { Some(m.auto_norm[ri]) } else { None };
                        let v = combined_value(&rt, auto, m.lfo_off[ri]);
                        self.midi_shared.set_value(oi, v);
                        let q = quantize(v, rt.min, rt.max, stepped);
                        if m.midi_quant[oi] != Some(q) {
                            m.midi_quant[oi] = Some(q);
                            self.midi_shared.mark_dirty(track);
                        }
                    } else if was_active {
                        // restoreParam: drop the override, regenerate once
                        self.midi_shared.clear_value(oi);
                        m.midi_quant[oi] = None;
                        self.midi_shared.mark_dirty(track);
                    }
                }
                _ => {
                    if is_active {
                        let auto = if m.auto_set[ri] { Some(m.auto_norm[ri]) } else { None };
                        let v = combined_value(&rt, auto, m.lfo_off[ri]);
                        self.apply_route(&rt, &m.subs, v);
                    } else if was_active {
                        self.apply_route(&rt, &m.subs, rt.base);
                    }
                }
            }
        }
        self.modul = m;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pts(v: &[(f64, f32)]) -> Vec<Pt> {
        v.iter().map(|&(t, v)| Pt { t, v }).collect()
    }

    #[test]
    fn env_value_at_matches_hand_computed_interpolation() {
        let p = pts(&[(0.0, 0.2), (100.0, 1.0), (200.0, 0.0)]);
        // inside the first segment: 0.2 + (1.0−0.2)·(25/100)
        assert!((env_value_at(&p, 25.0) - 0.4).abs() < 1e-6);
        // exactly on a breakpoint
        assert!((env_value_at(&p, 100.0) - 1.0).abs() < 1e-6);
        // before the first / after the last point clamps
        assert_eq!(env_value_at(&p, -5.0), 0.2);
        assert_eq!(env_value_at(&p, 500.0), 0.0);
        // single point is constant
        assert_eq!(env_value_at(&pts(&[(50.0, 0.7)]), 0.0), 0.7);
        // sub-tick segment guards with max(1) like the JS
        let tight = pts(&[(0.0, 0.0), (0.5, 1.0), (10.0, 1.0)]);
        assert!((env_value_at(&tight, 0.25) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn lfo_sync_phase_matches_div_ticks() {
        // rate index 5 = "1/4" = 96 ticks per cycle
        assert_eq!(lfo_div_ticks(5.0), 96.0);
        assert_eq!(lfo_div_ticks(0.0), 3072.0);
        assert_eq!(lfo_div_ticks(99.0), 384.0); // out of range → 1 bar
        // saw-up (shape 2) halfway through a 1/4 cycle at tick 48 reads 0
        let raw = lfo_shape_value(2, 48.0 / lfo_div_ticks(5.0));
        assert!(raw.abs() < 1e-6, "{raw}");
        // quarter cycle at tick 24 reads −0.5
        let raw = lfo_shape_value(2, 24.0 / lfo_div_ticks(5.0));
        assert!((raw + 0.5).abs() < 1e-6, "{raw}");
        // a full cycle later the phase wraps to the same value
        let a = lfo_shape_value(0, 24.0 / 96.0);
        let b = lfo_shape_value(0, (24.0 + 96.0) / 96.0);
        assert!((a - b).abs() < 1e-6);
    }

    #[test]
    fn automation_base_plus_lfo_offset_clamps_at_spec_edges() {
        let rt = Route {
            apply: Apply::Param { target: 0, slot: SLOT_INST, param: 0 },
            min: 0.0,
            max: 10.0,
            log: false,
            base: 5.0,
        };
        // automation at norm 1.0 → 10; +LFO pushes past max → clamps
        assert_eq!(combined_value(&rt, Some(1.0), 1.0), 10.0);
        // automation at norm 0 → 0; −LFO clamps at min
        assert_eq!(combined_value(&rt, Some(0.0), -1.0), 0.0);
        // mid automation + half-range LFO: 5 + 1·(10−0)/2·0.5 = 7.5
        assert!((combined_value(&rt, Some(0.5), 0.5) - 7.5).abs() < 1e-6);
        // no automation: the stored base anchors the swing
        assert!((combined_value(&rt, None, 0.2) - 6.0).abs() < 1e-6);
        // log specs map the normalized base along the exp curve
        let log_rt = Route { min: 80.0, max: 14000.0, log: true, ..rt };
        let mid = combined_value(&log_rt, Some(0.5), 0.0);
        assert!((mid - (80.0f32 * 14000.0).sqrt()).abs() < 1.0, "{mid}");
    }

    fn lfo_json(id: &str, on: bool, sync: bool, hz: f64, shape: f64, tg: (&str, &str, &str)) -> sf_project::project::LfoJSON {
        sf_project::project::LfoJSON {
            id: Some(id.into()),
            on: if on { 1.0 } else { 0.0 },
            shape,
            sync: if sync { 1.0 } else { 0.0 },
            rate: 5.0,
            hz,
            depth: 1.0,
            phase: 0.0,
            dest: tg.0.into(),
            fx_id: tg.1.into(),
            pkey: tg.2.into(),
            targets: None,
        }
    }

    /// One mono track with the given LFOs (no clips — LFO routes are active
    /// whenever the LFO is on, so nothing needs launching).
    fn lfo_project(lfos: Vec<sf_project::project::LfoJSON>) -> sf_project::project::ProjectJSON {
        use sf_project::project::*;
        use std::collections::BTreeMap;
        ProjectJSON {
            meta: MetaJSON {
                title: "t".into(),
                bpm: 120.0,
                swing: 0.0,
                swing_subdivision: None,
                humanize: None,
                root: 0.0,
                scale: "major".into(),
                launch_q: 1.0,
                master_gain: Some(0.0),
                loop_on: None,
                loop_start: None,
                loop_end: None,
                master_auto_open: None,
                master_auto_param: None,
                ts_top: None,
                ts_bottom: None,
            },
            tracks: vec![TrackJSON {
                id: Some("t1".into()),
                name: "Mono".into(),
                color: 0.0,
                kind: TrackKind::Synth,
                inst: InstJSON {
                    r#type: "mono".into(),
                    params: BTreeMap::new(),
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
                lfos: Some(lfos),
                macros: None,
                midifx: None,
                auto: None,
                auto_open: None,
                auto_param: None,
            }],
            scenes: vec![SceneJSON { id: Some("s1".into()), name: "1".into() }],
            clips: BTreeMap::new(),
            arr: BTreeMap::new(),
            returns: None,
            master_fx: None,
            master_auto: None,
        }
    }

    #[test]
    fn lfo_rate_route_drives_target_free_phase_and_reverts_when_stale() {
        use crate::graph::{build_graph, SampleBank};
        use crate::scheduler::identity_expand;
        // a: square (+1 for its whole first half-cycle at 0.001 Hz) → b's Hz
        // b: free-running 1 Hz, no target of its own
        // combined = clamp(1 + 1·1·(30−0.01)/2) ≈ 16 Hz while the route is live
        let a = lfo_json("a", true, false, 0.001, 4.0, ("lfo", "b", "hz"));
        let b = lfo_json("b", true, false, 1.0, 0.0, ("", "", ""));
        let blocks = 100usize; // 100·128/44100 ≈ 0.29 s
        let run = |a_on: bool| {
            let mut a = a.clone();
            a.on = if a_on { 1.0 } else { 0.0 };
            let p = lfo_project(vec![a, b.clone()]);
            let (mut g, _) = build_graph(&p, 44_100.0, identity_expand, &SampleBank::new());
            assert!(!g.modul.routes.is_empty(), "lfo route not resolved");
            assert!(matches!(g.modul.routes[0].apply, Apply::LfoRate { track: 0, lfo: 1 }));
            for _ in 0..blocks {
                g.apply_modulation(0.0, true, Mode::Session, 128);
            }
            g
        };
        let dt = 128.0 / 44_100.0;
        let base = run(false).modul.tracks[0].lfos[1].free_phase;
        assert!((base - blocks as f64 * dt).abs() < 1e-6, "baseline 1 Hz: {base}");
        let mut g = run(true);
        let modded = g.modul.tracks[0].lfos[1].free_phase;
        // first block runs at the stored Hz (one-block lag), the rest at ~16 Hz
        let expect = dt + (blocks - 1) as f64 * dt * (1.0 + 29.99 * 0.5);
        assert!((modded - expect).abs() < 1e-3, "modded {modded} vs {expect}");

        // route goes away (LFO a off): the block right after still carries
        // the override, then the target reverts to its stored 1 Hz
        g.modul.tracks[0].lfos[0].on = false;
        let before = g.modul.tracks[0].lfos[1].free_phase;
        g.apply_modulation(0.0, true, Mode::Session, 128);
        let lagged = g.modul.tracks[0].lfos[1].free_phase - before;
        assert!(lagged > dt * 10.0, "override should survive one block: {lagged}");
        let before = g.modul.tracks[0].lfos[1].free_phase;
        g.apply_modulation(0.0, true, Mode::Session, 128);
        let reverted = g.modul.tracks[0].lfos[1].free_phase - before;
        assert!((reverted - dt).abs() < 1e-9, "did not revert to stored Hz: {reverted}");
    }

    #[test]
    fn lfo_rate_route_leaves_synced_target_alone() {
        use crate::graph::{build_graph, SampleBank};
        use crate::scheduler::identity_expand;
        let a = lfo_json("a", true, false, 0.001, 4.0, ("lfo", "b", "hz"));
        let b = lfo_json("b", true, true, 1.0, 0.0, ("", "", ""));
        let p = lfo_project(vec![a, b]);
        let (mut g, _) = build_graph(&p, 44_100.0, identity_expand, &SampleBank::new());
        for _ in 0..10 {
            g.apply_modulation(0.0, true, Mode::Session, 128);
        }
        let l = &g.modul.tracks[0].lfos[1];
        assert!(l.rated, "override should still be published");
        assert_eq!(l.free_phase, 0.0, "synced LFO must keep its tick division");
        // unknown target id → no route (browser returns null)
        let p = lfo_project(vec![lfo_json("a", true, false, 1.0, 4.0, ("lfo", "nope", "hz"))]);
        let (g, _) = build_graph(&p, 44_100.0, identity_expand, &SampleBank::new());
        assert!(g.modul.routes.is_empty());
    }

    #[test]
    fn session_clip_envelope_drives_mix_gain() {
        use crate::command::Cmd;
        use crate::engine::Engine;
        use sf_project::project::*;
        use std::collections::BTreeMap;

        let build = |env: Option<AutoMap>| -> ProjectJSON {
            let mut notes = BTreeMap::new();
            notes.insert("n1".into(), Note { p: 48.0, s: 0.0, d: 300.0, v: 0.9, pr: 1.0 });
            let mut clips = BTreeMap::new();
            clips.insert(
                "t1|s1".to_string(),
                ClipJSON {
                    name: "c".into(),
                    color: 0.0,
                    len: 384.0,
                    notes,
                    audio: None,
                    env,
                    follow: None,
                },
            );
            ProjectJSON {
                meta: MetaJSON {
                    title: "t".into(),
                    bpm: 120.0,
                    swing: 0.0,
                    swing_subdivision: None,
                    humanize: None,
                    root: 0.0,
                    scale: "major".into(),
                    launch_q: 1.0,
                    master_gain: Some(0.0),
                    loop_on: None,
                    loop_start: None,
                    loop_end: None,
                    master_auto_open: None,
                    master_auto_param: None,
                    ts_top: None,
                    ts_bottom: None,
                },
                tracks: vec![TrackJSON {
                    id: Some("t1".into()),
                    name: "Mono".into(),
                    color: 0.0,
                    kind: TrackKind::Synth,
                    inst: InstJSON {
                        r#type: "mono".into(),
                        params: BTreeMap::new(),
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
        };
        let render = |p: &ProjectJSON| -> f32 {
            let mut e = Engine::new(44_100.0);
            e.load_project(p);
            e.push_cmd(Cmd::ClipLaunch { track: 0, clip: 0 });
            e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
            let n = 44_100usize;
            let mut l = vec![0.0f32; n];
            let mut r = vec![0.0f32; n];
            for (cl, cr) in l.chunks_mut(512).zip(r.chunks_mut(512)) {
                e.process(cl, cr);
            }
            // skip the first 100 ms: the fader smoother ramps from its
            // snapped start value toward the modulated target
            let tail = &l[4_410..];
            (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt()
        };

        let loud = render(&build(None));
        // envelope pins mix gain at normalized 0 → −48 dB fader
        let mut env = std::collections::BTreeMap::new();
        env.insert("mix||gain".to_string(), vec![AutoPoint { t: 0.0, v: 0.0 }]);
        let quiet = render(&build(Some(env)));
        assert!(loud > 1e-4, "baseline silent: {loud}");
        assert!(
            quiet < loud * 0.05,
            "clip envelope did not attenuate: quiet {quiet} vs loud {loud}"
        );
    }
}
