//! Tick-domain MIDI effects — exact port of `src/audio/midifx.ts`
//! (applyMidiFx/arpExpand/arpSequence) plus the theory helpers it leans on
//! (`snapToScale`/`getScale` from src/theory.ts). Pure note-list transforms:
//! a clip's notes run through the track's Scale/Chord/Arp/Velocity/Random
//! chain before scheduling, wherever `identity_expand` used to run (session
//! patterns, arrangement clips, offline render).
//!
//! Determinism: the browser rolls `Math.random()` for the velo/rand devices
//! and the arp's Rand direction. Here every roll comes from a [`XorShift`]
//! seeded per expansion from the track id, so repeated renders of the same
//! project are bit-identical (browser parity of the individual rolls is
//! neither possible nor required — fixtures compare spectra).

use sf_project::project::{MidiFxJSON, Note, TrackJSON, TrackKind};

use crate::scheduler::{NoteEv, XorShift};

/// Arp step length in ticks per `rate` index (schema.ts `ARP_DIV_TICKS`:
/// 1/4, 1/8, 1/8T, 1/16, 1/16T, 1/32 at PPQ 96).
pub const ARP_DIV_TICKS: [f64; 6] = [96.0, 48.0, 32.0, 24.0, 16.0, 12.0];

const BAR: f64 = 384.0;

/// src/theory.ts `SCALES` (interval sets by id; unknown ids fall back to major).
static SCALES: [(&str, &[i32]); 8] = [
    ("major", &[0, 2, 4, 5, 7, 9, 11]),
    ("minor", &[0, 2, 3, 5, 7, 8, 10]),
    ("dorian", &[0, 2, 3, 5, 7, 9, 10]),
    ("mixo", &[0, 2, 4, 5, 7, 9, 10]),
    ("pentMaj", &[0, 2, 4, 7, 9]),
    ("pentMin", &[0, 3, 5, 7, 10]),
    ("harmMin", &[0, 2, 3, 5, 7, 8, 11]),
    ("blues", &[0, 3, 5, 6, 7, 10]),
];

fn scale_ivs(id: &str) -> &'static [i32] {
    SCALES
        .iter()
        .find(|(s, _)| *s == id)
        .map(|(_, ivs)| *ivs)
        .unwrap_or(SCALES[0].1)
}

fn in_scale(pitch: f64, root: f64, ivs: &[i32]) -> bool {
    let rel = (pitch - root).rem_euclid(12.0);
    ivs.iter().any(|&iv| rel == iv as f64)
}

/// theory.ts `snapToScale`: nearest scale tone, ties resolve downward.
fn snap_to_scale(pitch: f64, root: f64, ivs: &[i32]) -> f64 {
    if in_scale(pitch, root, ivs) {
        return pitch;
    }
    for off in 1..=6 {
        if in_scale(pitch - off as f64, root, ivs) {
            return pitch - off as f64;
        }
        if in_scale(pitch + off as f64, root, ivs) {
            return pitch + off as f64;
        }
    }
    pitch
}

/// JS `x | 0` (truncate toward zero).
#[inline]
fn js_int(v: f64) -> i64 {
    v.trunc() as i64
}

#[inline]
fn param(fx: &MidiFxJSON, key: &str, def: f64) -> f64 {
    fx.params.get(key).copied().unwrap_or(def)
}

