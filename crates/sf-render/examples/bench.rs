//! CPU benchmark for the engine — no audio device involved, so the numbers
//! describe the machine, not the driver (docs/WINDOWS_PORT_PLAN.md W2).
//!
//! Two modes:
//!
//!   cargo run --release -p sf-render --example bench
//!       Renders every fixture in fixtures/projects/ flat out and prints how
//!       many times faster than real time each one runs (single core).
//!
//!   cargo run --release -p sf-render --example bench -- <project.json> [block] [rate]
//!       Headroom test: runs K copies of the project's engine back to back
//!       inside every audio block (K = 1, 2, 4, 8 …) and reports how much of
//!       the block's deadline that takes. This is what the audio callback
//!       does on ONE core, so "largest K under 70 %" is how many projects of
//!       this size fit before multi-core rendering would be needed.
//!       Defaults: block 128 frames, rate 48000 Hz.
//!
//! Always use --release: debug builds are not representative.

use std::time::Instant;

use sf_project::project::ProjectJSON;
use sf_render::{headroom, headroom_fit, prepare_engine, Fixture, RenderScope, SAFE_LOAD};

/// Seconds of audio rendered per headroom step.
const HEADROOM_SECONDS: f64 = 10.0;

fn load(path: &str) -> (ProjectJSON, RenderScope) {
    let json = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    if let Ok(f) = serde_json::from_str::<Fixture>(&json) {
        return (f.project, f.scope);
    }
    let p: ProjectJSON = serde_json::from_str(&json).unwrap_or_else(|e| panic!("parse {path}: {e}"));
    // bare project: play its first scene (like the in-app smoke test)
    let scope = match p.scenes.first() {
        Some(s) => RenderScope::Scene { scene_id: s.id.clone().unwrap_or_else(|| s.name.clone()) },
        None => RenderScope::Arr,
    };
    (p, scope)
}

fn fixtures_mode() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    let rate = 48_000.0f32;
    let bank = sf_engine::SampleBank::new();
    let (mut audio_s, mut wall_s) = (0.0f64, 0.0f64);
    println!("{:<18} {:>9} {:>10} {:>12}", "fixture", "audio s", "wall ms", "x realtime");
    for path in &paths {
        let (p, scope) = load(path.to_str().unwrap());
        let (mut engine, total) = match prepare_engine(&p, &scope, rate, &bank) {
            Ok(v) => v,
            Err(e) => {
                println!("{:<18} skipped: {e}", path.file_stem().unwrap().to_string_lossy());
                continue;
            }
        };
        let mut l = vec![0.0f32; 128];
        let mut r = vec![0.0f32; 128];
        let t0 = Instant::now();
        let mut done = 0;
        while done < total {
            engine.process(&mut l, &mut r);
            engine.service();
            done += 128;
        }
        let wall = t0.elapsed().as_secs_f64();
        let secs = total as f64 / rate as f64;
        audio_s += secs;
        wall_s += wall;
        println!(
            "{:<18} {:>9.2} {:>10.1} {:>12.0}",
            path.file_stem().unwrap().to_string_lossy(),
            secs,
            wall * 1e3,
            secs / wall
        );
    }
    println!("TOTAL: {audio_s:.1} s of audio in {:.0} ms = {:.0}x realtime (one core)", wall_s * 1e3, audio_s / wall_s);
}

fn headroom_mode(path: &str, block: usize, rate: f32) {
    let (p, scope) = load(path);
    let budget_ms = block as f64 / rate as f64 * 1e3;
    println!("project {path}: block {block} frames @ {rate} Hz = {budget_ms:.2} ms deadline; {HEADROOM_SECONDS} s per step");
    println!("{:>4} {:>9} {:>9} {:>9}", "K", "mean %", "p99 %", "max %");
    let steps = headroom(&p, &scope, block, rate, HEADROOM_SECONDS, 128).unwrap_or_else(|e| panic!("{e}"));
    for s in &steps {
        println!("{:>4} {:>9.1} {:>9.1} {:>9.1}", s.k, s.mean_pct, s.p99_pct, s.max_pct);
    }
    println!(
        "RESULT: {}x this project fits in {:.0}% of a {block}-frame deadline on one core (p99).",
        headroom_fit(&steps),
        SAFE_LOAD * 100.0
    );
    println!("        (max includes OS scheduling noise; p99 is the figure to compare between PCs)");
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build — rerun with --release for meaningful numbers");
    }
    match args.first() {
        None => fixtures_mode(),
        Some(path) => {
            let block = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(128);
            let rate = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(48_000.0);
            headroom_mode(path, block, rate);
        }
    }
}
