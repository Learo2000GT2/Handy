//! Offline mono resampling to 16 kHz for decoded files.
//!
//! Unlike `audio_toolkit::audio::FrameResampler` (live capture: fixed-size
//! frames, filter delay kept, tail padded), this drops the filter delay and
//! produces exactly round(n × out / in) samples, so times in the resampled
//! audio match times in the source file.

use anyhow::{ensure, Context};
use rubato::{FftFixedInOut, Resampler};

/// Requested input frames per FFT step; rubato rounds it up to a whole number
/// of rate units (for example 4410 frames at 44.1 kHz).
const CHUNK_HINT: usize = 4096;
/// Input rates outside this range are rejected: a corrupt header could
/// otherwise ask for an FFT as large as the bogus rate.
const MIN_RATE_HZ: u32 = 1_000;
const MAX_RATE_HZ: u32 = 768_000;
/// `finish` needs one round for the partial block and at most two more for
/// the delay line (half a block); the cap only guards against a logic error.
const MAX_FLUSH_ROUNDS: usize = 8;

/// Offline mono resampler: any rate → out_hz, delay-compensated, exact output
/// length round(in_frames × out / in). Uses rubato::FftFixedInOut<f32> with
/// chunk hint 4096 and 1 channel; in_hz == out_hz passes samples through.
pub struct StreamResampler {
    /// None when in_hz == out_hz.
    inner: Option<FftFixedInOut<f32>>,
    in_hz: u32,
    out_hz: u32,
    /// Input frames per FFT step.
    chunk_in: usize,
    in_buf: Vec<f32>,
    out_buf: Vec<Vec<f32>>,
    /// Leading output frames (the filter delay) still to drop.
    skip: usize,
    /// Input frames pushed since the stream started.
    in_total: u64,
    /// Output frames handed out since the stream started.
    emitted: u64,
}

impl StreamResampler {
    pub fn new(in_hz: u32, out_hz: u32) -> anyhow::Result<Self> {
        for rate in [in_hz, out_hz] {
            ensure!(
                (MIN_RATE_HZ..=MAX_RATE_HZ).contains(&rate),
                "unsupported sample rate {rate} Hz"
            );
        }
        let inner = if in_hz == out_hz {
            None
        } else {
            Some(
                FftFixedInOut::<f32>::new(in_hz as usize, out_hz as usize, CHUNK_HINT, 1)
                    .with_context(|| format!("cannot resample {in_hz} Hz to {out_hz} Hz"))?,
            )
        };
        let (chunk_in, skip, out_buf) = match &inner {
            Some(resampler) => (
                resampler.input_frames_next(),
                resampler.output_delay(),
                resampler.output_buffer_allocate(true),
            ),
            None => (0, 0, Vec::new()),
        };
        Ok(Self {
            inner,
            in_hz,
            out_hz,
            chunk_in,
            in_buf: Vec::with_capacity(chunk_in),
            out_buf,
            skip,
            in_total: 0,
            emitted: 0,
        })
    }

    pub fn in_hz(&self) -> u32 {
        self.in_hz
    }

    /// Output length of the stream so far, as `finish` will make it:
    /// round(input frames pushed × out / in).
    pub fn output_len(&self) -> u64 {
        self.expected()
    }

    pub fn push(&mut self, mut src: &[f32], out: &mut Vec<f32>) -> anyhow::Result<()> {
        self.in_total += src.len() as u64;
        if self.inner.is_none() {
            out.extend_from_slice(src);
            self.emitted += src.len() as u64;
            return Ok(());
        }
        while !src.is_empty() {
            let take = (self.chunk_in - self.in_buf.len()).min(src.len());
            self.in_buf.extend_from_slice(&src[..take]);
            src = &src[take..];
            if self.in_buf.len() == self.chunk_in {
                self.process_block(false, out)?;
            }
        }
        Ok(())
    }

    /// Zero-pads the tail, flushes the delay line (at most 8 rounds), trims to the exact length.
    ///
    /// Afterwards the resampler is reset and can take a new stream.
    pub fn finish(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        let flushed = self.flush(out);
        self.reset();
        flushed
    }

