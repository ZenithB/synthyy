//! Phase 0 exit-test probe: enumerate devices, open the default output at a
//! 64-frame buffer, play 2 s of quiet sine, and print callback stats.
//!
//!   cargo run -p sf-io --example tone [buffer_frames]
//!
//! Env: SF_HOST picks the driver type (e.g. WASAPI, ASIO — see the list it
//! prints), SF_DEVICE an output device by exact name.

use std::time::Duration;

fn main() {
    let buffer: u32 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(64);

    let host = std::env::var("SF_HOST").ok();
    let device = std::env::var("SF_DEVICE").ok();
    println!("driver types:");
    for h in sf_io::hosts() {
        println!("  {}{}", h.name, if h.is_default { "  (default)" } else { "" });
    }
    println!("output devices ({}):", host.as_deref().unwrap_or("default driver"));
    for d in sf_io::output_devices_on(host.as_deref()) {
        println!("  {}{}", d.name, if d.is_default { "  (default)" } else { "" });
    }

    let io = sf_io::IoHandle::spawn();
    io.set_host(host);
    match io.start_tone(device, Some(buffer), 440.0, 0.1) {
        Ok(info) => println!(
            "stream open: {} [{}] @ {} Hz, {} ch {}, buffer {:?} frames (requested {})",
            info.device, info.host, info.sample_rate, info.channels, info.sample_format, info.buffer_frames, buffer
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
    println!(
        "callback size seen: {}–{} frames  rt priority: {}",
        s.min_callback_frames,
        s.max_callback_frames,
        match s.rt_priority {
            sf_io::RT_MMCSS_OK => "MMCSS Pro Audio",
            sf_io::RT_MMCSS_FAILED => "MMCSS FAILED",
            _ => "host-managed",
        }
    );
    if s.callbacks == 0 {
        eprintln!("no callbacks fired — stream silently dead");
        std::process::exit(1);
    }
}
