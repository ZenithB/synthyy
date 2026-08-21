//! ProjectJSON round-trip tests against real fixtures: the render fixture
//! (`fixtures/projects/smoke-mono.json`), the web app's DEFAULT_PROJECT
//! (extracted by `node scripts/dump-default-project.mjs`), and a hand-built
//! maximal project exercising every optional field of the frozen schema.

use sf_project::project::*;
use sf_project::BAR;

fn round_trip(p: &ProjectJSON) -> ProjectJSON {
    let json = serde_json::to_string(p).expect("serialize");
    serde_json::from_str(&json).expect("re-parse")
}

#[test]
fn smoke_mono_round_trips() {
    let raw = include_str!("../../../fixtures/projects/smoke-mono.json");
    let fixture: serde_json::Value = serde_json::from_str(raw).unwrap();
    let p: ProjectJSON = serde_json::from_value(fixture["project"].clone()).unwrap();

    assert_eq!(p.meta.title, "fixture smoke-mono");
    assert_eq!(p.meta.bpm, 120.0);
    assert_eq!(p.meta.swing_subdivision.as_deref(), Some("16n"));
    assert_eq!(p.tracks.len(), 1);
    assert_eq!(p.tracks[0].kind, TrackKind::Synth);
    assert_eq!(p.tracks[0].inst.r#type, "mono");
    let clip = &p.clips[&clip_key("t1", "s1")];
    assert_eq!(clip.len, BAR as f64);
    assert_eq!(clip.notes.len(), 4);
    assert_eq!(clip.notes["n1"].p, 48.0);
    assert_eq!(clip.notes["n2"].s, 96.0);
    assert!(p.arr.is_empty());
    assert_eq!(p.returns.as_deref(), Some(&[][..]));
    assert_eq!(p.master_fx.as_deref(), Some(&[][..]));
    assert!(p.master_auto.is_none());

    assert_eq!(round_trip(&p), p);
}

#[test]
fn default_project_round_trips() {
    let raw = include_str!("assets/default-project.json");
    let p: ProjectJSON = serde_json::from_str(raw).unwrap();

    assert_eq!(p.meta.title, "Untitled Jam");
    assert_eq!(p.tracks.len(), 7); // 4 tracks + busA/busB/busF
    assert_eq!(p.scenes.len(), 6);
    let bus_f = p.tracks.iter().find(|t| t.id.as_deref() == Some("busF")).unwrap();
    assert_eq!(bus_f.kind, TrackKind::Bus);
    assert_eq!(bus_f.send, Some(SendBus::F));
    assert_eq!(bus_f.locked, Some(true));
    assert_eq!(bus_f.output.as_deref(), Some("master"));
    assert_eq!(bus_f.sends.as_ref().unwrap()["busF"], 0.3);
    let drums = &p.tracks[0];
    assert_eq!(drums.inst.params["p0_decay"], 0.8);
    assert_eq!(drums.fx[0].r#type, "comp");
    assert_eq!(drums.fx[0].params["thresh"], -18.0);

    assert_eq!(round_trip(&p), p);
}

#[test]
fn maximal_project_round_trips_every_optional_field() {
    let raw = include_str!("assets/maximal-project.json");
    let p: ProjectJSON = serde_json::from_str(raw).unwrap();

    // meta: every optional present
    let m = &p.meta;
    assert_eq!(m.swing_subdivision.as_deref(), Some("8n"));
    assert_eq!(m.humanize, Some(0.25));
    assert_eq!(m.master_gain, Some(-1.5));
    assert_eq!(m.loop_on, Some(true));
    assert_eq!(m.loop_start, Some(384.0));
    assert_eq!(m.loop_end, Some(1536.0));
    assert_eq!(m.master_auto_open, Some(true));
    assert_eq!(m.master_auto_param.as_deref(), Some("master||gain"));
    assert_eq!(m.ts_top, Some(7.0));
    assert_eq!(m.ts_bottom, Some(8.0));

    // track: every optional present
    let t = &p.tracks[0];
    assert_eq!(t.inst.sample_id.as_deref(), Some("smp01"));
    assert_eq!(t.inst.out, Some(3.0));
    assert_eq!(t.inst.pad_samples.as_ref().unwrap()["7"], "smpCrash");
    assert_eq!(t.inst.pad_names.as_ref().unwrap()["0"], "Kick X");
    assert_eq!(t.send_a, Some(0.4));
    assert_eq!(t.send_b, Some(0.1));
    assert_eq!(t.output.as_deref(), Some("busX"));
    assert_eq!(t.sends.as_ref().unwrap()["busX"], 0.5);
    assert_eq!(t.locked, Some(true));
    assert_eq!(t.send, Some(SendBus::F));
    assert_eq!(t.auto_open, Some(true));
    assert_eq!(t.auto_param.as_deref(), Some("mix||gain"));
    assert_eq!(t.auto.as_ref().unwrap()["mix||gain"][1].v, -12.5);

    // fx sidechain source
    let fx = &t.fx[0];
    assert_eq!(fx.id.as_deref(), Some("fx1"));
    assert_eq!(fx.out, Some(1.0));
    assert_eq!(fx.src_track.as_deref(), Some("tDrums"));
    assert_eq!(fx.src_pitch, Some(-1.0));

    // lfo with multi-targets, macro, midi fx
    let lfo = &t.lfos.as_ref().unwrap()[0];
    assert_eq!(lfo.fx_id, "fx1");
    assert_eq!(lfo.targets.as_ref().unwrap()[0].pkey, "cutoff");
    assert_eq!(t.macros.as_ref().unwrap()[0].targets[0].dest, "fx");
    assert_eq!(t.midifx.as_ref().unwrap()[0].r#type, "arp");

    // scene without id
    assert_eq!(p.scenes[1].id, None);

    // clip: audio (all optionals) + env + follow
    let c = &p.clips["tFull|s1"];
    let a = c.audio.as_ref().unwrap();
    assert_eq!(a.r#loop, 0.0);
    assert_eq!(a.offset, Some(0.125));
    assert_eq!(a.dur, Some(1.75));
    assert_eq!(a.cents, Some(-35.0));
    assert_eq!(a.xfade, Some(0.01));
    assert_eq!(c.env.as_ref().unwrap()["mix||gain"].len(), 2);
    assert_eq!(c.follow.as_ref().unwrap().chance, 0.85);
    assert_eq!(c.notes["n2"].pr, 0.6);

    // arrangement clip: flattened ClipJSON + trackId/start
    let arr = &p.arr["a1"];
    assert_eq!(arr.track_id, "tFull");
    assert_eq!(arr.start, 1152.0);
    assert_eq!(arr.clip.name, "Arr Clip");

    // returns / masterFx / masterAuto
    assert_eq!(p.returns.as_ref().unwrap()[0].fx_type, "reverb");
    assert_eq!(p.returns.as_ref().unwrap()[0].gain, -6.0);
    assert!(!p.master_fx.as_ref().unwrap()[0].on);
    assert_eq!(p.master_auto.as_ref().unwrap()["master||gain"][1].v, -3.0);

    assert_eq!(round_trip(&p), p);

    // Serialized form keeps the frozen field names (spot-check renames).
    let v: serde_json::Value = serde_json::to_value(&p).unwrap();
    assert!(v["meta"].get("launchQ").is_some());
    assert!(v["meta"].get("tsTop").is_some());
    assert!(v["tracks"][0].get("sendA").is_some());
    assert!(v["tracks"][0]["fx"][0].get("srcTrack").is_some());
    assert!(v["tracks"][0]["inst"].get("padSamples").is_some());
    assert!(v["tracks"][0]["lfos"][0].get("fxId").is_some());
    assert!(v["clips"]["tFull|s1"]["audio"].get("gainDb").is_some());
    assert!(v["clips"]["tFull|s1"]["audio"].get("loop").is_some());
    assert!(v["arr"]["a1"].get("trackId").is_some());
    assert_eq!(v["tracks"][0]["send"], "F");
    assert_eq!(v["tracks"][0]["kind"], "synth");
    assert!(v.get("masterFx").is_some());
}

#[test]
fn absent_optionals_stay_absent_and_unknown_fields_are_tolerated() {
    // Minimal project + fields the Rust types don't know about (forward compat).
    let raw = r#"{
        "meta": { "title": "T", "bpm": 100, "swing": 0, "root": 0, "scale": "major", "launchQ": 1,
                  "someFutureMetaField": 42 },
        "tracks": [{
            "name": "X", "color": 1, "kind": "audio",
            "inst": { "type": "audiobus", "params": {}, "futureInstField": true },
            "fx": [], "gain": 0, "pan": 0, "mute": false, "solo": false,
            "futureTrackField": "yes"
        }],
        "scenes": [],
        "clips": {},
        "arr": {},
        "futureTopLevel": {}
    }"#;
    let p: ProjectJSON = serde_json::from_str(raw).unwrap();
    assert_eq!(p.tracks[0].kind, TrackKind::Audio);

    let v: serde_json::Value = serde_json::to_value(&p).unwrap();
    // Optional fields that were absent must not be emitted (no nulls).
    assert!(v["meta"].get("humanize").is_none());
    assert!(v["meta"].get("swingSubdivision").is_none());
    assert!(v["tracks"][0].get("id").is_none());
    assert!(v["tracks"][0].get("sendA").is_none());
    assert!(v["tracks"][0].get("lfos").is_none());
    assert!(v.get("returns").is_none());
    assert!(v.get("masterAuto").is_none());
}
