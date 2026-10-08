//! Offline mono resampling to 16 kHz for decoded files (WP1).
// clippy::ptr_arg: the stub never grows `out`; WP1's implementation will.
#![allow(dead_code, unused_variables, unused_imports, clippy::ptr_arg)]

/// Offline mono resampler: any rate → out_hz, delay-compensated, exact output
/// length round(in_frames × out / in). Uses rubato::FftFixedInOut<f32> with
/// chunk hint 4096 and 1 channel; in_hz == out_hz passes samples through.
pub struct StreamResampler {
    // WP1: private state.
    _private: (),
}

impl StreamResampler {
    pub fn new(in_hz: u32, out_hz: u32) -> anyhow::Result<Self> {
        todo!("WP1")
    }

    pub fn in_hz(&self) -> u32 {
        todo!("WP1")
    }

    pub fn push(&mut self, src: &[f32], out: &mut Vec<f32>) -> anyhow::Result<()> {
        todo!("WP1")
    }

    /// Zero-pads the tail, flushes the delay line (at most 8 rounds), trims to the exact length.
    pub fn finish(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        todo!("WP1")
    }
}
