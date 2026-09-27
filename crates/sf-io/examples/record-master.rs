//! End-to-end check of desktop "Record Output" on real hardware, no GUI.
//!
//!   cargo run -p sf-io --example record-master -- <project.json> [idle_s] [rec_s]
//!
//! `idle_s` is the key knob: it plays for that long BEFORE arming the capture,
//! which is what a real session looks like. The master tap only pushes frames
//! while recording, so its delivery cursor lags the stream's frame counter by
//! exactly that idle period — the bug that made every take come back empty.

use std::time::Duration;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| {
        eprintln!("usage: record-master <project.json> [idle_s] [rec_s]");
        std::process::exit(2);
    });
    let idle_s: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(5.0);
    let rec_s: f64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(3.0);

    let raw = std::fs::read_to_string(&path).expect("read project");
    let project: sf_project::project::ProjectJSON =
        serde_json::from_str(&raw).expect("parse ProjectJSON");
    let project = sf_render::with_defaults(&project);

    let io = sf_io::IoHandle::spawn();
    let (info, mut ctl) = io.start_engine(None, Some(256)).expect("open engine stream");
    println!("stream: {} @ {} Hz", info.device, info.sample_rate);
    ctl.load_project(&project);

    // launch scene 0 on every track, session mode
    if let Some(scene) = project.scenes.first() {
        let sid = scene.id.clone().unwrap_or_else(|| scene.name.clone());
        if let Some(clip) = ctl.resolver().scene(&sid) {
            for track in 0..ctl.resolver().track_count() as u16 {
                ctl.push_cmd(sf_engine::Cmd::ClipLaunch { track, clip });
            }
        }
    }
    ctl.push_cmd(sf_engine::Cmd::TransportPlay {
        mode: sf_engine::Mode::Session,
        from_ticks: 0.0,
    });

    // play for a while WITHOUT recording — the real-world precondition
    println!("playing {idle_s}s before arming…");
    std::thread::sleep(Duration::from_secs_f64(idle_s));

    io.start_master_capture().expect("start master capture");
    println!("recording {rec_s}s…");
    std::thread::sleep(Duration::from_secs_f64(rec_s));
    let t0 = std::time::Instant::now();
    let rec = io.stop_master_capture().expect("stop master capture");
    let stop_ms = t0.elapsed().as_secs_f64() * 1e3;

    let frames = rec.channels.first().map(|c| c.len()).unwrap_or(0);
    let secs = frames as f64 / rec.sample_rate as f64;
    let peak = rec
        .channels
        .iter()
        .flat_map(|c| c.iter())
        .fold(0.0f32, |m, v| m.max(v.abs()));
    let rms = {
        let (mut s, mut n) = (0.0f64, 0usize);
        for c in &rec.channels {
            for v in c {
                s += (*v as f64) * (*v as f64);
                n += 1;
            }
        }
        if n > 0 { (s / n as f64).sqrt() } else { 0.0 }
    };
    let db = if rms > 0.0 { 20.0 * rms.log10() } else { f64::NEG_INFINITY };
    println!(
        "take: {frames} frames = {secs:.2}s @ {} Hz, peak {peak:.3}, rms {db:.1} dBFS (stop took {stop_ms:.0} ms)",
        rec.sample_rate
    );
    io.stop();

    let expected = rec_s * rec.sample_rate as f64;
    let drift = (frames as f64 - expected).abs() / expected;
    if frames == 0 {
        println!("FAIL: captured NOTHING");
        std::process::exit(1);
    }
    if drift > 0.25 {
        println!("FAIL: take length is {drift:.0}x off the {rec_s}s requested");
        std::process::exit(1);
    }
    if peak <= 0.0001 {
        println!("FAIL: captured silence");
        std::process::exit(1);
    }
    println!("RECORD ok");
}
