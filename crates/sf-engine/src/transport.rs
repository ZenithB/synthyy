//! Sample-accurate tick transport: an f64 sample position advanced by the
//! callback, mapped to ticks at the current tempo (PPQ = 96). Arrangement mode
//! honors the meta loop region; session mode runs free (RUST_ARCH.md).

use sf_project::PPQ;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Session,
    Arr,
}

#[derive(Debug)]
pub struct Transport {
    sample_rate: f64,
    bpm: f64,
    playing: bool,
    mode: Mode,
    pos_ticks: f64,
    pub loop_enabled: bool,
    pub loop_start: f64,
    pub loop_end: f64,
}

impl Transport {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            sample_rate: sample_rate.max(1.0) as f64,
            bpm: 120.0,
            playing: false,
            mode: Mode::Session,
            pos_ticks: 0.0,
            loop_enabled: false,
            loop_start: 0.0,
            loop_end: 4.0 * 384.0,
        }
    }

    #[inline]
    pub fn ticks_per_sample(&self) -> f64 {
        self.bpm / 60.0 * PPQ as f64 / self.sample_rate
    }

    pub fn set_bpm(&mut self, bpm: f64) {
        self.bpm = bpm.clamp(20.0, 999.0);
    }

    pub fn bpm(&self) -> f64 {
        self.bpm
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn playing(&self) -> bool {
        self.playing
    }

    pub fn ticks(&self) -> f64 {
        self.pos_ticks
    }

    pub fn play(&mut self, mode: Mode, from_ticks: f64) {
        self.mode = mode;
        self.pos_ticks = from_ticks.max(0.0);
        self.playing = true;
    }

    pub fn stop(&mut self) {
        self.playing = false;
    }

    /// Advance by `n` samples; returns true when the arr loop region wrapped
    /// (callers must relocate their event cursors).
    pub fn advance(&mut self, n: usize) -> bool {
        if !self.playing {
            return false;
        }
        self.pos_ticks += n as f64 * self.ticks_per_sample();
        if self.mode == Mode::Arr
            && self.loop_enabled
            && self.loop_end > self.loop_start
            && self.pos_ticks >= self.loop_end - 1e-9
        {
            let len = self.loop_end - self.loop_start;
            self.pos_ticks = self.loop_start + (self.pos_ticks - self.loop_start) % len;
            return true;
        }
        false
    }

    /// Samples until `tick` from the current position (0 when already due).
    #[inline]
    pub fn samples_until(&self, tick: f64) -> f64 {
        ((tick - self.pos_ticks) / self.ticks_per_sample()).max(0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_advance_at_tempo() {
        let mut t = Transport::new(48_000.0);
        t.set_bpm(120.0);
        t.play(Mode::Session, 0.0);
        // one second = 2 quarters = 192 ticks at 120 bpm
        t.advance(48_000);
        assert!((t.ticks() - 192.0).abs() < 1e-6);
    }

    #[test]
    fn arr_loop_wraps() {
        let mut t = Transport::new(44_100.0);
        t.set_bpm(120.0);
        t.loop_enabled = true;
        t.loop_start = 384.0;
        t.loop_end = 768.0;
        t.play(Mode::Arr, 700.0);
        let tps = t.ticks_per_sample();
        let n = ((768.0 - 700.0) / tps).ceil() as usize + 10;
        assert!(t.advance(n));
        assert!(t.ticks() >= 384.0 && t.ticks() < 768.0);
    }

    #[test]
    fn session_never_wraps() {
        let mut t = Transport::new(44_100.0);
        t.loop_enabled = true;
        t.loop_start = 0.0;
        t.loop_end = 384.0;
        t.play(Mode::Session, 0.0);
        assert!(!t.advance(400_000));
        assert!(t.ticks() > 384.0);
    }
}
