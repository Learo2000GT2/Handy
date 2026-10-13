//! Commands for speaker labels: the speaker model download, labelling the
//! speakers of a finished transcript, and fixing its labels (rename, merge,
//! change the speaker of a passage, undo).

use crate::file_transcription::formats;
use crate::file_transcription::label_job;
use crate::file_transcription::manager::LabelJobRequest;
use crate::file_transcription::pipeline;
use crate::file_transcription::speaker_model::{self, SpeakerModelManager};
use crate::file_transcription::speakers::{self, SpeakerState};
use crate::file_transcription::types::{
    FileJobSnapshot, FileTranscript, SpeakerModelStatus, TranscriptSource,
};
use crate::file_transcription::FileTranscriptionManager;
use crate::managers::history::HistoryManager;
use log::warn;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use tauri::{AppHandle, State};

/// Why speaker labels can't be changed right now.
const LABELLING: &str = "Speaker labels are being made for this transcript";

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
/// the jobs saved as that entry. Running jobs: Err("still running"). Refused while
/// speaker labels are being made for the transcript; can be undone.
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
    edit_speakers(manager.inner(), history.inner(), source, move |t| {
        speakers::rename_speaker(t, speaker_id, name.as_deref())
    })
    .await
}

/// Gives all of speaker `from_id`'s passages to speaker `into_id`, which keeps
/// its name (or takes `from_id`'s when it has none); `from_id` is removed. Saved
/// like a rename, and can be undone.
#[tauri::command]
#[specta::specta]
pub async fn merge_transcript_speakers(
    manager: State<'_, Arc<FileTranscriptionManager>>,
    history: State<'_, Arc<HistoryManager>>,
    source: TranscriptSource,
    from_id: u32,
    into_id: u32,
) -> Result<FileTranscript, String> {
    edit_speakers(manager.inner(), history.inner(), source, move |t| {
        speakers::merge_speakers(t, from_id, into_id)
    })
    .await
}

/// Gives the turn (paragraph) `paragraph_index`, which the caller saw start at
/// `start_ms` with speaker `from_speaker`, to speaker `speaker_id`, or to a new
/// speaker when that is None. Saved like a rename, and can be undone.
#[tauri::command]
#[specta::specta]
pub async fn reassign_transcript_turn(
    manager: State<'_, Arc<FileTranscriptionManager>>,
    history: State<'_, Arc<HistoryManager>>,
    source: TranscriptSource,
    paragraph_index: u32,
    start_ms: u64,
    from_speaker: Option<u32>,
    speaker_id: Option<u32>,
) -> Result<FileTranscript, String> {
    edit_speakers(manager.inner(), history.inner(), source, move |t| {
        speakers::reassign_turn(
            t,
            paragraph_index as usize,
            start_ms,
            from_speaker,
            speaker_id,
        )
        .map(|_| ())
    })
    .await
}

/// Undoes the last rename, merge or passage change of the transcript (one
/// level, and only while nothing else changed it since). Err("Nothing to undo")
/// otherwise. An undo can't itself be undone.
#[tauri::command]
#[specta::specta]
pub async fn undo_transcript_speaker_edit(
    manager: State<'_, Arc<FileTranscriptionManager>>,
    history: State<'_, Arc<HistoryManager>>,
    source: TranscriptSource,
) -> Result<FileTranscript, String> {
    let (manager, history) = (manager.inner(), history.inner());
    let target = editable_target(manager, history, source).await?;
    let undo = manager
        .take_undo(target)
        .ok_or_else(|| "Nothing to undo".to_string())?;
    let (shown, _) = apply_edit(manager, history, source, target, move |t| undo.undo(t)).await?;
    Ok(shown)
}

