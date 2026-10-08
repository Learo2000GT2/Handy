//! Lemonade Server (OpenAI-compatible Whisper endpoint) as a file
//! transcription engine.
//!
//! Each chunk is sent as a 16 kHz mono WAV to
//! `POST {root}/api/v1/audio/transcriptions` and the answer is post-processed
//! like a local chunk. A Lemonade job never touches Handy's own engine: no
//! lease, no run gate, no loaded model, and it never waits for dictation.
//!
//! The HTTP client is async (`reqwest`); the pipeline thread blocks on it
//! with `tauri::async_runtime::block_on`, polling the job's cancel flag so a
//! cancel drops the request within [`CANCEL_POLL_INTERVAL`].

use crate::engine_supervisor::WorkToken;
use crate::file_transcription::engine::{ChunkTranscriber, TranscribeError};
use crate::file_transcription::types::FileJobErrorCode;
use crate::managers::transcription::{
    ChunkLimits, ChunkTranscript, ExternalChunkShaper, ExternalRun, ExternalTimed,
};
use crate::settings::{AppSettings, LemonadeSettings};
use log::{debug, info, warn};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use specta::Type;
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Prefix of a Lemonade job's model id: "lemonade:Whisper-Base".
pub const LEMONADE_MODEL_PREFIX: &str = "lemonade:";

/// The API prefix older Lemonade releases serve, and newer ones still accept.
const API_PREFIX: &str = "/api/v1";
/// The OpenAI-style prefix, tried when the server has no `/api/v1`.
const OPENAI_PREFIX: &str = "/v1";
/// How often a blocking call checks its cancel flag.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Longest excerpt of an error body kept for messages.
const BODY_EXCERPT_CHARS: usize = 300;
/// Bodies that say a model is unknown contain "model" and one of these.
const MODEL_MISSING_PHRASES: &[&str] = &[
    "not found",
    "unknown",
    "does not exist",
    "not available",
    "no model",
    "not supported",
];

/// The model's display name for a Lemonade job's model id
/// ("lemonade:Whisper-Base" → "Whisper-Base (Lemonade)"); None for other ids.
pub fn display_model_name(model_id: &str) -> Option<String> {
    model_id
        .strip_prefix(LEMONADE_MODEL_PREFIX)
        .map(|model| format!("{} (Lemonade)", model))
}

/// The Lemonade Server a job uses, captured when the job starts.
#[derive(Clone, Debug)]
pub struct LemonadeJobConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

impl LemonadeJobConfig {
    /// From settings (+ optional CLI overrides); api_key None when blank.
    pub fn from_settings(s: &LemonadeSettings) -> Self {
        let api_key = s.api_key.trim();
        Self {
            base_url: s.base_url.trim().to_string(),
            model: s.model.trim().to_string(),
            api_key: (!api_key.is_empty()).then(|| api_key.to_string()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct LemonadeModel {
    pub id: String,
    pub recipe: Option<String>,
    pub labels: Vec<String>,
    /// None when the server did not say.
    pub downloaded: Option<bool>,
}

impl LemonadeModel {
    /// labels contain "transcription", or recipe == "whispercpp", or id starts with "whisper" (any case).
    pub fn is_transcription(&self) -> bool {
        self.labels
            .iter()
            .any(|label| label.eq_ignore_ascii_case("transcription"))
            || self
                .recipe
                .as_deref()
                .is_some_and(|recipe| recipe.eq_ignore_ascii_case("whispercpp"))
            || self
                .id
                .get(..7)
                .is_some_and(|start| start.eq_ignore_ascii_case("whisper"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "snake_case")]
pub enum LemonadeErrorKind {
    InvalidUrl,
    Unreachable,
    Timeout,
    Unauthorized,
    ModelNotFound,
    Http,
    BadResponse,
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize, Type)]
pub struct LemonadeCheck {
    pub ok: bool,
    pub error: Option<LemonadeErrorKind>,
    /// English detail (HTTP status/body excerpt, OS error) for a tooltip.
    pub detail: Option<String>,
    /// The URL actually used, e.g. "http://localhost:13305/api/v1".
    pub api_url: Option<String>,
    /// Transcription models only, server order.
    pub models: Vec<LemonadeModel>,
    pub other_models: u32,
}

impl LemonadeCheck {
    /// The answer to a server check: the transcription models of a model
    /// list, or the error.
    pub fn from_result(
        api_url: Option<String>,
        result: Result<Vec<LemonadeModel>, LemonadeError>,
    ) -> Self {
        match result {
            Ok(all) => {
                let (models, others): (Vec<_>, Vec<_>) =
                    all.into_iter().partition(LemonadeModel::is_transcription);
                Self {
                    ok: true,
                    error: None,
                    detail: None,
                    api_url,
                    models,
                    other_models: u32::try_from(others.len()).unwrap_or(u32::MAX),
                }
            }
            Err(error) => Self {
                ok: false,
                error: Some(error.kind()),
                detail: Some(error.to_string()),
                api_url,
                models: Vec::new(),
                other_models: 0,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum LemonadeError {
    InvalidUrl(String),
    Unreachable(String),
    Timeout(Duration),
    Unauthorized {
        status: u16,
    },
    ModelNotFound {
        model: String,
        detail: String,
    },
    /// `body` trimmed to 300 characters.
    Http {
        status: u16,
        body: String,
    },
    BadResponse(String),
    Cancelled,
}

impl LemonadeError {
    pub fn kind(&self) -> LemonadeErrorKind {
        match self {
            Self::InvalidUrl(_) => LemonadeErrorKind::InvalidUrl,
            Self::Unreachable(_) => LemonadeErrorKind::Unreachable,
            Self::Timeout(_) => LemonadeErrorKind::Timeout,
            Self::Unauthorized { .. } => LemonadeErrorKind::Unauthorized,
            Self::ModelNotFound { .. } => LemonadeErrorKind::ModelNotFound,
            Self::Http { .. } => LemonadeErrorKind::Http,
            Self::BadResponse(_) => LemonadeErrorKind::BadResponse,
            Self::Cancelled => LemonadeErrorKind::Cancelled,
        }
    }

    /// Unreachable, Timeout, Http 408/429/5xx.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Unreachable(_) | Self::Timeout(_) => true,
            Self::Http { status, .. } => matches!(status, 408 | 429 | 500..=599),
            _ => false,
        }
    }
}

impl fmt::Display for LemonadeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(input) => write!(
                f,
                "Invalid Lemonade Server address \"{}\" (expected something like {})",
                input,
                crate::settings::DEFAULT_LEMONADE_URL
            ),
            Self::Unreachable(detail) => write!(f, "Lemonade Server can't be reached: {}", detail),
            Self::Timeout(after) => write!(
                f,
                "Lemonade Server didn't answer within {} s",
                after.as_secs_f64().ceil()
            ),
            Self::Unauthorized { status } => write!(
                f,
                "Lemonade Server didn't accept the API key (HTTP {})",
                status
            ),
            Self::ModelNotFound { model, detail } => write!(
                f,
                "Lemonade Server doesn't have the model \"{}\": {}",
                model, detail
            ),
            Self::Http { status, body } if body.is_empty() => {
                write!(f, "Lemonade Server returned HTTP {}", status)
            }
            Self::Http { status, body } => {
                write!(f, "Lemonade Server returned HTTP {}: {}", status, body)
            }
            Self::BadResponse(detail) => write!(
                f,
                "Lemonade Server sent an answer Handy couldn't read: {}",
                detail
            ),
            Self::Cancelled => f.write_str("The Lemonade Server request was cancelled"),
        }
    }
}

impl std::error::Error for LemonadeError {}

#[derive(Clone, Copy, Debug)]
pub struct LemonadeTimeouts {
    pub connect: Duration,
    pub list: Duration,
    /// First request of a job when the server did not list the model as
    /// downloaded: Lemonade downloads and loads it on first use.
    pub first_request: Duration,
    /// First request of a job when the model is downloaded (it may still
    /// have to be loaded).
    pub first_request_loaded: Duration,
    pub request_base: Duration,
    /// Added per second of chunk audio to `request_base`.
    pub request_per_audio_sec: Duration,
    /// Waits before the second and the third attempt when the server can't
    /// be reached.
    pub unreachable_retry_delays: [Duration; 2],
}

impl Default for LemonadeTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            list: Duration::from_secs(10),
            first_request: Duration::from_secs(30 * 60),
            first_request_loaded: Duration::from_secs(10 * 60),
            request_base: Duration::from_secs(60),
            request_per_audio_sec: Duration::from_secs(6),
            unreachable_retry_delays: [Duration::from_secs(1), Duration::from_secs(4)],
        }
    }
}

/// One `POST /audio/transcriptions`.
pub struct TranscriptionRequest<'a> {
    pub model: &'a str,
    pub wav: &'a [u8],
    pub file_name: &'a str,
    /// ISO 639-1; None lets the server detect the language.
    pub language: Option<&'a str>,
    pub timeout: Duration,
}

