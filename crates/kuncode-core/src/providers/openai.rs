//! Official OpenAI Chat Completions provider.

use std::{env::VarError, time::Duration};

use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::{
    completion::{CompletionError, CompletionModel, CompletionRequest, CompletionResponse},
    json_utils,
    providers::chat_completions::streaming,
};

use self::protocol::{OpenAiCompletionRequest, OpenAiCompletionResponse, Usage};

mod protocol;

const OPENAI_COMPLETIONS_URL: &str = "https://api.openai.com/v1/chat/completions";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(360);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(360);

/// Errors produced while constructing an OpenAI client.
#[derive(Debug, Error)]
pub enum Error {
    /// The underlying HTTP client could not be built.
    #[error("HTTP client error: {0}")]
    Client(#[from] reqwest::Error),
    /// `OPENAI_API_KEY` was missing or invalid Unicode.
    #[error("environment variable `OPENAI_API_KEY` is not set or is invalid")]
    EnvironmentVariable(#[source] VarError),
}

/// Authenticated client for the official OpenAI API.
#[derive(Clone)]
pub struct OpenAiClient {
    http_client: reqwest::Client,
    api_key: String,
}

impl OpenAiClient {
    /// Builds a client for the fixed official OpenAI endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`enum@Error`] when the HTTP client cannot be configured.
    pub fn new(api_key: impl Into<String>) -> Result<Self, Error> {
        let http_client = reqwest::Client::builder()
            .read_timeout(READ_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        Ok(Self {
            http_client,
            api_key: api_key.into(),
        })
    }

    /// Reads `OPENAI_API_KEY` and builds an official OpenAI client.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EnvironmentVariable`] when the credential is unavailable,
    /// or [`Error::Client`] when the HTTP client cannot be configured.
    pub fn from_env() -> Result<Self, Error> {
        let api_key = std::env::var("OPENAI_API_KEY").map_err(Error::EnvironmentVariable)?;
        Self::new(api_key)
    }

    fn post(&self) -> reqwest::RequestBuilder {
        self.http_client
            .post(OPENAI_COMPLETIONS_URL)
            .bearer_auth(&self.api_key)
    }
}

/// Completion model for the official OpenAI Chat Completions API.
#[derive(Clone)]
pub struct OpenAiCompletionModel {
    client: OpenAiClient,
    model: String,
}

impl CompletionModel for OpenAiCompletionModel {
    type Response = Value;
    type Client = OpenAiClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self {
            client: client.clone(),
            model: model.into(),
        }
    }

    async fn completion(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        request.model.get_or_insert_with(|| self.model.clone());
        let extra = request.additional_params.take();
        let wire = OpenAiCompletionRequest::try_from(request)?;
        let builder = self.client.post().timeout(REQUEST_TIMEOUT);
        let response = match extra {
            Some(extra) => {
                let body = json_utils::merge(serde_json::to_value(&wire)?, extra);
                builder.json(&body).send().await?
            }
            None => builder.json(&wire).send().await?,
        };
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
        mut request: CompletionRequest,
    ) -> Result<crate::completion::CompletionStream, CompletionError> {
        request.model.get_or_insert_with(|| self.model.clone());
        let extra = request.additional_params.take();
        let wire = OpenAiCompletionRequest::try_from(request)?.into_streaming();
        let builder = self.client.post();
        let response = match extra {
            Some(extra) => {
                let body = json_utils::merge(serde_json::to_value(&wire)?, extra);
                builder.json(&body).send().await?
            }
            None => builder.json(&wire).send().await?,
        };
        let status = response.status();
        if !status.is_success() {
            return Err(CompletionError::ApiError {
                status: status.as_u16(),
                message: response.text().await.unwrap_or_default(),
            });
        }
        streaming::validate_stream_content_type(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
        )?;
        Ok(streaming::stream_events::<Usage>(response))
    }
}

/// Projects the server's JSON into the domain response while handing the
/// untouched original through as `raw_response`. Deserializes by reference so
/// the projection does not deep-copy the body.
fn normalize_response(raw: Value) -> Result<CompletionResponse<Value>, CompletionError> {
    let response = OpenAiCompletionResponse::deserialize(&raw)?;
    let normalized: CompletionResponse<OpenAiCompletionResponse> = response.try_into()?;
    Ok(CompletionResponse {
        choice: normalized.choice,
        usage: normalized.usage,
        raw_response: raw,
        message_id: normalized.message_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::AssistantContent;

    #[test]
    fn normalize_response_projects_content_and_keeps_the_original_json() {
        let raw = serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "finish_reason": "stop",
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "logprobs": null
            }],
            "created": 1,
            "model": "gpt-test",
            "object": "chat.completion",
            "system_fingerprint": "fp_1",
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6,
                      "unmodeled_extension": {"depth": 3}}
        });

        let normalized = normalize_response(raw.clone()).expect("normalize");

        assert!(matches!(
            normalized.choice.first(),
            AssistantContent::Text(text) if text.text_ref() == "hello"
        ));
        assert_eq!(normalized.usage.input_tokens, 4);
        // The raw side is the *server's* JSON verbatim — unmodeled fields
        // included — not a re-serialization of the typed DTO.
        assert_eq!(normalized.raw_response, raw);
        assert_eq!(
            normalized.raw_response["usage"]["unmodeled_extension"]["depth"],
            3
        );
    }

    #[test]
    fn normalize_response_rejects_a_non_completion_body() {
        let error = normalize_response(serde_json::json!({"error": "nope"}))
            .expect_err("not a completion body");
        assert!(matches!(error, CompletionError::JsonError(_)));
    }
}
