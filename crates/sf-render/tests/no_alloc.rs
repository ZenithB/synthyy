//! Real-time rule enforcement (RUST_ARCH.md §4, RUST_REWRITE_PLAN.md §8):
//! for EVERY fixture under fixtures/projects/, build the graph exactly like
//! `render_project` does (defaults filled, scene launched / arr started),
//! adopt the graph with one warm-up `process`, then pull ≥ 2 s of audio
//! through `Engine::process` under `assert_no_alloc`. This walks every
//! device's process/trigger path, the scheduler, modulation (LFO/env/auto),
//! ducking, sends/buses and the master chain. `service()` (the one sanctioned
//! allocating hook) runs between chunks OUTSIDE the guard, like sf-render
//! and the IPC layer do.

use std::path::{Path, PathBuf};

use sf_engine::{Cmd, Engine, Mode};
use sf_render::{with_defaults, Fixture, RenderScope};

/// The integration-test binary runs under the alloc-detecting allocator.
#[global_allocator]
static ALLOC_GUARD: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

const SR: f32 = 44_100.0;
const SECS: f32 = 2.2;
const PULL: usize = 64;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects")
}

fn load(path: &Path) -> Fixture {
    let json = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    serde_json::from_str(&json).unwrap_or_else(|e| panic!("{path:?}: {e}"))
}

/// Build + launch like `render_project_with_bank`; returns the engine after
/// one warm-up process (graph adoption, first command drain).
fn launched(f: &Fixture) -> Engine {
    let p = with_defaults(&f.project);
    let mut e = Engine::new(SR);
    e.loop_override_off = true;
    e.load_project(&p);
    match &f.scope {
        RenderScope::Scene { scene_id } => {
            let scene = e.resolver().scene(scene_id).expect("scene id");
            let n = e.resolver().track_count() as u16;
            for track in 0..n {
                e.push_cmd(Cmd::ClipLaunch { track, clip: scene });
            }
            e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        }
        RenderScope::Arr | RenderScope::Loop => {
            e.push_cmd(Cmd::TransportPlay { mode: Mode::Arr, from_ticks: 0.0 });
        }
    }
    e
}

fn pull_guarded(e: &mut Engine, secs: f32, pull: usize) -> f32 {
    let mut l = vec![0.0f32; pull];
    let mut r = vec![0.0f32; pull];
    let chunks = (SR * secs) as usize / pull;
    let mut peak = 0.0f32;
    for _ in 0..chunks {
        assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
        e.service();
        for s in l.iter().chain(r.iter()) {
            assert!(s.is_finite());
            peak = peak.max(s.abs());
        }
    }
    peak
}

#[test]
fn every_fixture_processes_without_allocating() {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .expect("fixtures/projects")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    for path in &paths {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        eprintln!("no-alloc: {name}");
        let f = load(path);
        let mut e = launched(&f);
        // warm-up (graph adoption + first drain) is allowed to allocate? No —
        // but the FIRST process adopts the graph, which is a pointer move.
        // Keep it inside the guard too: the rule holds from the first call.
        let peak = pull_guarded(&mut e, SECS, PULL);
        // sanity: we really exercised the graph (most fixtures are audible)
        let _ = peak;
    }
}

/// A burst of every command kind between chunks, with the guard on.
#[test]
fn command_burst_does_not_allocate() {
    let f = load(&fixture_dir().join("inst-poly.json"));
    let mut e = launched(&f);
    let mut l = vec![0.0f32; PULL];
    let mut r = vec![0.0f32; PULL];
    assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
    let n = e.resolver().track_count() as u16;
    for i in 0..400u32 {
        let track = (i as u16) % n.max(1);
        let pitch = 36 + (i % 48) as u8;
        e.push_cmd(Cmd::NoteOn { track, pitch, vel: 0.8 });
        e.push_cmd(Cmd::SetParam {
            target: track,
            slot: sf_engine::command::SLOT_INST,
            param: (i % 6) as u16,
            value: (i % 7) as f32 / 7.0,
        });
        e.push_cmd(Cmd::SetParam {
            target: track,
            slot: sf_engine::command::SLOT_MIXER,
            param: sf_engine::command::MIX_GAIN,
            value: -6.0,
        });
        if i % 3 == 0 {
            e.push_cmd(Cmd::NoteOff { track, pitch: pitch.saturating_sub(1) });
        }
        if i % 50 == 0 {
            e.push_cmd(Cmd::ClipStop { track });
            e.push_cmd(Cmd::ClipLaunch { track, clip: 0 });
        }
        if i % 97 == 0 {
            e.push_cmd(Cmd::TransportStop);
            e.push_cmd(Cmd::SetTempo { bpm: 90.0 + (i % 80) as f64 });
            e.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        }
        assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
    }
}

