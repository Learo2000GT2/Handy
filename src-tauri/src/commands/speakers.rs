//! Commands for speaker labels: the speaker model download and renaming
//! speakers in a transcript.

use crate::file_transcription::speaker_model::SpeakerModelManager;
use crate::file_transcription::speakers;
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
    let name = speakers::validate_speaker_name(name.as_deref())?;
    let history = Arc::clone(&history);
    match source {
        TranscriptSource::Job { job_id } => {
            let transcript = manager.edit_job_transcript(job_id, |t| {
                speakers::rename_speaker(t, speaker_id, name.as_deref())
            })?;
            if let Some(history_id) = transcript.history_id {
                let stored = transcript.clone();
                tauri::async_runtime::spawn_blocking(move || {
                    history.update_file_speakers(history_id, &stored)
                })
                .await
                .map_err(|e| format!("Failed to update history: {}", e))?
                .map_err(|e| e.to_string())?;
            }
            Ok(transcript)
        }
        TranscriptSource::History { history_id } => {
            tauri::async_runtime::spawn_blocking(move || {
                let mut transcript = history
                    .get_file_transcript(history_id)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "Not a file transcription".to_string())?;
                speakers::rename_speaker(&mut transcript, speaker_id, name.as_deref())?;
                history
                    .update_file_speakers(history_id, &transcript)
                    .map_err(|e| e.to_string())?;
                Ok(transcript)
            })
            .await
            .map_err(|e| format!("Failed to update history: {}", e))?
        }
    }
}
