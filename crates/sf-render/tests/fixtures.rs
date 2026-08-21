//! Golden-fixture parity harness (RUST_ARCH.md "Parity gates", advisory tier):
//! render every fixtures/projects/*.json mixdown at 44.1 kHz and, when a
//! golden WAV exists, score (a) RMS-null difference and (b) mean spectral
//! cosine similarity over 2048-sample Hann frames on log magnitudes. Scores
//! land in target/parity-report.json and print as a table.
//!
//! HARD assertions only: no panic, no NaN/inf, and — when the golden is
//! louder than −50 dBFS — the rendered output must not be all-zero, UNLESS
//! the device is still a stub (rendered RMS < 1e-7 ⇒ status "silent-stub";
//! STRICT=1 makes silence fatal). Exact null matches against Tone composites
//! are not expected this pass — record, don't chase.

use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use realfft::RealFftPlanner;
use sf_render::{decode_wav, render_project, Fixture};

/// Browser goldens start the transport at +0.02 s inside the buffer
/// (render.ts `transport.start(0.02, …)`); the Rust render starts at 0.
const GOLDEN_LEAD_SEC: f64 = 0.02;
const SR: f64 = 44_100.0;
const FFT_LEN: usize = 2048;
const HOP: usize = 1024;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rms(buf: &[f32]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    (buf.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / buf.len() as f64).sqrt()
}

fn dbfs(x: f64) -> f64 {
    if x <= 0.0 {
        -160.0
    } else {
        (20.0 * x.log10()).max(-160.0)
    }
}

fn mono(l: &[f32], r: &[f32]) -> Vec<f32> {
    l.iter().zip(r).map(|(a, b)| 0.5 * (a + b)).collect()
}

/// Mean cosine similarity between log-magnitude spectra of Hann frames.
fn spectral_similarity(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    if n < FFT_LEN {
        return 0.0;
    }
    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_LEN);
    let hann: Vec<f32> = (0..FFT_LEN)
        .map(|i| {
            let x = std::f32::consts::TAU * i as f32 / FFT_LEN as f32;
            0.5 * (1.0 - x.cos())
        })
        .collect();
    let mut in_a = fft.make_input_vec();
    let mut in_b = fft.make_input_vec();
    let mut out_a = fft.make_output_vec();
    let mut out_b = fft.make_output_vec();
    let logmag = |out: &[realfft::num_complex::Complex<f32>]| -> Vec<f64> {
        out.iter().map(|c| ((c.norm() as f64) + 1e-9).ln()).collect()
    };
    let mut sum = 0.0f64;
    let mut frames = 0usize;
    let mut pos = 0usize;
    while pos + FFT_LEN <= n {
        for i in 0..FFT_LEN {
            in_a[i] = a[pos + i] * hann[i];
            in_b[i] = b[pos + i] * hann[i];
        }
        fft.process(&mut in_a, &mut out_a).unwrap();
        fft.process(&mut in_b, &mut out_b).unwrap();
        let (la, lb) = (logmag(&out_a), logmag(&out_b));
        let dot: f64 = la.iter().zip(&lb).map(|(x, y)| x * y).sum();
        let na: f64 = la.iter().map(|x| x * x).sum::<f64>().sqrt();
        let nb: f64 = lb.iter().map(|x| x * x).sum::<f64>().sqrt();
        if na > 0.0 && nb > 0.0 {
            sum += dot / (na * nb);
            frames += 1;
        }
        pos += HOP;
    }
    if frames == 0 {
        0.0
    } else {
        sum / frames as f64
    }
}

#[derive(Debug)]
struct Row {
    name: String,
    status: String,
    rendered_db: f64,
    golden_db: Option<f64>,
    null_score: Option<f64>,
    spectral: Option<f64>,
}