    fn flush(&mut self, out: &mut Vec<f32>) -> anyhow::Result<()> {
        if self.inner.is_none() {
            return Ok(());
        }
        let target = self.expected();
        let mut rounds = 0;
        while self.emitted < target && rounds < MAX_FLUSH_ROUNDS {
            self.in_buf.resize(self.chunk_in, 0.0);
            self.process_block(true, out)?;
            rounds += 1;
        }
        ensure!(
            self.emitted == target,
            "resampler flushed {} of {target} samples",
            self.emitted
        );
        Ok(())
    }

    /// Output length for the input seen so far: round(in_total × out / in).
    fn expected(&self) -> u64 {
        let (in_hz, out_hz) = (u128::from(self.in_hz), u128::from(self.out_hz));
        ((u128::from(self.in_total) * out_hz + in_hz / 2) / in_hz) as u64
    }

    /// Resamples the full `in_buf`, dropping the leading delay and, when
    /// flushing, anything past the exact output length.
    fn process_block(&mut self, flushing: bool, out: &mut Vec<f32>) -> anyhow::Result<()> {
        let room = self.expected().saturating_sub(self.emitted);
        let resampler = self
            .inner
            .as_mut()
            .context("pass-through resampler has no blocks")?;
        let (_, produced) =
            resampler.process_into_buffer(&[&self.in_buf[..]], &mut self.out_buf[..], None)?;
        self.in_buf.clear();

        let mut data = &self.out_buf[0][..produced];
        let dropped = self.skip.min(data.len());
        data = &data[dropped..];
        self.skip -= dropped;
        if flushing {
            data = &data[..data.len().min(usize::try_from(room).unwrap_or(usize::MAX))];
        }
        out.extend_from_slice(data);
        self.emitted += data.len() as u64;
        Ok(())
    }

    fn reset(&mut self) {
        if let Some(resampler) = self.inner.as_mut() {
            resampler.reset();
            self.skip = resampler.output_delay();
        }
        self.in_buf.clear();
        self.in_total = 0;
        self.emitted = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    const RATES: [u32; 8] = [
        8_000, 11_025, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000,
    ];

    fn resample_all(in_hz: u32, input: &[f32]) -> Vec<f32> {
        let mut resampler = StreamResampler::new(in_hz, 16_000).unwrap();
        let mut out = Vec::new();
        resampler.push(input, &mut out).unwrap();
        resampler.finish(&mut out).unwrap();
        out
    }

    fn expected_len(n: usize, in_hz: u32) -> usize {
        ((n as f64) * 16_000.0 / f64::from(in_hz)).round() as usize
    }

    fn sine(rate: u32, freq: f64, amplitude: f64, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| (amplitude * (2.0 * PI * freq * i as f64 / f64::from(rate)).sin()) as f32)
            .collect()
    }

    /// Amplitude of `freq` in `samples` (Goertzel). Exact when the window
    /// holds a whole number of cycles.
    fn tone_amplitude(samples: &[f32], rate: u32, freq: f64) -> f64 {
        let coeff = 2.0 * (2.0 * PI * freq / f64::from(rate)).cos();
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &x in samples {
            let s0 = f64::from(x) + coeff * s1 - s2;
            s2 = s1;
            s1 = s0;
        }
        let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
        2.0 * power.max(0.0).sqrt() / samples.len() as f64
    }

    fn rms(samples: &[f32]) -> f64 {
        (samples.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>() / samples.len() as f64).sqrt()
    }

    /// Deterministic xorshift, so the tests need no extra crates.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    #[test]
    fn output_length_is_exact_for_common_rates() {
        for rate in RATES {
            for n in [0, 1, 7, 4_095, 4_096, 4_410, 33_333, rate as usize * 3 + 17] {
                let input = vec![0.25f32; n];
                let out = resample_all(rate, &input);
                assert_eq!(out.len(), expected_len(n, rate), "{rate} Hz, {n} frames");

                let mut resampler = StreamResampler::new(rate, 16_000).unwrap();
                resampler.push(&input, &mut Vec::new()).unwrap();
                assert_eq!(resampler.output_len(), out.len() as u64, "{rate} Hz, {n}");
            }
        }
    }