#[inline]
fn clampf(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

/// midifx.ts `arpSequence`: the ascending pitch set the arp cycles through
/// before direction is applied. See the source for the `pat` semantics
/// (0 Octave, 1 5ths, 2 Scale walk, 3 Select bitmask).
fn arp_sequence(
    held: &[f64],
    pat: i64,
    oct: f64,
    sel: i64,
    root: f64,
    ivs: &[i32],
) -> Vec<f64> {
    if pat == 2 {
        let s0 = snap_to_scale(held[0], root, ivs);
        let rel0 = (s0 - root).rem_euclid(12.0);
        let deg = ivs.iter().position(|&iv| iv as f64 == rel0).unwrap_or(0);
        let base_root = s0 - rel0; // the scale root in s0's octave
        let count = ivs.len() as f64 * oct;
        let mut seq = Vec::new();
        let mut k = 0usize;
        while (k as f64) < count {
            let idx = deg + k;
            seq.push(
                base_root
                    + ivs[idx % ivs.len()] as f64
                    + 12.0 * (idx / ivs.len()) as f64,
            );
            k += 1;
        }
        return seq;
    }
    let mut offsets: Vec<f64> = match pat {
        1 => vec![0.0, 7.0],
        3 => (0..12)
            .filter(|i| sel & (1 << i) != 0)
            .map(|i| i as f64)
            .collect(),
        _ => vec![0.0],
    };
    if offsets.is_empty() {
        offsets.push(0.0); // Select with nothing on → the note itself
    }
    let mut one = Vec::with_capacity(held.len() * offsets.len());
    for &h in held {
        for &o in &offsets {
            one.push(h + o);
        }
    }
    let mut seq = Vec::new();
    let mut o = 0usize;
    while (o as f64) < oct {
        for &pp in &one {
            seq.push(pp + o as f64 * 12.0);
        }
        o += 1;
    }
    // JS `[...new Set(seq)].sort(asc)` — dedup exact values, ascending
    seq.sort_by(|a, b| a.total_cmp(b));
    seq.dedup();
    seq
}

/// midifx.ts `arpExpand`. Notes sharing a start tick form one "held" group;
/// each group is replaced by a stepped run of the arp sequence.
fn arp_expand(
    notes: Vec<Note>,
    fx: &MidiFxJSON,
    _loop_len: f64,
    root: f64,
    ivs: &[i32],
    rng: &mut XorShift,
) -> Vec<Note> {
    if notes.is_empty() {
        return notes;
    }
    let rate = js_int(param(fx, "rate", 0.0)).clamp(0, ARP_DIV_TICKS.len() as i64 - 1);
    let step = ARP_DIV_TICKS[rate as usize];
    let mode = js_int(param(fx, "mode", 0.0)); // 0 up · 1 down · 2 up-down · 3 random
    let pat = js_int(param(fx, "pat", 0.0));
    let oct = param(fx, "oct", 1.0).max(1.0);
    let gate = param(fx, "gate", 0.8);
    let sel = js_int(param(fx, "sel", 1.0));

    // group by exact start tick, first-seen order (JS Map insertion order)
    let mut groups: Vec<(f64, Vec<Note>)> = Vec::new();
    for n in notes {
        match groups.iter_mut().find(|(s, _)| *s == n.s) {
            Some((_, g)) => g.push(n),
            None => groups.push((n.s, vec![n])),
        }
    }

    let mut out = Vec::new();
    for (_, g) in &groups {
        let end = g
            .iter()
            .map(|n| n.s + n.d)
            .fold(f64::NEG_INFINITY, f64::max);
        let mut held: Vec<f64> = g.iter().map(|n| n.p).collect();
        held.sort_by(|a, b| a.total_cmp(b));
        held.dedup();
        let mut seq = arp_sequence(&held, pat, oct, sel, root, ivs);
        if mode == 1 {
            seq.reverse();
        } else if mode == 2 && seq.len() > 2 {
            let mut tail: Vec<f64> = seq[1..seq.len() - 1].to_vec();
            tail.reverse();
            seq.extend(tail);
        }
        let mut i = 0usize;
        let mut t = g[0].s;
        while t < end {
            let pitch = if mode == 3 {
                seq[(rng.next_f64() * seq.len() as f64) as usize % seq.len()]
            } else {
                seq[i % seq.len()]
            };
            out.push(Note {
                p: clampf(pitch, 0.0, 127.0),
                s: t,
                d: (step * gate).max(6.0),
                v: g[0].v,
                pr: g[0].pr,
            });
            i += 1;
            t += step;
        }
    }
    out
}

/// midifx.ts `applyMidiFx`: run a note list through a MIDI-fx chain.
/// Disabled devices are skipped; unknown types pass through; scale/chord/arp
/// don't touch drum tracks (velo/rand do).
pub fn apply_midi_fx(
    chain: &[MidiFxJSON],
    notes: Vec<Note>,
    loop_len: f64,
    root: f64,
    scale: &str,
    is_drum: bool,
    rng: &mut XorShift,
) -> Vec<Note> {
    if chain.is_empty() {
        return notes;
    }
    let ivs = scale_ivs(scale);
    let mut out = notes;
    for d in chain {
        if !d.on {
            continue;
        }
        match d.r#type.as_str() {
            "scale" if !is_drum => {
                for n in &mut out {
                    n.p = snap_to_scale(n.p, root, ivs);
                }
            }
            "chord" if !is_drum => {
                // [0, i1, i2, i3] keeping the root and dropping zero intervals
                let mut iv_list = vec![0.0];
                for k in ["i1", "i2", "i3"] {
                    let v = param(d, k, 0.0);
                    if v != 0.0 {
                        iv_list.push(v);
                    }
                }
                let mut next = Vec::with_capacity(out.len() * iv_list.len());
                for n in &out {
                    for &iv in &iv_list {
                        next.push(Note {
                            p: clampf(n.p + iv, 0.0, 127.0),
                            v: if iv == 0.0 { n.v } else { n.v * 0.85 },
                            ..n.clone()
                        });
                    }
                }
                out = next;
            }
            "velo" => {
                let s = param(d, "scale", 1.0);
                let r = param(d, "rand", 0.0);
                for n in &mut out {
                    n.v = clampf(n.v * s + (rng.next_f64() * 2.0 - 1.0) * r, 0.05, 1.0);
                }
            }
            "rand" => {
                let ch = param(d, "chance", 1.0);
                let oc = param(d, "octave", 0.0);
                let mut next = Vec::with_capacity(out.len());
                for n in out {
                    if ch < 1.0 && rng.next_f64() > ch {
                        continue;
                    }
                    let mut pitch = n.p;
                    if oc > 0.0 && rng.next_f64() < oc {
                        pitch = clampf(
                            pitch + if rng.next_f64() < 0.5 { 12.0 } else { -12.0 },
                            0.0,
                            127.0,
                        );
                    }
                    next.push(Note { p: pitch, ..n });
                }
                out = next;
            }
            "arp" if !is_drum => {
                let ll = if loop_len != 0.0 { loop_len } else { BAR };
                out = arp_expand(out, d, ll, root, ivs, rng);
            }
            _ => {} // unknown type / drum-skipped device: pass through
        }
    }
    out
}