fn run_fixture(path: &Path, strict: bool) -> Row {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let json = match std::fs::read_to_string(path) {
        Ok(j) => j,
        Err(e) => {
            return Row {
                name,
                status: format!("fail:read:{e}"),
                rendered_db: -160.0,
                golden_db: None,
                null_score: None,
                spectral: None,
            }
        }
    };
    let fixture: Fixture = match serde_json::from_str(&json) {
        Ok(f) => f,
        Err(e) => {
            return Row {
                name,
                status: format!("fail:parse:{e}"),
                rendered_db: -160.0,
                golden_db: None,
                null_score: None,
                spectral: None,
            }
        }
    };
    let name = if fixture.name.is_empty() { name } else { fixture.name.clone() };

    let rendered = std::panic::catch_unwind(AssertUnwindSafe(|| {
        render_project(&fixture.project, &fixture.scope, SR as f32)
    }));
    let (l, r) = match rendered {
        Err(_) => {
            return Row {
                name,
                status: "fail:panic".into(),
                rendered_db: -160.0,
                golden_db: None,
                null_score: None,
                spectral: None,
            }
        }
        Ok(Err(e)) => {
            return Row {
                name,
                status: format!("fail:render:{e}"),
                rendered_db: -160.0,
                golden_db: None,
                null_score: None,
                spectral: None,
            }
        }
        Ok(Ok(lr)) => lr,
    };
    if l.iter().chain(r.iter()).any(|s| !s.is_finite()) {
        return Row {
            name,
            status: "fail:nan".into(),
            rendered_db: -160.0,
            golden_db: None,
            null_score: None,
            spectral: None,
        };
    }
    let rm = mono(&l, &r);
    let rendered_rms = rms(&rm);
    let rendered_db = dbfs(rendered_rms);

    let golden_path = repo_root().join("fixtures/golden").join(format!("{name}.wav"));
    if !golden_path.exists() {
        return Row {
            name,
            status: "no-golden".into(),
            rendered_db,
            golden_db: None,
            null_score: None,
            spectral: None,
        };
    }
    let bytes = std::fs::read(&golden_path).unwrap_or_default();
    let (gl, gr, gsr) = match decode_wav(&bytes) {
        Ok(g) => g,
        Err(e) => {
            return Row {
                name,
                status: format!("fail:golden:{e}"),
                rendered_db,
                golden_db: None,
                null_score: None,
                spectral: None,
            }
        }
    };
    let lead = (GOLDEN_LEAD_SEC * gsr as f64).round() as usize;
    let gm_full = mono(&gl, &gr);
    let gm = if gm_full.len() > lead { &gm_full[lead..] } else { &gm_full[..] };
    let golden_rms = rms(gm);
    let golden_db = dbfs(golden_rms);

    // silence gate (hard, unless the device is still a stub)
    if golden_db > -50.0 && rendered_rms < 1e-7 {
        return Row {
            name,
            status: if strict { "fail:silent".into() } else { "silent-stub".into() },
            rendered_db,
            golden_db: Some(golden_db),
            null_score: None,
            spectral: None,
        };
    }

    let n = gm.len().min(rm.len());
    let null_score = if golden_rms > 0.0 {
        let diff: f64 = gm[..n]
            .iter()
            .zip(&rm[..n])
            .map(|(g, r)| {
                let d = (*g - *r) as f64;
                d * d
            })
            .sum::<f64>()
            / n.max(1) as f64;
        Some(diff.sqrt() / golden_rms)
    } else {
        None
    };
    let spectral = Some(spectral_similarity(gm, &rm[..n.min(rm.len())]));

    Row {
        name,
        status: "ok".into(),
        rendered_db,
        golden_db: Some(golden_db),
        null_score,
        spectral,
    }
}

#[test]
fn fixture_parity() {
    let strict = std::env::var("STRICT").map(|v| v == "1").unwrap_or(false);
    let dir = repo_root().join("fixtures/projects");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("no fixtures dir {dir:?}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no fixture projects found");

    let rows: Vec<Row> = paths.iter().map(|p| run_fixture(p, strict)).collect();

    // ---- report ----
    let fmt_opt = |v: Option<f64>| v.map(|x| format!("{x:.4}")).unwrap_or_else(|| "-".into());
    println!("\n{:<18} {:<14} {:>9} {:>9} {:>8} {:>9}", "fixture", "status", "rend dB", "gold dB", "null", "spectral");
    for row in &rows {
        println!(
            "{:<18} {:<14} {:>9.1} {:>9} {:>8} {:>9}",
            row.name,
            row.status,
            row.rendered_db,
            row.golden_db.map(|d| format!("{d:.1}")).unwrap_or_else(|| "-".into()),
            fmt_opt(row.null_score),
            fmt_opt(row.spectral),
        );
    }

    let json: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "name": r.name,
                "status": r.status,
                "rendered_rms_dbfs": r.rendered_db,
                "golden_rms_dbfs": r.golden_db,
                "null_score": r.null_score,
                "spectral_similarity": r.spectral,
            })
        })
        .collect();
    let target = repo_root().join("target");
    let _ = std::fs::create_dir_all(&target);
    std::fs::write(
        target.join("parity-report.json"),
        serde_json::to_string_pretty(&json).unwrap(),
    )
    .expect("write parity report");

    let failures: Vec<&Row> = rows.iter().filter(|r| r.status.starts_with("fail")).collect();
    assert!(
        failures.is_empty(),
        "fixture hard failures: {:?}",
        failures.iter().map(|r| format!("{}: {}", r.name, r.status)).collect::<Vec<_>>()
    );
}
