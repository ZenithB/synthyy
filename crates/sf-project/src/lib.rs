//! Serde types for the project JSON contract, the param registry, and tick
//! math. THE SCHEMA IS FROZEN (CLAUDE.md): device type strings, param keys and
//! tick units must match `src/state/doc.ts`'s ProjectJSON exactly — a browser
//! project must open here byte-compatibly and vice versa. Lands in Phase 1.

pub mod project;
pub mod registry;

/// Ticks per quarter note — the project-wide grid (src/types.ts).
pub const PPQ: u32 = 96;
/// One 4/4 bar in ticks.
pub const BAR: u32 = PPQ * 4;

/// Transport ticks → seconds at a tempo (quarter note = PPQ ticks).
pub fn ticks_to_seconds(ticks: f64, bpm: f64) -> f64 {
    ticks / PPQ as f64 * 60.0 / bpm
}

/// Seconds → transport ticks at a tempo.
pub fn seconds_to_ticks(seconds: f64, bpm: f64) -> f64 {
    seconds * bpm / 60.0 * PPQ as f64
}

/// doc.ts `TS_TOP_MAX`.
pub const TS_TOP_MAX: u32 = 30;
/// doc.ts `TS_BOTTOMS`: legal bottom numerals (counted note value).
pub const TS_BOTTOMS: [u32; 4] = [2, 4, 8, 16];

/// Sanitized time signature (doc.ts `timeSig()`): PPQ stays the quarter note;
/// the meter only changes how ticks group into beats and bars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSig {
    pub top: u32,
    pub bottom: u32,
}

impl Default for TimeSig {
    fn default() -> Self {
        TimeSig { top: 4, bottom: 4 }
    }
}

impl TimeSig {
    /// From meta's optional `tsTop`/`tsBottom`, with doc.ts clamping: top is
    /// rounded and clamped to 1..=30, bottom must be one of 2/4/8/16 (else 4).
    pub fn from_meta(ts_top: Option<f64>, ts_bottom: Option<f64>) -> Self {
        let top = (ts_top.unwrap_or(4.0).round() as i64).clamp(1, TS_TOP_MAX as i64) as u32;
        let b = ts_bottom.unwrap_or(4.0);
        let bottom = TS_BOTTOMS
            .into_iter()
            .find(|&x| x as f64 == b)
            .unwrap_or(4);
        TimeSig { top, bottom }
    }

    /// Ticks in one beat — the bottom-numeral note value (doc.ts `beatTicks()`).
    /// PPQ=96 ⇒ ♩=96, ♪=48…
    pub fn beat_ticks(&self) -> u32 {
        PPQ * 4 / self.bottom
    }

    /// Ticks in one bar under this meter (doc.ts `barTicks()`; 4/4 ⇒ 384 = BAR).
    pub fn bar_ticks(&self) -> u32 {
        self.top * self.beat_ticks()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_seconds_round_trip() {
        // One quarter note at 120 bpm is half a second.
        assert_eq!(ticks_to_seconds(PPQ as f64, 120.0), 0.5);
        assert_eq!(seconds_to_ticks(0.5, 120.0), PPQ as f64);
        // One 4/4 bar at 96 bpm is 2.5 s.
        assert_eq!(ticks_to_seconds(BAR as f64, 96.0), 2.5);
        let t = 12345.678;
        assert!((seconds_to_ticks(ticks_to_seconds(t, 133.7), 133.7) - t).abs() < 1e-9);
    }

    #[test]
    fn time_sig_defaults_and_clamps() {
        assert_eq!(TimeSig::from_meta(None, None), TimeSig { top: 4, bottom: 4 });
        // Illegal bottoms fall back to 4; top clamps to 1..=30 (doc.ts clampInt).
        assert_eq!(TimeSig::from_meta(Some(3.0), Some(5.0)).bottom, 4);
        assert_eq!(TimeSig::from_meta(Some(99.0), Some(8.0)).top, 30);
        assert_eq!(TimeSig::from_meta(Some(0.0), Some(16.0)).top, 1);
        assert_eq!(TimeSig::from_meta(Some(6.4), Some(2.0)).top, 6);
    }

    #[test]
    fn bar_and_beat_ticks() {
        assert_eq!(TimeSig::default().bar_ticks(), BAR);
        assert_eq!(TimeSig::default().beat_ticks(), PPQ);
        assert_eq!(TimeSig { top: 6, bottom: 8 }.beat_ticks(), 48);
        assert_eq!(TimeSig { top: 6, bottom: 8 }.bar_ticks(), 288);
        assert_eq!(TimeSig { top: 7, bottom: 16 }.bar_ticks(), 168);
        assert_eq!(TimeSig { top: 2, bottom: 2 }.bar_ticks(), 384);
    }
}
