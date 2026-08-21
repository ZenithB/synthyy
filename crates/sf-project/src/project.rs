//! Serde mirror of the FROZEN project JSON contract (`src/state/doc.ts`
//! lines 55-98 + `Note` in `src/types.ts`). Field names, optionality and the
//! map keying (`clips` keyed `"trackId|sceneId"`) match the TypeScript types
//! exactly — never "improve" the schema here. Unknown fields are tolerated on
//! parse (no `deny_unknown_fields`) so newer web projects still open.
//!
//! TS `number` maps to `f64` across the board: the values travel through
//! JSON untyped, and several nominally-integer fields (ticks, colors) may
//! legally carry fractions.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `Record<string, number>` — device/fx parameter maps keyed by schema key.
pub type Params = BTreeMap<String, f64>;

/// `Note` from `src/types.ts`: pitch, start (ticks), duration (ticks),
/// velocity 0..1, probability 0..1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub p: f64,
    pub s: f64,
    pub d: f64,
    pub v: f64,
    pub pr: f64,
}

/// One automation breakpoint: `{ t, v }` (t in ticks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoPoint {
    pub t: f64,
    pub v: f64,
}

/// `paramId → breakpoints` automation map (track `auto` / `masterAuto` / clip `env`).
pub type AutoMap = BTreeMap<String, Vec<AutoPoint>>;

/// `TrackKind` from `src/types.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Synth,
    Drum,
    Audio,
    Bus,
}

/// Built-in send-bus marker (`send: 'A' | 'B' | 'F'`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendBus {
    A,
    B,
    F,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioClipData {
    pub sample_id: String,
    pub sample_name: String,
    pub gain_db: f64,
    pub pitch: f64,
    pub rev: f64,
    pub r#loop: f64,
    pub fade_in: f64,
    pub fade_out: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dur: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cents: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xfade: Option<f64>,
}

/// Clip follow action: `{ on, bars, action, chance }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FollowJSON {
    pub on: bool,
    pub bars: f64,
    pub action: f64,
    pub chance: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipJSON {
    pub name: String,
    pub color: f64,
    pub len: f64,
    pub notes: BTreeMap<String, Note>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioClipData>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<AutoMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow: Option<FollowJSON>,
}

/// Arrangement clip: `ClipJSON & { trackId: string; start: number }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArrClipJSON {
    #[serde(flatten)]
    pub clip: ClipJSON,
    pub track_id: String,
    pub start: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FxJSON {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub r#type: String,
    pub on: bool,
    pub params: Params,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<f64>,
    /// Sidechain trigger source (duck): track id, '' = tempo mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_track: Option<String>,
    /// Pad index within a drum track, -1 = any note on the track.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_pitch: Option<f64>,
}

/// Modulation target address: `{ dest, fxId, pkey }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModTarget {
    pub dest: String,
    pub fx_id: String,
    pub pkey: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LfoJSON {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub on: f64,
    pub shape: f64,
    pub sync: f64,
    pub rate: f64,
    pub hz: f64,
    pub depth: f64,
    pub phase: f64,
    pub dest: String,
    pub fx_id: String,
    pub pkey: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub targets: Option<Vec<ModTarget>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MacroJSON {
    pub name: String,
    pub value: f64,
    pub targets: Vec<ModTarget>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MidiFxJSON {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub r#type: String,
    pub on: bool,
    pub params: Params,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReturnJSON {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub fx_type: String,
    pub params: Params,
    pub gain: f64,
}

/// Track instrument slot (`TrackJSON['inst']`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InstJSON {
    pub r#type: String,
    pub params: Params,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pad_samples: Option<BTreeMap<String, String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pad_names: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackJSON {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub color: f64,
    pub kind: TrackKind,
    pub inst: InstJSON,
    pub fx: Vec<FxJSON>,
    pub gain: f64,
    pub pan: f64,
    pub mute: bool,
    pub solo: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_a: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send_b: Option<f64>,
    /// Routing target for buses: `'master'` | busTrackId.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    /// busTrackId → send level (sends into user buses).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sends: Option<BTreeMap<String, f64>>,
    /// Built-in (e.g. the A/B send buses) — cannot be deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    /// Marks the built-in send buses (id-independent): A, B, or Feedback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub send: Option<SendBus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lfos: Option<Vec<LfoJSON>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub macros: Option<Vec<MacroJSON>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub midifx: Option<Vec<MidiFxJSON>>,
    /// Arrangement automation: paramId ("dest|fxId|pkey") → breakpoints in
    /// absolute song ticks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto: Option<AutoMap>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_open: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_param: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MetaJSON {
    pub title: String,
    pub bpm: f64,
    pub swing: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swing_subdivision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub humanize: Option<f64>,
    pub root: f64,
    pub scale: String,
    pub launch_q: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_gain: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_on: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_end: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_auto_open: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_auto_param: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_top: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts_bottom: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneJSON {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectJSON {
    pub meta: MetaJSON,
    pub tracks: Vec<TrackJSON>,
    pub scenes: Vec<SceneJSON>,
    /// Keyed `"trackId|sceneId"`.
    pub clips: BTreeMap<String, ClipJSON>,
    /// Keyed by arrangement clip id.
    pub arr: BTreeMap<String, ArrClipJSON>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub returns: Option<Vec<ReturnJSON>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_fx: Option<Vec<FxJSON>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub master_auto: Option<AutoMap>,
}

/// The `clips` map key: `` `${trackId}|${sceneId}` `` (doc.ts `clipKey`).
pub fn clip_key(track_id: &str, scene_id: &str) -> String {
    format!("{track_id}|{scene_id}")
}
