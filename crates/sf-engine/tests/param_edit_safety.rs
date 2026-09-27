//! Editing a device parameter must never latch the device into silence or NaN.
//!
//! On desktop a param edit takes the `param_set` fast path, which does NOT
//! rebuild the graph — so a device that poisons its own state stays broken
//! until something forces a rebuild. That is the "playback stops after editing
//! a device; mute/unmute brings it back" bug: toggling mute re-pushes the
//! project, which builds fresh device instances and clears the bad state.

use sf_dsp::{make_effect, EffectDsp};
use sf_project::registry::fx_params;

const SR: f32 = 48_000.0;

fn block(phase: &mut f32, hz: f32) -> (Vec<f32>, Vec<f32>) {
    let mut l = vec![0.0f32; 128];
    for s in l.iter_mut() {
        *s = (*phase * std::f32::consts::TAU).sin() * 0.3;
        *phase = (*phase + hz / SR).fract();
    }
    let r = l.clone();
    (l, r)
}

/// Peak of `blocks` blocks pushed through the device, plus a finite check.
fn run(dev: &mut dyn EffectDsp, phase: &mut f32, blocks: usize) -> (f32, bool) {
    let (mut peak, mut finite) = (0.0f32, true);
    for _ in 0..blocks {
        let (mut l, mut r) = block(phase, 220.0);
        dev.process(&mut l, &mut r);
        for v in l.iter().chain(r.iter()) {
            if v.is_finite() { peak = peak.max(v.abs()) } else { finite = false }
        }
    }
    (peak, finite)
}

fn fresh(t: &str) -> Option<Box<dyn EffectDsp>> {
    let mut dev = make_effect(t)?;
    dev.prepare(SR);
    for s in fx_params(t) {
        dev.set_param(s.key, s.def);
    }
    Some(dev)
}

#[test]
fn param_edits_never_produce_nan() {
    let mut bad = Vec::new();
    for t in sf_dsp::fx::EFFECT_TYPES {
        for p in fx_params(t) {
            for (label, v) in [("min", p.min), ("def", p.def), ("max", p.max)] {
                let Some(mut dev) = fresh(t) else { continue };
                let mut ph = 0.0;
                run(&mut *dev, &mut ph, 8);
                dev.set_param(p.key, v);
                if !run(&mut *dev, &mut ph, 40).1 {
                    bad.push(format!("{t}.{}={label}({v})", p.key));
                }
            }
        }
    }
    assert!(bad.is_empty(), "NaN/inf after a param edit: {bad:#?}");
}

#[test]
fn a_param_edit_never_latches_silence() {
    let mut bad = Vec::new();
    for t in sf_dsp::fx::EFFECT_TYPES {
        for p in fx_params(t) {
            for (label, v) in [("min", p.min), ("max", p.max)] {
                let Some(mut dev) = fresh(t) else { continue };
                let mut ph = 0.0;
                let (base, _) = run(&mut *dev, &mut ph, 16);
                if base < 1e-4 {
                    continue; // silent at defaults (gate-like) — nothing to latch
                }
                dev.set_param(p.key, v);
                run(&mut *dev, &mut ph, 40);
                dev.set_param(p.key, p.def); // user puts it back
                let (after, finite) = run(&mut *dev, &mut ph, 200); // 0.5 s to recover
                if finite && after < base * 0.05 {
                    bad.push(format!("{t}.{} {label}({v}): {base:.4} → {after:.6}", p.key));
                }
            }
        }
    }
    assert!(bad.is_empty(), "still silent after restoring the param: {bad:#?}");
}