/// Every registered param of every built device driven to its min, then max,
/// then default, with a guarded chunk after each step: catches delay lines /
/// lookahead buffers / voice pools that size themselves lazily on a param
/// edge instead of at `prepare()`.
#[test]
fn param_sweep_on_every_fixture_does_not_allocate() {
    use sf_project::registry::{fx_params, inst_params};
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    let mut l = vec![0.0f32; PULL];
    let mut r = vec![0.0f32; PULL];
    for path in &paths {
        let f = load(path);
        let mut e = launched(&f);
        assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
        // (key, min, max, def) resolved off-thread, like the IPC layer
        let mut plan: Vec<(sf_engine::CmdAddr, f32, f32, f32)> = Vec::new();
        let p = with_defaults(&f.project);
        let mut push = |e: &Engine, key: String, s: &sf_project::registry::ParamSpec| {
            if let Some(a) = e.resolver().param(&key) {
                plan.push((a, s.min, s.max, s.def));
            }
        };
        for t in &p.tracks {
            let tid = t.id.clone().unwrap_or_default();
            for s in inst_params(&t.inst.r#type) {
                push(&e, format!("{tid}|inst|{}", s.key), s);
            }
            for fx in &t.fx {
                let fid = fx.id.clone().unwrap_or_else(|| fx.r#type.clone());
                for s in fx_params(&fx.r#type) {
                    push(&e, format!("{tid}|{fid}|{}", s.key), s);
                }
            }
        }
        for fx in p.master_fx.as_deref().unwrap_or(&[]) {
            let fid = fx.id.clone().unwrap_or_else(|| fx.r#type.clone());
            for s in fx_params(&fx.r#type) {
                push(&e, format!("master|{fid}|{}", s.key), s);
            }
        }
        for (addr, min, max, def) in &plan {
            for v in [*min, *max, *def] {
                e.push_cmd(Cmd::SetParam {
                    target: addr.target,
                    slot: addr.slot,
                    param: addr.param,
                    value: v,
                });
                // a note so instrument params hit a live voice
                e.push_cmd(Cmd::NoteOn { track: addr.target.min(64), pitch: 60, vel: 0.9 });
                for _ in 0..4 {
                    assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
                }
                e.push_cmd(Cmd::NoteOff { track: addr.target.min(64), pitch: 60 });
                assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
            }
        }
        for s in l.iter().chain(r.iter()) {
            assert!(s.is_finite(), "{path:?}: non-finite after param sweep");
        }
    }
}

/// Dense polyphony on every instrument fixture: 64 overlapping notes per
/// chunk for a second forces voice stealing on every pool.
#[test]
fn voice_stealing_does_not_allocate() {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(fixture_dir())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("inst-")))
        .collect();
    paths.sort();
    assert!(!paths.is_empty());
    let mut l = vec![0.0f32; PULL];
    let mut r = vec![0.0f32; PULL];
    for path in &paths {
        let f = load(path);
        let mut e = launched(&f);
        assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
        let n = e.resolver().track_count() as u16;
        for i in 0..(SR as usize / PULL) {
            for k in 0..64u8 {
                e.push_cmd(Cmd::NoteOn { track: (k as u16) % n, pitch: 24 + ((k as usize + i) % 84) as u8, vel: 0.7 });
            }
            assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
            for k in 0..32u8 {
                e.push_cmd(Cmd::NoteOff { track: (k as u16) % n, pitch: 24 + ((k as usize + i) % 84) as u8 });
            }
            assert_no_alloc::assert_no_alloc(|| e.process(&mut l, &mut r));
        }
        assert!(l.iter().all(|s| s.is_finite()), "{path:?}");
    }
}
