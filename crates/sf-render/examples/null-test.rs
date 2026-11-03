//! Cross-platform render check, part 2: null-test two folders written by
//! examples/render-fixtures.rs (e.g. the Windows and the macOS renders).
//!
//!   cargo run -p sf-render --example null-test -- <dir_a> <dir_b> [limit_db]
//!
//! For each fixture: the residual (A − B) level relative to A's level, in dB.
//! −∞ (printed as "exact") means bit-identical. Exits non-zero if any fixture
//! is above `limit_db` (default −90 dB: inaudible, well below 16-bit export
//! resolution) or is missing from either folder. Prints a markdown table
//! suitable for a CI step summary.

fn read_f32(path: &std::path::Path) -> Option<Vec<f32>> {
    let b = std::fs::read(path).ok()?;
    Some(b.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect())
}

/// Residual level of `b` against reference `a`, dB relative to `a`'s RMS.
/// None = bit-identical. Length mismatches count the extra tail as residual.
fn null_db(a: &[f32], b: &[f32]) -> Option<f64> {
    let n = a.len().max(b.len());
    let (mut sig, mut res) = (0.0f64, 0.0f64);
    for i in 0..n {
        let x = *a.get(i).unwrap_or(&0.0) as f64;
        let y = *b.get(i).unwrap_or(&0.0) as f64;
        sig += x * x;
        res += (x - y) * (x - y);
    }
    if res == 0.0 {
        return None;
    }
    Some(10.0 * (res / sig.max(1e-30)).log10())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: null-test <dir_a> <dir_b> [limit_db]");
        std::process::exit(2);
    }
    let (a, b) = (std::path::Path::new(&args[0]), std::path::Path::new(&args[1]));
    let limit: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(-90.0);
    let mut names: Vec<String> = std::fs::read_dir(a)
        .unwrap_or_else(|e| panic!("read {}: {e}", a.display()))
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".f32")).map(String::from))
        .collect();
    names.sort();
    if names.is_empty() {
        eprintln!("no renders in {}", a.display());
        std::process::exit(1);
    }
    let mut failed = 0;
    let mut worst: Option<f64> = None;
    let mut exact = 0;
    println!("| fixture | residual | result |");
    println!("|---|---|---|");
    for n in &names {
        let (ra, rb) = (read_f32(&a.join(format!("{n}.f32"))), read_f32(&b.join(format!("{n}.f32"))));
        let (Some(ra), Some(rb)) = (ra, rb) else {
            println!("| {n} | missing | FAIL |");
            failed += 1;
            continue;
        };
        match null_db(&ra, &rb) {
            None => {
                exact += 1;
                println!("| {n} | exact | ok |");
            }
            Some(db) => {
                worst = Some(worst.map_or(db, |w: f64| w.max(db)));
                let ok = db <= limit;
                if !ok {
                    failed += 1;
                }
                println!("| {n} | {db:.1} dB | {} |", if ok { "ok" } else { "FAIL" });
            }
        }
    }
    println!();
    println!(
        "{} fixtures: {exact} bit-identical, worst residual {}, limit {limit} dB, {failed} failed",
        names.len(),
        worst.map_or("none".to_string(), |w| format!("{w:.1} dB"))
    );
    std::process::exit(if failed == 0 { 0 } else { 1 });
}
