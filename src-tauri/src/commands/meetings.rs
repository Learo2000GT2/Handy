//! Commands for live meetings (see `crate::meeting`).

use crate::file_transcription::types::TranscriptSegment;
use crate::meeting::manifest::MeetingMarker;
use crate::meeting::{
    MeetingManager, MeetingStatus, MeetingSummary, MicCheckResult, StartMeetingOptions,
};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::sync::Arc;
use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;

fn manager(app: &AppHandle) -> Arc<MeetingManager> {
    Arc::clone(&app.state::<Arc<MeetingManager>>())
}

/// Runs blocking meeting work off the async runtime.
async fn blocking<T: Send + 'static>(
    app: AppHandle,
    work: impl FnOnce(Arc<MeetingManager>) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let manager = manager(&app);
    tauri::async_runtime::spawn_blocking(move || work(manager))
        .await
        .map_err(|e| e.to_string())?
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct MeetingState {
    /// The running meeting, if any.
    pub status: Option<MeetingStatus>,
    /// Its live transcript so far.
    pub segments: Vec<TranscriptSegment>,
    /// Meetings closed by recovery at this start, shown once.
    pub recovered: Vec<String>,
}

// The commands below are async so they never run on the main thread: they
// can wait briefly on a meeting's locks or the disk.

#[tauri::command]
#[specta::specta]
pub async fn get_meeting_state(app: AppHandle) -> Result<MeetingState, String> {
    blocking(app, |manager| {
        Ok(MeetingState {
            status: manager.status(),
            segments: manager.live_segments(),
            recovered: manager.take_recovered(),
        })
    })
    .await
}

/// Records 10 seconds from the meeting microphone and reports how it
/// sounds and what Handy heard.
#[tauri::command]
#[specta::specta]
pub async fn check_meeting_microphone(app: AppHandle) -> Result<MicCheckResult, String> {
    blocking(app, |manager| manager.check_microphone()).await
}

#[tauri::command]
#[specta::specta]
pub fn cancel_meeting_microphone_check(app: AppHandle) {
    manager(&app).cancel_check();
}

#[tauri::command]
#[specta::specta]
pub async fn start_meeting(
    app: AppHandle,
    options: StartMeetingOptions,
) -> Result<MeetingStatus, String> {
    blocking(app, move |manager| manager.start(options)).await
}

#[tauri::command]
#[specta::specta]
pub async fn pause_meeting(app: AppHandle, paused: bool) -> Result<MeetingStatus, String> {
    blocking(app, move |manager| manager.set_paused(paused)).await
}

#[tauri::command]
#[specta::specta]
pub async fn add_meeting_note(
    app: AppHandle,
    label: Option<String>,
) -> Result<MeetingMarker, String> {
    blocking(app, move |manager| manager.add_note(label)).await
}

/// Stops the meeting and sends the recording to the transcription queue.
#[tauri::command]
#[specta::specta]
pub async fn stop_meeting(app: AppHandle) -> Result<MeetingSummary, String> {
    blocking(app, |manager| manager.stop(true)).await
}

/// Stops the meeting, saves the recording without transcribing it (the
/// next start offers that), and quits.
#[tauri::command]
#[specta::specta]
pub async fn stop_meeting_and_quit(app: AppHandle) -> Result<(), String> {
    let exit_app = app.clone();
    blocking(app, |manager| {
        // Waits for a Stop already in progress; only a meeting still
        // recording afterwards keeps Handy open.
        let result = manager.stop(false);
        match result {
            Err(error) if crate::meeting::is_active() => Err(error),
            _ => Ok(()),
        }
    })
    .await?;
    crate::meeting::confirm_quit();
    exit_app.exit(0);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn list_meetings(app: AppHandle) -> Result<Vec<MeetingSummary>, String> {
    blocking(app, |manager| manager.list()).await
}

/// Sends a past meeting's recording to the transcription queue.
#[tauri::command]
#[specta::specta]
pub async fn transcribe_meeting(app: AppHandle, id: String) -> Result<MeetingSummary, String> {
    blocking(app, move |manager| manager.transcribe(&id)).await
}

/// Opens a meeting's folder, or the meetings folder for None.
#[tauri::command]
#[specta::specta]
pub fn open_meeting_folder(app: AppHandle, id: Option<String>) -> Result<(), String> {
    let root = crate::meeting::meetings_dir(&app)?;
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let path = match id {
        Some(id) if is_meeting_id(&id) => root.join(id),
        Some(_) => return Err("Invalid meeting id.".to_string()),
        None => root,
    };
    if !path.is_dir() {
        return Err("The meeting folder was not found.".to_string());
    }
    app.opener()
        .open_path(path.to_string_lossy().into_owned(), None::<String>)
        .map_err(|e| format!("Failed to open the folder: {}", e))
}

/// Meeting ids are folder names Handy made ("2026-10-13_14-05-22", maybe
/// with "_2"): letters, digits, '-' and '_' only, so a crafted id can never
/// name another place ("..", "C:", a path).
fn is_meeting_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::is_meeting_id;

    #[test]
    fn meeting_ids_cannot_leave_the_meetings_folder() {
        assert!(is_meeting_id("2026-10-13_14-05-22"));
        assert!(is_meeting_id("2026-10-13_14-05-22_2"));
        for bad in [
            "",
            "..",
            ".",
            "C:",
            "C:x",
            "a/b",
            "a\\b",
            "\\\\server\\share",
        ] {
            assert!(!is_meeting_id(bad), "{bad}");
        }
    }
}
