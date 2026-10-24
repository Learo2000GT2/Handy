use crate::file_transcription::lemonade;
use crate::settings::{PostProcessProvider, LEMONADE_PROVIDER_ID};
use log::{debug, error, info};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE, REFERER, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::error::Error as StdError;
use std::fmt;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Longest wait for a provider's model list.
const MODEL_LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a request to a post-processing provider failed. Display gives the
/// English detail, as logged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmError {
    /// The provider's address can't be used.
    InvalidAddress(String),
    /// The API key can't be sent: it holds characters a header can't.
    InvalidApiKey(String),
    /// The request couldn't be prepared.
    Setup(String),
    /// The server can't be reached (refused, unknown host, broke off).
    Unreachable(String),
    /// The server didn't answer in time.
    Timeout(String),
    /// The server answered with an error status; `body` is an excerpt.
    Status { status: u16, body: String },
    /// A success answer that couldn't be read.
    BadResponse(String),
}

impl fmt::Display for LlmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAddress(detail)
            | Self::InvalidApiKey(detail)
            | Self::Setup(detail)
            | Self::Unreachable(detail)
            | Self::Timeout(detail)
            | Self::BadResponse(detail) => f.write_str(detail),
            Self::Status { status, body } => {
                let status = reqwest::StatusCode::from_u16(*status)
                    .map(|code| code.to_string())
                    .unwrap_or_else(|_| status.to_string());
                write!(f, "API request failed with status {}: {}", status, body)
            }
        }
    }
}

impl std::error::Error for LlmError {}

/// The models a provider lists.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelList {
    /// The models that can post-process: all of them, except that a Lemonade
    /// Server's speech, embedding, image and similar models are left out.
    pub chat: Vec<String>,
    /// The listed models left out of `chat`.
    pub other: Vec<String>,
}

/// The API base requests go to, without a trailing "/". A Lemonade address
/// may be typed with or without its API prefix ("http://localhost:13305"
/// means ".../api/v1", which every Lemonade release serves). As in
/// [`lemonade::LemonadeClient`], a password in a Lemonade address is dropped
/// when an API key is sent instead.
fn api_base(provider: &PostProcessProvider, api_key: &str) -> Result<String, LlmError> {
    if provider.id != LEMONADE_PROVIDER_ID {
        return Ok(provider.base_url.trim().trim_end_matches('/').to_string());
    }
    let (root, prefix) = lemonade::normalize_base_url(&provider.base_url)
        .map_err(|e| LlmError::InvalidAddress(e.to_string()))?;
    let root = if api_key.trim().is_empty() {
        root
    } else {
        lemonade::redact_url(&root)
    };
    Ok(format!(
        "{}{}",
        root,
        prefix.unwrap_or(lemonade::API_PREFIX)
    ))
}

/// The provider's API address as shown to the user and logged: without any
/// password in it.
pub fn display_address(provider: &PostProcessProvider) -> String {
    let address = api_base(provider, "")
        .unwrap_or_else(|_| provider.base_url.trim().trim_end_matches('/').to_string());
    lemonade::redact_url(&address)
}

#[derive(Debug, Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Debug, Serialize)]
struct JsonSchema {
    name: String,
    strict: bool,
    schema: Value,
}

#[derive(Debug, Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: String,
    json_schema: JsonSchema,
}

#[derive(Debug, Serialize, Clone, Default, PartialEq)]
struct ReasoningConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exclude: Option<bool>,
}

/// Request fields used to ask an endpoint to skip reasoning/thinking.
/// Providers disagree on the field name and accepted values, so at most one of
/// these is set per request (see `reasoning_disable_params`).
#[derive(Debug, Serialize, Clone, Default, PartialEq)]
struct ReasoningParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Value>,
}

impl ReasoningParams {
    fn is_empty(&self) -> bool {
        self.reasoning_effort.is_none() && self.reasoning.is_none() && self.thinking.is_none()
    }
}

