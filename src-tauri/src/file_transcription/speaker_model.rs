//! The Sortformer speaker model: where it lives and its one-time download.
//! It is stored outside `models/`, so it is never offered as a dictation model.

use crate::file_transcription::types::{SpeakerModelEvent, SpeakerModelStatus};
use crate::managers::model::ModelManager;
use hf_hub::api::tokio::CancellationToken;
use log::{error, info};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use tauri::AppHandle;
use tauri_specta::Event;

pub const SPEAKER_MODEL_FILE: &str = "diar_streaming_sortformer_4spk-v2.1-Q8_0.gguf";
pub const SPEAKER_MODEL_URL: &str = "https://huggingface.co/handy-computer/diar_streaming_sortformer_4spk-v2.1-gguf/resolve/ae4afbb5c3d33b71cf2dbf600022b655ee706dd0/diar_streaming_sortformer_4spk-v2.1-Q8_0.gguf";
pub const SPEAKER_MODEL_SHA256: &str =
    "a5dacdc650790266c7a362e54e6bf51952015487edaa606c4e11632bc32442a9";
pub const SPEAKER_MODEL_SIZE: u64 = 139_310_336;

/// <app_data>/speaker-models (portable-aware via crate::portable::app_data_dir).
pub fn speaker_models_dir(app: &AppHandle) -> anyhow::Result<PathBuf> {
    Ok(crate::portable::app_data_dir(app)?.join("speaker-models"))
}

/// The verified model file if present (size == SPEAKER_MODEL_SIZE; the hash is
/// checked at download time only).
pub fn installed_speaker_model(app: &AppHandle) -> Option<PathBuf> {
    let path = speaker_models_dir(app).ok()?.join(SPEAKER_MODEL_FILE);
    is_installed(&path).then_some(path)
}

fn is_installed(path: &Path) -> bool {
    is_installed_at(path, SPEAKER_MODEL_SIZE)
}

fn is_installed_at(path: &Path, size: u64) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && m.len() == size)
}

fn partial_path(dir: &Path) -> PathBuf {
    dir.join(format!("{SPEAKER_MODEL_FILE}.partial"))
}

