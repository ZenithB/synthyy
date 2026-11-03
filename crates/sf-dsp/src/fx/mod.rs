//! Effect registry — SHARED FILE, pre-wired with every frozen effect type
//! string (src/audio/schema.ts). Device-port agents replace their own
//! `<type>.rs` (keeping the `<Type>Fx` struct name) and never edit this file.

pub mod autofilt;
pub mod autopan;
pub mod autotune;
pub mod cheby;
pub mod chorus;
pub mod comp;
pub mod crush;
pub mod delay;
pub mod destroyer;
pub mod dist;
pub mod duck;
pub mod eq;
pub mod eq7;
pub mod filter;
pub mod gate;
pub mod mbcomp;
pub mod opto;
pub mod phaser;
pub mod pingpong;
pub mod plate;
pub mod reverb;
pub mod shift;
pub mod trem;
pub mod vib;
pub mod widen;

use crate::traits::EffectDsp;

/// All frozen effect type strings, registry order.
pub const EFFECT_TYPES: [&str; 25] = [
    "eq", "filter", "delay", "reverb", "plate", "eq7", "chorus", "dist", "crush", "comp", "opto",
    "mbcomp", "gate", "phaser", "pingpong", "autofilt", "trem", "autopan", "vib", "cheby", "widen",
    "shift", "duck", "autotune", "destroyer",
];

pub fn make_effect(type_: &str) -> Option<Box<dyn EffectDsp>> {
    Some(match type_ {
        "eq" => Box::new(eq::EqFx::new()),
        "filter" => Box::new(filter::FilterFx::new()),
        "delay" => Box::new(delay::DelayFx::new()),
        "reverb" => Box::new(reverb::ReverbFx::new()),
        "plate" => Box::new(plate::PlateFx::new()),
        "eq7" => Box::new(eq7::Eq7Fx::new()),
        "chorus" => Box::new(chorus::ChorusFx::new()),
        "dist" => Box::new(dist::DistFx::new()),
        "crush" => Box::new(crush::CrushFx::new()),
        "comp" => Box::new(comp::CompFx::new()),
        "opto" => Box::new(opto::OptoFx::new()),
        "mbcomp" => Box::new(mbcomp::MbcompFx::new()),
        "gate" => Box::new(gate::GateFx::new()),
        "phaser" => Box::new(phaser::PhaserFx::new()),
        "pingpong" => Box::new(pingpong::PingpongFx::new()),
        "autofilt" => Box::new(autofilt::AutofiltFx::new()),
        "trem" => Box::new(trem::TremFx::new()),
        "autopan" => Box::new(autopan::AutopanFx::new()),
        "vib" => Box::new(vib::VibFx::new()),
        "cheby" => Box::new(cheby::ChebyFx::new()),
        "widen" => Box::new(widen::WidenFx::new()),
        "shift" => Box::new(shift::ShiftFx::new()),
        "duck" => Box::new(duck::DuckFx::new()),
        "autotune" => Box::new(autotune::AutotuneFx::new()),
        "destroyer" => Box::new(destroyer::DestroyerFx::new()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frozen_type_constructs_and_runs() {
        for t in EFFECT_TYPES {
            let mut fx = make_effect(t).unwrap_or_else(|| panic!("no effect for {t:?}"));
            fx.prepare(44_100.0);
            fx.set_param("mix", 0.5);
            let mut l = [0.1f32; 128];
            let mut r = [0.1f32; 128];
            fx.process(&mut l, &mut r);
            fx.tick(0.0, true, 120.0);
            fx.trigger();
            assert!(fx.gr_db() <= 0.0);
            assert_eq!(fx.gr_bands(), [0.0; 3]);
            fx.reset();
            assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
        }
    }

    #[test]
    fn unknown_type_is_none() {
        assert!(make_effect("nope").is_none());
        assert!(make_effect("").is_none());
    }
}
