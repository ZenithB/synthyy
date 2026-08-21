//! Instrument registry — SHARED FILE, pre-wired with every frozen instrument
//! type string (src/audio/schema.ts). Device-port agents replace their own
//! `<type>.rs` (keeping the `<Type>Inst` struct name) and never edit this file.

pub mod audiobus;
pub mod drum;
pub mod duo;
pub mod fm;
pub mod granular;
pub mod keys;
pub mod ksampler;
pub mod mono;
pub mod pluck;
pub mod poly;
pub mod sampler;

use crate::traits::InstrumentDsp;

/// All frozen instrument type strings, registry order.
pub const INSTRUMENT_TYPES: [&str; 11] = [
    "poly", "duo", "fm", "mono", "pluck", "keys", "sampler", "ksampler", "granular", "drum",
    "audiobus",
];

pub fn make_instrument(type_: &str) -> Option<Box<dyn InstrumentDsp>> {
    Some(match type_ {
        "poly" => Box::new(poly::PolyInst::new()),
        "duo" => Box::new(duo::DuoInst::new()),
        "fm" => Box::new(fm::FmInst::new()),
        "mono" => Box::new(mono::MonoInst::new()),
        "pluck" => Box::new(pluck::PluckInst::new()),
        "keys" => Box::new(keys::KeysInst::new()),
        "sampler" => Box::new(sampler::SamplerInst::new()),
        "ksampler" => Box::new(ksampler::KsamplerInst::new()),
        "granular" => Box::new(granular::GranularInst::new()),
        "drum" => Box::new(drum::DrumInst::new()),
        // audiobus stays a silence "instrument": bus/audio input is wired by
        // the engine outside the InstrumentDsp (RUST_ARCH.md track chain).
        "audiobus" => Box::new(audiobus::AudiobusInst::new()),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_frozen_type_constructs_and_runs() {
        for t in INSTRUMENT_TYPES {
            let mut inst =
                make_instrument(t).unwrap_or_else(|| panic!("no instrument for {t:?}"));
            inst.prepare(44_100.0);
            inst.set_param("wave", 0.0);
            inst.note_on(60, 0.9);
            inst.trigger(64, 0.1, 0.5);
            let mut l = [0.0f32; 128];
            let mut r = [0.0f32; 128];
            inst.process(&mut l, &mut r);
            inst.note_off(60);
            inst.reset();
            assert!(l.iter().chain(r.iter()).all(|s| s.is_finite()));
        }
    }

    #[test]
    fn unknown_type_is_none() {
        assert!(make_instrument("nope").is_none());
        assert!(make_instrument("").is_none());
    }
}
