//! The Post Processing page's Test button, and the problems it and the model
//! list report, in a form the page can put in plain words.

use crate::actions::{run_post_process, PostProcessFailure};
use crate::file_transcription::lemonade::{self, LemonadeError};
use crate::llm_client::{self, LlmError, ModelList};
use crate::settings::{self, AppSettings, PostProcessProvider, LEMONADE_PROVIDER_ID};
use serde::{Deserialize, Serialize};
use specta::Type;
use std::time::{Duration, Instant};
use tauri::AppHandle;

/// Longest wait for a test answer: a local server may have to load the
/// model first.
const TEST_TIMEOUT: Duration = Duration::from_secs(180);

/// What kept post-processing (or the model list) from working.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Type)]
#[serde(rename_all = "snake_case")]
pub enum PostProcessProblemKind {
    /// There is no text to test with.
    EmptyText,
    /// No provider is selected.
    NoProvider,
    /// The provider has no model chosen.
    NoModel,
    /// No prompt is selected.
    NoPrompt,
    /// The selected prompt is empty.
    EmptyPrompt,
    /// Apple Intelligence can't run on this device.
    AppleIntelligenceUnavailable,
    /// The provider needs an API key and none is set.
    MissingApiKey,
    /// The API key holds characters that can't be sent.
    InvalidApiKey,
    /// The provider's address isn't usable.
    InvalidAddress,
    /// The server can't be reached.
    Unreachable,
    /// The server didn't answer in time.
    Timeout,
    /// The server didn't accept the API key.
    Unauthorized,
    /// The server doesn't have the model.
    ModelNotFound,
    /// The server answered with an error.
    ServerError,
    /// The server answered without any text.
    NoAnswer,
    /// The server's answer couldn't be read.
    BadAnswer,
    /// A Lemonade Server has no chat models.
    NoChatModels,
    /// Something failed inside Handy.
    Internal,
}

/// A problem with post-processing; the page words it from `kind`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Type)]
pub struct PostProcessProblem {
    pub kind: PostProcessProblemKind,
    /// The provider's name, e.g. "Lemonade Server".
    pub provider: Option<String>,
    /// The provider's API address, without any password in it.
    pub address: Option<String>,
    /// The model asked for.
    pub model: Option<String>,
    /// The HTTP status of an error answer.
    pub status: Option<u16>,
    /// English detail: the server's message, the system's error, or (for
    /// `NoChatModels`) the server's other models.
    pub detail: Option<String>,
}

impl PostProcessProblem {
    fn new(kind: PostProcessProblemKind) -> Self {
        Self {
            kind,
            provider: None,
            address: None,
            model: None,
            status: None,
            detail: None,
        }
    }

    fn for_provider(kind: PostProcessProblemKind, provider: &PostProcessProvider) -> Self {
        Self {
            provider: Some(provider.label.clone()),
            address: Some(llm_client::display_address(provider)),
            ..Self::new(kind)
        }
    }
}

/// The answer to a test.
#[derive(Serialize, Deserialize, Clone, Debug, Type)]
pub struct PostProcessTest {
    /// The post-processed text; None when the test failed.
    pub output: Option<String>,
    pub problem: Option<PostProcessProblem>,
    /// How long post-processing took.
    pub elapsed_ms: u64,
}

/// Post-processes `text` exactly as a dictation would be, with the saved
/// provider, model and prompt. Problems are reported in the answer.
#[tauri::command]
#[specta::specta]
pub async fn test_post_process(app: AppHandle, text: String) -> Result<PostProcessTest, String> {
    let settings = settings::get_settings(&app);
    Ok(run_test(&settings, &text, TEST_TIMEOUT).await)
}

async fn run_test(settings: &AppSettings, text: &str, timeout: Duration) -> PostProcessTest {
    let started = Instant::now();
    let result = tokio::time::timeout(timeout, run_post_process(settings, text)).await;
    let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let failure = match result {
        Ok(Ok(output)) => {
            return PostProcessTest {
                output: Some(output),
                problem: None,
                elapsed_ms,
            }
        }
        Ok(Err(failure)) => failure,
        Err(_) => PostProcessFailure::Request(LlmError::Timeout(format!(
            "No answer within {} s",
            timeout.as_secs()
        ))),
    };
    PostProcessTest {
        output: None,
        problem: Some(failure_problem(settings, &failure)),
        elapsed_ms,
    }
}

