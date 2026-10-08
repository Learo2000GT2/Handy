//! Silence-aware chunking of 16 kHz mono audio, and the per-chunk speech gate.
//!
//! Chunks never overlap and cover the input exactly, so a chunk's offset in
//! the file is simply `start_sample / 16` ms. Each cut sits at the centre of
//! the quietest 300 ms window near the end of the chunk, so words are rarely
//! split.

use crate::audio_toolkit::{EarshotVad, VoiceActivityDetector};

pub const SAMPLES_PER_MS: u64 = 16;

/// Cut-search step: 10 ms.
const HOP: usize = 160;
/// Length of the window whose energy is measured: 300 ms.
const QUIET: usize = 4_800;
const MIN_MAX_CHUNK_MS: u32 = 5_000;
/// No cut is ever placed earlier than this into a chunk.
const MIN_CUT_MS: u32 = 2_000;
/// The earliest candidate's energy is weighted by 1 + LATE_BIAS (the latest
/// by 1), so among similar pauses the later one wins and chunks stay long.
const LATE_BIAS: f64 = 0.1;

/// Below −60 dBFS peak a chunk is treated as silence.
const SILENCE_PEAK: f32 = 1e-3;
/// Same Earshot threshold as live dictation.
const VAD_THRESHOLD: f32 = 0.5;
/// 6 Earshot frames of 16 ms: about 100 ms of speech.
const MIN_SPEECH_FRAMES: usize = 6;
/// Chunks shorter than 1 s are zero-padded to this length (some models
/// reject very short input).
const MIN_TRANSCRIBE_SAMPLES: usize = 16_000;

#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub index: u32,
    pub start_sample: u64,
    pub samples: Vec<f32>,
}

impl AudioChunk {
    /// start_sample / 16
    pub fn start_ms(&self) -> u64 {
        self.start_sample / SAMPLES_PER_MS
    }

    /// samples.len() / 16
    pub fn len_ms(&self) -> u64 {
        self.samples.len() as u64 / SAMPLES_PER_MS
    }

    /// (start_sample + samples.len()) / 16: equals the next chunk's `start_ms`.
    pub fn end_ms(&self) -> u64 {
        (self.start_sample + self.samples.len() as u64) / SAMPLES_PER_MS
    }
}

/// Streaming splitter for 16 kHz mono. Never holds more than max_chunk samples.
pub struct SilenceChunker {
    /// None: never split.
    max_len: Option<usize>,
    /// Earliest allowed cut, in samples from the chunk start.
    min_cut: usize,
    buf: Vec<f32>,
    /// Absolute position of `buf[0]`.
    buf_start: u64,
    next_index: u32,
}

impl SilenceChunker {
    /// max_chunk_ms == 0: never split (one chunk with all input).
    /// Otherwise max_chunk_ms >= 5000 (assert), and the cut is searched with
    /// min_cut = max(max - search_window, 2000) ms.
    pub fn new(max_chunk_ms: u32, search_window_ms: u32) -> Self {
        let (max_len, min_cut) = if max_chunk_ms == 0 {
            (None, 0)
        } else {
            assert!(
                max_chunk_ms >= MIN_MAX_CHUNK_MS,
                "chunks must be at least {MIN_MAX_CHUNK_MS} ms, got {max_chunk_ms} ms"
            );
            let min_cut_ms = max_chunk_ms
                .saturating_sub(search_window_ms)
                .max(MIN_CUT_MS);
            (Some(ms_to_samples(max_chunk_ms)), ms_to_samples(min_cut_ms))
        };
        Self {
            max_len,
            min_cut,
            buf: Vec::with_capacity(max_len.unwrap_or(0)),
            buf_start: 0,
            next_index: 0,
        }
    }

