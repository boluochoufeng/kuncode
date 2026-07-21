//! OpenAI-compatible `/chat/completions` provider with configurable endpoint.

use std::time::Duration;

use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use thiserror::Error;

use crate::{
    completion::{
        CompletionError, CompletionModel, CompletionRequest, CompletionResponse, ReasoningEffort,
    },
    json_utils,
    providers::deepseek::protocol::{DeepSeekCompletionRequest, DeepSeekCompletionResponse},
};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(360);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(360);

/// Errors produced while constructing an OpenAI-compatible client.
#[derive(Debug, Error)]
pub enum Error {
    /// The endpoint is empty and cannot be normalized.
    #[error("OpenAI-compatible base URL must not be blank")]
    BlankBaseUrl,
    /// The underlying HTTP client could not be built.
    #[error("HTTP client error: {0}")]
    Client(#[from] reqwest::Error),
}

/// Authenticated client for OpenAI and compatible Chat Completions endpoints.
#[derive(Clone)]
pub struct OpenAiCompatibleClient {
    http_client: reqwest::Client,
    api_key: String,
    endpoint: String,
}

impl OpenAiCompatibleClient {
    /// Builds a client, appending `/chat/completions` when `base_url` names a
    /// service root such as `https://api.openai.com/v1`.
    ///
    /// An empty API key is accepted for local endpoints that do not authenticate.
    ///
    /// # Errors
    /// Returns [`enum@Error`] when the base URL is blank or the HTTP client fails.
    pub fn new(api_key: impl Into<String>, base_url: impl AsRef<str>) -> Result<Self, Error> {
        let endpoint = completion_endpoint(base_url.as_ref())?;
        let http_client = reqwest::Client::builder()
            .read_timeout(READ_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        Ok(Self {
            http_client,
            api_key: api_key.into(),
            endpoint,
        })
    }

    /// Builds a client for the official OpenAI endpoint.
    ///
    /// # Errors
    /// Returns [`enum@Error`] when the HTTP client cannot be built.
    pub fn openai(api_key: impl Into<String>) -> Result<Self, Error> {
        Self::new(api_key, DEFAULT_BASE_URL)
    }

    fn post(&self) -> reqwest::RequestBuilder {
        let request = self.http_client.post(&self.endpoint);
        if self.api_key.is_empty() {
            request
        } else {
            request.bearer_auth(&self.api_key)
        }
    }
}

fn completion_endpoint(base_url: &str) -> Result<String, Error> {
    let base_url = base_url.trim().trim_end_matches('/');
    if base_url.is_empty() {
        return Err(Error::BlankBaseUrl);
    }
    if base_url.ends_with("/chat/completions") {
        Ok(base_url.to_string())
    } else {
        Ok(format!("{base_url}/chat/completions"))
    }
}

/// Completion model for OpenAI-compatible Chat Completions APIs.
#[derive(Clone)]
pub struct OpenAiCompatibleCompletionModel {
    client: OpenAiCompatibleClient,
    model: String,
}

impl CompletionModel for OpenAiCompatibleCompletionModel {
    type Response = Value;
    type Client = OpenAiCompatibleClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self {
            client: client.clone(),
            model: model.into(),
        }
    }

    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        let body = request_body(request, &self.model, false)?;
        let response = self
            .client
            .post()
            .timeout(REQUEST_TIMEOUT)
            .json(&body)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(CompletionError::ApiError {
                status: status.as_u16(),
                message: response.text().await.unwrap_or_default(),
            });
        }

        let raw: Value = serde_json::from_slice(&response.bytes().await?)?;
        normalize_response(raw)
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<crate::completion::CompletionStream, CompletionError> {
        let body = request_body(request, &self.model, true)?;
        let response = self.client.post().json(&body).send().await?;
        let status = response.status();
        if !status.is_success() {
            return Err(CompletionError::ApiError {
                status: status.as_u16(),
                message: response.text().await.unwrap_or_default(),
            });
        }
        validate_stream_content_type(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
        )?;
        Ok(crate::providers::deepseek::protocol::streaming::stream_events(response))
    }
}

fn validate_stream_content_type(content_type: Option<&str>) -> Result<(), CompletionError> {
    let Some(content_type) = content_type else {
        return Ok(());
    };
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case("text/event-stream") {
        return Ok(());
    }

    Err(CompletionError::ResponseError(format!(
        "expected an SSE response with content type `text/event-stream`, but the provider returned `{media_type}`; verify `baseUrl` points to the API root (usually ending in `/v1`) or the full `/chat/completions` endpoint"
    )))
}