/// Queues a speaker-label job for a finished transcript (a job's result or a
/// History entry): its recording is diarized and the labels stored with the
/// transcript. `replace` relabels a labelled transcript, dropping names and
/// fixes; `path` is a recording the user found in place of the stored one.
/// Returns the job's snapshot. All errors are English.
#[tauri::command]
#[specta::specta]
pub async fn label_transcript_speakers(
    app: AppHandle,
    manager: State<'_, Arc<FileTranscriptionManager>>,
    history: State<'_, Arc<HistoryManager>>,
    source: TranscriptSource,
    replace: bool,
    path: Option<String>,
) -> Result<FileJobSnapshot, String> {
    if let TranscriptSource::Job { job_id } = source {
        // A running job, or a finished one without a transcript.
        manager.job_history_id(job_id)?;
    }
    let (manager, history) = (Arc::clone(&manager), Arc::clone(&history));
    tauri::async_runtime::spawn_blocking(move || {
        let target = manager.canonical_target(source, &history)?;
        let transcript = match target {
            TranscriptSource::Job { job_id } => manager
                .transcript(job_id)
                .ok_or_else(|| format!("No transcript for file transcription job {}", job_id))?,
            TranscriptSource::History { history_id } => history
                .get_file_transcript(history_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "Not a file transcription".to_string())?,
        };
        check_labellable(&transcript, replace)?;
        if speaker_model::installed_speaker_model(&app).is_none() {
            return Err("The speaker model isn't downloaded".to_string());
        }
        let path_override = path.is_some();
        let path = match path {
            Some(path) => {
                check_picked_recording(Path::new(&path))?;
                path
            }
            None => transcript.source_path.clone(),
        };
        manager.enqueue_label_job(LabelJobRequest {
            target,
            replace,
            file_name: pipeline::source_display_name(Path::new(&path)),
            path,
            path_override,
            duration_ms: transcript.duration_ms,
            limit_ms: label_job::diarize_limit(&transcript),
        })
    })
    .await
    .map_err(|e| format!("Failed to queue speaker labels: {}", e))?
}

/// Why `transcript` can't be given speaker labels (`replace`: its labels may
/// be replaced).
fn check_labellable(transcript: &FileTranscript, replace: bool) -> Result<(), String> {
    if transcript.text.trim().is_empty()
        || (transcript.paragraphs.is_empty() && transcript.segments.is_empty())
    {
        return Err("This transcript has no text to label".to_string());
    }
    if transcript.speakers.is_some() && !replace {
        return Err("This transcript already has speaker labels".to_string());
    }
    Ok(())
}

/// Why a recording the user picked (Find Recording…) can't be used.
fn check_picked_recording(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(format!(
            "The recording's path must be absolute: {}",
            path.display()
        ));
    }
    if !path.is_file() {
        return Err(format!(
            "{} does not exist or is not a file",
            path.display()
        ));
    }
    if formats::route_for_path(path).is_none() {
        return Err(format!("{} is not a supported recording", path.display()));
    }
    Ok(())
}

/// The canonical target of `source` (see
/// [`FileTranscriptionManager::canonical_target`]), refused while speaker
/// labels are being made for it.
async fn editable_target(
    manager: &Arc<FileTranscriptionManager>,
    history: &Arc<HistoryManager>,
    source: TranscriptSource,
) -> Result<TranscriptSource, String> {
    let (manager_ref, history) = (Arc::clone(manager), Arc::clone(history));
    let target = tauri::async_runtime::spawn_blocking(move || {
        manager_ref.canonical_target(source, &history)
    })
    .await
    .map_err(|e| format!("Failed to read history: {}", e))??;
    if manager.label_job_active(target) {
        return Err(LABELLING.to_string());
    }
    Ok(target)
}

/// Applies a speaker edit to `source` and keeps it for undo.
async fn edit_speakers(
    manager: &Arc<FileTranscriptionManager>,
    history: &Arc<HistoryManager>,
    source: TranscriptSource,
    edit: impl Fn(&mut FileTranscript) -> Result<(), String> + Clone + Send + 'static,
) -> Result<FileTranscript, String> {
    let target = editable_target(manager, history, source).await?;
    // The state the edit started from, captured where it runs (in the
    // History transaction for a saved transcript).
    let before: Arc<Mutex<Option<SpeakerState>>> = Arc::default();
    let capture = {
        let before = Arc::clone(&before);
        move |t: &mut FileTranscript| {
            let state = SpeakerState::of(t);
            edit(t)?;
            *before.lock().unwrap_or_else(PoisonError::into_inner) = Some(state);
            Ok(())
        }
    };
    let (shown, edited) = apply_edit(manager, history, source, target, capture).await?;
    let before = before.lock().unwrap_or_else(PoisonError::into_inner).take();
    if let Some(before) = before {
        manager.remember_edit(edited, before, SpeakerState::of(&shown));
    }
    Ok(shown)
}

