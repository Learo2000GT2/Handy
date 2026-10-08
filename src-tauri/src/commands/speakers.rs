//! Commands for speaker labels: the speaker model download and renaming
//! speakers in a transcript.
//!
//! Contract stub: renaming lands with the speaker labels work.
#![allow(dead_code, unused_variables, unused_imports)]

use crate::file_transcription::speaker_model::SpeakerModelManager;
use crate::file_transcription::types::{FileTranscript, SpeakerModelStatus, TranscriptSource};
use crate::file_transcription::FileTranscriptionManager;
use crate::managers::history::HistoryManager;
use std::sync::Arc;
use tauri::State;

#[tauri::command]
#[specta::specta]
pub async fn get_speaker_model_status(
    m: State<'_, Arc<SpeakerModelManager>>,
) -> Result<SpeakerModelStatus, String> {
    Ok(m.status())
}

#[tauri::command]
#[specta::specta]
pub async fn download_speaker_model(m: State<'_, Arc<SpeakerModelManager>>) -> Result<(), String> {
    m.download().await
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_speaker_model_download(
    m: State<'_, Arc<SpeakerModelManager>>,
) -> Result<(), String> {
    m.cancel_download();
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn delete_speaker_model(m: State<'_, Arc<SpeakerModelManager>>) -> Result<(), String> {
    m.delete()
}

/// Renames (or with name None/"" resets) speaker `speaker_id` in a job's result or
/// a History entry; returns the updated transcript. For a job whose transcript has
/// history_id, the History entry is updated too. Running jobs: Err("still running").
#[tauri::command]
#[specta::specta]
pub async fn rename_transcript_speaker(
    manager: State<'_, Arc<FileTranscriptionManager>>,
    history: State<'_, Arc<HistoryManager>>,
    source: TranscriptSource,
    speaker_id: u32,
    name: Option<String>,
) -> Result<FileTranscript, String> {
    Err("not implemented".to_string())
}
