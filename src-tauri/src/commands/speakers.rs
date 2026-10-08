//! Commands for speaker labels: the speaker model download and renaming
//! speakers in a transcript.

use crate::file_transcription::speaker_model::SpeakerModelManager;
use crate::file_transcription::speakers;
use crate::file_transcription::types::{FileTranscript, SpeakerModelStatus, TranscriptSource};
use crate::file_transcription::FileTranscriptionManager;
use crate::managers::history::HistoryManager;
use log::warn;
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
/// a History entry; returns the updated transcript. History is the source of truth
/// for a saved transcript: a job whose result has a history_id is renamed in History
/// first and then takes the entry's labels, so a failure changes neither copy (if
/// that entry was deleted, only the job is renamed). A History rename also updates
/// the jobs saved as that entry. Running jobs: Err("still running").
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
    let rename =
        move |t: &mut FileTranscript| speakers::rename_speaker(t, speaker_id, name.as_deref());
    match source {
        TranscriptSource::Job { job_id } => {
            if let Some(history_id) = manager.job_history_id(job_id)? {
                match rename_in_history(history, history_id, rename.clone()).await? {
                    Some(stored) => {
                        return manager.edit_job_transcript(job_id, |t| {
                            t.speakers = stored.speakers;
                            t.text = stored.text;
                            Ok(())
                        });
                    }
                    None => warn!(
                        "History entry {} no longer exists; renaming the speaker in job {} only",
                        history_id, job_id
                    ),
                }
            }
            manager.edit_job_transcript(job_id, rename)
        }
        TranscriptSource::History { history_id } => {
            let stored = rename_in_history(history, history_id, rename)
                .await?
                .ok_or_else(|| "Not a file transcription".to_string())?;
            manager.sync_history_speakers(&stored);
            Ok(stored)
        }
    }
}

/// Applies `rename` to History entry `history_id`; the renamed transcript, or
/// None when the entry no longer exists.
async fn rename_in_history(
    history: Arc<HistoryManager>,
    history_id: i64,
    rename: impl FnOnce(&mut FileTranscript) -> Result<(), String> + Send + 'static,
) -> Result<Option<FileTranscript>, String> {
    tauri::async_runtime::spawn_blocking(move || history.edit_file_speakers(history_id, rename))
        .await
        .map_err(|e| format!("Failed to update history: {}", e))?
        .map_err(|e| e.to_string())
}