/// The problem for a post-processing failure with `settings`.
fn failure_problem(settings: &AppSettings, failure: &PostProcessFailure) -> PostProcessProblem {
    use PostProcessProblemKind as Kind;
    let Some(provider) = settings.active_post_process_provider() else {
        return PostProcessProblem::new(match failure {
            PostProcessFailure::BlankTranscription => Kind::EmptyText,
            _ => Kind::NoProvider,
        });
    };
    let model = settings
        .post_process_models
        .get(&provider.id)
        .map(|model| model.trim().to_string())
        .unwrap_or_default();
    let api_key = settings
        .post_process_api_keys
        .get(&provider.id)
        .map(String::as_str)
        .unwrap_or_default();
    let kind = match failure {
        PostProcessFailure::Request(error) => {
            return request_problem(provider, &model, api_key, error);
        }
        PostProcessFailure::BlankTranscription => Kind::EmptyText,
        PostProcessFailure::NoProvider => Kind::NoProvider,
        PostProcessFailure::NoModel => Kind::NoModel,
        PostProcessFailure::NoPrompt => Kind::NoPrompt,
        PostProcessFailure::EmptyPrompt => Kind::EmptyPrompt,
        PostProcessFailure::AppleIntelligenceUnavailable => Kind::AppleIntelligenceUnavailable,
        PostProcessFailure::AppleIntelligence(detail) => {
            return PostProcessProblem {
                detail: Some(detail.clone()),
                ..PostProcessProblem::for_provider(Kind::ServerError, provider)
            };
        }
        PostProcessFailure::NoContent => Kind::NoAnswer,
    };
    PostProcessProblem {
        model: (!model.is_empty()).then_some(model),
        ..PostProcessProblem::for_provider(kind, provider)
    }
}

/// The problem for a failed request to `provider` for `model` ("" when
/// listing models).
pub(crate) fn request_problem(
    provider: &PostProcessProvider,
    model: &str,
    api_key: &str,
    error: &LlmError,
) -> PostProcessProblem {
    use PostProcessProblemKind as Kind;
    let (kind, status, detail) = match error {
        LlmError::InvalidAddress(detail) => (Kind::InvalidAddress, None, detail.clone()),
        LlmError::InvalidApiKey(detail) => (Kind::InvalidApiKey, None, detail.clone()),
        LlmError::Setup(detail) => (Kind::Internal, None, detail.clone()),
        LlmError::Unreachable(detail) => (Kind::Unreachable, None, detail.clone()),
        LlmError::Timeout(detail) => (Kind::Timeout, None, detail.clone()),
        LlmError::BadResponse(detail) => (Kind::BadAnswer, None, detail.clone()),
        LlmError::Status { status, body } => {
            let kind = match lemonade::classify_status(*status, body, model) {
                LemonadeError::Unauthorized { .. } if api_key.trim().is_empty() => {
                    Kind::MissingApiKey
                }
                LemonadeError::Unauthorized { .. } => Kind::Unauthorized,
                LemonadeError::ModelNotFound { .. } if !model.is_empty() => Kind::ModelNotFound,
                _ => Kind::ServerError,
            };
            (kind, Some(*status), body.clone())
        }
    };
    PostProcessProblem {
        model: (!model.is_empty()).then(|| model.to_string()),
        status,
        detail: (!detail.is_empty()).then_some(detail),
        ..PostProcessProblem::for_provider(kind, provider)
    }
}

/// The models to offer for `provider` from its model list: its chat models,
/// and `current` (the model chosen before) when the server lists it among
/// its other models. A Lemonade Server with nothing to offer is a
/// `NoChatModels` problem, which names its other models.
pub(crate) fn models_to_offer(
    provider: &PostProcessProvider,
    list: ModelList,
    current: &str,
) -> Result<Vec<String>, PostProcessProblem> {
    let current = current.trim();
    let mut models = list.chat;
    if !current.is_empty()
        && !models.iter().any(|model| model == current)
        && list.other.iter().any(|model| model == current)
    {
        models.push(current.to_string());
    }
    if models.is_empty() && provider.id == LEMONADE_PROVIDER_ID {
        return Err(PostProcessProblem {
            detail: (!list.other.is_empty()).then(|| list.other.join(", ")),
            ..PostProcessProblem::for_provider(PostProcessProblemKind::NoChatModels, provider)
        });
    }
    Ok(models)
}

