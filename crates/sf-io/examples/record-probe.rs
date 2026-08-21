//! Phase 6 input probe: open the default input for 2 s, arm a take for the
//! middle second, print level (RMS / peak), callback + xrun counts, and the
//! current latency-compensation figures. A silent room is fine — the point
//! is that frames flow callback → ring → writer → take without drops.
//!
//!   cargo run -p sf-io --example record-probe [device-name] [buffer-frames]

use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let device = args.first().cloned();
    let buffer = args.get(1).and_then(|s| s.parse().ok());

    for d in sf_io::input_devices() {
        println!("input: {}{}", d.name, if d.is_default { " (default)" } else { "" });
    }
    let io = sf_io::IoHandle::spawn();
    let info = match io.start_input(device, buffer) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("start_input failed: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "opened {} @ {} Hz (buffer {:?})",
        info.device, info.sample_rate, info.buffer_frames
    );
    std::thread::sleep(Duration::from_millis(500));
    let id = io.start_take().expect("arm");
    std::thread::sleep(Duration::from_millis(1000));
    let rec = io.stop_take().expect("take");
    std::thread::sleep(Duration::from_millis(500));
    let st = io.stats();
    let ch = rec.channels.len();
    let frames = rec.channels.first().map(|c| c.len()).unwrap_or(0);
    let (mut sum, mut peak) = (0.0f64, 0.0f32);
    for c in &rec.channels {
        for &v in c {
            sum += (v as f64) * (v as f64);
            peak = peak.max(v.abs());
        }
    }
    let rms = (sum / (frames.max(1) * ch.max(1)) as f64).sqrt();
    let db = |x: f64| 20.0 * x.max(1e-9).log10();
    println!(
        "take #{id}: {frames} frames × {ch} ch ({:.3} s) @ {} Hz, compensation {} frames",
        frames as f64 / rec.sample_rate as f64,
        rec.sample_rate,
        rec.compensation_frames
    );
    println!("level: RMS {:.1} dBFS, peak {:.1} dBFS", db(rms), db(peak as f64));
    println!(
        "input callbacks {} · input xruns {} · latency {:?}",
        st.input_callbacks,
        st.input_xruns,
        io.latency()
    );
    io.stop_input();
}