/// Applies `edit` to `target`, the canonical target of `source`: a History
/// entry, then the jobs saved as it, or a job's result. Returns the transcript
/// as `source` shows it, and the target that was edited (the job, when the
/// entry was deleted meanwhile).
async fn apply_edit(
    manager: &Arc<FileTranscriptionManager>,
    history: &Arc<HistoryManager>,
    source: TranscriptSource,
    target: TranscriptSource,
    edit: impl Fn(&mut FileTranscript) -> Result<(), String> + Clone + Send + 'static,
) -> Result<(FileTranscript, TranscriptSource), String> {
    let history_id = match target {
        TranscriptSource::Job { job_id } => {
            return Ok((manager.edit_job_transcript(job_id, edit)?, target));
        }
        TranscriptSource::History { history_id } => history_id,
    };
    match edit_in_history(Arc::clone(history), history_id, edit.clone()).await? {
        Some(stored) => {
            manager.sync_history_transcript(&stored);
            let shown = match source {
                TranscriptSource::Job { job_id } => {
                    manager.transcript(job_id).ok_or_else(|| {
                        format!("No transcript for file transcription job {}", job_id)
                    })?
                }
                TranscriptSource::History { .. } => stored,
            };
            Ok((shown, target))
        }
        None => match source {
            TranscriptSource::Job { job_id } => {
                warn!(
                    "History entry {} no longer exists; changing the speakers of job {} only",
                    history_id, job_id
                );
                Ok((manager.edit_job_transcript(job_id, edit)?, source))
            }
            TranscriptSource::History { .. } => Err("Not a file transcription".to_string()),
        },
    }
}

/// Applies `edit` to History entry `history_id`; the edited transcript, or
/// None when the entry no longer exists.
async fn edit_in_history(
    history: Arc<HistoryManager>,
    history_id: i64,
    edit: impl FnOnce(&mut FileTranscript) -> Result<(), String> + Send + 'static,
) -> Result<Option<FileTranscript>, String> {
    tauri::async_runtime::spawn_blocking(move || history.edit_file_speakers(history_id, edit))
        .await
        .map_err(|e| format!("Failed to update history: {}", e))?
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::types::{
        TimingGrain, TranscriptSegment, TranscriptSpeakers, TranscriptStatus,
    };

    fn transcript(text: &str) -> FileTranscript {
        let paragraphs = if text.is_empty() {
            Vec::new()
        } else {
            vec![TranscriptSegment::new(0, 1_000, text)]
        };
        FileTranscript {
            source_path: "/audio/talk.m4a".to_string(),
            source_name: "talk.m4a".to_string(),
            duration_ms: 1_000,
            model_id: "small".to_string(),
            model_name: None,
            language: Some("en".to_string()),
            status: TranscriptStatus::Completed,
            status_detail: None,
            timing: TimingGrain::Segment,
            text: text.to_string(),
            segments: paragraphs.clone(),
            paragraphs,
            created_at: 0,
            history_id: None,
            speakers: None,
        }
    }

    #[test]
    fn only_transcripts_with_text_are_labelled() {
        assert!(check_labellable(&transcript("Hello."), false).is_ok());
        assert!(check_labellable(&transcript(""), false).is_err());
        assert!(check_labellable(&transcript("   "), true).is_err());
        let mut no_turns = transcript("Hello.");
        no_turns.paragraphs.clear();
        no_turns.segments.clear();
        assert!(check_labellable(&no_turns, false).is_err());

        // Labels are replaced only when asked.
        let mut labelled = transcript("Speaker 1: Hello.");
        labelled.speakers = Some(TranscriptSpeakers {
            speakers: Vec::new(),
            approximate: false,
            unassigned_segments: 0,
            model: "m".to_string(),
        });
        assert!(check_labellable(&labelled, false).is_err());
        assert!(check_labellable(&labelled, true).is_ok());
    }

    #[test]
    fn a_picked_recording_must_be_an_absolute_supported_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let audio = dir.path().join("talk.m4a");
        std::fs::write(&audio, b"x").expect("write");
        let notes = dir.path().join("notes.xyz");
        std::fs::write(&notes, b"x").expect("write");

        assert!(check_picked_recording(&audio).is_ok());
        assert!(check_picked_recording(Path::new("talk.m4a")).is_err());
        assert!(check_picked_recording(&dir.path().join("missing.m4a")).is_err());
        assert!(check_picked_recording(dir.path()).is_err());
        assert!(check_picked_recording(&notes).is_err());
    }
}
