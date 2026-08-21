//! Xrun soak test (RUST_REWRITE_PLAN.md §8): host the Engine on the default
//! output at a small buffer, play scene 0 of a project in session mode for
//! N minutes — optionally under synthetic CPU load — and report the callback
//! statistics once a minute. Exit code 0 iff the whole run saw zero xruns
//! AND zero over-budget callbacks, else 1 with a summary.
//!
//!   cargo run --release -p sf-io --example soak -- <project.json> <minutes> [buffer_frames=64] [--load N]
//!
//! Accepts a bare ProjectJSON or a fixture file ({ name, scope, project }).
//! Master gain is forced to −40 dB (project meta) so a long run is quiet.
//! `--load N` spawns N busy-loop threads for the duration (contention from
//! ordinary-priority threads; the audio thread must still make its deadline).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use sf_engine::{Cmd, Mode};
use sf_project::project::ProjectJSON;

const MASTER_GAIN_DB: f64 = -40.0;

fn usage() -> ! {
    eprintln!("usage: soak <project.json> <minutes> [buffer_frames=64] [--load N]");
    std::process::exit(2);
}

fn main() {
    let mut path: Option<String> = None;
    let mut minutes: Option<f64> = None;
    let mut buffer_frames: u32 = 64;
    let mut load: usize = 0;
    let mut positional = 0;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--load" {
            load = args.next().and_then(|s| s.parse().ok()).unwrap_or_else(|| usage());
            continue;
        }
        match positional {
            0 => path = Some(a),
            1 => minutes = Some(a.parse().unwrap_or_else(|_| usage())),
            2 => buffer_frames = a.parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
        positional += 1;
    }
    let (Some(path), Some(minutes)) = (path, minutes) else { usage() };

    let json = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let mut project: ProjectJSON = serde_json::from_str(&json).unwrap_or_else(|raw_err| {
        serde_json::from_str::<sf_render::Fixture>(&json)
            .map(|f| f.project)
            .unwrap_or_else(|_| panic!("parse {path} as ProjectJSON: {raw_err}"))
    });
    project.meta.master_gain = Some(MASTER_GAIN_DB);
    let project = sf_render::with_defaults(&project);

    // synthetic CPU load: ordinary-priority spinners
    let stop = Arc::new(AtomicBool::new(false));
    let mut spinners = Vec::with_capacity(load);
    for i in 0..load {
        let stop = Arc::clone(&stop);
        spinners.push(
            std::thread::Builder::new()
                .name(format!("soak-load-{i}"))
                .spawn(move || {
                    let mut x = 1u64;
                    while !stop.load(Ordering::Relaxed) {
                        for _ in 0..10_000 {
                            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                        }
                        std::hint::black_box(x);
                    }
                })
                .expect("spawn load thread"),
        );
    }

    let io = sf_io::IoHandle::spawn();
    let (info, mut ctl) = io
        .start_engine(None, Some(buffer_frames))
        .unwrap_or_else(|e| panic!("start engine stream: {e}"));
    println!(
        "soak: project={path} minutes={minutes} requested_frames={buffer_frames} load={load}\n\
         stream: device={:?} sample_rate={} buffer_frames={:?}",
        info.device, info.sample_rate, info.buffer_frames
    );

    ctl.load_project(&project);
    let n = ctl.resolver().track_count() as u16;
    for track in 0..n {
        ctl.push_cmd(Cmd::ClipLaunch { track, clip: 0 });
    }
    ctl.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });

    let total = Duration::from_secs_f64(minutes * 60.0);
    let start = Instant::now();
    let mut next_report = Duration::from_secs(60);
    let report = |elapsed: Duration| {
        let s = io.stats();
        println!(
            "t={:>5}s callbacks={} xruns={} over_budget={} peak_load={:.3} max_callback_us={:.1} (budget_us={:.1})",
            elapsed.as_secs(),
            s.callbacks,
            s.xruns,
            s.over_budget,
            s.peak_load,
            s.max_callback_us,
            s.budget_us,
        );
    };
    while start.elapsed() < total {
        std::thread::sleep(Duration::from_millis(20));
        ctl.service(); // dest-"midi" modulation re-expansions (off-thread)
        if start.elapsed() >= next_report {
            report(next_report);
            next_report += Duration::from_secs(60);
        }
    }

    ctl.push_cmd(Cmd::TransportStop);
    std::thread::sleep(Duration::from_millis(100));
    let stats = io.stats();
    io.stop();
    stop.store(true, Ordering::Relaxed);
    for t in spinners {
        let _ = t.join();
    }

    let pass = stats.xruns == 0 && stats.over_budget == 0;
    println!(
        "SOAK {}: {:.1} min at {:?} frames, callbacks={} xruns={} over_budget={} peak_load={:.3} max_callback_us={:.1}",
        if pass { "ok" } else { "FAIL" },
        start.elapsed().as_secs_f64() / 60.0,
        info.buffer_frames,
        stats.callbacks,
        stats.xruns,
        stats.over_budget,
        stats.peak_load,
        stats.max_callback_us,
    );
    std::process::exit(if pass { 0 } else { 1 });
}
