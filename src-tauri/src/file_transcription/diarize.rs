//! Speaker diarization of a file with NVIDIA Sortformer, in a dedicated
//! CPU-only, low-priority engine worker that never touches dictation's engine.
//!
//! Contract stub: the entry points the pipeline calls. The diarizer itself
//! lands with the speaker labels work.
#![allow(dead_code, unused_variables, unused_imports)]

use crate::engine_supervisor::{EngineSupervisor, WorkToken};
use crate::file_transcription::decode::DecodeOptions;
use crate::file_transcription::speakers::SpeakerTurn;
use crate::file_transcription::types::{SpeakerJobState, SpeakerSkipReason};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Diarizer id stored in `TranscriptSpeakers.model`.
pub const DIARIZER_MODEL_ID: &str = "sortformer-4spk-v2.1-q8_0";
/// Sanity bound on the audio length to diarize. Long files are diarized in
/// windows, so this is not a memory limit.
pub const MAX_DIARIZE_MS: u64 = 12 * 3_600_000;
/// How often a held diarization re-checks dictation, and `wait` its token.
pub const DIARIZE_POLL: Duration = Duration::from_millis(150);

#[derive(Clone, Debug)]
pub struct DiarizeRequest {
    pub path: PathBuf,
    pub decode: DecodeOptions,
    pub model_path: PathBuf,
    /// Probe estimate: sizes the sample buffer and fails early with TooLong.
    pub estimated_duration_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiarizeError {
    ModelMissing(PathBuf),
    /// Longer than `MAX_DIARIZE_MS`.
    TooLong {
        duration_ms: u64,
    },
    Decode(String),
    Engine(String),
    Cancelled,
}

impl DiarizeError {
    /// ModelMissing → Skipped{ModelMissing}; Cancelled → Failed{"cancelled"};
    /// others (TooLong included) → Failed{message}.
    pub fn job_state(&self) -> SpeakerJobState {
        match self {
            Self::ModelMissing(_) => SpeakerJobState::Skipped {
                reason: SpeakerSkipReason::ModelMissing,
            },
            Self::Cancelled => SpeakerJobState::Failed {
                message: "cancelled".to_string(),
            },
            other => SpeakerJobState::Failed {
                message: other.to_string(),
            },
        }
    }
}

impl fmt::Display for DiarizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelMissing(path) => {
                write!(f, "the speaker model is missing: {}", path.display())
            }
            Self::TooLong { duration_ms } => write!(
                f,
                "the recording is too long to label speakers ({} min)",
                duration_ms / 60_000
            ),
            Self::Decode(message) => write!(f, "could not decode the audio: {}", message),
            Self::Engine(message) => write!(f, "the speaker model failed: {}", message),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for DiarizeError {}

/// Runs Sortformer on 16 kHz mono samples in `engine` (a dedicated
/// supervisor). Returns the raw turns (unsorted; normalized later by
/// `speakers::normalize_turns`).
pub fn diarize_samples(
    engine: &EngineSupervisor,
    model_path: &Path,
    samples: Vec<f32>,
    token: &WorkToken,
) -> Result<Vec<SpeakerTurn>, DiarizeError> {
    Err(DiarizeError::Engine("not implemented".to_string()))
}

/// A diarization running on its own "file-diarize" thread.
pub struct DiarizationHandle {
    token: WorkToken,
}

/// Starts diarizing `req.path` on a "file-diarize" thread, waiting while
/// `hold()` (dictation active) before the diarizer worker starts.
pub fn start_diarization(
    req: DiarizeRequest,
    token: WorkToken,
    hold: Arc<dyn Fn() -> bool + Send + Sync>,
) -> DiarizationHandle {
    DiarizationHandle { token }
}

impl DiarizationHandle {
    /// Blocks until the diarization ends. On cancel of the token, kills only
    /// the diarizer worker.
    pub fn wait(self) -> Result<Vec<SpeakerTurn>, DiarizeError> {
        Err(DiarizeError::Engine("not implemented".to_string()))
    }

    /// Cancels the token and the diarizer's work; returns at once.
    pub fn cancel(&self) {
        self.token.cancel();
    }
}