/// Pick the reasoning-disable request fields an endpoint understands.
/// Unknown endpoints get the common OpenAI-style field; if they reject it,
/// the request is retried without it (see `send_chat_completion_with_schema`).
fn reasoning_disable_params(provider: &PostProcessProvider) -> ReasoningParams {
    let base_url = provider.base_url.to_lowercase();
    if base_url.contains("api.deepseek.com") {
        // DeepSeek rejects reasoning_effort "none" and uses its own field:
        // https://api-docs.deepseek.com/guides/thinking_mode
        ReasoningParams {
            thinking: Some(serde_json::json!({ "type": "disabled" })),
            ..Default::default()
        }
    } else if provider.id == "openrouter" {
        // OpenRouter nested object; exclude:true also keeps reasoning text out
        // of the response so it can't pollute structured-output JSON parsing
        ReasoningParams {
            reasoning: Some(ReasoningConfig {
                effort: Some("none".to_string()),
                exclude: Some(true),
            }),
            ..Default::default()
        }
    } else {
        ReasoningParams {
            reasoning_effort: Some("none".to_string()),
            ..Default::default()
        }
    }
}

/// Endpoints (base_url|model) that rejected the reasoning-disable fields with a
/// 4xx. Remembered for the lifetime of the process so every dictation after the
/// first skips the doomed attempt and goes straight to a plain request.
fn reasoning_rejections() -> &'static Mutex<HashSet<String>> {
    static REJECTED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    REJECTED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn endpoint_key(provider: &PostProcessProvider, model: &str) -> String {
    format!("{}|{}", provider.base_url.trim_end_matches('/'), model)
}

fn is_known_rejected(key: &str) -> bool {
    reasoning_rejections()
        .lock()
        .map(|set| set.contains(key))
        .unwrap_or(false)
}

fn remember_rejection(key: String) {
    if let Ok(mut set) = reasoning_rejections().lock() {
        set.insert(key);
    }
}

#[derive(Debug, Serialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Vec<ChatMessage>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(flatten)]
    reasoning: ReasoningParams,
}

#[derive(Debug, Deserialize)]
struct ChatCompletionResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatMessageResponse,
}

#[derive(Debug, Deserialize)]
struct ChatMessageResponse {
    content: Option<String>,
}

/// Build headers for API requests based on provider type
fn build_headers(provider: &PostProcessProvider, api_key: &str) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();

    // Common headers
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        REFERER,
        HeaderValue::from_static("https://github.com/cjpais/Handy"),
    );
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("Handy/1.0 (+https://github.com/cjpais/Handy)"),
    );
    headers.insert("X-Title", HeaderValue::from_static("Handy"));

    // Provider-specific auth headers
    if !api_key.is_empty() {
        if provider.id == "anthropic" {
            headers.insert(
                "x-api-key",
                HeaderValue::from_str(api_key)
                    .map_err(|e| format!("Invalid API key header value: {}", e))?,
            );
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        } else {
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {}", api_key))
                    .map_err(|e| format!("Invalid authorization header value: {}", e))?,
            );
        }
    }

    Ok(headers)
}

/// Create an HTTP client with provider-specific headers. A server on this
/// computer (`base` is loopback) is never reached through a system proxy.
fn create_client(
    provider: &PostProcessProvider,
    api_key: &str,
    base: &str,
) -> Result<reqwest::Client, LlmError> {
    let headers = build_headers(provider, api_key).map_err(LlmError::InvalidApiKey)?;
    let mut builder = reqwest::Client::builder().default_headers(headers);
    if lemonade::is_loopback_url(base) {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| LlmError::Setup(report_reqwest_error("Failed to build HTTP client", &e)))
}

