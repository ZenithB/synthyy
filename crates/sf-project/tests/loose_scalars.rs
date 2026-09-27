//! Regression: the frozen schema's TS types are hints, not guarantees — the
//! Yjs doc stores `any`. `addLfo()` writes `on: true` (boolean) while the LFO
//! power button writes `on: 0|1` (number), yet `LfoJSON.on` is declared
//! `number`. A strict f64 rejected the whole ProjectJSON, which made
//! `project_load` fail and left the desktop app with NO AUDIO AT ALL.

use sf_project::project::ProjectJSON;

fn project_with_lfo_on(value: &str) -> String {
    format!(r#"{{
      "meta": {{ "title": "t", "bpm": 120, "swing": 0, "root": 9, "scale": "minor", "launchQ": 1 }},
      "tracks": [{{
        "name": "T", "color": 1, "kind": "synth",
        "inst": {{ "type": "mono", "params": {{}} }},
        "fx": [], "gain": 0, "pan": 0, "mute": false, "solo": false,
        "lfos": [{{ "on": {value}, "shape": 0, "sync": true, "rate": 5, "hz": 1,
                    "depth": 0.5, "phase": 0, "dest": "", "fxId": "", "pkey": "" }}]
      }}],
      "scenes": [], "clips": {{}}, "arr": {{}}
    }}"#)
}

#[test]
fn lfo_on_accepts_boolean_and_number() {
    for (raw, want) in [("true", 1.0), ("false", 0.0), ("1", 1.0), ("0", 0.0)] {
        let p: ProjectJSON = serde_json::from_str(&project_with_lfo_on(raw))
            .unwrap_or_else(|e| panic!("lfo on={raw} must parse: {e}"));
        let lfo = &p.tracks[0].lfos.as_ref().unwrap()[0];
        assert_eq!(lfo.on, want, "on={raw}");
        assert_eq!(lfo.sync, 1.0, "sync:true normalises to 1.0");
    }
}

#[test]
fn booleans_accept_numeric_spellings() {
    let raw = r#"{
      "meta": { "title": "t", "bpm": 120, "swing": 0, "root": 9, "scale": "minor", "launchQ": 1 },
      "tracks": [{
        "name": "T", "color": 1, "kind": "synth",
        "inst": { "type": "mono", "params": {} },
        "fx": [{ "type": "delay", "on": 1, "params": {} }],
        "gain": 0, "pan": 0, "mute": 1, "solo": 0
      }],
      "scenes": [], "clips": {}, "arr": {}
    }"#;
    let p: ProjectJSON = serde_json::from_str(raw).expect("numeric booleans must parse");
    assert!(p.tracks[0].mute);
    assert!(!p.tracks[0].solo);
    assert!(p.tracks[0].fx[0].on);
}
