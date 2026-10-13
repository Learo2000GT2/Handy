//! The engine a file job transcribes its chunks with: Handy's own model
//! (through a `FileJobLease`) or an external server. The pipeline only sees
//! `ChunkTranscriber`; only Handy's implementation takes the lease and the
//! run gate, or waits for dictation.

use crate::file_transcription::pipeline;
use crate::file_transcription::types::FileJobErrorCode;
use crate::managers::transcription::{ChunkError, ChunkLimits, ChunkTranscript, FileJobLease};

/// What a chunk's transcription gives up with.
#[derive(Debug)]
pub enum TranscribeError {
    /// Dictation is active; nothing was sent. Wait and call again (Handy only).
    Yield,
    /// The job's own WorkToken was cancelled.
    Cancelled,
    /// The job must stop (model changed, server unreachable, bad key, unknown model).
    Stop {
        code: FileJobErrorCode,
        message: String,
    },
    /// This chunk could not be transcribed; the job may go on (the pipeline retries once).
    Failed(anyhow::Error),
}

/// One engine for one file job. Implementations hold their per-job state
/// (settings snapshot, language evidence, HTTP client).
pub trait ChunkTranscriber {
    /// Stored as FileTranscript.model_id ("small", "lemonade:Whisper-Base").
    fn model_id(&self) -> &str;
    fn limits(&self) -> ChunkLimits;
    /// true: chunks wait while a dictation is active and may get `Yield`
    /// (Handy's engine, which shares the run gate). false: never waits.
    fn yields_to_dictation(&self) -> bool;
    /// Blocking. `samples` 16 kHz mono (padded to ≥ 1 s); `offset_ms` where
    /// the chunk starts in the file; `real_len_ms` its length before padding.
    fn transcribe(
        &self,
        samples: &[f32],
        offset_ms: u64,
        real_len_ms: u64,
    ) -> Result<ChunkTranscript, TranscribeError>;
}

impl ChunkTranscriber for FileJobLease {
    fn model_id(&self) -> &str {
        FileJobLease::model_id(self)
    }

    fn limits(&self) -> ChunkLimits {
        FileJobLease::limits(self)
    }

    fn yields_to_dictation(&self) -> bool {
        true
    }

    fn transcribe(
        &self,
        samples: &[f32],
        offset_ms: u64,
        real_len_ms: u64,
    ) -> Result<ChunkTranscript, TranscribeError> {
        FileJobLease::transcribe_chunk(self, samples, offset_ms, real_len_ms)
            .map_err(chunk_error_to_transcribe)
    }
}

/// What the pipeline does with a chunk's error. A model that can't be loaded
/// because it is gone (deleted mid-job) stops the job at once: retrying the
/// next chunks would only fail the same way.
fn chunk_error_to_transcribe(error: ChunkError) -> TranscribeError {
    match error {
        ChunkError::Yield => TranscribeError::Yield,
        ChunkError::Cancelled => TranscribeError::Cancelled,
        ChunkError::ModelChanged {
            job_model,
            current_model,
        } => TranscribeError::Stop {
            code: FileJobErrorCode::ModelChanged,
            message: format!(
                "The model changed from {} to {} during transcription",
                job_model, current_model
            ),
        },
        ChunkError::Failed(error)
            if pipeline::model_load_error_code(&error) == FileJobErrorCode::NoModel =>
        {
            TranscribeError::Stop {
                code: FileJobErrorCode::NoModel,
                message: format!("{:#}", error),
            }
        }
        ChunkError::Failed(error) => TranscribeError::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn a_missing_model_stops_the_job() {
        for error in [
            anyhow!("Model not downloaded"),
            anyhow!("Model not found: cohere"),
        ] {
            match chunk_error_to_transcribe(ChunkError::Failed(error)) {
                TranscribeError::Stop { code, message } => {
                    assert_eq!(code, FileJobErrorCode::NoModel);
                    assert!(message.starts_with("Model not"), "{message}");
                }
                other => panic!("expected a stop, got {other:?}"),
            }
        }
    }

    #[test]
    fn other_failures_are_retried() {
        for error in [
            anyhow!("transcribe-cpp transcription failed: worker crashed"),
            anyhow!("Model cohere can't be used for recordings"),
        ] {
            assert!(matches!(
                chunk_error_to_transcribe(ChunkError::Failed(error)),
                TranscribeError::Failed(_)
            ));
        }
        assert!(matches!(
            chunk_error_to_transcribe(ChunkError::Yield),
            TranscribeError::Yield
        ));
        assert!(matches!(
            chunk_error_to_transcribe(ChunkError::Cancelled),
            TranscribeError::Cancelled
        ));
        assert!(matches!(
            chunk_error_to_transcribe(ChunkError::ModelChanged {
                job_model: "a".to_string(),
                current_model: "b".to_string(),
            }),
            TranscribeError::Stop {
                code: FileJobErrorCode::ModelChanged,
                ..
            }
        ));
    }
}