/// Format a bounded error source chain.
///
/// `reqwest::Error`'s Display implementation intentionally gives only a short
/// summary. Nested causes contain the useful transport details, such as a
/// certificate validation failure, an HTTP/2 error, or a connection reset.
/// Callers must skip source types whose Display text can quote payload data.
fn error_source_chain(error: &(dyn StdError + 'static)) -> Vec<String> {
    let mut causes = Vec::new();
    let mut source = error.source();

    // Defensive cap in case a third-party error exposes a cyclic source chain.
    for _ in 0..16 {
        let Some(cause) = source else {
            break;
        };
        causes.push(cause.to_string());
        source = cause.source();
    }

    causes
}

fn reqwest_error_kinds(error: &reqwest::Error) -> String {
    let mut kinds = Vec::new();

    if error.is_builder() {
        kinds.push("builder");
    }
    if error.is_connect() {
        kinds.push("connect");
    }
    if error.is_request() {
        kinds.push("request");
    }
    if error.is_redirect() {
        kinds.push("redirect");
    }
    if error.is_timeout() {
        kinds.push("timeout");
    }
    if error.is_status() {
        kinds.push("status");
    }
    if error.is_body() {
        kinds.push("body");
    }
    if error.is_decode() {
        kinds.push("decode");
    }
    if error.is_upgrade() {
        kinds.push("upgrade");
    }

    if kinds.is_empty() {
        "unknown".to_string()
    } else {
        kinds.join(", ")
    }
}

fn sanitized_url(url: &reqwest::Url) -> String {
    let mut url = url.clone();

    // Custom endpoints should not contain credentials or query-string tokens,
    // but omit them from diagnostics in case one does.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);

    url.to_string()
}

fn sanitized_url_for_log(url: &str) -> String {
    reqwest::Url::parse(url)
        .map(|url| sanitized_url(&url))
        // Do not echo an invalid URL: the parse failure might have been caused
        // by sensitive data entered in the custom endpoint field.
        .unwrap_or_else(|_| "<invalid URL>".to_string())
}

/// A failed request as an [`LlmError`] (and logged, see
/// [`report_reqwest_error`]).
fn request_error(context: &str, error: &reqwest::Error) -> LlmError {
    let details = report_reqwest_error(context, error);
    if error.is_builder() {
        LlmError::InvalidAddress(details)
    } else if error.is_timeout() {
        LlmError::Timeout(details)
    } else if error.is_decode() || error.is_body() {
        LlmError::BadResponse(details)
    } else {
        LlmError::Unreachable(details)
    }
}

/// The error for a non-2xx answer, with an excerpt of its body.
async fn status_error(response: reqwest::Response, context: &str) -> LlmError {
    let status = response.status().as_u16();
    let body = match response.bytes().await {
        Ok(body) => lemonade::body_excerpt(&body),
        Err(e) => report_reqwest_error(context, &e),
    };
    LlmError::Status { status, body }
}

fn report_reqwest_error(context: &str, error: &reqwest::Error) -> String {
    let kinds = reqwest_error_kinds(error);
    let url = error
        .url()
        .map(sanitized_url)
        .map(|url| format!(", url: {url}"))
        .unwrap_or_default();

    // serde_json's error text can quote values from a malformed response. That
    // response may contain transcription content, so retain the useful decode
    // classification but never put its nested source in logs or UI errors.
    let causes = if error.is_decode() {
        Vec::new()
    } else {
        error_source_chain(error)
    };
    let cause_details = if !causes.is_empty() {
        format!(": caused by: {}", causes.join(" -> "))
    } else if error.url().is_none() {
        // Reqwest's short Display text is safe when it cannot append a raw URL.
        format!(": {error}")
    } else {
        // The sanitized URL is already included above. Avoid formatting the
        // original error because its Display implementation includes the raw URL.
        String::new()
    };

    let details = format!("{context} (kind: {kinds}{url}){cause_details}");
    error!("{details}");
    details
}

/// Send a chat completion request to an OpenAI-compatible API, with
/// structured output support.
/// Returns Ok(Some(content)) on success, Ok(None) if response has no content,
/// or Err on actual errors (HTTP, parsing, etc.)
/// When json_schema is provided, uses structured outputs mode.
/// system_prompt is used as the system message when provided.
///
/// When disable_reasoning is set, the request carries the reasoning-disable
/// fields the endpoint is expected to understand. Not every OpenAI-compatible
/// endpoint accepts them (DeepSeek, Gemini's compat layer, and some OpenRouter
/// upstreams reject with 400), so a 400/422 answer to such a request triggers
/// one retry without the fields, and the rejection is remembered per
/// (base_url, model) so later requests skip the failing attempt entirely.
pub async fn send_chat_completion_with_schema(
    provider: &PostProcessProvider,
    api_key: String,
    model: &str,
    user_content: String,
    system_prompt: Option<String>,
    json_schema: Option<Value>,
    disable_reasoning: bool,
) -> Result<Option<String>, LlmError> {
    let base_url = api_base(provider, &api_key)?;
    let base_url = base_url.as_str();
    let url = format!("{}/chat/completions", base_url);

    debug!(
        "Sending chat completion request to: {}",
        sanitized_url_for_log(&url)
    );

    let client = create_client(provider, &api_key, base_url)?;

    // Build messages vector
    let mut messages = Vec::new();

    // Add system prompt if provided
    if let Some(system) = system_prompt {
        messages.push(ChatMessage {
            role: "system".to_string(),
            content: system,
        });
    }

    // Add user message
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: user_content,
    });

    // Build response_format if schema is provided
    let response_format = json_schema.map(|schema| ResponseFormat {
        format_type: "json_schema".to_string(),
        json_schema: JsonSchema {
            name: "transcription_output".to_string(),
            strict: true,
            schema,
        },
    });

    let key = endpoint_key(provider, model);
    let reasoning = if disable_reasoning && !is_known_rejected(&key) {
        reasoning_disable_params(provider)
    } else {
        ReasoningParams::default()
    };

    let mut request_body = ChatCompletionRequest {
        model: model.to_string(),
        messages,
        stream: false,
        response_format,
        reasoning,
    };

    let mut response = client
        .post(&url)
        .json(&request_body)
        .send()
        .await
        .map_err(|e| request_error("HTTP request failed", &e))?;
    let mut status = response.status();
    debug!(
        "Chat completion response received with status {} over {:?} from {}",
        status,
        response.version(),
        sanitized_url(response.url())
    );

    // A 400/422 on a request carrying reasoning-disable fields is almost always
    // the endpoint rejecting those fields — retry once without them.
    if !status.is_success()
        && matches!(status.as_u16(), 400 | 422)
        && !request_body.reasoning.is_empty()
    {
        let error_text = response.text().await.unwrap_or_else(|e| {
            report_reqwest_error("Failed to read reasoning rejection response", &e)
        });
        info!(
            "Endpoint rejected request with reasoning disabled (status {}): {}. Retrying without reasoning fields",
            status, error_text
        );

        request_body.reasoning = ReasoningParams::default();
        response = client
            .post(&url)
            .json(&request_body)
            .send()
            .await
            .map_err(|e| request_error("HTTP retry failed", &e))?;
        status = response.status();
        debug!(
            "Chat completion retry response received with status {} over {:?} from {}",
            status,
            response.version(),
            sanitized_url(response.url())
        );

        if status.is_success() {
            info!(
                "Retry without reasoning fields succeeded; '{}' (model '{}') will skip them from now on",
                sanitized_url_for_log(base_url), model
            );
            remember_rejection(key);
        }
    }

    if !status.is_success() {
        return Err(status_error(response, "Failed to read API error response").await);
    }

    let completion: ChatCompletionResponse = response
        .json()
        .await
        .map_err(|e| request_error("Failed to parse API response", &e))?;

    Ok(completion
        .choices
        .first()
        .and_then(|choice| choice.message.content.clone()))
}

