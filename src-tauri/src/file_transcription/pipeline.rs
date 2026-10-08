//! The decode → chunk → transcribe pipeline shared by the GUI job manager and
//! the headless CLI (WP4).
#![allow(dead_code, unused_variables, unused_imports)]

use crate::engine_supervisor::WorkToken;
use crate::file_transcription::types::*;
use crate::managers::transcription::TranscriptionManager;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct PipelineOptions {
    pub model_id: String,
    pub device_index: Option<usize>,
    /// None = lease.limits().max_chunk_ms. Some(0) = whole file in one engine
    /// call. Some(n): n >= 5000, used as-is (callers warn if > hard_max_ms).
    pub max_chunk_ms: Option<u32>,
    pub ffmpeg: Option<PathBuf>,
    pub skip_silent_chunks: bool,
}

pub enum PipelineEvent {
    Probed {
        duration_ms: Option<u64>,
    },
    ModelReady,
    /// Dictation is active; the next chunk waits.
    Waiting,
    /// A chunk is being sent to the engine; processed_ms = its start.
    Transcribing {
        chunk_index: u32,
        processed_ms: u64,
    },
    Chunk {
        index: u32,
        paragraph: Option<TranscriptSegment>,
        segments: Vec<TranscriptSegment>,
        language: Option<String>,
        processed_ms: u64,
        skipped: bool,
    },
}

pub struct PipelineOutput {
    /// history_id = None, model_name = None (callers fill these).
    pub transcript: FileTranscript,
    pub decoder: DecoderKind,
    pub chunks: u32,
    pub skipped_chunks: u32,
    pub failed_chunks: u32,
    pub warnings: Vec<String>,
    pub source_size_bytes: u64,
    pub source_modified_at: Option<i64>,
}

pub enum PipelineError {
    Cancelled {
        partial: Option<Box<PipelineOutput>>,
    },
    Failed {
        code: FileJobErrorCode,
        message: String,
        partial: Option<Box<PipelineOutput>>,
    },
}

impl PipelineError {
    /// 2 for NotFound | UnsupportedFormat | NeedsFfmpeg | DecodeFailed | NoModel; 1 otherwise.
    pub fn exit_code(&self) -> i32 {
        todo!("WP4")
    }
}

pub fn run_pipeline(
    tm: &TranscriptionManager,
    path: &Path,
    opts: &PipelineOptions,
    token: &WorkToken,
    on_event: &mut dyn FnMut(PipelineEvent),
) -> Result<PipelineOutput, PipelineError> {
    todo!("WP4")
}
