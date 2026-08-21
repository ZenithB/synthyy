//! Device trait contract — verbatim from docs/RUST_ARCH.md ("Device traits").
//! When this file and the doc disagree, fix this file.

pub trait EffectDsp: Send {
    fn prepare(&mut self, sample_rate: f32);
    /// Param keys are the FROZEN schema keys (src/audio/schema.ts). Values are
    /// raw (unnormalized) spec-range values. Called at block boundaries only.
    fn set_param(&mut self, key: &str, value: f32);
    fn process(&mut self, l: &mut [f32], r: &mut [f32]);
    /// Tempo-synced devices (duck, autofilt, trem…): transport position in ticks.
    fn tick(&mut self, _pos_ticks: f64, _playing: bool, _bpm: f64) {}
    /// Sidechain trigger (duck): a source note fired.
    fn trigger(&mut self) {}
    /// Gain reduction in dB (≤ 0) for GR meters; per-band variant for mbcomp.
    fn gr_db(&self) -> f32 {
        0.0
    }
    fn gr_bands(&self) -> [f32; 3] {
        [0.0; 3]
    }
    fn reset(&mut self) {}
}

pub trait InstrumentDsp: Send {
    fn prepare(&mut self, sample_rate: f32);
    fn set_param(&mut self, key: &str, value: f32);
    fn note_on(&mut self, pitch: u8, vel: f32);
    fn note_off(&mut self, pitch: u8);
    /// Fixed-length trigger (scheduler path): duration in seconds.
    fn trigger(&mut self, pitch: u8, dur_sec: f32, vel: f32);
    /// ADDITIVE render: mix into l/r (the track buffer may carry other voices).
    fn process(&mut self, l: &mut [f32], r: &mut [f32]);
    /// Load/clear a sample (control path — Arc clone is fine here). Slot 0 is
    /// the instrument's main sample; drum pads use slots 0–7. Defaulted no-op
    /// so non-sampled instruments ignore it.
    fn set_sample(&mut self, _slot: u32, _buf: Option<std::sync::Arc<crate::sample::SampleBuf>>) {}
    fn reset(&mut self);
}