/// Fetch available models from an OpenAI-compatible API
/// Returns the model IDs; for a Lemonade Server, only its chat models are in
/// `chat` (see [`lemonade::LemonadeModel::is_chat`]).
pub async fn fetch_models(
    provider: &PostProcessProvider,
    api_key: String,
) -> Result<ModelList, LlmError> {
    let base_url = api_base(provider, &api_key)?;
    let url = format!("{}/models", base_url);

    debug!("Fetching models from: {}", sanitized_url_for_log(&url));

    let client = create_client(provider, &api_key, &base_url)?;

    let response = client
        .get(&url)
        .timeout(MODEL_LIST_TIMEOUT)
        .send()
        .await
        .map_err(|e| request_error("Failed to fetch models", &e))?;

    let status = response.status();
    debug!(
        "Model list response received with status {} over {:?} from {}",
        status,
        response.version(),
        sanitized_url(response.url())
    );
    if !status.is_success() {
        return Err(status_error(response, "Failed to read model list error").await);
    }

    let parsed: serde_json::Value = response
        .json()
        .await
        .map_err(|e| request_error("Failed to parse model list response", &e))?;

    if provider.id == LEMONADE_PROVIDER_ID {
        return lemonade_model_list(&parsed);
    }

    Ok(ModelList {
        chat: model_ids(&parsed),
        other: Vec::new(),
    })
}

