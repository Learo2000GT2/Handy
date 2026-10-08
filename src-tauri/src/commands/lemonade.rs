//! Commands for the Lemonade Server file transcription engine.
//!
//! Contract stub: the server check and the Lemonade settings land with the
//! Lemonade engine work.
#![allow(dead_code, unused_variables, unused_imports)]

use crate::file_transcription::lemonade::LemonadeCheck;
use crate::settings::{self, FileTranscriptionEngine, LemonadeSettings};
use tauri::AppHandle;

/// Lists the server's transcription models. Server problems are reported in
/// `LemonadeCheck.error`; Err only for internal failures.
#[tauri::command]
#[specta::specta]
pub async fn check_lemonade_server(
    base_url: String,
    api_key: String,
) -> Result<LemonadeCheck, String> {
    Err("not implemented".to_string())
}

#[tauri::command]
#[specta::specta]
pub fn change_file_transcription_engine_setting(
    app: AppHandle,
    engine: FileTranscriptionEngine,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.file_transcription_engine = engine;
    settings::write_settings(&app, settings);
    Ok(())
}

/// Validates `base_url` (Err "Invalid Lemonade Server address") and stores
/// the trimmed values.
#[tauri::command]
#[specta::specta]
pub fn change_lemonade_settings(app: AppHandle, lemonade: LemonadeSettings) -> Result<(), String> {
    Err("not implemented".to_string())
}