fn request_body(
    mut request: CompletionRequest,
    model: &str,
    streaming: bool,
) -> Result<Value, CompletionError> {
    request.model.get_or_insert_with(|| model.to_string());
    let extra = request.additional_params.take();
    let reasoning = request.reasoning.take();
    let request = DeepSeekCompletionRequest::try_from(request)?;
    let mut body = if streaming {
        serde_json::to_value(request.into_streaming())?
    } else {
        serde_json::to_value(request)?
    };

    // The shared DTO carries DeepSeek's `thinking` object. OpenAI-compatible
    // endpoints use the flat `reasoning_effort` field instead.
    if let Value::Object(fields) = &mut body {
        fields.remove("thinking");
        fields.remove("reasoning_effort");
        if let Some(effort) = openai_reasoning_effort(reasoning) {
            fields.insert(
                "reasoning_effort".to_string(),
                Value::String(effort.to_string()),
            );
        }
    }

    match extra {
        Some(extra) => Ok(json_utils::merge(body, extra)),
        None => Ok(body),
    }
}

fn normalize_response(raw: Value) -> Result<CompletionResponse<Value>, CompletionError> {
    let provider: DeepSeekCompletionResponse = serde_json::from_value(raw.clone())?;
    let normalized: CompletionResponse<DeepSeekCompletionResponse> = provider.try_into()?;
    Ok(CompletionResponse {
        choice: normalized.choice,
        usage: normalized.usage,
        raw_response: raw,
        message_id: normalized.message_id,
    })
}

fn openai_reasoning_effort(effort: Option<ReasoningEffort>) -> Option<&'static str> {
    match effort {
        None | Some(ReasoningEffort::Off) => None,
        Some(ReasoningEffort::Minimal) => Some("minimal"),
        Some(ReasoningEffort::Low) => Some("low"),
        Some(ReasoningEffort::Medium) => Some("medium"),
        Some(ReasoningEffort::High) => Some("high"),
        Some(ReasoningEffort::Xhigh) => Some("xhigh"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{AssistantContent, CompletionRequestBuilder, Message};

    #[test]
    fn appends_chat_completions_to_service_root() {
        assert_eq!(
            completion_endpoint("https://api.openai.com/v1/").expect("valid endpoint"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn preserves_full_chat_completions_endpoint() {
        assert_eq!(
            completion_endpoint("http://localhost:8000/v1/chat/completions")
                .expect("valid endpoint"),
            "http://localhost:8000/v1/chat/completions"
        );
    }

    #[test]
    fn accepts_sse_content_type_with_parameters() {
        validate_stream_content_type(Some("text/event-stream; charset=utf-8"))
            .expect("SSE content type should be accepted");
    }

    #[test]
    fn accepts_missing_content_type_for_compatible_gateways() {
        validate_stream_content_type(None)
            .expect("a missing content type should fall through to the SSE decoder");
    }

    #[test]
    fn rejects_html_with_base_url_guidance() {
        let error = validate_stream_content_type(Some("text/html; charset=utf-8"))
            .expect_err("HTML is not an SSE response")
            .to_string();

        assert!(error.contains("`text/html`"));
        assert!(error.contains("`baseUrl`"));
        assert!(error.contains("/v1"));
    }

    #[test]
    fn request_uses_openai_reasoning_field_without_deepseek_thinking() {
        let request = CompletionRequestBuilder::new(Message::user("test"))
            .reasoning(Some(ReasoningEffort::Low))
            .build();
        let body = request_body(request, "gpt-test", true).expect("request body");

        assert_eq!(body["reasoning_effort"], "low");
        assert!(body.get("thinking").is_none());
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn accepts_openai_tool_call_with_null_content_and_no_fingerprint() {
        let response = normalize_response(serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "finish_reason": "tool_calls",
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "bash",
                            "arguments": "{\"cmd\":\"pwd\"}"
                        }
                    }]
                },
                "logprobs": null
            }],
            "created": 1,
            "model": "gpt-test",
            "object": "chat.completion",
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 4,
                "total_tokens": 14,
                "prompt_tokens_details": { "cached_tokens": 3 }
            }
        }))
        .expect("OpenAI response normalizes");

        assert_eq!(response.usage.cached_input_tokens, 3);
        assert!(matches!(
            response.choice.first(),
            AssistantContent::ToolCall(call)
                if call.function.name == "bash"
                    && call.function.arguments.get("cmd").and_then(Value::as_str) == Some("pwd")
        ));
    }
}
