//! Commands for the Lemonade Server file transcription engine.

use crate::file_transcription::lemonade::{
    normalize_base_url, LemonadeCheck, LemonadeClient, LemonadeTimeouts,
};
use crate::file_transcription::FileTranscriptionManager;
use crate::settings::{self, FileTranscriptionEngine, LemonadeSettings};
use std::sync::Arc;
use tauri::{AppHandle, Manager};

/// Lists the server's transcription models. Server problems are reported in
/// `LemonadeCheck.error`; Err only for internal failures.
#[tauri::command]
#[specta::specta]
pub async fn check_lemonade_server(
    base_url: String,
    api_key: String,
) -> Result<LemonadeCheck, String> {
    let client = match LemonadeClient::new(&base_url, Some(&api_key), LemonadeTimeouts::default()) {
        Ok(client) => client,
        Err(error) => return Ok(LemonadeCheck::from_result(None, Err(error))),
    };
    // A single attempt: someone is waiting for the answer.
    let result = client.list_models_once().await;
    Ok(LemonadeCheck::from_result(Some(client.api_url()), result))
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
    // Queued jobs show the engine they will start on; let their rows follow.
    if let Some(manager) = app.try_state::<Arc<FileTranscriptionManager>>() {
        manager.refresh_queued_engine(engine);
    }
    Ok(())
}

/// Validates `base_url` (Err "Invalid Lemonade Server address") and stores
/// the trimmed values.
#[tauri::command]
#[specta::specta]
pub fn change_lemonade_settings(app: AppHandle, lemonade: LemonadeSettings) -> Result<(), String> {
    let lemonade = trimmed(lemonade)?;
    let mut settings = settings::get_settings(&app);
    settings.lemonade = lemonade;
    settings::write_settings(&app, settings);
    Ok(())
}

/// `lemonade` with every value trimmed; Err when the address is not usable.
fn trimmed(lemonade: LemonadeSettings) -> Result<LemonadeSettings, String> {
    let base_url = lemonade.base_url.trim().to_string();
    normalize_base_url(&base_url).map_err(|_| "Invalid Lemonade Server address".to_string())?;
    Ok(LemonadeSettings {
        base_url,
        model: lemonade.model.trim().to_string(),
        api_key: lemonade.api_key.trim().to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::lemonade::LemonadeErrorKind;

    #[test]
    fn lemonade_settings_are_trimmed_and_the_address_checked() {
        let stored = trimmed(LemonadeSettings {
            base_url: "  http://localhost:8000/api/v1/ ".to_string(),
            model: " Whisper-Base ".to_string(),
            api_key: " secret\n".to_string(),
        })
        .unwrap();
        // Stored as typed (trimmed); normalized at use.
        assert_eq!(stored.base_url, "http://localhost:8000/api/v1/");
        assert_eq!(stored.model, "Whisper-Base");
        assert_eq!(stored.api_key, "secret");

        for bad in ["", "localhost:13305", "ftp://host", "http://"] {
            let error = trimmed(LemonadeSettings {
                base_url: bad.to_string(),
                ..LemonadeSettings::default()
            })
            .unwrap_err();
            assert_eq!(error, "Invalid Lemonade Server address", "{bad:?}");
        }
    }

    #[test]
    fn checking_a_bad_address_reports_it_without_failing() {
        let check = tauri::async_runtime::block_on(check_lemonade_server(
            "not a url".to_string(),
            String::new(),
        ))
        .unwrap();
        assert!(!check.ok);
        assert_eq!(check.error, Some(LemonadeErrorKind::InvalidUrl));
        assert!(check.api_url.is_none());
        assert!(check.models.is_empty());
    }

    #[test]
    fn checking_an_address_with_a_password_never_shows_it() {
        use crate::file_transcription::lemonade::tests::{json, MockServer, MODELS};
        let server = MockServer::start(vec![json(200, MODELS)]);
        let check = tauri::async_runtime::block_on(check_lemonade_server(
            server
                .url()
                .replacen("http://", "http://alice:s3cretpw@", 1),
            String::new(),
        ))
        .unwrap();
        assert!(check.ok);
        assert_eq!(check.api_url, Some(format!("{}/api/v1", server.url())));

        let check = tauri::async_runtime::block_on(check_lemonade_server(
            "http://alice:s3cretpw@lemon.lan/?x=1".to_string(),
            String::new(),
        ))
        .unwrap();
        assert_eq!(check.error, Some(LemonadeErrorKind::InvalidUrl));
        assert!(!check.detail.unwrap().contains("s3cretpw"));
    }
}