/// A parsed transcription answer.
#[derive(Clone, Debug, PartialEq)]
pub struct LemonadeTranscription {
    /// Trimmed.
    pub text: String,
    /// ISO 639-1, when the server reported a language it could be mapped from.
    pub language: Option<String>,
    /// Times relative to the uploaded audio; empty when the server gave none.
    pub segments: Vec<ExternalTimed>,
}

/// HTTP client for one Lemonade Server.
pub struct LemonadeClient {
    http: reqwest::Client,
    /// The address without a trailing "/" or API prefix.
    root: String,
    /// The API prefix in use: given in the address, or found by `list_models`.
    prefix: Mutex<&'static str>,
    /// The address named its prefix, so the `/v1` fallback is not tried.
    explicit_prefix: bool,
    api_key: Option<String>,
    /// Cleared for good when the server rejects `response_format=verbose_json`.
    verbose_json: AtomicBool,
    timeouts: LemonadeTimeouts,
}

impl LemonadeClient {
    pub fn new(
        base_url: &str,
        api_key: Option<&str>,
        timeouts: LemonadeTimeouts,
    ) -> Result<Self, LemonadeError> {
        let (root, prefix) = normalize_base_url(base_url)?;
        let mut builder = reqwest::Client::builder().connect_timeout(timeouts.connect);
        // A server on this computer is never reached through a system proxy.
        if is_loopback_url(&root) {
            builder = builder.no_proxy();
        }
        let http = builder.build().map_err(|e| {
            LemonadeError::Unreachable(format!("could not start the HTTP client: {}", e))
        })?;
        Ok(Self {
            http,
            root,
            prefix: Mutex::new(prefix.unwrap_or(API_PREFIX)),
            explicit_prefix: prefix.is_some(),
            api_key: api_key
                .map(str::trim)
                .filter(|key| !key.is_empty())
                .map(str::to_string),
            verbose_json: AtomicBool::new(true),
            timeouts,
        })
    }

    /// The API base in use, e.g. "http://localhost:13305/api/v1".
    pub fn api_url(&self) -> String {
        format!("{}{}", self.root, self.prefix())
    }

    fn prefix(&self) -> &'static str {
        *self.prefix.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `GET /models` (all models, downloaded or not). Retries a server that
    /// can't be reached, and falls back to `/v1` when `/api/v1` is missing.
    pub async fn list_models(&self) -> Result<Vec<LemonadeModel>, LemonadeError> {
        self.retry_unreachable(|| self.list_models_once()).await
    }

    /// [`list_models`](Self::list_models) without retries, for a quick
    /// server check.
    pub async fn list_models_once(&self) -> Result<Vec<LemonadeModel>, LemonadeError> {
        let prefixes = if self.explicit_prefix {
            vec![self.prefix()]
        } else {
            vec![API_PREFIX, OPENAI_PREFIX]
        };
        let timeout = self.timeouts.list;
        let mut not_found = None;
        for prefix in prefixes {
            let url = format!("{}{}/models?show_all=true", self.root, prefix);
            let response = self
                .request(Method::GET, &url)?
                .timeout(timeout)
                .send()
                .await
                .map_err(|e| transport_error(&e, timeout))?;
            let status = response.status().as_u16();
            let body = response
                .bytes()
                .await
                .map_err(|e| transport_error(&e, timeout))?;
            match status {
                200..=299 => {
                    *self.prefix.lock().unwrap_or_else(|e| e.into_inner()) = prefix;
                    return parse_models(&body);
                }
                401 | 403 => return Err(LemonadeError::Unauthorized { status }),
                404 => {
                    debug!("No Lemonade API at {}{}", self.root, prefix);
                    not_found = Some(body_excerpt(&body));
                }
                _ => {
                    return Err(LemonadeError::Http {
                        status,
                        body: body_excerpt(&body),
                    })
                }
            }
        }
        Err(LemonadeError::Http {
            status: 404,
            body: not_found.unwrap_or_default(),
        })
    }

    /// `POST /audio/transcriptions`. Retries a server that can't be reached,
    /// and falls back to `response_format=json` for good when the server
    /// rejects `verbose_json`.
    pub async fn transcribe(
        &self,
        req: &TranscriptionRequest<'_>,
    ) -> Result<LemonadeTranscription, LemonadeError> {
        self.retry_unreachable(|| self.transcribe_once(req)).await
    }

    async fn transcribe_once(
        &self,
        req: &TranscriptionRequest<'_>,
    ) -> Result<LemonadeTranscription, LemonadeError> {
        let verbose = self.verbose_json.load(Ordering::Acquire);
        match self.post_transcription(req, verbose).await {
            Err(LemonadeError::Http { status: 400, body })
                if verbose && body.to_ascii_lowercase().contains("response_format") =>
            {
                info!(
                    "Lemonade Server rejected verbose_json ({}); using json, without timestamps",
                    body
                );
                self.verbose_json.store(false, Ordering::Release);
                self.post_transcription(req, false).await
            }
            other => other,
        }
    }

    async fn post_transcription(
        &self,
        req: &TranscriptionRequest<'_>,
        verbose: bool,
    ) -> Result<LemonadeTranscription, LemonadeError> {
        let format = if verbose { "verbose_json" } else { "json" };
        let mut fields = vec![("model", req.model), ("response_format", format)];
        if let Some(language) = req.language {
            fields.push(("language", language));
        }
        let boundary = new_boundary(req.wav);
        let body = build_multipart(
            &boundary,
            &fields,
            "file",
            req.file_name,
            "audio/wav",
            req.wav,
        );
        let url = format!("{}/audio/transcriptions", self.api_url());
        let response = self
            .request(Method::POST, &url)?
            .header(
                CONTENT_TYPE,
                format!("multipart/form-data; boundary={}", boundary),
            )
            .body(body)
            .timeout(req.timeout)
            .send()
            .await
            .map_err(|e| transport_error(&e, req.timeout))?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let body = response
            .bytes()
            .await
            .map_err(|e| transport_error(&e, req.timeout))?;
        if !(200..300).contains(&status) {
            return Err(classify_status(
                status,
                &String::from_utf8_lossy(&body),
                req.model,
            ));
        }
        parse_transcription(&body, content_type.as_deref())
    }

    fn request(&self, method: Method, url: &str) -> Result<reqwest::RequestBuilder, LemonadeError> {
        let url = Url::parse(url).map_err(|_| LemonadeError::InvalidUrl(url.to_string()))?;
        let mut request = self.http.request(method, url);
        if let Some(key) = &self.api_key {
            request = request.header(AUTHORIZATION, format!("Bearer {}", key));
        }
        Ok(request)
    }

    /// Runs `attempt` up to three times while the server can't be reached,
    /// waiting `unreachable_retry_delays` in between.
    async fn retry_unreachable<T, F, Fut>(&self, attempt: F) -> Result<T, LemonadeError>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<T, LemonadeError>>,
    {
        let mut delays = self.timeouts.unreachable_retry_delays.into_iter();
        loop {
            match attempt().await {
                Err(LemonadeError::Unreachable(detail)) => match delays.next() {
                    Some(delay) => {
                        debug!(
                            "Lemonade Server can't be reached ({}); retrying in {:?}",
                            detail, delay
                        );
                        tokio::time::sleep(delay).await;
                    }
                    None => return Err(LemonadeError::Unreachable(detail)),
                },
                other => return other,
            }
        }
    }

    /// [`list_models`](Self::list_models), blocking until it is done or
    /// `cancel` is set. Must not be called on a tokio runtime thread.
    pub fn list_models_blocking(
        &self,
        cancel: &AtomicBool,
    ) -> Result<Vec<LemonadeModel>, LemonadeError> {
        block_on_cancellable(self.list_models(), cancel)
    }

