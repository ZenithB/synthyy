//! Decoded PCM sample storage shared between the control side (SampleBank in
//! sf-engine, filled over IPC by `sample_put`) and the sampler instruments.
//! Buffers are immutable once built and handed to devices as `Arc<SampleBuf>`
//! on the control path — the audio thread only ever reads.

/// Planar f32 PCM at the SOURCE sample rate (devices resample on read).
/// `r` empty = mono: readers use the left channel for both sides.
pub struct SampleBuf {
    pub sample_rate: f32,
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

impl SampleBuf {
    /// Frame count (left channel is authoritative).
    #[inline]
    pub fn frames(&self) -> usize {
        self.l.len()
    }

    /// Duration in seconds at the source rate.
    #[inline]
    pub fn duration(&self) -> f64 {
        if self.sample_rate > 0.0 {
            self.l.len() as f64 / self.sample_rate as f64
        } else {
            0.0
        }
    }

    /// Linear-interpolated stereo read at fractional frame `pos`. Out-of-range
    /// positions read as silence (never panics — RT path).
    #[inline]
    pub fn read_lin(&self, pos: f64) -> (f32, f32) {
        let n = self.l.len();
        if n == 0 || pos < 0.0 {
            return (0.0, 0.0);
        }
        let i = pos as usize;
        if i >= n {
            return (0.0, 0.0);
        }
        let frac = (pos - i as f64) as f32;
        let i2 = if i + 1 < n { i + 1 } else { i };
        let l = self.l[i] + (self.l[i2] - self.l[i]) * frac;
        let r = if self.r.is_empty() {
            l
        } else {
            self.r[i] + (self.r[i2] - self.r[i]) * frac
        };
        (l, r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_reads_left_on_both_sides() {
        let b = SampleBuf { sample_rate: 44_100.0, l: vec![0.0, 1.0], r: vec![] };
        assert_eq!(b.frames(), 2);
        let (l, r) = b.read_lin(0.5);
        assert_eq!(l, 0.5);
        assert_eq!(r, 0.5);
    }

    #[test]
    fn out_of_range_is_silent() {
        let b = SampleBuf { sample_rate: 44_100.0, l: vec![1.0; 4], r: vec![] };
        assert_eq!(b.read_lin(-1.0), (0.0, 0.0));
        assert_eq!(b.read_lin(4.0), (0.0, 0.0));
        let empty = SampleBuf { sample_rate: 44_100.0, l: vec![], r: vec![] };
        assert_eq!(empty.read_lin(0.0), (0.0, 0.0));
    }
}
