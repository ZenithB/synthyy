//! `audiobus` instrument.
//! audiobus renders silence here by design: bus/audio-clip input is
//! wired by the engine outside the instrument, so this stays a no-op even
//! after the device-port wave.
// STUB — replaced in device-port wave

use crate::traits::InstrumentDsp;

/// Silent until the real port lands (process is ADDITIVE, so a no-op mixes
/// nothing). Keep the struct name — the registry (inst/mod.rs) refers to it
/// and must not be edited.
#[derive(Default)]
pub struct AudiobusInst;

impl AudiobusInst {
    pub fn new() -> Self {
        Self
    }
}

impl InstrumentDsp for AudiobusInst {
    fn prepare(&mut self, _sample_rate: f32) {}
    fn set_param(&mut self, _key: &str, _value: f32) {}
    fn note_on(&mut self, _pitch: u8, _vel: f32) {}
    fn note_off(&mut self, _pitch: u8) {}
    fn trigger(&mut self, _pitch: u8, _dur_sec: f32, _vel: f32) {}
    fn process(&mut self, _l: &mut [f32], _r: &mut [f32]) {}
    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_renders_silence_additively() {
        let mut d = AudiobusInst::new();
        d.prepare(48_000.0);
        d.note_on(60, 1.0);
        d.trigger(64, 0.25, 0.8);
        let mut l = [0.5f32; 8]; // pre-existing content must survive (additive)
        let mut r = [0.5f32; 8];
        d.process(&mut l, &mut r);
        assert!(l.iter().chain(r.iter()).all(|s| *s == 0.5));
        d.note_off(60);
        d.reset();
    }
}