    /// [`transcribe`](Self::transcribe), blocking until it is done or
    /// `cancel` is set (which drops the request). Must not be called on a
    /// tokio runtime thread.
    pub fn transcribe_blocking(
        &self,
        req: &TranscriptionRequest<'_>,
        cancel: &AtomicBool,
    ) -> Result<LemonadeTranscription, LemonadeError> {
        block_on_cancellable(self.transcribe(req), cancel)
    }
}

/// Blocks on `operation`, checking `cancel` every [`CANCEL_POLL_INTERVAL`];
/// a cancel drops the operation, and with it its request.
fn block_on_cancellable<T>(
    operation: impl Future<Output = Result<T, LemonadeError>>,
    cancel: &AtomicBool,
) -> Result<T, LemonadeError> {
    debug_assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "blocking Lemonade call on an async runtime thread"
    );
    tauri::async_runtime::block_on(async {
        tokio::pin!(operation);
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(LemonadeError::Cancelled);
            }
            if let Ok(result) = tokio::time::timeout(CANCEL_POLL_INTERVAL, operation.as_mut()).await
            {
                return result;
            }
        }
    })
}

/// A transport failure: a timeout, a body that broke off, or a server that
/// can't be reached (refused, DNS, reset before an answer).
fn transport_error(error: &reqwest::Error, timeout: Duration) -> LemonadeError {
    if error.is_timeout() {
        return LemonadeError::Timeout(timeout);
    }
    let detail = error_chain(error);
    if error.is_body() || error.is_decode() {
        LemonadeError::BadResponse(detail)
    } else {
        LemonadeError::Unreachable(detail)
    }
}

/// An error and its causes, e.g. "error sending request for url (…): client
/// error (Connect): tcp connect error: Connection refused (os error 111)".
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut source = error.source();
    // Bounded in case a source chain is cyclic.
    for _ in 0..8 {
        let Some(cause) = source else {
            break;
        };
        let text = cause.to_string();
        if !parts.iter().any(|part| part.contains(&text)) {
            parts.push(text);
        }
        source = cause.source();
    }
    parts.join(": ")
}

/// Whether `url` names this computer (localhost, 127.0.0.0/8, ::1).
fn is_loopback_url(url: &str) -> bool {
    let Some(host) = Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
    else {
        return false;
    };
    match host.trim_start_matches('[').trim_end_matches(']').parse() {
        Ok(IpAddr::V4(ip)) => ip.is_loopback(),
        Ok(IpAddr::V6(ip)) => ip.is_loopback(),
        Err(_) => host == "localhost" || host.ends_with(".localhost"),
    }
}

/// Splits an address as typed into its root and the API prefix it names, if
/// any: trims, drops trailing "/", then a trailing "/api/v1" or "/v1".
/// Requires http(s) and a host.
pub fn normalize_base_url(input: &str) -> Result<(String, Option<&'static str>), LemonadeError> {
    let invalid = || LemonadeError::InvalidUrl(input.trim().to_string());
    let mut root = input.trim().trim_end_matches('/');
    let mut prefix = None;
    for candidate in [API_PREFIX, OPENAI_PREFIX] {
        let Some(cut) = root.len().checked_sub(candidate.len()) else {
            continue;
        };
        if root.is_char_boundary(cut) && root[cut..].eq_ignore_ascii_case(candidate) {
            root = root[..cut].trim_end_matches('/');
            prefix = Some(candidate);
            break;
        }
    }
    let url = Url::parse(root).map_err(|_| invalid())?;
    let has_host = url.host_str().is_some_and(|host| !host.is_empty());
    if !matches!(url.scheme(), "http" | "https")
        || !has_host
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    Ok((root.to_string(), prefix))
}

/// A 16 kHz mono 16-bit PCM WAV of `samples` (clamped; NaN is silence).
pub fn encode_wav_16k_mono(samples: &[f32]) -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16_000;
    const BYTES_PER_SAMPLE: u16 = 2;
    const HEADER_LEN: usize = 44;
    // A WAV can't describe more than 4 GiB (about 37 h); no chunk is near that.
    let max_samples = (u32::MAX as usize - HEADER_LEN) / BYTES_PER_SAMPLE as usize;
    let samples = &samples[..samples.len().min(max_samples)];
    let data_len = samples.len() * BYTES_PER_SAMPLE as usize;
    // Both fit in u32 because of the cap above.
    let data_len_u32 = data_len as u32;

    let mut wav = Vec::with_capacity(HEADER_LEN + data_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(data_len_u32 + 36).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * u32::from(BYTES_PER_SAMPLE)).to_le_bytes());
    wav.extend_from_slice(&BYTES_PER_SAMPLE.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len_u32.to_le_bytes());
    for &sample in samples {
        let sample = if sample.is_nan() { 0.0 } else { sample };
        let value = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        wav.extend_from_slice(&value.to_le_bytes());
    }
    wav
}

/// A multipart boundary ("handy-" and 32 hex digits) that does not occur in
/// `payload`.
pub fn new_boundary(payload: &[u8]) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        let seed = nanos ^ u128::from(count) ^ (u128::from(std::process::id()) << 64);
        let digest = Sha256::digest(seed.to_le_bytes());
        let hex: String = digest[..16]
            .iter()
            .map(|byte| format!("{:02x}", byte))
            .collect();
        let boundary = format!("handy-{}", hex);
        if !contains_subslice(payload, boundary.as_bytes()) {
            return boundary;
        }
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// A `multipart/form-data` body: the text `fields` in order, then the file.
pub fn build_multipart(
    boundary: &str,
    fields: &[(&str, &str)],
    file_field: &str,
    file_name: &str,
    content_type: &str,
    file: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(file.len() + 512);
    for (name, value) in fields {
        body.extend_from_slice(
            format!(
                "--{}\r\nContent-Disposition: form-data; name=\"{}\"\r\n\r\n{}\r\n",
                boundary, name, value
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{}\r\nContent-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
            boundary, file_field, file_name, content_type
        )
        .as_bytes(),
    );
    body.extend_from_slice(file);
    body.extend_from_slice(format!("\r\n--{}--\r\n", boundary).as_bytes());
    body
}

/// Reads a 2xx transcription body: OpenAI `verbose_json` (segments in
/// seconds), compact `json`, or plain text from a server that ignored the
/// requested format.
pub fn parse_transcription(
    body: &[u8],
    content_type: Option<&str>,
) -> Result<LemonadeTranscription, LemonadeError> {
    let text = String::from_utf8_lossy(body);
    if !text.trim_start().starts_with('{') {
        let says_json = content_type.is_some_and(|ct| ct.to_ascii_lowercase().contains("json"));
        if says_json && !text.trim().is_empty() {
            return Err(LemonadeError::BadResponse(format!(
                "expected a JSON object, got: {}",
                body_excerpt(body)
            )));
        }
        return Ok(LemonadeTranscription {
            text: text.trim().to_string(),
            language: None,
            segments: Vec::new(),
        });
    }
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| LemonadeError::BadResponse(format!("invalid JSON: {}", e)))?;
    let Some(transcript) = value.get("text").and_then(Value::as_str) else {
        return Err(LemonadeError::BadResponse(format!(
            "no \"text\" in: {}",
            body_excerpt(body)
        )));
    };
    let language = value
        .get("language")
        .and_then(Value::as_str)
        .and_then(normalize_language);
    let segments = value
        .get("segments")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(parse_segment).collect())
        .unwrap_or_default();
    Ok(LemonadeTranscription {
        text: transcript.trim().to_string(),
        language,
        segments,
    })
}

/// One `{start, end, text}` row (seconds); None unless its times are finite,
/// non-negative and in order.
fn parse_segment(row: &Value) -> Option<ExternalTimed> {
    let start = row.get("start")?.as_f64()?;
    let end = row.get("end")?.as_f64()?;
    let text = row.get("text")?.as_str()?;
    if !start.is_finite() || !end.is_finite() || start < 0.0 || end < start {
        return None;
    }
    Some(ExternalTimed {
        start_ms: (start * 1000.0).round() as i64,
        end_ms: (end * 1000.0).round() as i64,
        text: text.to_string(),
    })
}

