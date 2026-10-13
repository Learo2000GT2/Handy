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

#[tauri::command]
#[specta::specta]
pub fn get_meeting_state(app: AppHandle) -> MeetingState {
    let manager = manager(&app);
    MeetingState {
        status: manager.status(),
        segments: manager.live_segments(),
        recovered: manager.take_recovered(),
    }
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
pub fn pause_meeting(app: AppHandle, paused: bool) -> Result<MeetingStatus, String> {
    manager(&app).set_paused(paused)
}

#[tauri::command]
#[specta::specta]
pub fn add_meeting_note(app: AppHandle, label: Option<String>) -> Result<MeetingMarker, String> {
    manager(&app).add_note(label)
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
        if crate::meeting::is_active() {
            manager.stop(false)?;
        }
        Ok(())
    })
    .await?;
    crate::meeting::confirm_quit();
    exit_app.exit(0);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn list_meetings(app: AppHandle) -> Result<Vec<MeetingSummary>, String> {
    manager(&app).list()
}

/// Sends a past meeting's recording to the transcription queue.
#[tauri::command]
#[specta::specta]
pub fn transcribe_meeting(app: AppHandle, id: String) -> Result<MeetingSummary, String> {
    manager(&app).transcribe(&id)
}

/// Opens a meeting's folder, or the meetings folder for None.
#[tauri::command]
#[specta::specta]
pub fn open_meeting_folder(app: AppHandle, id: Option<String>) -> Result<(), String> {
    let root = crate::meeting::meetings_dir(&app)?;
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let path = match id {
        // Only a folder directly inside the meetings folder.
        Some(id) if !id.is_empty() && !id.contains(['/', '\\']) && id != ".." && id != "." => {
            root.join(id)
        }
        Some(_) => return Err("Invalid meeting id.".to_string()),
        None => root,
    };
    app.opener()
        .open_path(path.to_string_lossy().into_owned(), None::<String>)
        .map_err(|e| format!("Failed to open the folder: {}", e))
}