/// The problem for a model list that wasn't fetched for `kind`'s reason.
pub(crate) fn provider_problem(
    kind: PostProcessProblemKind,
    provider: &PostProcessProvider,
) -> PostProcessProblem {
    PostProcessProblem::for_provider(kind, provider)
}

/// The problem for an unknown provider id.
pub(crate) fn unknown_provider_problem(provider_id: &str) -> PostProcessProblem {
    PostProcessProblem {
        detail: Some(format!("Provider '{}' not found", provider_id)),
        ..PostProcessProblem::new(PostProcessProblemKind::NoProvider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_transcription::lemonade::tests::{json, MockServer, Reply};
    use crate::settings::{get_default_settings, CLEAN_UP_DICTATION_PROMPT_ID};
    use serde_json::Value;

    const SAMPLE: &str = "um so can you uh send me the report by friday";

    /// Default settings using a Lemonade Server at `base_url` with `model`.
    fn lemonade_settings(base_url: &str, model: &str) -> AppSettings {
        let mut settings = get_default_settings();
        settings.post_process_enabled = true;
        settings.post_process_provider_id = LEMONADE_PROVIDER_ID.to_string();
        settings
            .post_process_provider_mut(LEMONADE_PROVIDER_ID)
            .unwrap()
            .base_url = base_url.to_string();
        settings
            .post_process_models
            .insert(LEMONADE_PROVIDER_ID.to_string(), model.to_string());
        settings
    }

    fn chat_answer(content: &str) -> Reply {
        json(
            200,
            &serde_json::json!({
                "choices": [{ "message": { "role": "assistant", "content": content } }]
            })
            .to_string(),
        )
    }

    fn test(settings: &AppSettings, text: &str) -> PostProcessTest {
        tauri::async_runtime::block_on(run_test(settings, text, Duration::from_secs(20)))
    }

    #[test]
    fn test_runs_the_selected_prompt_on_the_text_like_a_dictation() {
        let server = MockServer::start(vec![chat_answer("Can you send me the report by Friday?")]);
        let settings = lemonade_settings(&server.url(), "Qwen3-4B-Instruct-2507-GGUF");
        assert_eq!(
            settings.post_process_selected_prompt_id.as_deref(),
            Some(CLEAN_UP_DICTATION_PROMPT_ID)
        );

        let result = test(&settings, SAMPLE);
        assert!(result.problem.is_none(), "{:?}", result.problem);
        assert_eq!(
            result.output.as_deref(),
            Some("Can you send me the report by Friday?")
        );

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/api/v1/chat/completions");
        assert!(requests[0].header("authorization").is_none());
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body["model"], "Qwen3-4B-Instruct-2507-GGUF");
        assert_eq!(body["stream"], false);
        assert!(body.get("response_format").is_none());
        assert!(body.get("reasoning_effort").is_none());
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert!(messages[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("You are a transcript cleanup engine"));
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(
            messages[1]["content"],
            format!("<transcript>\n{SAMPLE}\n</transcript>")
        );
    }

    #[test]
    fn an_api_key_is_sent_when_set() {
        let server = MockServer::start(vec![chat_answer("Hi.")]);
        let mut settings = lemonade_settings(&format!("{}/api/v1/", server.url()), "m");
        settings
            .post_process_api_keys
            .insert(LEMONADE_PROVIDER_ID.to_string(), "lemon-key".to_string());
        assert_eq!(test(&settings, "hi").output.as_deref(), Some("Hi."));
        assert_eq!(
            server.requests()[0].header("authorization"),
            Some("Bearer lemon-key")
        );
    }

    #[test]
    fn settings_problems_are_reported_before_any_request() {
        let settings = lemonade_settings("http://127.0.0.1:9", "");
        let problem = test(&settings, SAMPLE).problem.unwrap();
        assert_eq!(problem.kind, PostProcessProblemKind::NoModel);
        assert_eq!(problem.provider.as_deref(), Some("Lemonade Server"));
        assert_eq!(
            problem.address.as_deref(),
            Some("http://127.0.0.1:9/api/v1")
        );

        let settings = lemonade_settings("http://127.0.0.1:9", "m");
        let problem = test(&settings, "  \n").problem.unwrap();
        assert_eq!(problem.kind, PostProcessProblemKind::EmptyText);

        let mut settings = lemonade_settings("http://127.0.0.1:9", "m");
        settings.post_process_selected_prompt_id = None;
        let problem = test(&settings, SAMPLE).problem.unwrap();
        assert_eq!(problem.kind, PostProcessProblemKind::NoPrompt);

        let mut settings = lemonade_settings("http://127.0.0.1:9", "m");
        settings.post_process_provider_id = "gone".to_string();
        let problem = test(&settings, SAMPLE).problem.unwrap();
        assert_eq!(problem.kind, PostProcessProblemKind::NoProvider);
    }

    #[test]
    fn a_server_that_is_not_running_is_unreachable_at_its_address() {
        // Bind and drop a listener so the port is free and refuses.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let settings = lemonade_settings(&format!("http://alice:s3cretpw@127.0.0.1:{port}"), "m");
        let problem = test(&settings, SAMPLE).problem.unwrap();
        assert_eq!(problem.kind, PostProcessProblemKind::Unreachable);
        assert_eq!(
            problem.address.as_deref(),
            Some(format!("http://127.0.0.1:{port}/api/v1").as_str())
        );
        assert!(!format!("{problem:?}").contains("s3cretpw"));
    }

    #[test]
    fn error_answers_become_plain_problems() {
        for (reply, key, kind, status) in [
            (
                json(401, r#"{"error":"unauthorized"}"#),
                "",
                PostProcessProblemKind::MissingApiKey,
                Some(401),
            ),
            (
                json(403, r#"{"error":"forbidden"}"#),
                "wrong-key",
                PostProcessProblemKind::Unauthorized,
                Some(403),
            ),
            (
                json(404, r#"{"error":"model 'nope' not found"}"#),
                "",
                PostProcessProblemKind::ModelNotFound,
                Some(404),
            ),
            (
                json(500, r#"{"error":"llama-server crashed"}"#),
                "",
                PostProcessProblemKind::ServerError,
                Some(500),
            ),
            (
                json(200, r#"{"choices":[{"message":{"content":null}}]}"#),
                "",
                PostProcessProblemKind::NoAnswer,
                None,
            ),
            (
                json(200, r#"{"choices":"PRIVATE"}"#),
                "",
                PostProcessProblemKind::BadAnswer,
                None,
            ),
        ] {
            let server = MockServer::start(vec![reply]);
            let mut settings = lemonade_settings(&server.url(), "nope");
            // Any OpenAI-compatible server, through the Custom provider.
            settings.post_process_provider_id = "custom".to_string();
            settings
                .post_process_provider_mut("custom")
                .unwrap()
                .base_url = format!("{}/v1", server.url());
            settings
                .post_process_models
                .insert("custom".to_string(), "nope".to_string());
            settings
                .post_process_api_keys
                .insert("custom".to_string(), key.to_string());
            let problem = test(&settings, SAMPLE).problem.unwrap();
            assert_eq!(problem.kind, kind, "{problem:?}");
            assert_eq!(problem.status, status, "{problem:?}");
            assert_eq!(problem.provider.as_deref(), Some("Custom"));
            if status.is_some() {
                assert!(problem.detail.unwrap().contains("error"));
            }
        }
    }

    #[test]
    fn a_slow_server_times_out() {
        let server = MockServer::start(vec![Reply::Delay(
            Duration::from_secs(3),
            Box::new(chat_answer("late")),
        )]);
        let settings = lemonade_settings(&server.url(), "m");
        let result =
            tauri::async_runtime::block_on(run_test(&settings, SAMPLE, Duration::from_millis(300)));
        let problem = result.problem.unwrap();
        assert_eq!(problem.kind, PostProcessProblemKind::Timeout);
        assert!(result.output.is_none());
        assert!(result.elapsed_ms < 3000);
    }

    /// The model list and Test paths against a real (or stand-in) Lemonade
    /// Server, e.g. `HANDY_LEMONADE_TEST_URL=http://127.0.0.1:13305 cargo
    /// test --lib stand_in -- --ignored --nocapture` (optionally with
    /// `HANDY_LEMONADE_TEST_KEY`). It needs a chat model named
    /// `HANDY_LEMONADE_TEST_MODEL` (default: the first chat model listed).
    #[test]
    #[ignore = "needs a Lemonade Server; set HANDY_LEMONADE_TEST_URL"]
    fn stand_in_lemonade_server_lists_chat_models_and_cleans_up_a_dictation() {
        let url = std::env::var("HANDY_LEMONADE_TEST_URL").expect("HANDY_LEMONADE_TEST_URL");
        let key = std::env::var("HANDY_LEMONADE_TEST_KEY").unwrap_or_default();
        let mut settings = lemonade_settings(&url, "");
        settings
            .post_process_api_keys
            .insert(LEMONADE_PROVIDER_ID.to_string(), key.clone());
        let provider = settings.active_post_process_provider().cloned().unwrap();

        // What the model list command does.
        let list = tauri::async_runtime::block_on(llm_client::fetch_models(&provider, key))
            .expect("model list");
        println!("chat models: {:?}; others: {:?}", list.chat, list.other);
        let models = models_to_offer(&provider, list, "").expect("chat models");
        let model =
            std::env::var("HANDY_LEMONADE_TEST_MODEL").unwrap_or_else(|_| models[0].clone());
        assert!(models.contains(&model), "{model} not in {models:?}");

        // What the Test button does, with "Clean Up Dictation".
        settings
            .post_process_models
            .insert(LEMONADE_PROVIDER_ID.to_string(), model.clone());
        let result = test(&settings, SAMPLE);
        println!("test with {model}: {result:?}");
        assert!(result.problem.is_none(), "{:?}", result.problem);
        assert!(!result.output.unwrap().trim().is_empty());

        // A model the server doesn't have is reported in plain words.
        settings.post_process_models.insert(
            LEMONADE_PROVIDER_ID.to_string(),
            "No-Such-Model".to_string(),
        );
        let problem = test(&settings, SAMPLE).problem.unwrap();
        println!("unknown model: {problem:?}");
        assert_eq!(problem.kind, PostProcessProblemKind::ModelNotFound);
    }

    #[test]
    fn lemonade_offers_chat_models_and_keeps_the_chosen_one() {
        let lemonade = get_default_settings()
            .post_process_provider(LEMONADE_PROVIDER_ID)
            .cloned()
            .unwrap();
        let list = || ModelList {
            chat: vec!["Qwen3-4B".to_string()],
            other: vec!["Whisper-Base".to_string(), "kokoro-v1".to_string()],
        };
        assert_eq!(
            models_to_offer(&lemonade, list(), "").unwrap(),
            ["Qwen3-4B"]
        );
        assert_eq!(
            models_to_offer(&lemonade, list(), "Whisper-Base").unwrap(),
            ["Qwen3-4B", "Whisper-Base"]
        );
        assert_eq!(
            models_to_offer(&lemonade, list(), "Qwen3-4B").unwrap(),
            ["Qwen3-4B"]
        );

        let only_speech = ModelList {
            chat: Vec::new(),
            other: vec!["Whisper-Base".to_string(), "kokoro-v1".to_string()],
        };
        let problem = models_to_offer(&lemonade, only_speech.clone(), "").unwrap_err();
        assert_eq!(problem.kind, PostProcessProblemKind::NoChatModels);
        assert_eq!(problem.detail.as_deref(), Some("Whisper-Base, kokoro-v1"));
        assert_eq!(
            problem.address.as_deref(),
            Some("http://localhost:13305/api/v1")
        );
        let problem = models_to_offer(&lemonade, ModelList::default(), "").unwrap_err();
        assert_eq!(problem.kind, PostProcessProblemKind::NoChatModels);
        assert_eq!(problem.detail, None);

        // Other providers may list nothing.
        let custom = get_default_settings()
            .post_process_provider("custom")
            .cloned()
            .unwrap();
        assert!(models_to_offer(&custom, ModelList::default(), "")
            .unwrap()
            .is_empty());
    }
}