/// Reads a `GET /models` body (`{"data": [...]}` or a bare array). Rows
/// without an id are skipped.
pub fn parse_models(body: &[u8]) -> Result<Vec<LemonadeModel>, LemonadeError> {
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| LemonadeError::BadResponse(format!("invalid model list: {}", e)))?;
    let rows = value
        .get("data")
        .and_then(Value::as_array)
        .or_else(|| value.as_array())
        .ok_or_else(|| {
            LemonadeError::BadResponse(format!("no model list in: {}", body_excerpt(body)))
        })?;
    Ok(rows
        .iter()
        .filter_map(|row| {
            let id = row.get("id")?.as_str()?.trim();
            if id.is_empty() {
                return None;
            }
            let labels = row
                .get("labels")
                .and_then(Value::as_array)
                .map(|labels| {
                    labels
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Some(LemonadeModel {
                id: id.to_string(),
                recipe: row
                    .get("recipe")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                labels,
                downloaded: row.get("downloaded").and_then(Value::as_bool),
            })
        })
        .collect())
}

/// The error for a non-2xx answer to a request for `model`.
pub fn classify_status(status: u16, body: &str, model: &str) -> LemonadeError {
    let lower = body.to_ascii_lowercase();
    let says_model_missing = || {
        lower.contains("model")
            && MODEL_MISSING_PHRASES
                .iter()
                .any(|phrase| lower.contains(phrase))
    };
    match status {
        401 | 403 => LemonadeError::Unauthorized { status },
        // A rejected verbose_json (retried with json). Such bodies may also
        // say "not supported", which must not read as a missing model.
        400 if lower.contains("response_format") => LemonadeError::Http {
            status,
            body: body_excerpt(body.as_bytes()),
        },
        400 | 404 | 422 | 500 if says_model_missing() => LemonadeError::ModelNotFound {
            model: model.to_string(),
            detail: body_excerpt(body.as_bytes()),
        },
        _ => LemonadeError::Http {
            status,
            body: body_excerpt(body.as_bytes()),
        },
    }
}

/// The first [`BODY_EXCERPT_CHARS`] characters of a body, on one line.
fn body_excerpt(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(BODY_EXCERPT_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// Settings language → ISO 639-1 to send, None for "auto" or codes Lemonade can't take.
pub fn request_language(selected_language: &str) -> Option<String> {
    let selected = selected_language.trim();
    if selected.is_empty() || selected.eq_ignore_ascii_case("auto") {
        return None;
    }
    let code = crate::managers::model::canonical_language_code(selected);
    (code.len() == 2 && code.bytes().all(|b| b.is_ascii_alphabetic()))
        .then(|| code.to_ascii_lowercase())
}

/// "en", "EN", "en-US", "eng", "english", "English" → "en"
/// (isolang::Language::from_name / from_639_1).
pub fn normalize_language(value: &str) -> Option<String> {
    let value = value.trim();
    let base = value.split(['-', '_']).next().unwrap_or(value);
    let language = if base.len() <= 3 && base.bytes().all(|b| b.is_ascii_alphabetic()) {
        let code = base.to_ascii_lowercase();
        match code.len() {
            2 => isolang::Language::from_639_1(&code),
            3 => isolang::Language::from_639_3(&code),
            _ => None,
        }
    } else {
        // An English name, as Whisper reports it ("english" → "English").
        let mut chars = value.chars();
        let name: String = match chars.next() {
            Some(first) => first
                .to_uppercase()
                .chain(chars.flat_map(char::to_lowercase))
                .collect(),
            None => return None,
        };
        isolang::Language::from_name(&name)
    };
    language
        .and_then(|language| language.to_639_1())
        .map(str::to_string)
}

/// A file job's Lemonade engine: one HTTP request per chunk, post-processed
/// like a local chunk. Never takes the lease, the run gate or the loaded model.
pub struct LemonadeTranscriber {
    client: LemonadeClient,
    model: String,
    model_id: String,
    /// Sent with every request; None lets the server detect it.
    language: Option<String>,
    shaper: ExternalChunkShaper,
    token: WorkToken,
    timeouts: LemonadeTimeouts,
    /// The server listed the model as downloaded.
    model_downloaded: bool,
    /// A request of this job has succeeded, so the model is loaded.
    first_done: AtomicBool,
    /// Requests sent, for the uploaded file names.
    requests: AtomicU32,
}

impl LemonadeTranscriber {
    /// Preflight GET /models (list timeout). Unreachable → Stop{ServerUnreachable};
    /// 401/403 → Stop{ServerUnauthorized}; cancel → Cancelled. A model missing from the
    /// list is NOT fatal (Lemonade may download on first use) but makes the first request
    /// use `first_request` timeout.
    pub fn begin(
        cfg: &LemonadeJobConfig,
        settings: AppSettings,
        token: WorkToken,
    ) -> Result<Self, TranscribeError> {
        Self::begin_with(cfg, settings, token, LemonadeTimeouts::default())
    }

    fn begin_with(
        cfg: &LemonadeJobConfig,
        settings: AppSettings,
        token: WorkToken,
        timeouts: LemonadeTimeouts,
    ) -> Result<Self, TranscribeError> {
        if token.is_cancelled() {
            return Err(TranscribeError::Cancelled);
        }
        let client = LemonadeClient::new(&cfg.base_url, cfg.api_key.as_deref(), timeouts)
            .map_err(preflight_error)?;
        let models = client
            .list_models_blocking(token.flag())
            .map_err(preflight_error)?;
        let listed = models.iter().find(|model| model.id == cfg.model);
        match listed {
            None => warn!(
                "Lemonade Server at {} doesn't list the model {}; it may download it on first use",
                client.api_url(),
                cfg.model
            ),
            Some(model) if !model.is_transcription() => warn!(
                "The Lemonade model {} doesn't look like a transcription model",
                cfg.model
            ),
            Some(_) => {}
        }
        if settings.translate_to_english {
            info!("Translate to English isn't available with Lemonade Server; transcribing in the spoken language");
        }
        let model_downloaded = listed.is_some_and(|model| model.downloaded != Some(false));
        let language = request_language(&settings.selected_language);
        debug!(
            "Lemonade file job on {} with {} (language {:?})",
            client.api_url(),
            cfg.model,
            language
        );
        Ok(Self {
            client,
            model: cfg.model.clone(),
            model_id: format!("{}{}", LEMONADE_MODEL_PREFIX, cfg.model),
            language,
            shaper: ExternalChunkShaper::new(settings),
            token,
            timeouts,
            model_downloaded,
            first_done: AtomicBool::new(false),
            requests: AtomicU32::new(0),
        })
    }

    /// The job's first request may wait for the model to be downloaded or
    /// loaded; later ones are given time in proportion to their audio.
    fn request_timeout(&self, real_len_ms: u64) -> Duration {
        let secs = u32::try_from(real_len_ms.div_ceil(1000)).unwrap_or(u32::MAX);
        let scaled = self
            .timeouts
            .request_base
            .saturating_add(self.timeouts.request_per_audio_sec.saturating_mul(secs));
        if self.first_done.load(Ordering::Acquire) {
            return scaled;
        }
        let first = if self.model_downloaded {
            self.timeouts.first_request_loaded
        } else {
            self.timeouts.first_request
        };
        first.max(scaled)
    }

    /// How a chunk's failed request ends: `Stop` when no later chunk could
    /// succeed either, `Failed` when the pipeline may retry.
    fn chunk_error(&self, error: LemonadeError) -> TranscribeError {
        match error {
            LemonadeError::Cancelled => TranscribeError::Cancelled,
            LemonadeError::InvalidUrl(_) | LemonadeError::Unreachable(_) => TranscribeError::Stop {
                code: FileJobErrorCode::ServerUnreachable,
                message: error.to_string(),
            },
            LemonadeError::Unauthorized { .. } => TranscribeError::Stop {
                code: FileJobErrorCode::ServerUnauthorized,
                message: error.to_string(),
            },
            LemonadeError::ModelNotFound { .. } => TranscribeError::Stop {
                code: FileJobErrorCode::ServerModelMissing,
                message: error.to_string(),
            },
            // Its model list answered, so the server is there but has no
            // transcription endpoint.
            LemonadeError::Http { status: 404, .. } => TranscribeError::Stop {
                code: FileJobErrorCode::EngineFailed,
                message: format!(
                    "This Lemonade Server doesn't offer transcription; update Lemonade Server ({})",
                    error
                ),
            },
            LemonadeError::Timeout(_)
            | LemonadeError::Http { .. }
            | LemonadeError::BadResponse(_) => TranscribeError::Failed(anyhow::Error::new(error)),
        }
    }
}

/// How a failed preflight ends the job.
fn preflight_error(error: LemonadeError) -> TranscribeError {
    let code = match &error {
        LemonadeError::Cancelled => return TranscribeError::Cancelled,
        LemonadeError::InvalidUrl(_)
        | LemonadeError::Unreachable(_)
        | LemonadeError::Timeout(_) => FileJobErrorCode::ServerUnreachable,
        LemonadeError::Unauthorized { .. } => FileJobErrorCode::ServerUnauthorized,
        LemonadeError::ModelNotFound { .. }
        | LemonadeError::Http { .. }
        | LemonadeError::BadResponse(_) => FileJobErrorCode::EngineFailed,
    };
    TranscribeError::Stop {
        code,
        message: error.to_string(),
    }
}

impl ChunkTranscriber for LemonadeTranscriber {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn limits(&self) -> ChunkLimits {
        ChunkLimits {
            max_chunk_ms: 28_000,
            search_window_ms: 10_000,
            hard_max_ms: None,
        }
    }

    fn yields_to_dictation(&self) -> bool {
        false
    }

    fn transcribe(
        &self,
        samples: &[f32],
        offset_ms: u64,
        real_len_ms: u64,
    ) -> Result<ChunkTranscript, TranscribeError> {
        if self.token.is_cancelled() {
            return Err(TranscribeError::Cancelled);
        }
        let wav = encode_wav_16k_mono(samples);
        let number = self.requests.fetch_add(1, Ordering::Relaxed);
        let file_name = format!("chunk-{:04}.wav", number);
        let request = TranscriptionRequest {
            model: &self.model,
            wav: &wav,
            file_name: &file_name,
            language: self.language.as_deref(),
            timeout: self.request_timeout(real_len_ms),
        };
        let transcription = self
            .client
            .transcribe_blocking(&request, self.token.flag())
            .map_err(|error| self.chunk_error(error))?;
        self.first_done.store(true, Ordering::Release);
        let run = ExternalRun {
            text: transcription.text,
            segments: transcription.segments,
            applied_language_hint: self.language.clone(),
            detected_language: transcription.language,
        };
        Ok(self.shaper.shape(run, offset_ms, real_len_ms))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::Arc;
    use std::thread::{self, JoinHandle};
    use std::time::Instant;

    /// One request as the mock server received it.
    #[derive(Clone, Debug)]
    pub(crate) struct Captured {
        pub method: String,
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Captured {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }

        /// The text fields of a multipart body, in order.
        pub fn fields(&self) -> Vec<(String, String)> {
            let content_type = self.header("content-type").unwrap_or_default();
            let boundary = content_type
                .split("boundary=")
                .nth(1)
                .expect("multipart boundary");
            let body = String::from_utf8_lossy(&self.body);
            body.split(&format!("--{}", boundary))
                .filter_map(|part| {
                    let (head, value) = part.split_once("\r\n\r\n")?;
                    if head.contains("filename=") {
                        return None;
                    }
                    let name = head.split("name=\"").nth(1)?.split('"').next()?;
                    Some((name.to_string(), value.trim_end_matches("\r\n").to_string()))
                })
                .collect()
        }
    }

    /// What the mock server answers one connection with.
    pub(crate) enum Reply {
        Status(u16, &'static str, String),
        Delay(Duration, Box<Reply>),
        /// Closes the connection without an answer.
        Close,
    }

    pub(crate) fn json(status: u16, body: &str) -> Reply {
        Reply::Status(status, "application/json", body.to_string())
    }

    pub(crate) const MODELS: &str = r#"{"object":"list","data":[
        {"id":"Whisper-Tiny","recipe":"whispercpp","labels":["audio","transcription"],"downloaded":true},
        {"id":"Whisper-Base","recipe":"whispercpp","labels":["audio","transcription"],"downloaded":false},
        {"id":"Qwen3-4B-GGUF","recipe":"llamacpp","labels":["reasoning"],"downloaded":true}]}"#;

    /// Serves `replies` to successive connections on 127.0.0.1, recording
    /// each request; refuses connections once they are used up.
    pub(crate) struct MockServer {
        addr: SocketAddr,
        requests: Arc<Mutex<Vec<Captured>>>,
        _thread: JoinHandle<()>,
    }

    impl MockServer {
        pub fn start(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = Arc::clone(&requests);
            let thread = thread::spawn(move || {
                for reply in replies {
                    let Ok((mut stream, _)) = listener.accept() else {
                        return;
                    };
                    if let Some(request) = read_request(&mut stream) {
                        recorded.lock().unwrap().push(request);
                    }
                    respond(stream, reply);
                }
            });
            Self {
                addr,
                requests,
                _thread: thread,
            }
        }

        pub fn url(&self) -> String {
            format!("http://{}", self.addr)
        }

        pub fn requests(&self) -> Vec<Captured> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn read_request(stream: &mut TcpStream) -> Option<Captured> {
        let mut data = Vec::new();
        let mut buffer = [0u8; 8192];
        let head_end = loop {
            let read = stream.read(&mut buffer).ok()?;
            if read == 0 {
                return None;
            }
            data.extend_from_slice(&buffer[..read]);
            if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                break end;
            }
        };
        let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let mut request_line = lines.next()?.split(' ');
        let method = request_line.next()?.to_string();
        let path = request_line.next()?.to_string();
        let headers: Vec<(String, String)> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
            .collect();
        let length = headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.parse::<usize>().ok())
            .unwrap_or(0);
        let mut body = data[head_end + 4..].to_vec();
        while body.len() < length {
            let read = stream.read(&mut buffer).ok()?;
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buffer[..read]);
        }
        Some(Captured {
            method,
            path,
            headers,
            body,
        })
    }

    fn respond(mut stream: TcpStream, reply: Reply) {
        match reply {
            Reply::Delay(delay, reply) => {
                thread::sleep(delay);
                respond(stream, *reply);
            }
            Reply::Close => drop(stream),
            Reply::Status(status, content_type, body) => {
                let response = format!(
                    "HTTP/1.1 {} Mock\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    content_type,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        }
    }

    pub(crate) fn fast_timeouts() -> LemonadeTimeouts {
        LemonadeTimeouts {
            connect: Duration::from_secs(2),
            list: Duration::from_secs(5),
            first_request: Duration::from_secs(20),
            first_request_loaded: Duration::from_secs(15),
            request_base: Duration::from_secs(10),
            request_per_audio_sec: Duration::from_millis(100),
            unreachable_retry_delays: [Duration::from_millis(20), Duration::from_millis(20)],
        }
    }

    fn client(server: &MockServer, key: Option<&str>) -> LemonadeClient {
        LemonadeClient::new(&server.url(), key, fast_timeouts()).unwrap()
    }

    fn request<'a>(wav: &'a [u8], model: &'a str) -> TranscriptionRequest<'a> {
        TranscriptionRequest {
            model,
            wav,
            file_name: "chunk-0000.wav",
            language: None,
            timeout: Duration::from_secs(10),
        }
    }

    pub(crate) fn config(server: &MockServer, model: &str) -> LemonadeJobConfig {
        LemonadeJobConfig {
            base_url: server.url(),
            model: model.to_string(),
            api_key: None,
        }
    }

    pub(crate) fn begin(
        server: &MockServer,
        model: &str,
        token: WorkToken,
    ) -> Result<LemonadeTranscriber, TranscribeError> {
        begin_with_settings(server, model, AppSettings::default(), token)
    }

    pub(crate) fn begin_with_settings(
        server: &MockServer,
        model: &str,
        settings: AppSettings,
        token: WorkToken,
    ) -> Result<LemonadeTranscriber, TranscribeError> {
        LemonadeTranscriber::begin_with(&config(server, model), settings, token, fast_timeouts())
    }

    const VERBOSE: &str = r#"{"text":" Hello there. General Kenobi.","language":"english","duration":4.0,
        "segments":[{"id":0,"start":0.0,"end":1.52,"text":" Hello there."},
                    {"id":1,"start":1.52,"end":3.9,"text":" General Kenobi."}]}"#;

    #[test]
    fn normalize_base_url_variants() {
        let ok = |input: &str| normalize_base_url(input).unwrap();
        assert_eq!(
            ok("http://localhost:13305"),
            ("http://localhost:13305".to_string(), None)
        );
        assert_eq!(
            ok(" http://localhost:13305/ "),
            ("http://localhost:13305".to_string(), None)
        );
        assert_eq!(
            ok("http://localhost:8000/api/v1/"),
            ("http://localhost:8000".to_string(), Some("/api/v1"))
        );
        assert_eq!(
            ok("https://box.lan/lemonade/v1"),
            ("https://box.lan/lemonade".to_string(), Some("/v1"))
        );
        assert_eq!(
            ok("HTTP://127.0.0.1:13305/API/V1"),
            ("HTTP://127.0.0.1:13305".to_string(), Some("/api/v1"))
        );
        assert_eq!(
            ok("http://[::1]:13305"),
            ("http://[::1]:13305".to_string(), None)
        );
        for bad in [
            "",
            "localhost:13305",
            "ftp://localhost/",
            "http://",
            "http:///api/v1",
            "file:///tmp/x",
            "http://host/?q=1",
            "not a url",
        ] {
            assert!(
                matches!(normalize_base_url(bad), Err(LemonadeError::InvalidUrl(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn loopback_hosts() {
        for url in [
            "http://localhost:13305",
            "http://LOCALHOST",
            "http://127.0.0.1:8000",
            "http://127.4.5.6",
            "http://[::1]:13305",
            "http://api.localhost",
        ] {
            assert!(is_loopback_url(url), "{url}");
        }
        for url in [
            "http://192.168.1.20:13305",
            "https://example.com",
            "http://[::2]",
        ] {
            assert!(!is_loopback_url(url), "{url}");
        }
    }

    #[test]
    fn wav_header_and_round_trip() {
        let samples = [0.0, 0.5, -0.5, 1.5, -2.0, f32::NAN, 1.0 / 32767.0];
        let wav = encode_wav_16k_mono(&samples);
        assert_eq!(wav.len(), 44 + samples.len() * 2);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");

        let mut reader = hound::WavReader::new(std::io::Cursor::new(wav)).unwrap();
        let spec = reader.spec();
        assert_eq!(spec.channels, 1);
        assert_eq!(spec.sample_rate, 16_000);
        assert_eq!(spec.bits_per_sample, 16);
        assert_eq!(spec.sample_format, hound::SampleFormat::Int);
        let read: Vec<i16> = reader.samples::<i16>().map(Result::unwrap).collect();
        assert_eq!(read, [0, 16384, -16384, 32767, -32767, 0, 1]);

        // A full 28 s chunk.
        assert_eq!(encode_wav_16k_mono(&vec![0.0; 28 * 16_000]).len(), 896_044);
    }

    #[test]
    fn multipart_golden_bytes() {
        let body = build_multipart(
            "handy-b",
            &[
                ("model", "Whisper-Base"),
                ("response_format", "verbose_json"),
            ],
            "file",
            "chunk-0003.wav",
            "audio/wav",
            b"RIFF",
        );
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "--handy-b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nWhisper-Base\r\n\
             --handy-b\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\nverbose_json\r\n\
             --handy-b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"chunk-0003.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\nRIFF\r\n\
             --handy-b--\r\n"
        );
    }

    #[test]
    fn boundary_never_in_payload() {
        let payload = encode_wav_16k_mono(&[0.25; 16_000]);
        let first = new_boundary(&payload);
        let second = new_boundary(&payload);
        assert_ne!(first, second);
        for boundary in [&first, &second] {
            assert!(boundary.starts_with("handy-"));
            assert_eq!(boundary.len(), 6 + 32);
            assert!(boundary[6..].bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(!contains_subslice(&payload, boundary.as_bytes()));
        }
        // A payload made of a boundary still gets one it does not contain.
        let payload = first.repeat(3).into_bytes();
        assert!(!contains_subslice(
            &payload,
            new_boundary(&payload).as_bytes()
        ));
        assert!(contains_subslice(b"abcdef", b"cde"));
        assert!(!contains_subslice(b"abc", b""));
    }

    #[test]
    fn parse_verbose_json_segments_seconds_to_ms() {
        let parsed = parse_transcription(VERBOSE.as_bytes(), Some("application/json")).unwrap();
        assert_eq!(parsed.text, "Hello there. General Kenobi.");
        assert_eq!(parsed.language.as_deref(), Some("en"));
        assert_eq!(
            parsed.segments,
            vec![
                ExternalTimed {
                    start_ms: 0,
                    end_ms: 1_520,
                    text: " Hello there.".to_string()
                },
                ExternalTimed {
                    start_ms: 1_520,
                    end_ms: 3_900,
                    text: " General Kenobi.".to_string()
                },
            ]
        );
    }

    #[test]
    fn parse_compact_json_without_segments() {
        let parsed = parse_transcription(br#"{"text":" Hi. "}"#, None).unwrap();
        assert_eq!(
            parsed,
            LemonadeTranscription {
                text: "Hi.".to_string(),
                language: None,
                segments: Vec::new(),
            }
        );
    }

    #[test]
    fn parse_plain_text() {
        let parsed = parse_transcription(b"  Just text.\n", Some("text/plain")).unwrap();
        assert_eq!(parsed.text, "Just text.");
        assert!(parsed.segments.is_empty());
        assert_eq!(parse_transcription(b"", None).unwrap().text, "");
    }

    #[test]
    fn parse_rejects_json_without_text() {
        for body in [
            &br#"{"error":{"message":"busy"}}"#[..],
            br#"{"text": 42}"#,
            br#"{"text": "unterminated"#,
        ] {
            assert!(
                matches!(
                    parse_transcription(body, Some("application/json")),
                    Err(LemonadeError::BadResponse(_))
                ),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
        assert!(matches!(
            parse_transcription(b"[1, 2]", Some("application/json")),
            Err(LemonadeError::BadResponse(_))
        ));
    }

    #[test]
    fn drops_inverted_and_nan_segments() {
        let body = br#"{"text":"a b c d e","segments":[
            {"start":1.0,"end":0.5,"text":"a"},
            {"start":-1.0,"end":0.5,"text":"b"},
            {"start":"x","end":1.0,"text":"c"},
            {"start":1.0,"end":2.0},
            {"start":2.0,"end":2.0,"text":"e"}]}"#;
        let parsed = parse_transcription(body, None).unwrap();
        assert_eq!(
            parsed.segments,
            vec![ExternalTimed {
                start_ms: 2_000,
                end_ms: 2_000,
                text: "e".to_string()
            }]
        );
    }

    #[test]
    fn normalize_language_names_and_codes() {
        for (value, expected) in [
            ("en", Some("en")),
            ("EN", Some("en")),
            ("en-US", Some("en")),
            ("eng", Some("en")),
            ("english", Some("en")),
            ("English", Some("en")),
            ("german", Some("de")),
            ("zh", Some("zh")),
            ("chinese", Some("zh")),
            ("", None),
            ("auto", None),
            ("klingonish", None),
            ("xx", None),
        ] {
            assert_eq!(normalize_language(value).as_deref(), expected, "{value:?}");
        }
    }

    #[test]
    fn request_language_rules() {
        for (selected, expected) in [
            ("auto", None),
            ("", None),
            ("en", Some("en")),
            ("zh-Hans", Some("zh")),
            ("zh-Hant", Some("zh")),
            ("pt-BR", Some("pt")),
            ("nb", Some("no")),
            ("yue", None),
            ("haw", None),
        ] {
            assert_eq!(
                request_language(selected).as_deref(),
                expected,
                "{selected:?}"
            );
        }
    }

    #[test]
    fn classify_status_table() {
        let model = "Whisper-Base";
        assert_eq!(
            classify_status(401, "nope", model),
            LemonadeError::Unauthorized { status: 401 }
        );
        assert_eq!(
            classify_status(403, "", model),
            LemonadeError::Unauthorized { status: 403 }
        );
        for (status, body) in [
            (404, r#"{"error":{"message":"model Nope not found"}}"#),
            (422, "Unknown model: Nope"),
            (400, "The model Nope does not exist"),
            (500, "Model 'Nope' is not available"),
        ] {
            assert!(
                matches!(
                    classify_status(status, body, "Nope"),
                    LemonadeError::ModelNotFound { ref model, .. } if model == "Nope"
                ),
                "{status} {body}"
            );
        }
        assert_eq!(
            classify_status(404, "<html><body>Not Found</body></html>", model),
            LemonadeError::Http {
                status: 404,
                body: "<html><body>Not Found</body></html>".to_string()
            }
        );
        assert!(matches!(
            classify_status(429, "slow down", model),
            LemonadeError::Http { status: 429, .. }
        ));
        assert!(matches!(
            classify_status(500, "worker crashed", model),
            LemonadeError::Http { status: 500, .. }
        ));
        // A rejected format is not a missing model, even if it says "not supported".
        assert!(matches!(
            classify_status(
                400,
                "response_format verbose_json is not supported by this model",
                model
            ),
            LemonadeError::Http { status: 400, .. }
        ));
        // Bodies are cut to an excerpt.
        let LemonadeError::Http { body, .. } = classify_status(502, &"x".repeat(1_000), model)
        else {
            panic!("expected Http");
        };
        assert_eq!(body.chars().count(), BODY_EXCERPT_CHARS + 1);
    }

    #[test]
    fn models_filter_transcription() {
        let models = parse_models(MODELS.as_bytes()).unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[1].downloaded, Some(false));
        assert_eq!(
            models
                .iter()
                .map(LemonadeModel::is_transcription)
                .collect::<Vec<_>>(),
            [true, true, false]
        );
        let model = |id: &str, recipe: Option<&str>, labels: &[&str]| LemonadeModel {
            id: id.to_string(),
            recipe: recipe.map(str::to_string),
            labels: labels.iter().map(|l| l.to_string()).collect(),
            downloaded: None,
        };
        assert!(model("whisper-large-v3", None, &[]).is_transcription());
        assert!(model("X", Some("WhisperCpp"), &[]).is_transcription());
        assert!(model("X", None, &["Transcription"]).is_transcription());
        assert!(!model("Whis", Some("llamacpp"), &["vision"]).is_transcription());

        // A bare array works too; rows without an id are skipped.
        let models = parse_models(br#"[{"id":"Whisper-Tiny"},{"name":"x"},{"id":""}]"#).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].labels, Vec::<String>::new());
        assert!(matches!(
            parse_models(b"{}"),
            Err(LemonadeError::BadResponse(_))
        ));

        let check = LemonadeCheck::from_result(
            Some("http://x/api/v1".to_string()),
            parse_models(MODELS.as_bytes()),
        );
        assert!(check.ok);
        assert_eq!(check.models.len(), 2);
        assert_eq!(check.other_models, 1);
        let check =
            LemonadeCheck::from_result(None, Err(LemonadeError::Unauthorized { status: 401 }));
        assert!(!check.ok);
        assert_eq!(check.error, Some(LemonadeErrorKind::Unauthorized));
        assert!(check.detail.unwrap().contains("API key"));
    }

    #[test]
    fn retryable_kinds() {
        assert!(LemonadeError::Unreachable("x".into()).is_retryable());
        assert!(LemonadeError::Timeout(Duration::from_secs(1)).is_retryable());
        for status in [408, 429, 500, 503] {
            assert!(
                LemonadeError::Http {
                    status,
                    body: String::new()
                }
                .is_retryable(),
                "{status}"
            );
        }
        for error in [
            LemonadeError::Http {
                status: 400,
                body: String::new(),
            },
            LemonadeError::Unauthorized { status: 401 },
            LemonadeError::ModelNotFound {
                model: "m".into(),
                detail: String::new(),
            },
            LemonadeError::BadResponse("x".into()),
            LemonadeError::InvalidUrl("x".into()),
            LemonadeError::Cancelled,
        ] {
            assert!(!error.is_retryable(), "{error:?}");
        }
    }

    #[test]
    fn display_names_lemonade_models_only() {
        assert_eq!(
            display_model_name("lemonade:Whisper-Base").as_deref(),
            Some("Whisper-Base (Lemonade)")
        );
        assert_eq!(display_model_name("small"), None);
    }

    #[test]
    fn transcribe_sends_model_file_and_format_without_auth_by_default() {
        let server = MockServer::start(vec![json(200, VERBOSE)]);
        let wav = encode_wav_16k_mono(&[0.1; 1_600]);

        let parsed = client(&server, None)
            .transcribe_blocking(&request(&wav, "Whisper-Base"), &AtomicBool::new(false))
            .unwrap();

        assert_eq!(parsed.text, "Hello there. General Kenobi.");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let sent = &requests[0];
        assert_eq!(sent.method, "POST");
        assert_eq!(sent.path, "/api/v1/audio/transcriptions");
        assert!(sent.header("authorization").is_none());
        assert_eq!(
            sent.fields(),
            vec![
                ("model".to_string(), "Whisper-Base".to_string()),
                ("response_format".to_string(), "verbose_json".to_string()),
            ]
        );
        assert!(contains_subslice(&sent.body, &wav));
        assert!(contains_subslice(
            &sent.body,
            b"filename=\"chunk-0000.wav\"\r\nContent-Type: audio/wav"
        ));
    }

    #[test]
    fn bearer_header_only_with_key() {
        let server = MockServer::start(vec![json(200, MODELS), json(200, r#"{"text":"ok"}"#)]);
        let lemonade = client(&server, Some(" k1 "));
        lemonade
            .list_models_blocking(&AtomicBool::new(false))
            .unwrap();
        let wav = encode_wav_16k_mono(&[0.0; 16]);
        let mut req = request(&wav, "Whisper-Tiny");
        req.language = Some("de");
        lemonade
            .transcribe_blocking(&req, &AtomicBool::new(false))
            .unwrap();

        let requests = server.requests();
        assert_eq!(requests[0].path, "/api/v1/models?show_all=true");
        for sent in &requests {
            assert_eq!(sent.header("authorization"), Some("Bearer k1"));
        }
        assert_eq!(
            requests[1].fields().last(),
            Some(&("language".to_string(), "de".to_string()))
        );
        // A blank key sends no header.
        assert!(
            LemonadeClient::new("http://localhost", Some("  "), fast_timeouts())
                .unwrap()
                .api_key
                .is_none()
        );
    }

    #[test]
    fn falls_back_to_v1_prefix_on_404() {
        let server = MockServer::start(vec![
            json(404, r#"{"detail":"Not Found"}"#),
            json(200, MODELS),
            json(200, r#"{"text":"ok"}"#),
        ]);
        let lemonade = client(&server, None);
        assert_eq!(lemonade.api_url(), format!("{}/api/v1", server.url()));

        let models = lemonade
            .list_models_blocking(&AtomicBool::new(false))
            .unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(lemonade.api_url(), format!("{}/v1", server.url()));
        let wav = encode_wav_16k_mono(&[0.0; 16]);
        lemonade
            .transcribe_blocking(&request(&wav, "Whisper-Tiny"), &AtomicBool::new(false))
            .unwrap();

        let paths: Vec<String> = server.requests().into_iter().map(|r| r.path).collect();
        assert_eq!(
            paths,
            [
                "/api/v1/models?show_all=true",
                "/v1/models?show_all=true",
                "/v1/audio/transcriptions"
            ]
        );
    }

    #[test]
    fn explicit_prefix_is_used_as_given() {
        let server = MockServer::start(vec![json(404, "not found")]);
        let lemonade =
            LemonadeClient::new(&format!("{}/v1/", server.url()), None, fast_timeouts()).unwrap();
        let error = lemonade
            .list_models_blocking(&AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(error, LemonadeError::Http { status: 404, .. }));
        assert_eq!(server.requests().len(), 1);
        assert_eq!(server.requests()[0].path, "/v1/models?show_all=true");
    }

    #[test]
    fn unauthorized_maps_to_stop() {
        let server = MockServer::start(vec![json(401, r#"{"error":"invalid api key"}"#)]);
        match begin(&server, "Whisper-Tiny", WorkToken::new()) {
            Err(TranscribeError::Stop { code, message }) => {
                assert_eq!(code, FileJobErrorCode::ServerUnauthorized);
                assert!(message.contains("API key"), "{message}");
            }
            other => panic!("expected Stop, got {:?}", other.err()),
        }

        // A key rejected mid-job stops it too.
        let server = MockServer::start(vec![json(200, MODELS), json(403, "forbidden")]);
        let transcriber = begin(&server, "Whisper-Tiny", WorkToken::new()).unwrap();
        assert!(matches!(
            transcriber.transcribe(&[0.0; 16_000], 0, 1_000),
            Err(TranscribeError::Stop {
                code: FileJobErrorCode::ServerUnauthorized,
                ..
            })
        ));
    }

    #[test]
    fn unknown_model_maps_to_model_missing() {
        let server = MockServer::start(vec![
            json(200, MODELS),
            json(404, r#"{"error":{"message":"model Nope not found"}}"#),
        ]);
        // Not listed is not fatal: Lemonade may download it on first use.
        let transcriber = begin(&server, "Nope", WorkToken::new()).unwrap();
        assert!(!transcriber.model_downloaded);
        match transcriber.transcribe(&[0.0; 16_000], 0, 1_000) {
            Err(TranscribeError::Stop { code, message }) => {
                assert_eq!(code, FileJobErrorCode::ServerModelMissing);
                assert!(message.contains("Nope"), "{message}");
            }
            other => panic!("expected Stop, got {:?}", other.err()),
        }
    }

    #[test]
    fn missing_transcription_endpoint_stops_the_job() {
        let server = MockServer::start(vec![
            json(200, MODELS),
            Reply::Status(404, "text/html", "<h1>Not Found</h1>".to_string()),
        ]);
        let transcriber = begin(&server, "Whisper-Tiny", WorkToken::new()).unwrap();
        match transcriber.transcribe(&[0.0; 16_000], 0, 1_000) {
            Err(TranscribeError::Stop { code, message }) => {
                assert_eq!(code, FileJobErrorCode::EngineFailed);
                assert!(message.contains("doesn't offer transcription"), "{message}");
            }
            other => panic!("expected Stop, got {:?}", other.err()),
        }
    }

    #[test]
    fn server_error_is_failed() {
        let server = MockServer::start(vec![
            json(200, MODELS),
            json(500, r#"{"error":{"message":"simulated failure"}}"#),
            Reply::Status(200, "application/json", "{\"oops\":1}".to_string()),
        ]);
        let transcriber = begin(&server, "Whisper-Tiny", WorkToken::new()).unwrap();
        for _ in 0..2 {
            assert!(matches!(
                transcriber.transcribe(&[0.0; 16_000], 0, 1_000),
                Err(TranscribeError::Failed(_))
            ));
        }
    }

    #[test]
    fn refused_connection_is_unreachable_after_three_attempts() {
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let lemonade =
            LemonadeClient::new(&format!("http://127.0.0.1:{}", port), None, fast_timeouts())
                .unwrap();
        let started = Instant::now();
        let error = lemonade
            .list_models_blocking(&AtomicBool::new(false))
            .unwrap_err();
        assert!(matches!(error, LemonadeError::Unreachable(_)), "{error:?}");
        // Two waits between the three attempts.
        assert!(started.elapsed() >= Duration::from_millis(40));
        assert!(error.to_string().contains("127.0.0.1"), "{error}");

        let error = LemonadeTranscriber::begin_with(
            &LemonadeJobConfig {
                base_url: format!("http://127.0.0.1:{}", port),
                model: "Whisper-Tiny".to_string(),
                api_key: None,
            },
            AppSettings::default(),
            WorkToken::new(),
            fast_timeouts(),
        )
        .err()
        .expect("unreachable");
        assert!(matches!(
            error,
            TranscribeError::Stop {
                code: FileJobErrorCode::ServerUnreachable,
                ..
            }
        ));
    }

    #[test]
    fn closed_connection_is_retried() {
        let server = MockServer::start(vec![Reply::Close, json(200, r#"{"text":"ok"}"#)]);
        let wav = encode_wav_16k_mono(&[0.0; 16]);
        let parsed = client(&server, None)
            .transcribe_blocking(&request(&wav, "Whisper-Tiny"), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(parsed.text, "ok");
        assert_eq!(server.requests().len(), 2);
    }

    #[test]
    fn slow_server_times_out() {
        let server = MockServer::start(vec![Reply::Delay(
            Duration::from_secs(2),
            Box::new(json(200, VERBOSE)),
        )]);
        let wav = encode_wav_16k_mono(&[0.0; 16]);
        let mut req = request(&wav, "Whisper-Tiny");
        req.timeout = Duration::from_millis(200);
        let started = Instant::now();
        let error = client(&server, None)
            .transcribe_blocking(&req, &AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(error, LemonadeError::Timeout(Duration::from_millis(200)));
        assert!(started.elapsed() < Duration::from_millis(1_500));
    }

    #[test]
    fn cancel_aborts_in_flight_request_quickly() {
        let server = MockServer::start(vec![
            json(200, MODELS),
            Reply::Delay(Duration::from_secs(5), Box::new(json(200, VERBOSE))),
        ]);
        let token = WorkToken::new();
        let transcriber = begin(&server, "Whisper-Tiny", token.clone()).unwrap();
        let canceller = {
            let token = token.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                token.cancel();
            })
        };
        let started = Instant::now();
        let result = transcriber.transcribe(&[0.0; 16_000], 0, 1_000);
        canceller.join().unwrap();
        assert!(matches!(result, Err(TranscribeError::Cancelled)));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        // A cancelled job sends nothing more.
        assert!(matches!(
            transcriber.transcribe(&[0.0; 16_000], 0, 1_000),
            Err(TranscribeError::Cancelled)
        ));
    }

    #[test]
    fn verbose_json_rejected_falls_back_to_json() {
        let server = MockServer::start(vec![
            json(
                400,
                r#"{"error":{"message":"response_format 'verbose_json' is not supported"}}"#,
            ),
            json(200, r#"{"text":"first"}"#),
            json(200, r#"{"text":"second"}"#),
        ]);
        let lemonade = client(&server, None);
        let wav = encode_wav_16k_mono(&[0.0; 16]);
        for expected in ["first", "second"] {
            let parsed = lemonade
                .transcribe_blocking(&request(&wav, "Whisper-Tiny"), &AtomicBool::new(false))
                .unwrap();
            assert_eq!(parsed.text, expected);
        }
        let formats: Vec<String> = server
            .requests()
            .iter()
            .map(|r| {
                r.fields()
                    .into_iter()
                    .find(|(name, _)| name == "response_format")
                    .map(|(_, value)| value)
                    .unwrap()
            })
            .collect();
        assert_eq!(formats, ["verbose_json", "json", "json"]);
    }

    #[test]
    fn transcriber_shapes_chunks_and_times_requests() {
        let server = MockServer::start(vec![json(200, MODELS), json(200, VERBOSE)]);
        let transcriber = begin(&server, "Whisper-Base", WorkToken::new()).unwrap();
        assert_eq!(transcriber.model_id(), "lemonade:Whisper-Base");
        assert!(!transcriber.yields_to_dictation());
        assert_eq!(transcriber.limits().max_chunk_ms, 28_000);
        // Not downloaded yet: the first request may wait for the download.
        assert_eq!(
            transcriber.request_timeout(28_000),
            fast_timeouts().first_request
        );

        let chunk = transcriber
            .transcribe(&[0.1; 64_000], 56_000, 4_000)
            .unwrap();

        assert_eq!(chunk.text, "Hello there. General Kenobi.");
        assert_eq!(chunk.language.as_deref(), Some("en"));
        assert_eq!(
            chunk
                .segments
                .iter()
                .map(|s| (s.start_ms, s.end_ms, s.text.as_str()))
                .collect::<Vec<_>>(),
            [
                (56_000, 57_520, "Hello there."),
                (57_520, 59_900, "General Kenobi.")
            ]
        );
        // Later requests get time in proportion to their audio.
        assert_eq!(
            transcriber.request_timeout(28_000),
            Duration::from_secs(10) + Duration::from_millis(100) * 28
        );
        let sent = &server.requests()[1];
        assert!(contains_subslice(
            &sent.body,
            b"filename=\"chunk-0000.wav\""
        ));
    }
}
