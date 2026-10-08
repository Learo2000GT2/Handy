use crate::file_transcription::export;
use crate::file_transcription::formats;
use crate::file_transcription::types::{
    FileJobSnapshot, FileTranscript, FileTranscriptionOptions, SupportedAudioFormats,
    TranscriptFormat, TranscriptSource,
};
use crate::file_transcription::FileTranscriptionManager;
use crate::managers::history::HistoryManager;
use std::path::PathBuf;
use std::sync::Arc;
use tauri::State;

#[tauri::command]
#[specta::specta]
pub async fn get_supported_audio_formats() -> Result<SupportedAudioFormats, String> {
    // May run `ffmpeg -version`, so keep it off the async runtime.
    tauri::async_runtime::spawn_blocking(|| formats::supported_formats(None))
        .await
        .map_err(|e| format!("Failed to list supported formats: {}", e))
}

#[tauri::command]
#[specta::specta]
pub async fn enqueue_file_transcriptions(
    manager: State<'_, Arc<FileTranscriptionManager>>,
    paths: Vec<String>,
    options: FileTranscriptionOptions,
) -> Result<Vec<FileJobSnapshot>, String> {
    Ok(manager.enqueue(paths, options))
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_file_transcription(
    manager: State<'_, Arc<FileTranscriptionManager>>,
    job_id: u64,
) -> Result<(), String> {
    manager.cancel(job_id)
}

#[tauri::command]
#[specta::specta]
pub async fn cancel_all_file_transcriptions(
    manager: State<'_, Arc<FileTranscriptionManager>>,
) -> Result<(), String> {
    manager.cancel_all();
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn list_file_transcription_jobs(
    manager: State<'_, Arc<FileTranscriptionManager>>,
) -> Result<Vec<FileJobSnapshot>, String> {
    Ok(manager.list())
}

#[tauri::command]
#[specta::specta]
pub async fn dismiss_file_transcription_job(
    manager: State<'_, Arc<FileTranscriptionManager>>,
    job_id: u64,
) -> Result<(), String> {
    manager.dismiss(job_id)
}

#[tauri::command]
#[specta::specta]
pub async fn get_file_transcript(
    manager: State<'_, Arc<FileTranscriptionManager>>,
    history: State<'_, Arc<HistoryManager>>,
    source: TranscriptSource,
) -> Result<FileTranscript, String> {
    resolve_transcript(&manager, &history, source).await
}

/// Writes the transcript in `format` to `path` (absolute; its folder must
/// exist; the format's extension is added when the name has none) and
/// returns the path written.
#[tauri::command]
#[specta::specta]
pub async fn export_file_transcript(
    manager: State<'_, Arc<FileTranscriptionManager>>,
    history: State<'_, Arc<HistoryManager>>,
    source: TranscriptSource,
    format: TranscriptFormat,
    path: String,
) -> Result<String, String> {
    let transcript = resolve_transcript(&manager, &history, source).await?;
    let path = export_path(&path, format)?;

    let target = path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        export::write_atomic(&target, &export::render(&transcript, format))
    })
    .await
    .map_err(|e| format!("Failed to export transcript: {}", e))?
    .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;

    Ok(path.to_string_lossy().into_owned())
}

async fn resolve_transcript(
    manager: &FileTranscriptionManager,
    history: &Arc<HistoryManager>,
    source: TranscriptSource,
) -> Result<FileTranscript, String> {
    match source {
        TranscriptSource::Job { job_id } => manager
            .transcript(job_id)
            .ok_or_else(|| format!("No transcript for file transcription job {}", job_id)),
        TranscriptSource::History { history_id } => {
            let history = Arc::clone(history);
            tauri::async_runtime::spawn_blocking(move || history.get_file_transcript(history_id))
                .await
                .map_err(|e| format!("Failed to read history: {}", e))?
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "Not a file transcription".to_string())
        }
    }
}

/// Validates an export destination chosen in the save dialog.
fn export_path(path: &str, format: TranscriptFormat) -> Result<PathBuf, String> {
    let mut path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!("Export path must be absolute: {}", path.display()));
    }
    if !path.parent().is_some_and(|parent| parent.is_dir()) {
        return Err(format!("The folder for {} does not exist", path.display()));
    }
    if path.extension().is_none() {
        path.set_extension(format.extension());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_path_requires_an_absolute_path_in_an_existing_folder() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path();

        assert!(export_path("transcript.srt", TranscriptFormat::Srt).is_err());
        assert!(export_path(
            &dir_path.join("missing").join("a.srt").to_string_lossy(),
            TranscriptFormat::Srt
        )
        .is_err());
        assert_eq!(
            export_path(
                &dir_path.join("a.srt").to_string_lossy(),
                TranscriptFormat::Srt
            ),
            Ok(dir_path.join("a.srt"))
        );
    }

    #[test]
    fn export_path_adds_the_extension_only_when_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path();

        assert_eq!(
            export_path(
                &dir_path.join("talk").to_string_lossy(),
                TranscriptFormat::Vtt
            ),
            Ok(dir_path.join("talk.vtt"))
        );
        assert_eq!(
            export_path(
                &dir_path.join("talk.text").to_string_lossy(),
                TranscriptFormat::Txt
            ),
            Ok(dir_path.join("talk.text"))
        );
        assert_eq!(
            export_path(
                &dir_path.join("talk.final").to_string_lossy(),
                TranscriptFormat::Md
            ),
            Ok(dir_path.join("talk.final"))
        );
    }
}
