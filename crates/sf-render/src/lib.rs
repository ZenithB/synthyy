//! Offline render driver: the SAME sf-engine graph pulled faster than real
//! time in ≤ MAX_BLOCK chunks (render-path parity is law — there is no
//! separate render engine). Scope semantics mirror render.ts scopeBounds():
//! scene = longest clip ×2 from 0 (session mode, the scene's clips launched
//! at 0); arr = 0 → arrangement end (min 1 bar, error when empty); loop = the
//! meta loop region. Output lands at 44.1 kHz (rubato resample when the
//! engine rate differs) and encodes to 16-bit stereo WAV with the canonical
//! 44-byte header, byte-compatible with src/audio/encode.ts encodeWav.

use serde::Deserialize;

use sf_engine::{Cmd, Engine, Mode};
use sf_project::project::ProjectJSON;
use sf_project::registry::{fx_params, inst_params, midifx_params};
use sf_project::{ticks_to_seconds, BAR};

pub const OUT_SR: u32 = 44_100;
/// render.ts adds 1.5 s of reverb/release tail; the Rust harness keeps 1.0 s
/// (comparisons run over the overlapping samples).
pub const TAIL_SEC: f64 = 1.0;

/// What slice of the project to render (fixture `scope` field shape).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind")]
pub enum RenderScope {
    #[serde(rename = "scene")]
    Scene {
        #[serde(rename = "sceneId")]
        scene_id: String,
    },
    #[serde(rename = "arr")]
    Arr,
    #[serde(rename = "loop")]
    Loop,
}

/// Fixture file format under fixtures/projects/ (src/audio/fixtures.ts).
#[derive(Debug, Clone, Deserialize)]
pub struct Fixture {
    pub name: String,
    pub scope: RenderScope,
    pub project: ProjectJSON,
}

