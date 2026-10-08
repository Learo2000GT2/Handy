//! Lemonade Server (OpenAI-compatible Whisper endpoint) as a file
//! transcription engine.
//!
//! Contract stub: the shared types and the entry points other parts of the
//! app call. The HTTP client lands with the Lemonade engine work.
#![allow(dead_code, unused_variables, unused_imports)]

use crate::engine_supervisor::WorkToken;
use crate::file_transcription::engine::{ChunkTranscriber, TranscribeError};
use crate::file_transcription::types::FileJobErrorCode;
use crate::managers::transcription::{ChunkLimits, ChunkTranscript};
use crate::settings::{AppSettings, LemonadeSettings};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::fmt;
use std::time::Duration;

/// Prefix of a Lemonade job's model id: "lemonade:Whisper-Base".
pub const LEMONADE_MODEL_PREFIX: &str = "lemonade:";

/// The Lemonade Server a job uses, captured when the job starts.
#[derive(Clone, Debug)]
pub struct LemonadeJobConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

impl LemonadeJobConfig {
    /// From settings (+ optional CLI overrides); api_key None when blank.
    pub fn from_settings(s: &LemonadeSettings) -> Self {
        let api_key = s.api_key.trim();
        Self {
            base_url: s.base_url.trim().to_string(),
            model: s.model.trim().to_string(),
            api_key: (!api_key.is_empty()).then(|| api_key.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct LemonadeModel {
    pub id: String,
    pub recipe: Option<String>,
    pub labels: Vec<String>,
    /// None when the server did not say.
    pub downloaded: Option<bool>,
}

impl LemonadeModel {
    /// labels contain "transcription", or recipe == "whispercpp", or id starts with "whisper" (any case).
    pub fn is_transcription(&self) -> bool {
        unimplemented!("Lemonade model filter")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum LemonadeErrorKind {
    InvalidUrl,
    Unreachable,
    Timeout,
    Unauthorized,
    ModelNotFound,
    Http,
    BadResponse,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct LemonadeCheck {
    pub ok: bool,
    pub error: Option<LemonadeErrorKind>,
    /// English detail (HTTP status/body excerpt, OS error) for a tooltip.
    pub detail: Option<String>,
    /// The URL actually used, e.g. "http://localhost:13305/api/v1".
    pub api_url: Option<String>,
    /// Transcription models only, server order.
    pub models: Vec<LemonadeModel>,
    pub other_models: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum LemonadeError {
    InvalidUrl(String),
    Unreachable(String),
    Timeout(Duration),
    Unauthorized {
        status: u16,
    },
    ModelNotFound {
        model: String,
        detail: String,
    },
    /// `body` trimmed to 300 characters.
    Http {
        status: u16,
        body: String,
    },
    BadResponse(String),
    Cancelled,
}

impl LemonadeError {
    pub fn kind(&self) -> LemonadeErrorKind {
        match self {
            Self::InvalidUrl(_) => LemonadeErrorKind::InvalidUrl,
            Self::Unreachable(_) => LemonadeErrorKind::Unreachable,
            Self::Timeout(_) => LemonadeErrorKind::Timeout,
            Self::Unauthorized { .. } => LemonadeErrorKind::Unauthorized,
            Self::ModelNotFound { .. } => LemonadeErrorKind::ModelNotFound,
            Self::Http { .. } => LemonadeErrorKind::Http,
            Self::BadResponse(_) => LemonadeErrorKind::BadResponse,
            Self::Cancelled => LemonadeErrorKind::Cancelled,
        }
    }

    /// Unreachable, Timeout, Http 408/429/5xx.
    pub fn is_retryable(&self) -> bool {
        unimplemented!("Lemonade retry policy")
    }
}

impl fmt::Display for LemonadeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Lemonade Server error ({:?})", self.kind())
    }
}

impl std::error::Error for LemonadeError {}

#[derive(Clone, Copy, Debug)]
pub struct LemonadeTimeouts {
    pub connect: Duration,
    pub list: Duration,
    /// Model download/load on first use.
    pub first_request: Duration,
    pub request_base: Duration,
    /// Added per second of chunk audio to `request_base`.
    pub request_per_audio_sec: Duration,
}

impl Default for LemonadeTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            list: Duration::from_secs(10),
            first_request: Duration::from_secs(30 * 60),
            request_base: Duration::from_secs(60),
            request_per_audio_sec: Duration::from_secs(6),
        }
    }
}

/// A file job's Lemonade engine: one HTTP request per chunk, post-processed
/// like a local chunk. Never takes the lease, the run gate or the loaded model.
pub struct LemonadeTranscriber {
    model_id: String,
}

impl LemonadeTranscriber {
    /// Preflight GET /models (list timeout). Unreachable → Stop{ServerUnreachable};
    /// 401/403 → Stop{ServerUnauthorized}; cancel → Cancelled. A model missing from the
    /// list is NOT fatal (Lemonade may download on first use) but makes the first request
    /// use `first_request` timeout.
    pub fn begin(
        cfg: &LemonadeJobConfig,
        settings: AppSettings,
        token: WorkToken,
    ) -> Result<Self, TranscribeError> {
        Err(TranscribeError::Stop {
            code: FileJobErrorCode::EngineFailed,
            message: "Lemonade Server transcription is not implemented".to_string(),
        })
    }
}

impl ChunkTranscriber for LemonadeTranscriber {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn limits(&self) -> ChunkLimits {
        ChunkLimits {
            max_chunk_ms: 28_000,
            search_window_ms: 10_000,
            hard_max_ms: None,
        }
    }

    fn yields_to_dictation(&self) -> bool {
        false
    }

    fn transcribe(
        &self,
        samples: &[f32],
        offset_ms: u64,
        real_len_ms: u64,
    ) -> Result<ChunkTranscript, TranscribeError> {
        Err(TranscribeError::Failed(anyhow::anyhow!(
            "Lemonade Server transcription is not implemented"
        )))
    }
}