/// Removes the model and any partial download from `dir`; files that are
/// already gone are fine.
fn remove_model_files(dir: &Path) -> io::Result<()> {
    for path in [dir.join(SPEAKER_MODEL_FILE), partial_path(dir)] {
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// How a download ended, when it did not fail.
enum Downloaded {
    Completed,
    Cancelled,
}

pub struct SpeakerModelManager {
    app: AppHandle,
    /// Set while a download runs.
    cancel: Mutex<Option<CancellationToken>>,
}

impl SpeakerModelManager {
    pub fn new(app: &AppHandle) -> Arc<Self> {
        Arc::new(Self {
            app: app.clone(),
            cancel: Mutex::new(None),
        })
    }

    fn lock_cancel(&self) -> MutexGuard<'_, Option<CancellationToken>> {
        self.cancel.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn status(&self) -> SpeakerModelStatus {
        let path = installed_speaker_model(&self.app);
        SpeakerModelStatus {
            downloaded: path.is_some(),
            downloading: self.lock_cancel().is_some(),
            size_bytes: SPEAKER_MODEL_SIZE,
            path: path.map(|path| path.to_string_lossy().into_owned()),
        }
    }

    /// Resumable download into <dir>/<file>.partial, sha256 verify, rename.
    /// Emits SpeakerModelEvent::{Progress (≤10/s), Verifying, Completed | Failed | Cancelled}.
    /// Err("already downloading") if one runs. A cancelled download is not an
    /// error. A download that does not complete removes its partial file.
    pub async fn download(&self) -> Result<(), String> {
        let token = {
            let mut cancel = self.lock_cancel();
            if cancel.is_some() {
                return Err("already downloading".to_string());
            }
            let token = CancellationToken::new();
            *cancel = Some(token.clone());
            token
        };
        // Clears the slot however this ends, the future being dropped included.
        let downloading = DownloadSlot(&self.cancel);

        let result = self.fetch(&token).await;
        // Free the slot before announcing the outcome, so a status read in
        // reply to the event never still says a download runs.
        drop(downloading);
        let event = match &result {
            Ok(Downloaded::Completed) => SpeakerModelEvent::Completed,
            Ok(Downloaded::Cancelled) => SpeakerModelEvent::Cancelled,
            Err(e) => {
                error!("Speaker model download failed: {:#}", e);
                SpeakerModelEvent::Failed {
                    message: format!("{e:#}"),
                }
            }
        };
        emit(&self.app, event);
        result.map(|_| ()).map_err(|e| format!("{e:#}"))
    }

    async fn fetch(&self, token: &CancellationToken) -> anyhow::Result<Downloaded> {
        let dir = speaker_models_dir(&self.app)?;
        let app = self.app.clone();
        let on_progress = move |downloaded: u64, total: u64| {
            emit(&app, SpeakerModelEvent::Progress { downloaded, total });
        };
        let app = self.app.clone();
        let on_verifying = move || emit(&app, SpeakerModelEvent::Verifying);
        let source = Source {
            url: SPEAKER_MODEL_URL,
            size: SPEAKER_MODEL_SIZE,
            sha256: SPEAKER_MODEL_SHA256,
        };
        download_into(&dir, &source, token, &on_progress, &on_verifying).await
    }

    pub fn cancel_download(&self) {
        if let Some(token) = self.lock_cancel().as_ref() {
            token.cancel();
        }
    }

    /// Removes the file and any .partial; refused while downloading. A job
    /// labelling speakers keeps its open file on Unix; on Windows removing
    /// it fails and the error is shown.
    pub fn delete(&self) -> Result<(), String> {
        if self.lock_cancel().is_some() {
            return Err("the speaker model is downloading".to_string());
        }
        let dir = speaker_models_dir(&self.app).map_err(|e| e.to_string())?;
        remove_model_files(&dir).map_err(|e| e.to_string())?;
        info!("Removed the speaker model from {}", dir.display());
        Ok(())
    }
}

/// Where the model is downloaded from.
struct Source<'a> {
    url: &'a str,
    size: u64,
    sha256: &'a str,
}

/// Downloads `source` into `dir` as the model (through `<file>.partial`,
/// resuming one already there). A download that is cancelled or fails
/// removes its partial file: the app offers no way to remove it while the
/// model is not installed, and resuming is not worth leaving up to 139 MB
/// behind.
async fn download_into(
    dir: &Path,
    source: &Source<'_>,
    token: &CancellationToken,
    on_progress: &(dyn Fn(u64, u64) + Send + Sync),
    on_verifying: &(dyn Fn() + Send + Sync),
) -> anyhow::Result<Downloaded> {
    let path = dir.join(SPEAKER_MODEL_FILE);
    if is_installed_at(&path, source.size) {
        return Ok(Downloaded::Completed);
    }
    fs::create_dir_all(dir)?;
    let partial = partial_path(dir);
    info!("Downloading the speaker model to {}", path.display());

    let completed = ModelManager::download_verified_file(
        source.url,
        &partial,
        source.size,
        source.sha256,
        token,
        on_progress,
        on_verifying,
    )
    .await;
    match completed {
        Ok(true) => {
            fs::rename(&partial, &path)?;
            info!("Speaker model installed at {}", path.display());
            Ok(Downloaded::Completed)
        }
        Ok(false) => {
            info!("Speaker model download cancelled");
            remove_partial(&partial);
            Ok(Downloaded::Cancelled)
        }
        Err(e) => {
            remove_partial(&partial);
            Err(e)
        }
    }
}

fn remove_partial(partial: &Path) {
    match fs::remove_file(partial) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            error!(
                "Could not remove the partial download {}: {}",
                partial.display(),
                e
            )
        }
        _ => {}
    }
}

