//! Silence-aware chunking of 16 kHz mono audio and the per-chunk speech gate (WP1).
#![allow(dead_code, unused_variables, unused_imports)]

use crate::audio_toolkit::EarshotVad;

pub const SAMPLES_PER_MS: u64 = 16;

#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub index: u32,
    pub start_sample: u64,
    pub samples: Vec<f32>,
}

impl AudioChunk {
    /// start_sample / 16
    pub fn start_ms(&self) -> u64 {
        todo!("WP1")
    }

    /// samples.len() / 16
    pub fn len_ms(&self) -> u64 {
        todo!("WP1")
    }

    pub fn end_ms(&self) -> u64 {
        todo!("WP1")
    }
}

/// Streaming splitter for 16 kHz mono. Never holds more than max_chunk samples.
pub struct SilenceChunker {
    // WP1: private state.
    _private: (),
}

impl SilenceChunker {
    /// max_chunk_ms == 0: never split (one chunk with all input).
    /// Otherwise max_chunk_ms >= 5000 (assert), and the cut is searched with
    /// min_cut = max(max - search_window, 2000) ms.
    pub fn new(max_chunk_ms: u32, search_window_ms: u32) -> Self {
        todo!("WP1")
    }

    pub fn push(
        &mut self,
        src: &[f32],
        emit: &mut dyn FnMut(AudioChunk) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        todo!("WP1")
    }

    /// Emits the buffered tail (if any).
    pub fn finish(
        &mut self,
        emit: &mut dyn FnMut(AudioChunk) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        todo!("WP1")
    }
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
        todo!("WP1")
    }

    /// SkipSilent if peak < 1e-3 (−60 dBFS), or (use_vad and fewer than 6 of
    /// the 256-sample frames are speech; the VAD is reset per chunk).
    /// Otherwise returns the samples, zero-padded at the end to 16,000 if shorter than 1 s.
    pub fn prepare(&mut self, samples: &[f32]) -> ChunkVerdict {
        todo!("WP1")
    }
}
