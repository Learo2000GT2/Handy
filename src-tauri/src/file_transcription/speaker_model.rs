//! The Sortformer speaker model: where it lives and its one-time download.
//! It is stored outside `models/`, so it is never offered as a dictation model.
//!
//! Contract stub: the download lands with the speaker labels work.
#![allow(dead_code, unused_variables, unused_imports)]

use crate::file_transcription::types::{SpeakerModelEvent, SpeakerModelStatus};
use hf_hub::api::tokio::CancellationToken;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::AppHandle;

pub const SPEAKER_MODEL_FILE: &str = "diar_streaming_sortformer_4spk-v2.1-Q8_0.gguf";
pub const SPEAKER_MODEL_URL: &str = "https://huggingface.co/handy-computer/diar_streaming_sortformer_4spk-v2.1-gguf/resolve/ae4afbb5c3d33b71cf2dbf600022b655ee706dd0/diar_streaming_sortformer_4spk-v2.1-Q8_0.gguf";
pub const SPEAKER_MODEL_SHA256: &str =
    "a5dacdc650790266c7a362e54e6bf51952015487edaa606c4e11632bc32442a9";
pub const SPEAKER_MODEL_SIZE: u64 = 139_310_336;

/// <app_data>/speaker-models (portable-aware via crate::portable::app_data_dir).
pub fn speaker_models_dir(app: &AppHandle) -> anyhow::Result<PathBuf> {
    Ok(crate::portable::app_data_dir(app)?.join("speaker-models"))
}

/// The verified model file if present (size == SPEAKER_MODEL_SIZE; the hash is
/// checked at download time only).
pub fn installed_speaker_model(app: &AppHandle) -> Option<PathBuf> {
    None
}

pub struct SpeakerModelManager {
    app: AppHandle,
    /// Set while a download runs.
    cancel: Mutex<Option<CancellationToken>>,
}

impl SpeakerModelManager {
    pub fn new(app: &AppHandle) -> Arc<Self> {
        Arc::new(Self {
            app: app.clone(),
            cancel: Mutex::new(None),
        })
    }

    pub fn status(&self) -> SpeakerModelStatus {
        SpeakerModelStatus {
            downloaded: false,
            downloading: false,
            size_bytes: SPEAKER_MODEL_SIZE,
            path: None,
        }
    }

    /// Resumable download into <dir>/<file>.partial, sha256 verify, rename.
    /// Emits SpeakerModelEvent::{Progress (≤10/s), Verifying, Completed | Failed | Cancelled}.
    /// Err("already downloading") if one runs.
    pub async fn download(&self) -> Result<(), String> {
        Err("not implemented".to_string())
    }

    pub fn cancel_download(&self) {}

    /// Removes the file and any .partial.
    pub fn delete(&self) -> Result<(), String> {
        Err("not implemented".to_string())
    }
}
