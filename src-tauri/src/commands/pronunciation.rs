//! Pronunciation samples for the "Add word or phrase" dialog, Handy's take on
//! Dragon's "train the pronunciation". Handy cannot retrain its speech model,
//! so instead the user says the word once and the dialog is told exactly what
//! Handy types for it. If that differs from the word, the dialog saves it as a
//! spoken form, which the custom-word pass then rewrites to the word.
//!
//! The sample goes through the same recorder and transcription as dictation
//! (including custom-word corrections), so the result is what dictating the
//! word would produce. Nothing is pasted, saved to History or shown in the
//! recording overlay.

use crate::audio_toolkit::VadPolicy;
use crate::managers::audio::AudioRecordingManager;
use crate::managers::transcription::TranscriptionManager;
use crate::settings::get_settings;
use std::sync::Arc;
use tauri::{AppHandle, Manager};

/// Recorder binding id for pronunciation samples. A dictation shortcut
/// pressed meanwhile is refused as "already recording", and vice versa.
const BINDING: &str = "pronunciation_sample";

/// Starts recording a pronunciation sample from the selected microphone.
#[tauri::command]
#[specta::specta]
pub fn start_pronunciation_sample(app: AppHandle) -> Result<(), String> {
    if crate::meeting::is_active() {
        return Err("A meeting is being recorded. Training is off until it stops.".to_string());
    }
    let tm = app.state::<Arc<TranscriptionManager>>();
    // Load the model while the user speaks, as dictation does.
    tm.initiate_model_load();

    let policy = if get_settings(&app).vad_enabled {
        VadPolicy::Offline
    } else {
        VadPolicy::Disabled
    };
    app.state::<Arc<AudioRecordingManager>>()
        .try_start_recording(BINDING, policy)
        .map(|_| ())
}

/// Stops the sample and returns what Handy typed for it ("" when nothing was
/// heard).
#[tauri::command]
#[specta::specta]
pub async fn stop_pronunciation_sample(app: AppHandle) -> Result<String, String> {
    let rm = Arc::clone(&app.state::<Arc<AudioRecordingManager>>());
    let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
    tauri::async_runtime::spawn_blocking(move || {
        // Pause file transcription jobs for this short run, as dictation does.
        let _hold = tm.hold_for_dictation();
        let samples = rm
            .stop_recording(BINDING, rm.cancel_generation())
            .ok_or_else(|| "No pronunciation sample is being recorded".to_string())?;
        if samples.is_empty() {
            return Ok(String::new());
        }
        tm.transcribe(samples).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Stops a pronunciation sample without transcribing it. Does nothing when no
/// sample is being recorded, so it never cancels a dictation.
#[tauri::command]
#[specta::specta]
pub fn cancel_pronunciation_sample(app: AppHandle) {
    let rm = app.state::<Arc<AudioRecordingManager>>();
    let _ = rm.stop_recording(BINDING, rm.cancel_generation());
}