    pub fn push(
        &mut self,
        mut src: &[f32],
        emit: &mut dyn FnMut(AudioChunk) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let Some(max_len) = self.max_len else {
            self.buf.extend_from_slice(src);
            return Ok(());
        };
        while !src.is_empty() {
            // Cut only once more audio is known to follow, so input of
            // exactly the maximum length stays one chunk.
            if self.buf.len() == max_len {
                let cut = self.find_cut();
                let mut rest = Vec::with_capacity(max_len);
                rest.extend_from_slice(&self.buf[cut..]);
                self.buf.truncate(cut);
                let samples = std::mem::replace(&mut self.buf, rest);
                self.emit_chunk(samples, emit)?;
            }
            let take = (max_len - self.buf.len()).min(src.len());
            self.buf.extend_from_slice(&src[..take]);
            src = &src[take..];
        }
        Ok(())
    }

    /// Emits the buffered tail (if any).
    pub fn finish(
        &mut self,
        emit: &mut dyn FnMut(AudioChunk) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let samples = std::mem::take(&mut self.buf);
        self.emit_chunk(samples, emit)
    }

    fn emit_chunk(
        &mut self,
        samples: Vec<f32>,
        emit: &mut dyn FnMut(AudioChunk) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let chunk = AudioChunk {
            index: self.next_index,
            start_sample: self.buf_start,
            samples,
        };
        self.next_index += 1;
        self.buf_start += chunk.samples.len() as u64;
        emit(chunk)
    }

    /// Where to end the (full) buffered chunk: the centre of the 300 ms
    /// window, starting on a 10 ms step and lying inside the buffer, whose
    /// centre is in [min_cut, max) and whose energy, weighted towards later
    /// cuts, is lowest. Falls back to a hard cut at the maximum when the
    /// search range is empty.
    fn find_cut(&self) -> usize {
        let len = self.buf.len();
        let Some(last) = len.checked_sub(QUIET).map(|start| start / HOP) else {
            return len;
        };
        let first = self.min_cut.saturating_sub(QUIET / 2).div_ceil(HOP);
        if first > last {
            return len;
        }

        // prefix[i] = energy of hops first .. first + i.
        let hops_per_window = QUIET / HOP;
        let hop_count = last - first + hops_per_window;
        let mut prefix = Vec::with_capacity(hop_count + 1);
        let mut energy = 0.0f64;
        prefix.push(energy);
        for hop in self.buf[first * HOP..(first + hop_count) * HOP].chunks_exact(HOP) {
            energy += hop
                .iter()
                .map(|&s| f64::from(s) * f64::from(s))
                .sum::<f64>();
            prefix.push(energy);
        }

        let span = len.saturating_sub(self.min_cut).max(1) as f64;
        let mut best = (f64::INFINITY, len);
        // Latest first, so exact ties (digital silence) keep the latest cut.
        for start_hop in (first..=last).rev() {
            let i = start_hop - first;
            let window_energy = prefix[i + hops_per_window] - prefix[i];
            let centre = start_hop * HOP + QUIET / 2;
            let earliness = len.saturating_sub(centre) as f64 / span;
            let score = window_energy * (1.0 + LATE_BIAS * earliness);
            if score < best.0 {
                best = (score, centre);
            }
        }
        best.1
    }
}

fn ms_to_samples(ms: u32) -> usize {
    ms as usize * SAMPLES_PER_MS as usize
}

pub enum ChunkVerdict {
    Transcribe(Vec<f32>),
    SkipSilent,
}

pub struct SpeechGate {
    vad: Option<EarshotVad>,
}

impl SpeechGate {
    /// use_vad=false: peak gate only. true: also EarshotVad(0.5).
    pub fn new(use_vad: bool) -> anyhow::Result<Self> {
        let vad = if use_vad {
            Some(EarshotVad::new(VAD_THRESHOLD)?)
        } else {
            None
        };
        Ok(Self { vad })
    }