/// FNV-1a over the track identity — a stable per-track RNG seed so the
/// velo/rand rolls decorrelate between tracks yet repeat across renders.
fn track_seed(track: &TrackJSON) -> u64 {
    let id = track.id.as_deref().unwrap_or(&track.name);
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// [`crate::scheduler::ExpandMidi`] implementation: `track.midifx` applied to
/// the clip's notes, then flattened to sorted scheduler events.
pub fn expand_midi(
    track: &TrackJSON,
    notes: &[Note],
    loop_len: f64,
    root: f64,
    scale: &str,
) -> Vec<NoteEv> {
    let chain = track.midifx.as_deref().unwrap_or(&[]);
    let mut rng = XorShift::new(track_seed(track));
    let expanded = apply_midi_fx(
        chain,
        notes.to_vec(),
        loop_len,
        root,
        scale,
        track.kind == TrackKind::Drum,
        &mut rng,
    );
    let mut evs: Vec<NoteEv> = expanded
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn note(p: f64, s: f64, d: f64, v: f64) -> Note {
        Note { p, s, d, v, pr: 1.0 }
    }

    fn fx(ty: &str, params: &[(&str, f64)]) -> MidiFxJSON {
        MidiFxJSON {
            id: None,
            r#type: ty.into(),
            on: true,
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), *v))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn run(chain: &[MidiFxJSON], notes: Vec<Note>, is_drum: bool) -> Vec<Note> {
        let mut rng = XorShift::new(42);
        apply_midi_fx(chain, notes, 384.0, 0.0, "major", is_drum, &mut rng)
    }

    #[test]
    fn arp_expands_to_more_events_than_input() {
        // one whole-bar note at 1/16 rate → 384/24 = 16 steps
        let chain = [fx(
            "arp",
            &[("rate", 3.0), ("mode", 0.0), ("pat", 0.0), ("oct", 2.0), ("gate", 0.6)],
        )];
        let out = run(&chain, vec![note(60.0, 0.0, 384.0, 0.8)], false);
        assert_eq!(out.len(), 16);
        assert!(out.len() > 1);
        // Octave pattern over 2 octaves alternates 60/72
        assert_eq!(out[0].p, 60.0);
        assert_eq!(out[1].p, 72.0);
        assert_eq!(out[2].p, 60.0);
        // gate: 24 · 0.6 = 14.4 ticks
        assert!((out[0].d - 14.4).abs() < 1e-9);
    }

    #[test]
    fn arp_up_down_folds_the_sequence() {
        let chain = [fx(
            "arp",
            &[("rate", 3.0), ("mode", 2.0), ("pat", 0.0), ("oct", 3.0), ("gate", 1.0)],
        )];
        let out = run(&chain, vec![note(60.0, 0.0, 384.0, 0.8)], false);
        // seq [60,72,84] → up-down [60,72,84,72]
        let pitches: Vec<f64> = out.iter().take(4).map(|n| n.p).collect();
        assert_eq!(pitches, vec![60.0, 72.0, 84.0, 72.0]);
    }

    #[test]
    fn chord_stacks_intervals() {
        let chain = [fx("chord", &[("i1", 3.0), ("i2", 7.0), ("i3", 0.0)])];
        let out = run(&chain, vec![note(60.0, 0.0, 96.0, 0.8)], false);
        assert_eq!(out.len(), 3); // root + two non-zero intervals
        assert_eq!(out[0].p, 60.0);
        assert_eq!(out[1].p, 63.0);
        assert_eq!(out[2].p, 67.0);
        assert!((out[0].v - 0.8).abs() < 1e-12);
        assert!((out[1].v - 0.68).abs() < 1e-12); // stacked notes at 85%
    }

    #[test]
    fn velo_scales_velocities() {
        let chain = [fx("velo", &[("scale", 0.5), ("rand", 0.0)])];
        let out = run(&chain, vec![note(60.0, 0.0, 96.0, 0.8)], false);
        assert!((out[0].v - 0.4).abs() < 1e-12);
        // floor at 0.05
        let quiet = run(&[fx("velo", &[("scale", 0.0), ("rand", 0.0)])], vec![note(60.0, 0.0, 96.0, 0.8)], false);
        assert_eq!(quiet[0].v, 0.05);
    }

    #[test]
    fn scale_snaps_off_scale_pitch() {
        // C# (61) in C major snaps down to C (60) — ties resolve downward
        let chain = [fx("scale", &[])];
        let out = run(&chain, vec![note(61.0, 0.0, 96.0, 0.8)], false);
        assert_eq!(out[0].p, 60.0);
        // in-scale pitches untouched
        let kept = run(&chain, vec![note(64.0, 0.0, 96.0, 0.8)], false);
        assert_eq!(kept[0].p, 64.0);
    }

    #[test]
    fn drum_tracks_skip_pitch_devices_but_not_velo() {
        let chain = [
            fx("scale", &[]),
            fx("chord", &[("i1", 4.0), ("i2", 7.0), ("i3", 0.0)]),
            fx("arp", &[("rate", 3.0), ("mode", 0.0), ("pat", 0.0), ("oct", 1.0), ("gate", 0.8)]),
            fx("velo", &[("scale", 0.5), ("rand", 0.0)]),
        ];
        let out = run(&chain, vec![note(61.0, 0.0, 96.0, 0.8)], true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].p, 61.0);
        assert!((out[0].v - 0.4).abs() < 1e-12);
    }

    #[test]
    fn disabled_and_unknown_devices_pass_through() {
        let mut off = fx("chord", &[("i1", 4.0), ("i2", 7.0), ("i3", 0.0)]);
        off.on = false;
        let chain = [off, fx("wobble", &[])];
        let out = run(&chain, vec![note(60.0, 0.0, 96.0, 0.8)], false);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].p, 60.0);
    }

    #[test]
    fn rand_chance_zero_drops_everything_and_is_deterministic() {
        let chain = [fx("rand", &[("chance", 0.0), ("octave", 0.0)])];
        let out = run(&chain, vec![note(60.0, 0.0, 96.0, 0.8), note(62.0, 96.0, 96.0, 0.8)], false);
        assert!(out.is_empty());

        let roll = |seed: u64| {
            let chain = [fx("rand", &[("chance", 0.5), ("octave", 0.5)])];
            let notes: Vec<Note> = (0..32).map(|i| note(60.0, i as f64 * 12.0, 12.0, 0.8)).collect();
            let mut rng = XorShift::new(seed);
            apply_midi_fx(&chain, notes, 384.0, 0.0, "major", false, &mut rng)
                .iter()
                .map(|n| (n.p, n.s))
                .collect::<Vec<_>>()
        };
        assert_eq!(roll(7), roll(7));
        assert_ne!(roll(7), roll(8));
    }

    #[test]
    fn expand_midi_sorts_and_applies_track_chain() {
        let track = TrackJSON {
            id: Some("t1".into()),
            name: "T".into(),
            color: 0.0,
            kind: TrackKind::Synth,
            inst: sf_project::project::InstJSON {
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
            midifx: Some(vec![fx("chord", &[("i1", 12.0), ("i2", 0.0), ("i3", 0.0)])]),
            auto: None,
            auto_open: None,
            auto_param: None,
        };
        let evs = expand_midi(
            &track,
            &[note(60.0, 96.0, 48.0, 0.8), note(48.0, 0.0, 48.0, 0.8)],
            384.0,
            0.0,
            "major",
        );
        assert_eq!(evs.len(), 4);
        assert!(evs.windows(2).all(|w| w[0].tick <= w[1].tick));
        assert_eq!(evs[0].pitch, 48);
        assert_eq!(evs[1].pitch, 60);
    }
}
