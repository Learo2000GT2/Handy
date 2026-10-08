//! Probing and streaming any supported file as 16 kHz mono f32 (WP1).
#![allow(dead_code, unused_variables, unused_imports)]

use crate::file_transcription::{formats::DecodeRoute, types::DecoderKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

pub const TARGET_SAMPLE_RATE: u32 = 16_000;

#[derive(Clone, Debug, Default)]
pub struct DecodeOptions {
    /// Explicit ffmpeg (CLI --ffmpeg). None = find_ffmpeg(None).
    pub ffmpeg: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct AudioProbe {
    pub route: DecodeRoute,
    /// Best-known duration of the audio track (None if unknown).
    pub duration_ms: Option<u64>,
    /// The ffmpeg that would be used if the route or a fallback needs it.
    pub ffmpeg: Option<PathBuf>,
    pub source_size_bytes: u64,
    /// Unix seconds.
    pub source_modified_at: Option<i64>,
}

#[derive(Debug)]
pub enum DecodeError {
    /// Missing, not a regular file, or unreadable.
    NotFound(String),
    /// Unknown extension, no audio track, or undecodable with no ffmpeg fallback possible.
    Unsupported(String),
    /// Needs ffmpeg and none was found (the message names the extension/codec).
    NeedsFfmpeg(String),
    Cancelled,
    /// Failed before any audio reached the sink (after fallbacks), or the sink failed.
    Failed(anyhow::Error),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // WP1: human-readable English.
        todo!("WP1")
    }
}

impl std::error::Error for DecodeError {}

#[derive(Clone, Debug)]
pub struct DecodeOutcome {
    pub decoder: DecoderKind,
    /// 16 kHz mono samples handed to the sink.
    pub samples: u64,
    /// Set when decoding stopped early after producing audio (truncated/corrupt tail).
    pub truncated: Option<String>,
}

/// Cheap: never decodes the whole file. symphonia track params (n_frames via
/// time_base), hound for WAV, ffmpeg::probe_duration_ms for the ffmpeg route
/// or when symphonia has no duration.
pub fn probe(path: &Path, opts: &DecodeOptions) -> Result<AudioProbe, DecodeError> {
    todo!("WP1")
}

/// Streams the whole file as 16 kHz mono f32 into `sink` (blocks of any size).
pub fn decode_to_16k_mono(
    path: &Path,
    opts: &DecodeOptions,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(&[f32]) -> anyhow::Result<()>,
) -> Result<DecodeOutcome, DecodeError> {
    todo!("WP1")
}