    /// SkipSilent if peak < 1e-3 (−60 dBFS), or (use_vad and fewer than 6 of
    /// the 256-sample frames are speech; the VAD is reset per chunk).
    /// Otherwise returns the samples, zero-padded at the end to 16,000 if shorter than 1 s.
    pub fn prepare(&mut self, samples: &[f32]) -> ChunkVerdict {
        // f32::max ignores NaN, so a stray non-finite sample cannot make
        // silence look loud.
        let peak = samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        if peak < SILENCE_PEAK {
            return ChunkVerdict::SkipSilent;
        }

        if let Some(vad) = self.vad.as_mut() {
            vad.reset();
            let mut speech_frames = 0;
            for frame in samples.chunks_exact(vad.frame_samples()) {
                // A frame the VAD cannot judge counts as speech: never drop
                // audio on doubt.
                if vad.is_voice(frame).unwrap_or(true) {
                    speech_frames += 1;
                    if speech_frames >= MIN_SPEECH_FRAMES {
                        break;
                    }
                }
            }
            if speech_frames < MIN_SPEECH_FRAMES {
                return ChunkVerdict::SkipSilent;
            }
        }

        let mut audio = Vec::with_capacity(samples.len().max(MIN_TRANSCRIBE_SAMPLES));
        audio.extend_from_slice(samples);
        if audio.len() < MIN_TRANSCRIBE_SAMPLES {
            audio.resize(MIN_TRANSCRIBE_SAMPLES, 0.0);
        }
        ChunkVerdict::Transcribe(audio)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_MS: u32 = 28_000;
    const WINDOW_MS: u32 = 10_000;

    /// Deterministic xorshift, so the tests need no extra crates.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// Uniform in [-amplitude, amplitude), never exactly 0.
        fn noise(&mut self, amplitude: f32, len: usize) -> Vec<f32> {
            (0..len)
                .map(|_| {
                    let unit = (self.next() >> 40) as f32 / (1u64 << 24) as f32;
                    let sample = (unit * 2.0 - 1.0) * amplitude;
                    if sample == 0.0 {
                        amplitude / 2.0
                    } else {
                        sample
                    }
                })
                .collect()
        }
    }

    fn split(input: &[f32], max_ms: u32, window_ms: u32, block: usize) -> Vec<AudioChunk> {
        let mut chunker = SilenceChunker::new(max_ms, window_ms);
        let mut chunks = Vec::new();
        let mut emit = |chunk: AudioChunk| {
            chunks.push(chunk);
            Ok(())
        };
        for piece in input.chunks(block.max(1)) {
            chunker.push(piece, &mut emit).unwrap();
        }
        chunker.finish(&mut emit).unwrap();
        chunks
    }

