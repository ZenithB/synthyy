//! End-to-end smoke: load a ProjectJSON, host an Engine on the DEFAULT output
//! device, launch scene 0 in session mode and play for N seconds, printing a
//! snapshot line per second. Exit code 0 iff the run was audible
//! (master peak > −40 dBFS) with zero xruns.
//!
//!   cargo run -p sf-io --example play-project -- <path.json> [secs]
//!
//! Accepts either a bare ProjectJSON (src-tauri/assets/demo-project.json) or
//! a fixture file ({ name, scope, project } — fixtures/projects/*.json).

use std::time::{Duration, Instant};

use sf_engine::{Cmd, Mode};
use sf_project::project::ProjectJSON;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: play-project <path.json> [secs]");
        std::process::exit(2);
    });
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(4);

    let json = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {path}: {e}"));
    let project: ProjectJSON = serde_json::from_str(&json).unwrap_or_else(|raw_err| {
        serde_json::from_str::<sf_render::Fixture>(&json)
            .map(|f| f.project)
            .unwrap_or_else(|_| panic!("parse {path} as ProjectJSON: {raw_err}"))
    });
    let project = sf_render::with_defaults(&project);

    sf_io::prefer_performance(); // same as the app (Windows power throttling off)
    let io = sf_io::IoHandle::spawn();
    // SF_HOST = driver type (WASAPI / ASIO / CoreAudio), SF_DEVICE = output
    // device by exact name; both optional (docs/WINDOWS.md)
    io.set_host(std::env::var("SF_HOST").ok());
    let (info, mut ctl) = io
        .start_engine(std::env::var("SF_DEVICE").ok(), None)
        .unwrap_or_else(|e| panic!("start engine stream: {e}"));
    println!(
        "stream: device={:?} host={} sample_rate={} channels={} format={} buffer_frames={:?}",
        info.device, info.host, info.sample_rate, info.channels, info.sample_format, info.buffer_frames
    );

    ctl.load_project(&project);
    let n = ctl.resolver().track_count() as u16;
    for track in 0..n {
        ctl.push_cmd(Cmd::ClipLaunch { track, clip: 0 });
    }
    ctl.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });

    // Meter peaks are per-render-chunk (no hold), so poll fast and keep the
    // running max; print one line per second.
    let mut max_peak = 0.0f32;
    let start = Instant::now();
    let mut next_print = 1u64;
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(20));
        ctl.service(); // dest-"midi" modulation re-expansions (off-thread)
        let snap = ctl.snapshot();
        max_peak = max_peak.max(snap.master.peak);
        if start.elapsed().as_secs() >= next_print {
            let stats = io.stats();
            println!(
                "t={next_print}s playhead={:.0} ticks master_peak={:+.1} dBFS xruns={} callbacks={} peak_load={:.2}",
                snap.playhead_ticks,
                db(max_peak),
                stats.xruns,
                stats.callbacks,
                stats.peak_load,
            );
            next_print += 1;
        }
    }
    ctl.push_cmd(Cmd::TransportStop);
    std::thread::sleep(Duration::from_millis(100));

    let stats = io.stats();
    let peak_db = db(max_peak);
    let pass = peak_db > -40.0 && stats.xruns == 0;
    println!(
        "SMOKE {}: peak={peak_db:+.1} dBFS xruns={} over_budget={}",
        if pass { "ok" } else { "FAIL" },
        stats.xruns,
        stats.over_budget,
    );
    std::process::exit(if pass { 0 } else { 1 });
}

fn db(lin: f32) -> f32 {
    20.0 * lin.max(1e-6).log10()
}