    #[test]
    fn same_rate_passes_through_bit_identical() {
        let input: Vec<f32> = (0..50_001)
            .map(|i| ((i * 7919) % 2001) as f32 / 1000.0 - 1.0)
            .collect();
        assert_eq!(resample_all(16_000, &input), input);
    }

    #[test]
    fn block_sizes_do_not_change_the_output() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for rate in [44_100, 48_000, 11_025] {
            let input: Vec<f32> = (0..rate as usize * 2 + 123)
                .map(|_| (rng.next() % 2_000) as f32 / 1_000.0 - 1.0)
                .collect();
            let whole = resample_all(rate, &input);

            let mut resampler = StreamResampler::new(rate, 16_000).unwrap();
            let mut pieces = Vec::new();
            let mut rest = &input[..];
            while !rest.is_empty() {
                let take = ((rng.next() % 9_000) as usize).min(rest.len());
                resampler.push(&rest[..take], &mut pieces).unwrap();
                rest = &rest[take..];
            }
            resampler.finish(&mut pieces).unwrap();

            assert_eq!(pieces.len(), whole.len());
            for (a, b) in pieces.iter().zip(&whole) {
                assert!((a - b).abs() <= 1e-6, "{rate} Hz: {a} vs {b}");
            }
        }
    }

    #[test]
    fn finish_resets_for_a_new_stream() {
        let input = sine(44_100, 440.0, 0.5, 30_000);
        let mut resampler = StreamResampler::new(44_100, 16_000).unwrap();
        let mut first = Vec::new();
        resampler.push(&input, &mut first).unwrap();
        resampler.finish(&mut first).unwrap();
        let mut second = Vec::new();
        resampler.push(&input, &mut second).unwrap();
        resampler.finish(&mut second).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn passband_tone_keeps_its_level() {
        for rate in RATES {
            let input = sine(rate, 1_000.0, 0.5, rate as usize * 2);
            let out = resample_all(rate, &input);
            // A whole number of 1 kHz cycles away from both edges.
            let window = &out[4_000..4_000 + 16_000];
            let amplitude = tone_amplitude(window, 16_000, 1_000.0);
            let db = 20.0 * (amplitude / 0.5).log10();
            assert!(db.abs() <= 1.0, "{rate} Hz: {db:.2} dB");
        }
    }

    #[test]
    fn tone_above_the_output_nyquist_is_removed() {
        let input = sine(48_000, 9_000.0, 0.5, 96_000);
        let out = resample_all(48_000, &input);
        let level = rms(&out[2_000..out.len() - 2_000]) / rms(&input);
        let db = 20.0 * level.log10();
        assert!(db <= -40.0, "9 kHz only attenuated by {db:.1} dB");
    }

    #[test]
    fn impulse_lands_at_the_matching_output_index() {
        for rate in RATES {
            for at in [0, rate as usize / 10, rate as usize / 2 + 3] {
                let mut input = vec![0.0f32; rate as usize];
                input[at] = 1.0;
                let out = resample_all(rate, &input);
                let peak = out
                    .iter()
                    .enumerate()
                    .max_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                    .map(|(i, _)| i)
                    .unwrap();
                let want = (at as f64 * 16_000.0 / f64::from(rate)).round() as i64;
                assert!(
                    (peak as i64 - want).abs() <= 2,
                    "{rate} Hz impulse at {at}: peak {peak}, want {want}"
                );
            }
        }
    }

    #[test]
    fn rejects_unusable_rates() {
        for (in_hz, out_hz) in [(0, 16_000), (16_000, 0), (500, 16_000), (5_000_000, 16_000)] {
            assert!(
                StreamResampler::new(in_hz, out_hz).is_err(),
                "{in_hz} → {out_hz}"
            );
        }
        assert_eq!(
            StreamResampler::new(44_100, 16_000).unwrap().in_hz(),
            44_100
        );
    }
}
