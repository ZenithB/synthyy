//! Cross-platform render check, part 1 (docs/WINDOWS_PORT_PLAN.md W6):
//! render every fixture in fixtures/projects/ offline and write each mixdown
//! as raw little-endian f32 (L then R) to `<out_dir>/<name>.f32`.
//!
//!   cargo run -p sf-render --example render-fixtures -- <out_dir>
//!
//! CI runs this on Windows, macOS and Linux and compares the folders with
//! examples/null-test.rs — the same project must render the same audio on
//! every OS (a Windows export nulls against a Mac export).

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: render-fixtures <out_dir>");
        std::process::exit(2);
    });
    let out = std::path::PathBuf::from(out);
    std::fs::create_dir_all(&out).unwrap_or_else(|e| panic!("create {}: {e}", out.display()));
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    let mut n = 0;
    for path in &paths {
        let json = std::fs::read_to_string(path).unwrap();
        let fx: sf_render::Fixture = serde_json::from_str(&json).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let (l, r) = match sf_render::render_project(&fx.project, &fx.scope, sf_render::OUT_SR as f32) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{}: skipped ({e})", fx.name);
                continue;
            }
        };
        let mut bytes = Vec::with_capacity((l.len() + r.len()) * 4);
        for v in l.iter().chain(r.iter()) {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(out.join(format!("{}.f32", fx.name)), bytes).unwrap();
        n += 1;
    }
    println!("rendered {n} fixtures to {} ({})", out.display(), std::env::consts::OS);
}
