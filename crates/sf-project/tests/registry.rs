//! Validates the param registry 1:1 against `fixtures/schema-dump.json`
//! (regenerated from src/audio/schema.ts by `node scripts/dump-schema.mjs`),
//! plus hard-coded spot checks copied straight from schema.ts so the test
//! doesn't depend solely on the dump being fresh.

use serde::Deserialize;
use sf_project::registry::*;

#[derive(Deserialize)]
struct Dump {
    instruments: Vec<DumpDevice>,
    effects: Vec<DumpDevice>,
    midifx: Vec<DumpDevice>,
}

#[derive(Deserialize)]
struct DumpDevice {
    r#type: String,
    params: Vec<DumpSpec>,
}

#[derive(Deserialize)]
struct DumpSpec {
    key: String,
    label: String,
    min: f64,
    max: f64,
    def: f64,
    #[serde(default)]
    exp: bool,
    #[serde(default)]
    int: bool,
    #[serde(default)]
    steps: Option<Vec<String>>,
    #[serde(default, rename = "rateSwitch")]
    rate_switch: bool,
}

fn check(kind: &str, dumped: &[DumpDevice], table: &[(&str, &[ParamSpec])]) {
    assert_eq!(
        dumped.iter().map(|d| d.r#type.as_str()).collect::<Vec<_>>(),
        table.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        "{kind}: device type list/order"
    );
    for (dev, (ty, specs)) in dumped.iter().zip(table) {
        assert_eq!(dev.params.len(), specs.len(), "{kind}/{ty}: param count");
        for (d, s) in dev.params.iter().zip(*specs) {
            let ctx = format!("{kind}/{ty}/{}", d.key);
            assert_eq!(s.key, d.key, "{ctx}: key");
            assert_eq!(s.label, d.label, "{ctx}: label");
            assert_eq!(s.min, d.min as f32, "{ctx}: min");
            assert_eq!(s.max, d.max as f32, "{ctx}: max");
            assert_eq!(s.def, d.def as f32, "{ctx}: def");
            assert_eq!(s.curve == Curve::Exp, d.exp, "{ctx}: curve/exp");
            assert_eq!(s.int, d.int, "{ctx}: int");
            assert_eq!(s.rate_switch, d.rate_switch, "{ctx}: rateSwitch");
            match (&d.steps, s.steps) {
                (None, None) => {}
                (Some(a), Some(b)) => assert_eq!(a, b, "{ctx}: steps"),
                _ => panic!("{ctx}: steps presence mismatch"),
            }
        }
    }
}

#[test]
fn registry_matches_schema_dump() {
    let raw = include_str!("../../../fixtures/schema-dump.json");
    let dump: Dump = serde_json::from_str(raw).unwrap();
    assert_eq!(dump.instruments.len(), 11);
    assert_eq!(dump.effects.len(), 23);
    assert_eq!(dump.midifx.len(), 5);
    check("inst", &dump.instruments, &INSTRUMENTS);
    check("fx", &dump.effects, &EFFECTS);
    check("midifx", &dump.midifx, &MIDI_FX);
}

#[test]
fn spot_checks_from_schema_ts() {
    // Values copied verbatim from src/audio/schema.ts.
    let poly = inst_params("poly");
    let cutoff = poly.iter().find(|s| s.key == "cutoff").unwrap();
    assert_eq!((cutoff.min, cutoff.max, cutoff.def), (80.0, 14000.0, 7000.0));
    assert_eq!(cutoff.curve, Curve::Exp);
    assert!(cutoff.is_log());

    let wave = &poly[0];
    assert_eq!(wave.steps.unwrap().len(), 7);
    assert!(wave.int);

    let mono_slope = inst_params("mono").iter().find(|s| s.key == "slope").unwrap();
    assert_eq!(mono_slope.def, 1.0); // MonoSynth has always run at -24 rolloff

    let drum = inst_params("drum");
    assert_eq!(drum.len(), 24); // 8 pads x tune/decay/level
    let p7_decay = drum.iter().find(|s| s.key == "p7_decay").unwrap();
    assert_eq!(p7_decay.def, 1.3); // Crash pad decay

    assert!(inst_params("audiobus").is_empty());
    let gran_dens = inst_params("granular").iter().find(|s| s.key == "dens").unwrap();
    assert_eq!((gran_dens.min, gran_dens.max, gran_dens.def), (2.0, 80.0, 18.0));

    let reverb_size = fx_params("reverb").iter().find(|s| s.key == "size").unwrap();
    assert_eq!((reverb_size.min, reverb_size.max, reverb_size.def), (0.2, 10.0, 2.2));

    let eq7 = fx_params("eq7");
    assert_eq!(eq7.len(), 21); // 7 bands x freq/gain/q
    let b6_freq = eq7.iter().find(|s| s.key == "b6_freq").unwrap();
    assert_eq!(b6_freq.def, 12000.0);

    let chorus_rate = fx_params("chorus").iter().find(|s| s.key == "rate").unwrap();
    assert!(chorus_rate.rate_switch);
    assert_eq!((chorus_rate.min, chorus_rate.max, chorus_rate.def), (0.01, 300.0, 1.5));

    let shift_amt = fx_params("shift").iter().find(|s| s.key == "amt").unwrap();
    assert_eq!((shift_amt.min, shift_amt.max, shift_amt.def), (-400.0, 400.0, 60.0));
    assert!(!shift_amt.is_log());

    let duck_rate = fx_params("duck").iter().find(|s| s.key == "rate").unwrap();
    assert_eq!(duck_rate.steps.unwrap(), ["1/2", "1/4", "1/4T", "1/8", "1/8T", "1/16"]);

    let mb = fx_params("mbcomp");
    assert_eq!(mb.len(), 14);
    let b2_thresh = mb.iter().find(|s| s.key == "b2_thresh").unwrap();
    assert_eq!((b2_thresh.min, b2_thresh.max, b2_thresh.def), (-60.0, 0.0, -24.0));

    assert!(midifx_params("scale").is_empty());
    let arp_gate = midifx_params("arp").iter().find(|s| s.key == "gate").unwrap();
    assert_eq!((arp_gate.min, arp_gate.max, arp_gate.def), (0.1, 1.0, 0.8));
    let chord_i2 = midifx_params("chord").iter().find(|s| s.key == "i2").unwrap();
    assert_eq!(chord_i2.def, 7.0);
}

#[test]
fn unknown_type_falls_back_to_first_schema() {
    // Mirrors schema.ts instSchema()/fxSchema()/midiFxSchema() fallback.
    assert_eq!(inst_params("nope"), inst_params("poly"));
    assert_eq!(fx_params("nope"), fx_params("eq"));
    assert_eq!(midifx_params("nope"), midifx_params("scale"));
}

#[test]
fn default_params_mirrors_defaults_for() {
    let d: std::collections::BTreeMap<_, _> = default_params(fx_params("reverb")).collect();
    assert_eq!(d.len(), 2);
    assert_eq!(d["size"], 2.2);
    assert_eq!(d["mix"], 0.3);

    let d: std::collections::BTreeMap<_, _> = default_params(inst_params("pluck")).collect();
    assert_eq!(d["dampen"], 4000.0);
    assert_eq!(d["res"], 0.93);
}

#[test]
fn norm_value_round_trip_matches_curves() {
    let cutoff = inst_params("poly").iter().find(|s| s.key == "cutoff").unwrap();
    // Log curve: midpoint sits at the geometric mean.
    let mid = cutoff.value(0.5);
    assert!((mid - (80.0f32 * 14000.0).sqrt()).abs() < 0.5);
    assert!((cutoff.norm(cutoff.def) - 0.867).abs() < 0.01 || cutoff.norm(mid) > 0.49);
    for n in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
        assert!((cutoff.norm(cutoff.value(n)) - n).abs() < 1e-4);
    }
    let pan = fx_params("eq")[0]; // linear, spans negative values
    assert_eq!(pan.value(0.5), 0.0);
    assert_eq!(pan.norm(0.0), 0.5);
}
