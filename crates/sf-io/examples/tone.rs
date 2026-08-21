//! Phase 0 exit-test probe: enumerate devices, open the default output at a
//! 64-frame buffer, play 2 s of quiet sine, and print callback stats.
//!
//!   cargo run -p sf-io --example tone [buffer_frames]

use std::time::Duration;

fn main() {
    let buffer: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(64);

    println!("output devices:");
    for d in sf_io::output_devices() {
        println!("  {}{}", d.name, if d.is_default { "  (default)" } else { "" });
    }

    let io = sf_io::IoHandle::spawn();
    match io.start_tone(None, Some(buffer), 440.0, 0.1) {
        Ok(info) => println!(
            "stream open: {} @ {} Hz, buffer {:?} frames (requested {})",
            info.device, info.sample_rate, info.buffer_frames, buffer
        ),
        Err(e) => {
            eprintln!("failed to open stream: {e}");
            std::process::exit(1);
        }
    }

    std::thread::sleep(Duration::from_secs(2));
    let s = io.stats();
    io.stop();
    println!(
        "callbacks: {}  xruns: {}  over-budget: {}  last: {:.1}µs  peak: {:.1}µs / {:.1}µs budget (peak load {:.1}%)",
        s.callbacks,
        s.xruns,
        s.over_budget,
        s.last_callback_us,
        s.max_callback_us,
        s.budget_us,
        s.peak_load * 100.0
    );
    if s.callbacks == 0 {
        eprintln!("no callbacks fired — stream silently dead");
        std::process::exit(1);
    }
}