/// A Lemonade Server's models, chat models apart, in server order.
fn lemonade_model_list(parsed: &Value) -> Result<ModelList, LlmError> {
    let models = lemonade::models_from_value(parsed).ok_or_else(|| {
        LlmError::BadResponse("The model list answer holds no list of models".to_string())
    })?;
    let (chat, other): (Vec<_>, Vec<_>) = models
        .into_iter()
        .partition(lemonade::LemonadeModel::is_chat);
    Ok(ModelList {
        chat: chat.into_iter().map(|model| model.id).collect(),
        other: other.into_iter().map(|model| model.id).collect(),
    })
}

/// The model ids of an OpenAI-style model list.
fn model_ids(parsed: &Value) -> Vec<String> {
    let mut models = Vec::new();

    // Handle OpenAI format: { data: [ { id: "..." }, ... ] }
    if let Some(data) = parsed.get("data").and_then(|d| d.as_array()) {
        for entry in data {
            if let Some(id) = entry.get("id").and_then(|i| i.as_str()) {
                models.push(id.to_string());
            } else if let Some(name) = entry.get("name").and_then(|n| n.as_str()) {
                models.push(name.to_string());
            }
        }
    }
    // Handle array format: [ "model1", "model2", ... ]
    else if let Some(array) = parsed.as_array() {
        for entry in array {
            if let Some(model) = entry.as_str() {
                models.push(model.to_string());
            }
        }
    }

    models
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Debug)]
    struct TestError {
        message: &'static str,
        source: Option<Box<TestError>>,
    }

    impl fmt::Display for TestError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str(self.message)
        }
    }

    impl StdError for TestError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            self.source
                .as_deref()
                .map(|source| source as &(dyn StdError + 'static))
        }
    }

    fn provider(id: &str, base_url: &str) -> PostProcessProvider {
        PostProcessProvider {
            id: id.to_string(),
            label: id.to_string(),
            base_url: base_url.to_string(),
            allow_base_url_edit: true,
            models_endpoint: None,
            supports_structured_output: false,
        }
    }

    fn request_json(reasoning: ReasoningParams) -> Value {
        let request = ChatCompletionRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
            stream: false,
            response_format: None,
            reasoning,
        };
        serde_json::to_value(&request).unwrap()
    }

    async fn serve_one_response(status: &str, body: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        });

        format!("http://{address}")
    }

    #[test]
    fn error_source_chain_includes_all_nested_causes() {
        let error = TestError {
            message: "request failed",
            source: Some(Box::new(TestError {
                message: "TLS handshake failed",
                source: Some(Box::new(TestError {
                    message: "unknown certificate authority",
                    source: None,
                })),
            })),
        };

        assert_eq!(
            error_source_chain(&error),
            vec!["TLS handshake failed", "unknown certificate authority"]
        );
    }

    #[test]
    fn log_url_sanitization_removes_credentials_and_tokens() {
        let url = "https://user:password@example.com/v1/models?api_key=secret#private";
        assert_eq!(sanitized_url_for_log(url), "https://example.com/v1/models");
    }

    #[test]
    fn invalid_log_urls_are_not_echoed() {
        assert_eq!(
            sanitized_url_for_log("not a URL containing secret"),
            "<invalid URL>"
        );
    }

    #[tokio::test]
    async fn decode_error_does_not_echo_response_values() {
        let base_url =
            serve_one_response("200 OK", r#"{"choices":"PRIVATE TRANSCRIPTION CONTENT"}"#).await;
        let error = reqwest::get(base_url)
            .await
            .unwrap()
            .json::<ChatCompletionResponse>()
            .await
            .unwrap_err();

        let details = report_reqwest_error("Failed to parse API response", &error);
        assert!(details.contains("kind: decode"));
        assert!(!details.contains("PRIVATE TRANSCRIPTION CONTENT"));
    }

    #[tokio::test]
    async fn raw_error_url_is_not_reintroduced_without_a_source() {
        let base_url = serve_one_response("400 Bad Request", "bad request").await;
        let error = reqwest::get(format!(
            "{base_url}/private?api_key=SECRET_QUERY_TOKEN#private"
        ))
        .await
        .unwrap()
        .error_for_status()
        .unwrap_err();

        let details = report_reqwest_error("Request failed", &error);
        assert!(details.contains(&format!("url: {base_url}/private")));
        assert!(!details.contains("SECRET_QUERY_TOKEN"));
        assert!(!details.contains("#private"));
    }

    #[test]
    fn lemonade_address_is_used_with_or_without_its_api_prefix() {
        let lemonade = |base_url: &str| provider(LEMONADE_PROVIDER_ID, base_url);
        for (typed, base) in [
            ("http://localhost:13305", "http://localhost:13305/api/v1"),
            ("http://localhost:13305/", "http://localhost:13305/api/v1"),
            (
                "http://localhost:13305/api/v1/",
                "http://localhost:13305/api/v1",
            ),
            (" http://lemon.lan:8000/v1 ", "http://lemon.lan:8000/v1"),
        ] {
            assert_eq!(api_base(&lemonade(typed), "").unwrap(), base, "{typed}");
            assert_eq!(display_address(&lemonade(typed)), base, "{typed}");
        }
        assert!(matches!(
            api_base(&lemonade("localhost:13305"), ""),
            Err(LlmError::InvalidAddress(_))
        ));

        // A password in the address is sent unless a key is, and never shown.
        let secret = lemonade("http://alice:s3cretpw@lemon.lan:13305");
        assert_eq!(
            api_base(&secret, "").unwrap(),
            "http://alice:s3cretpw@lemon.lan:13305/api/v1"
        );
        assert_eq!(
            api_base(&secret, "key").unwrap(),
            "http://lemon.lan:13305/api/v1"
        );
        assert_eq!(display_address(&secret), "http://lemon.lan:13305/api/v1");
        let error = api_base(&lemonade("http://alice:s3cretpw@lemon.lan/?x=1"), "").unwrap_err();
        assert!(!error.to_string().contains("s3cretpw"), "{error}");

        // Other providers use their address as it is.
        assert_eq!(
            api_base(&provider("custom", "http://localhost:11434/v1/"), "").unwrap(),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            display_address(&provider("custom", "https://bob:pw@llm.example/v1")),
            "https://llm.example/v1"
        );
    }

    #[test]
    fn lemonade_model_lists_keep_chat_models_apart() {
        let parsed: Value =
            serde_json::from_str(crate::file_transcription::lemonade::tests::MIXED_MODELS).unwrap();
        let list = lemonade_model_list(&parsed).unwrap();
        assert_eq!(
            list.chat,
            [
                "Qwen3-4B-Instruct-2507-GGUF",
                "Gemma-3-4b-it-GGUF",
                "Qwen2.5-Omni-3B-GGUF",
                "Llama-3.2-1B-Instruct-Hybrid"
            ]
        );
        assert_eq!(list.other.len(), 6);
        assert!(list.other.contains(&"Whisper-Large-v3-Turbo".to_string()));
        assert!(matches!(
            lemonade_model_list(&serde_json::json!({"error": "x"})),
            Err(LlmError::BadResponse(_))
        ));
        // Other providers list every model.
        assert_eq!(model_ids(&parsed).len(), 10);
    }

    #[test]
    fn model_list_request_lists_downloaded_models_without_a_key() {
        use crate::file_transcription::lemonade::tests::{json, MockServer, MIXED_MODELS};
        let server = MockServer::start(vec![json(200, MIXED_MODELS)]);
        let list = tauri::async_runtime::block_on(fetch_models(
            &provider(LEMONADE_PROVIDER_ID, &server.url()),
            String::new(),
        ))
        .unwrap();
        assert_eq!(list.chat.len(), 4);
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/api/v1/models");
        assert!(requests[0].header("authorization").is_none());
    }

    #[test]
    fn error_answers_keep_their_status_and_an_excerpt() {
        use crate::file_transcription::lemonade::tests::{json, MockServer};
        let body = format!("{{\"error\":\"{}\"}}", "x".repeat(1000));
        let server = MockServer::start(vec![json(500, &body)]);
        let error = tauri::async_runtime::block_on(fetch_models(
            &provider("custom", &server.url()),
            String::new(),
        ))
        .unwrap_err();
        let LlmError::Status { status, body } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(*status, 500);
        assert!(body.starts_with("{\"error\":\"xxx"));
        assert!(body.chars().count() <= 301);
        assert!(error
            .to_string()
            .starts_with("API request failed with status 500"));
    }

    #[test]
    fn requests_explicitly_disable_streaming() {
        let json = request_json(ReasoningParams::default());
        assert_eq!(json["stream"], false);
    }

    #[test]
    fn default_reasoning_params_serialize_to_no_fields() {
        let json = request_json(ReasoningParams::default());
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("reasoning").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn custom_provider_uses_top_level_reasoning_effort() {
        let params = reasoning_disable_params(&provider("custom", "http://localhost:11434/v1"));
        let json = request_json(params);
        assert_eq!(json["reasoning_effort"], "none");
        assert!(json.get("reasoning").is_none());
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn openrouter_uses_nested_reasoning_object() {
        let params =
            reasoning_disable_params(&provider("openrouter", "https://openrouter.ai/api/v1"));
        let json = request_json(params);
        assert!(json.get("reasoning_effort").is_none());
        assert_eq!(json["reasoning"]["effort"], "none");
        assert_eq!(json["reasoning"]["exclude"], true);
        assert!(json.get("thinking").is_none());
    }

    #[test]
    fn deepseek_base_url_uses_thinking_disabled() {
        let params = reasoning_disable_params(&provider("custom", "https://api.deepseek.com"));
        let json = request_json(params);
        assert!(json.get("reasoning_effort").is_none());
        assert!(json.get("reasoning").is_none());
        assert_eq!(json["thinking"]["type"], "disabled");
    }

    #[test]
    fn reasoning_params_is_empty_tracks_all_fields() {
        assert!(ReasoningParams::default().is_empty());
        assert!(!ReasoningParams {
            reasoning_effort: Some("none".to_string()),
            ..Default::default()
        }
        .is_empty());
        assert!(!ReasoningParams {
            thinking: Some(serde_json::json!({ "type": "disabled" })),
            ..Default::default()
        }
        .is_empty());
    }

    #[test]
    fn rejection_memo_is_keyed_by_base_url_and_model() {
        let deepseek = provider("custom", "https://api.deepseek.com/");
        let key = endpoint_key(&deepseek, "deepseek-chat");
        assert_eq!(key, "https://api.deepseek.com|deepseek-chat");
        assert!(!is_known_rejected(&key));
        remember_rejection(key.clone());
        assert!(is_known_rejected(&key));
        // A different model on the same endpoint is tracked separately
        assert!(!is_known_rejected(&endpoint_key(&deepseek, "other-model")));
    }
}
