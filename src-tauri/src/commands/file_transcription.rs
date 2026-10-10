use crate::file_transcription::export;
use crate::file_transcription::formats;
use crate::file_transcription::types::{
    FileJobSnapshot, FileTranscript, FileTranscriptionOptions, SupportedAudioFormats,
    TranscriptFormat, TranscriptSource,
};
use crate::file_transcription::FileTranscriptionManager;
use crate::managers::history::HistoryManager;
use std::path::{Path, PathBuf};
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
    refuse_source_as_target(&path, &transcript.source_path)?;

    let target = path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let bytes = export::render_file(
            &transcript,
            format,
            &export::ExportOptions::local(transcript.created_at),
        )?;
        export::write_atomic(&target, bytes)
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

/// Refuses an export onto the recording it was transcribed from: the save
/// dialog lets the user pick any file, and replacing the original would
/// lose it.
fn refuse_source_as_target(target: &Path, source_path: &str) -> Result<(), String> {
    if let (Ok(target), Ok(source)) = (target.canonicalize(), Path::new(source_path).canonicalize())
    {
        if target == source {
            return Err(
                "The export would replace the recording it was transcribed from. Choose another name."
                    .to_string(),
            );
        }
    }
    Ok(())
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
        // The dialog asked about replacing the name as typed, not this one.
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(format!(
                "{} already exists. Choose another name, or include .{} in the name to replace it.",
                path.display(),
                format.extension()
            ));
        }
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

    #[test]
    fn export_path_never_adds_an_extension_onto_an_existing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path();
        std::fs::write(dir_path.join("interview.srt"), "1\n").expect("write file");

        // The dialog confirmed "interview", which does not exist.
        let err = export_path(
            &dir_path.join("interview").to_string_lossy(),
            TranscriptFormat::Srt,
        )
        .expect_err("would replace interview.srt unasked");
        assert!(err.contains("interview.srt already exists"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir_path.join("interview.srt")).expect("read file"),
            "1\n"
        );

        // Named in full, the dialog asked before replacing it.
        assert_eq!(
            export_path(
                &dir_path.join("interview.srt").to_string_lossy(),
                TranscriptFormat::Srt
            ),
            Ok(dir_path.join("interview.srt"))
        );
        // Another format's extension is still free.
        assert_eq!(
            export_path(
                &dir_path.join("interview").to_string_lossy(),
                TranscriptFormat::Vtt
            ),
            Ok(dir_path.join("interview.vtt"))
        );
    }

    #[test]
    fn export_path_adds_docx_and_never_replaces_an_unnamed_docx() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path();

        assert_eq!(
            export_path(
                &dir_path.join("interview").to_string_lossy(),
                TranscriptFormat::Docx
            ),
            Ok(dir_path.join("interview.docx"))
        );

        std::fs::write(dir_path.join("interview.docx"), b"PK").expect("write file");
        let err = export_path(
            &dir_path.join("interview").to_string_lossy(),
            TranscriptFormat::Docx,
        )
        .expect_err("would replace interview.docx unasked");
        assert!(err.contains("interview.docx already exists"), "{err}");
        assert_eq!(
            std::fs::read(dir_path.join("interview.docx")).expect("read file"),
            b"PK"
        );
    }

    #[test]
    fn exports_never_replace_the_source_recording() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("interview.m4a");
        std::fs::write(&source, b"audio").expect("write file");
        let source_path = source.to_string_lossy();

        let err = refuse_source_as_target(&source, &source_path).expect_err("the source");
        assert_eq!(
            err,
            "The export would replace the recording it was transcribed from. Choose another name."
        );
        // The same file under another spelling.
        let indirect = dir.path().join(".").join("interview.m4a");
        assert!(refuse_source_as_target(&indirect, &source_path).is_err());

        // Another file, or one that does not exist yet, is fine.
        assert!(refuse_source_as_target(&dir.path().join("interview.docx"), &source_path).is_ok());
        let other = dir.path().join("notes.txt");
        std::fs::write(&other, "notes").expect("write file");
        assert!(refuse_source_as_target(&other, &source_path).is_ok());
        // A source that is gone cannot be replaced.
        assert!(refuse_source_as_target(&other, "/no/such/recording.m4a").is_ok());
        assert_eq!(std::fs::read(&source).expect("read file"), b"audio");
    }
}