fn emit(app: &AppHandle, event: SpeakerModelEvent) {
    if let Err(e) = event.emit(app) {
        error!("Failed to emit speaker-model-event: {}", e);
    }
}

/// Marks the download as over when dropped.
struct DownloadSlot<'a>(&'a Mutex<Option<CancellationToken>>);

impl Drop for DownloadSlot<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_only_at_the_expected_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SPEAKER_MODEL_FILE);
        assert!(!is_installed(&path));
        fs::write(&path, b"truncated").unwrap();
        assert!(!is_installed(&path));
        fs::File::create(&path)
            .unwrap()
            .set_len(SPEAKER_MODEL_SIZE)
            .unwrap();
        assert!(is_installed(&path));
        // A directory of that name is not the model.
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(!is_installed(&path));
    }

    #[test]
    fn remove_model_files_removes_model_and_partial() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(SPEAKER_MODEL_FILE), b"model").unwrap();
        fs::write(partial_path(dir.path()), b"part").unwrap();
        fs::write(dir.path().join("other.gguf"), b"keep").unwrap();

        remove_model_files(dir.path()).unwrap();
        // Nothing left to remove is fine too.
        remove_model_files(dir.path()).unwrap();

        let left: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["other.gguf"]);
    }

    /// Serves one connection: the head of a `length`-byte 200 response and
    /// `body`, then hangs up (`close`) or stalls.
    async fn serve(length: usize, body: &'static [u8], close: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && sock.read_exact(&mut byte).await.is_ok() {
                head.push(byte[0]);
            }
            let reply = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n");
            let _ = sock.write_all(reply.as_bytes()).await;
            let _ = sock.write_all(body).await;
            if close {
                let _ = sock.shutdown().await;
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
            }
        });
        format!("http://{addr}/model.gguf")
    }

    fn files_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[tokio::test]
    async fn a_cancelled_download_leaves_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        let url = serve(100, b"abc", false).await;
        let token = CancellationToken::new();
        let trigger = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            trigger.cancel();
        });
        let source = Source {
            url: &url,
            size: 100,
            sha256: "00",
        };

        let outcome = download_into(dir.path(), &source, &token, &|_, _| {}, &|| {}).await;

        assert!(matches!(outcome, Ok(Downloaded::Cancelled)));
        assert!(
            files_in(dir.path()).is_empty(),
            "{:?}",
            files_in(dir.path())
        );
    }

    #[tokio::test]
    async fn a_failed_download_leaves_no_partial() {
        let dir = tempfile::tempdir().unwrap();
        // The server hangs up after 3 of 100 bytes.
        let url = serve(100, b"abc", true).await;
        let source = Source {
            url: &url,
            size: 100,
            sha256: "00",
        };

        let outcome = download_into(
            dir.path(),
            &source,
            &CancellationToken::new(),
            &|_, _| {},
            &|| {},
        )
        .await;

        assert!(outcome.is_err());
        assert!(
            files_in(dir.path()).is_empty(),
            "{:?}",
            files_in(dir.path())
        );
    }

    #[tokio::test]
    async fn a_completed_download_installs_the_model() {
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().unwrap();
        let url = serve(5, b"model", true).await;
        let sha256 = format!("{:x}", Sha256::digest(b"model"));
        let source = Source {
            url: &url,
            size: 5,
            sha256: &sha256,
        };

        let outcome = download_into(
            dir.path(),
            &source,
            &CancellationToken::new(),
            &|_, _| {},
            &|| {},
        )
        .await;

        assert!(matches!(outcome, Ok(Downloaded::Completed)));
        assert_eq!(files_in(dir.path()), [SPEAKER_MODEL_FILE]);
    }

    #[test]
    fn download_slot_clears_when_dropped() {
        let slot = Mutex::new(Some(CancellationToken::new()));
        drop(DownloadSlot(&slot));
        assert!(slot.lock().unwrap().is_none());
    }
}