    fn assert_contiguous(chunks: &[AudioChunk], input: &[f32]) {
        let mut next = 0u64;
        for (i, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.index, i as u32);
            assert_eq!(chunk.start_sample, next);
            assert!(!chunk.samples.is_empty());
            let start = chunk.start_sample as usize;
            assert_eq!(chunk.samples[..], input[start..start + chunk.samples.len()]);
            next += chunk.samples.len() as u64;
        }
        assert_eq!(next, input.len() as u64);
    }

    #[test]
    fn short_input_is_one_identical_chunk() {
        let input = Rng(7).noise(0.5, 16_000 * 12 + 5);
        let chunks = split(&input, MAX_MS, WINDOW_MS, 1_000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].samples, input);
        assert_eq!(chunks[0].start_sample, 0);
    }

    #[test]
    fn input_of_exactly_the_maximum_is_one_chunk() {
        let input = Rng(8).noise(0.5, ms_to_samples(MAX_MS));
        for block in [4_096, input.len()] {
            let chunks = split(&input, MAX_MS, WINDOW_MS, block);
            assert_eq!(chunks.len(), 1);
            assert_eq!(chunks[0].samples, input);
        }
        let longer = Rng(8).noise(0.5, ms_to_samples(MAX_MS) + 1);
        assert_eq!(split(&longer, MAX_MS, WINDOW_MS, 4_096).len(), 2);
    }

    #[test]
    fn cuts_land_in_the_silent_gaps() {
        let mut rng = Rng(42);
        let total = 16_000 * 95;
        let mut input = rng.noise(0.3, total);
        // 400 ms of digital silence every 7 s, so every 10 s search window
        // holds at least one gap.
        let gap = 6_400;
        let mut gap_centres = Vec::new();
        let mut start = 16_000 * 5;
        while start + gap < total {
            input[start..start + gap].fill(0.0);
            gap_centres.push(start + gap / 2);
            start += 16_000 * 7;
        }

        let chunks = split(&input, MAX_MS, WINDOW_MS, 3_000);
        assert_contiguous(&chunks, &input);
        assert!(chunks.len() >= 4, "{} chunks", chunks.len());
        for chunk in &chunks[1..] {
            let cut = chunk.start_sample as i64;
            let nearest = gap_centres
                .iter()
                .map(|&c| (c as i64 - cut).abs())
                .min()
                .unwrap();
            assert!(
                nearest <= 150 * 16,
                "cut at {cut} is {nearest} samples from a gap"
            );
        }
    }

    #[test]
    fn chunks_respect_the_maximum_and_cover_the_input() {
        let input = Rng(3).noise(0.4, 16_000 * 200 + 77);
        for (max_ms, window_ms) in [
            (MAX_MS, WINDOW_MS),
            (24_000, 10_000),
            (5_000, 2_500),
            (6_000, 0),
        ] {
            let chunks = split(&input, max_ms, window_ms, 10_007);
            assert_contiguous(&chunks, &input);
            let min_cut = ms_to_samples(max_ms.saturating_sub(window_ms).max(MIN_CUT_MS));
            for chunk in &chunks[..chunks.len() - 1] {
                assert!(chunk.samples.len() <= ms_to_samples(max_ms));
                assert!(chunk.samples.len() >= min_cut, "{max_ms}/{window_ms}");
            }
        }
    }

    #[test]
    fn push_sizes_do_not_change_the_boundaries() {
        let mut rng = Rng(99);
        let mut input = rng.noise(0.5, 16_000 * 90);
        for i in (0..input.len()).step_by(16_000 * 3) {
            let end = (i + 3_000).min(input.len());
            input[i..end].iter_mut().for_each(|s| *s *= 0.01);
        }
        let reference = split(&input, MAX_MS, WINDOW_MS, input.len());
        let expected: Vec<u64> = reference.iter().map(|c| c.start_sample).collect();
        assert!(expected.len() >= 3);
        for block in [1, 160, 4_799, 65_536, 448_000, 448_001] {
            let chunks = split(&input, MAX_MS, WINDOW_MS, block);
            let starts: Vec<u64> = chunks.iter().map(|c| c.start_sample).collect();
            assert_eq!(starts, expected, "block {block}");
        }

        // Random push sizes, as a decoder delivers them.
        let mut chunker = SilenceChunker::new(MAX_MS, WINDOW_MS);
        let mut starts = Vec::new();
        let mut emit = |chunk: AudioChunk| {
            starts.push(chunk.start_sample);
            Ok(())
        };
        let mut rest = &input[..];
        while !rest.is_empty() {
            let take = ((rng.next() % 20_000) as usize).min(rest.len());
            chunker.push(&rest[..take], &mut emit).unwrap();
            rest = &rest[take..];
        }
        chunker.finish(&mut emit).unwrap();
        assert_eq!(starts, expected, "random blocks");
    }

    #[test]
    fn empty_input_gives_no_chunks() {
        assert!(split(&[], MAX_MS, WINDOW_MS, 100).is_empty());
        assert!(split(&[], 0, 0, 100).is_empty());
    }

    #[test]
    fn zero_maximum_never_splits() {
        let input = Rng(5).noise(0.5, 16_000 * 70);
        let chunks = split(&input, 0, WINDOW_MS, 12_345);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].samples, input);
    }

    #[test]
    fn uniform_noise_gives_chunks_near_the_maximum() {
        let input = Rng(11).noise(0.5, 16_000 * 300);
        let chunks = split(&input, MAX_MS, WINDOW_MS, 8_000);
        assert_contiguous(&chunks, &input);
        let full = &chunks[..chunks.len() - 1];
        let max = ms_to_samples(MAX_MS) as f64;
        let mean = full.iter().map(|c| c.samples.len() as f64).sum::<f64>() / full.len() as f64;
        assert!(
            mean >= max - 16_000.0 * 2.5,
            "mean chunk {:.1} s",
            mean / 16_000.0
        );
        for chunk in full {
            assert!(
                chunk.samples.len() as f64 >= max - 16_000.0 * 5.0,
                "chunk of {:.1} s",
                chunk.samples.len() as f64 / 16_000.0
            );
        }
    }

    #[test]
    fn hard_cut_when_the_search_window_is_empty() {
        let input = Rng(13).noise(0.5, 16_000 * 20);
        let chunks = split(&input, 6_000, 0, 1_000);
        assert_contiguous(&chunks, &input);
        for chunk in &chunks[..chunks.len() - 1] {
            assert_eq!(chunk.samples.len(), ms_to_samples(6_000));
        }
    }

    #[test]
    #[should_panic(expected = "at least 5000 ms")]
    fn rejects_tiny_maximum() {
        SilenceChunker::new(4_999, 1_000);
    }

    #[test]
    fn emit_errors_stop_the_push() {
        let input = Rng(17).noise(0.5, 16_000 * 60);
        let mut chunker = SilenceChunker::new(MAX_MS, WINDOW_MS);
        let mut calls = 0;
        let err = chunker
            .push(&input, &mut |_| {
                calls += 1;
                anyhow::bail!("receiver gone")
            })
            .unwrap_err();
        assert_eq!(err.to_string(), "receiver gone");
        assert_eq!(calls, 1);
    }

    #[test]
    fn chunk_times_follow_the_samples() {
        let chunk = AudioChunk {
            index: 3,
            start_sample: 16_000 * 30 + 15,
            samples: vec![0.0; 16_000 * 20 + 10],
        };
        assert_eq!(chunk.start_ms(), 30_000);
        assert_eq!(chunk.len_ms(), 20_000);
        assert_eq!(chunk.end_ms(), 50_001);
    }

    #[test]
    fn gate_skips_digital_silence() {
        for use_vad in [false, true] {
            let mut gate = SpeechGate::new(use_vad).unwrap();
            assert!(matches!(
                gate.prepare(&[0.0; 32_000]),
                ChunkVerdict::SkipSilent
            ));
            assert!(matches!(
                gate.prepare(&[0.0005; 32_000]),
                ChunkVerdict::SkipSilent
            ));
            assert!(matches!(gate.prepare(&[]), ChunkVerdict::SkipSilent));
        }
    }

    #[test]
    fn gate_pads_short_chunks_to_one_second() {
        let mut gate = SpeechGate::new(false).unwrap();
        let noise = Rng(21).noise(0.5, 8_000);
        match gate.prepare(&noise) {
            ChunkVerdict::Transcribe(audio) => {
                assert_eq!(audio.len(), 16_000);
                assert_eq!(audio[..8_000], noise[..]);
                assert!(audio[8_000..].iter().all(|&s| s == 0.0));
            }
            ChunkVerdict::SkipSilent => panic!("loud noise was skipped"),
        }
    }

    #[test]
    fn gate_passes_long_loud_chunks_unchanged() {
        let mut gate = SpeechGate::new(false).unwrap();
        for len in [16_000, 16_001, 16_000 * 28] {
            let noise = Rng(len as u64).noise(0.8, len);
            match gate.prepare(&noise) {
                ChunkVerdict::Transcribe(audio) => assert_eq!(audio, noise),
                ChunkVerdict::SkipSilent => panic!("loud noise was skipped"),
            }
        }
    }

    #[test]
    fn vad_gate_skips_a_steady_hum() {
        let mut gate = SpeechGate::new(true).unwrap();
        let hum: Vec<f32> = (0..16_000 * 5)
            .map(|i| 0.05 * (2.0 * std::f32::consts::PI * 60.0 * i as f32 / 16_000.0).sin())
            .collect();
        assert!(matches!(gate.prepare(&hum), ChunkVerdict::SkipSilent));
    }
}