/// Mirror of fixtures.ts withDefaults(): device constructors expect FULL
/// param maps, so fill everything a hand-written fixture omitted from the
/// frozen schema registry before building the graph.
pub fn with_defaults(project: &ProjectJSON) -> ProjectJSON {
    let mut p = project.clone();
    let fill = |params: &mut sf_project::project::Params,
                specs: &'static [sf_project::registry::ParamSpec]| {
        for s in specs {
            params.entry(s.key.to_string()).or_insert(s.def as f64);
        }
    };
    for t in &mut p.tracks {
        fill(&mut t.inst.params, inst_params(&t.inst.r#type));
        for f in &mut t.fx {
            fill(&mut f.params, fx_params(&f.r#type));
        }
        if let Some(mfx) = &mut t.midifx {
            for m in mfx {
                fill(&mut m.params, midifx_params(&m.r#type));
            }
        }
    }
    if let Some(mfx) = &mut p.master_fx {
        for f in mfx {
            fill(&mut f.params, fx_params(&f.r#type));
        }
    }
    p
}

/// render.ts scopeBounds(): (from_ticks, length_ticks, scene id).
fn scope_bounds<'a>(
    project: &ProjectJSON,
    scope: &'a RenderScope,
) -> Result<(f64, f64, Option<&'a str>), String> {
    match scope {
        RenderScope::Loop => {
            let start = project.meta.loop_start.unwrap_or(0.0);
            let end = project.meta.loop_end.unwrap_or(4.0 * BAR as f64);
            Ok((start, (end - start).max(BAR as f64), None))
        }
        RenderScope::Scene { scene_id } => {
            let mut longest = BAR as f64;
            for (key, c) in &project.clips {
                if key.ends_with(&format!("|{scene_id}")) {
                    longest = longest.max(c.len);
                }
            }
            Ok((0.0, longest * 2.0, Some(scene_id)))
        }
        RenderScope::Arr => {
            let mut end = 0.0f64;
            for a in project.arr.values() {
                end = end.max(a.start + a.clip.len);
            }
            let ticks = end.max(BAR as f64);
            if ticks <= BAR as f64 && project.arr.is_empty() {
                return Err("Arrangement is empty — nothing to render".into());
            }
            Ok((0.0, ticks, None))
        }
    }
}

/// Render a project mixdown at `sample_rate`; returns planar stereo f32.
pub fn render_project(
    project: &ProjectJSON,
    scope: &RenderScope,
    sample_rate: f32,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    render_project_with_bank(project, scope, sample_rate, &sf_engine::SampleBank::new())
}

/// [`render_project`] with a sample bank: sampler instruments and audio clips
/// render from the bank's decoded PCM (missing ids stay silent, like a
/// collaborator without the file).
pub fn render_project_with_bank(
    project: &ProjectJSON,
    scope: &RenderScope,
    sample_rate: f32,
    bank: &sf_engine::SampleBank,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    let (mut engine, total) = prepare_engine(project, scope, sample_rate, bank)?;
    let mut l = vec![0.0f32; total];
    let mut r = vec![0.0f32; total];
    const PULL: usize = 128; // MAX_BLOCK
    for (cl, cr) in l.chunks_mut(PULL).zip(r.chunks_mut(PULL)) {
        engine.process(cl, cr);
        // dest-"midi" modulation: pending re-expansions run here, off the
        // render path, so offline output matches live playback
        engine.service();
    }
    Ok((l, r))
}

/// Build an offline engine for `scope` — project loaded, clips launched /
/// arrangement started, transport playing — and return it with the render
/// length in frames (scope length + [`TAIL_SEC`]). The caller pulls audio
/// with `Engine::process` (and calls `service()` between pulls). Used by the
/// renderer and by the CPU benchmark (examples/bench.rs).
pub fn prepare_engine(
    project: &ProjectJSON,
    scope: &RenderScope,
    sample_rate: f32,
    bank: &sf_engine::SampleBank,
) -> Result<(Engine, usize), String> {
    let p = with_defaults(project);
    let (from, ticks, scene_id) = scope_bounds(&p, scope)?;

    let mut engine = Engine::new(sample_rate);
    engine.loop_override_off = true; // offline: play through, never wrap
    engine.load_project_with_bank(&p, bank);

    match scene_id {
        Some(sid) => {
            // fixtures reference the ORIGINAL scene ids (no remapping here)
            let scene = engine
                .resolver()
                .scene(sid)
                .ok_or_else(|| format!("Scene {sid:?} not in project"))?;
            let n = engine.resolver().track_count() as u16;
            for track in 0..n {
                engine.push_cmd(Cmd::ClipLaunch { track, clip: scene });
            }
            engine.push_cmd(Cmd::TransportPlay { mode: Mode::Session, from_ticks: 0.0 });
        }
        None => {
            engine.push_cmd(Cmd::TransportPlay { mode: Mode::Arr, from_ticks: from });
        }
    }

    let bpm = if p.meta.bpm > 0.0 { p.meta.bpm } else { 120.0 };
    let total = (ticks_to_seconds(ticks, bpm) + TAIL_SEC) * sample_rate as f64;
    Ok((engine, total.ceil() as usize))
}

/// Resample planar stereo to `to_sr` via rubato SincFixedIn (no-op when equal).
pub fn resample_stereo(
    l: &[f32],
    r: &[f32],
    from_sr: u32,
    to_sr: u32,
) -> Result<(Vec<f32>, Vec<f32>), String> {
    if from_sr == to_sr {
        return Ok((l.to_vec(), r.to_vec()));
    }
    use rubato::{
        Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
        WindowFunction,
    };
    let params = SincInterpolationParameters {
        sinc_len: 256,
        f_cutoff: 0.95,
        interpolation: SincInterpolationType::Linear,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    const CHUNK: usize = 1024;
    let mut rs = SincFixedIn::<f32>::new(to_sr as f64 / from_sr as f64, 2.0, params, CHUNK, 2)
        .map_err(|e| e.to_string())?;
    let mut out_l = Vec::with_capacity(l.len() * to_sr as usize / from_sr as usize + CHUNK);
    let mut out_r = Vec::with_capacity(out_l.capacity());
    let mut pos = 0usize;
    while pos < l.len() {
        let end = (pos + CHUNK).min(l.len());
        let (a, b) = (&l[pos..end], &r[pos..end]);
        let waves = [a.to_vec(), b.to_vec()];
        let out = if end - pos == CHUNK {
            rs.process(&waves, None)
        } else {
            rs.process_partial(Some(&waves), None)
        }
        .map_err(|e| e.to_string())?;
        out_l.extend_from_slice(&out[0]);
        out_r.extend_from_slice(&out[1]);
        pos = end;
    }
    // drain the resampler's tail
    for _ in 0..2 {
        let out = rs
            .process_partial::<Vec<f32>>(None, None)
            .map_err(|e| e.to_string())?;
        if out[0].is_empty() {
            break;
        }
        out_l.extend_from_slice(&out[0]);
        out_r.extend_from_slice(&out[1]);
    }
    Ok((out_l, out_r))
}

/// encode.ts float→PCM16: `s < 0 ? s·0x8000 : s·0x7fff`, clamped.
#[inline]
pub fn f32_to_i16(s: f32) -> i16 {
    let s = s.clamp(-1.0, 1.0);
    if s < 0.0 {
        (s * 32768.0) as i16
    } else {
        (s * 32767.0) as i16
    }
}

/// Encode 44.1 kHz planar stereo to 16-bit PCM WAV bytes (canonical 44-byte
/// header — hound writes exactly the encodeWav layout for this spec).
pub fn encode_wav(l: &[f32], r: &[f32]) -> Result<Vec<u8>, String> {
    let spec = hound::WavSpec {
        channels: 2,
        sample_rate: OUT_SR,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec).map_err(|e| e.to_string())?;
        for i in 0..l.len().min(r.len()) {
            w.write_sample(f32_to_i16(l[i])).map_err(|e| e.to_string())?;
            w.write_sample(f32_to_i16(r[i])).map_err(|e| e.to_string())?;
        }
        w.finalize().map_err(|e| e.to_string())?;
    }
    Ok(cursor.into_inner())
}

/// Render + resample + encode to a WAV file on disk.
pub fn render_to_wav(
    project: &ProjectJSON,
    scope: &RenderScope,
    engine_sr: f32,
    path: &std::path::Path,
) -> Result<(), String> {
    render_to_wav_with_bank(project, scope, engine_sr, path, &sf_engine::SampleBank::new())
}

/// [`render_to_wav`] with a sample bank (the desktop export path).
pub fn render_to_wav_with_bank(
    project: &ProjectJSON,
    scope: &RenderScope,
    engine_sr: f32,
    path: &std::path::Path,
    bank: &sf_engine::SampleBank,
) -> Result<(), String> {
    let (l, r) = render_project_with_bank(project, scope, engine_sr, bank)?;
    let (l, r) = resample_stereo(&l, &r, engine_sr as u32, OUT_SR)?;
    let bytes = encode_wav(&l, &r)?;
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}

/// Decode a 16-bit stereo WAV (the golden files) to planar f32.
pub fn decode_wav(bytes: &[u8]) -> Result<(Vec<f32>, Vec<f32>, u32), String> {
    let mut reader =
        hound::WavReader::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let spec = reader.spec();
    if spec.bits_per_sample != 16 {
        return Err(format!("expected 16-bit PCM, got {}", spec.bits_per_sample));
    }
    let ch = spec.channels.max(1) as usize;
    let mut l = Vec::new();
    let mut r = Vec::new();
    for (i, s) in reader.samples::<i16>().enumerate() {
        let v = s.map_err(|e| e.to_string())? as f32 / 32768.0;
        if ch == 1 {
            l.push(v);
            r.push(v);
        } else if i % ch == 0 {
            l.push(v);
        } else if i % ch == 1 {
            r.push(v);
        }
    }
    Ok((l, r, spec.sample_rate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_bytes_match_encode_ts_layout() {
        let l = [0.0f32, 0.5, -0.5, 1.0];
        let r = [0.0f32, -1.0, 0.25, -0.25];
        let bytes = encode_wav(&l, &r).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 16);
        assert_eq!(u16::from_le_bytes(bytes[20..22].try_into().unwrap()), 1); // PCM
        assert_eq!(u16::from_le_bytes(bytes[22..24].try_into().unwrap()), 2); // stereo
        assert_eq!(u32::from_le_bytes(bytes[24..28].try_into().unwrap()), 44_100);
        assert_eq!(u32::from_le_bytes(bytes[28..32].try_into().unwrap()), 44_100 * 4);
        assert_eq!(u16::from_le_bytes(bytes[32..34].try_into().unwrap()), 4); // block align
        assert_eq!(u16::from_le_bytes(bytes[34..36].try_into().unwrap()), 16);
        assert_eq!(&bytes[36..40], b"data");
        let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(data as usize, 4 * 4);
        assert_eq!(bytes.len(), 44 + 16);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 36 + 16);
        // sample encoding: 1.0 → 0x7fff, −1.0 → −0x8000 (encode.ts law)
        let s3 = i16::from_le_bytes(bytes[44 + 12..44 + 14].try_into().unwrap());
        assert_eq!(s3, 0x7fff);
        let s1r = i16::from_le_bytes(bytes[44 + 6..44 + 8].try_into().unwrap());
        assert_eq!(s1r, -0x8000);
    }

    #[test]
    fn wav_round_trips_through_decode() {
        let l: Vec<f32> = (0..64).map(|i| ((i as f32) * 0.09).sin() * 0.7).collect();
        let r: Vec<f32> = l.iter().map(|s| -s).collect();
        let bytes = encode_wav(&l, &r).unwrap();
        let (dl, dr, sr) = decode_wav(&bytes).unwrap();
        assert_eq!(sr, 44_100);
        assert_eq!(dl.len(), 64);
        // 1 LSB of truncation + the asymmetric ±32767/32768 encode law
        for i in 0..64 {
            assert!((dl[i] - l[i]).abs() < 2.5 / 32768.0);
            assert!((dr[i] - r[i]).abs() < 2.5 / 32768.0);
        }
    }

    #[test]
    fn resample_preserves_duration_and_tone() {
        let from = 88_200u32;
        let n = from as usize; // 1 s of 440 Hz
        let l: Vec<f32> = (0..n)
            .map(|i| (core::f32::consts::TAU * 440.0 * i as f32 / from as f32).sin() * 0.5)
            .collect();
        let r = l.clone();
        let (ol, _or) = resample_stereo(&l, &r, from, 44_100).unwrap();
        // output = ratio·input plus the sinc filter's transient delay
        let dur = ol.len() as f32 / 44_100.0;
        assert!((1.0..1.06).contains(&dur), "duration {dur}");
        let mid = &ol[10_000..34_000];
        let rms = (mid.iter().map(|s| s * s).sum::<f32>() / mid.len() as f32).sqrt();
        assert!((rms - 0.3535).abs() < 0.03, "rms {rms}");
    }

    #[test]
    fn ksampler_with_bank_entry_renders_non_silence() {
        let json = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/projects/smoke-mono.json"),
        )
        .unwrap();
        let fx: Fixture = serde_json::from_str(&json).unwrap();
        let mut p = fx.project.clone();
        p.tracks[0].inst.r#type = "ksampler".into();
        p.tracks[0].inst.params.clear(); // with_defaults refills from schema
        p.tracks[0].inst.sample_id = Some("kb".into());

        let sr = 44_100.0f32;
        let n = (sr * 1.5) as usize;
        let sine: Vec<f32> = (0..n)
            .map(|i| (core::f32::consts::TAU * 220.0 * i as f32 / sr).sin())
            .collect();
        let mut bank = sf_engine::SampleBank::new();
        bank.insert(
            "kb".to_string(),
            std::sync::Arc::new(sf_dsp::sample::SampleBuf {
                sample_rate: sr,
                l: sine,
                r: vec![],
            }),
        );

        let (l, r) = render_project_with_bank(&p, &fx.scope, sr, &bank).unwrap();
        assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
        let rms = (l.iter().map(|s| s * s).sum::<f32>() / l.len().max(1) as f32).sqrt();
        assert!(rms > 1e-4, "ksampler render silent: rms {rms}");

        // no bank → the same project renders silence (never panics)
        let (l0, _) = render_project(&p, &fx.scope, sr).unwrap();
        let rms0 = (l0.iter().map(|s| s * s).sum::<f32>() / l0.len().max(1) as f32).sqrt();
        assert!(rms0 < 1e-6, "missing sample should be silent: rms {rms0}");
    }

    #[test]
    fn arr_scope_errors_on_empty_arrangement() {
        let json = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures/projects/smoke-mono.json"),
        )
        .unwrap();
        let fx: Fixture = serde_json::from_str(&json).unwrap();
        assert!(render_project(&fx.project, &RenderScope::Arr, 44_100.0).is_err());
    }
}
