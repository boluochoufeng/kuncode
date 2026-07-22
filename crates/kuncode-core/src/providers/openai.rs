//! Official OpenAI Chat Completions provider.

use std::{env::VarError, time::Duration};

use reqwest::header::CONTENT_TYPE;
use serde_json::Value;
use thiserror::Error;

use crate::{
    completion::{CompletionError, CompletionModel, CompletionRequest, CompletionResponse},
    json_utils,
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
        validate_stream_content_type(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
        )?;
        Ok(crate::providers::chat_completions::streaming::stream_events::<Usage>(response))
    }
}

fn normalize_response(raw: Value) -> Result<CompletionResponse<Value>, CompletionError> {
    let response: OpenAiCompletionResponse = serde_json::from_value(raw.clone())?;
    let normalized: CompletionResponse<OpenAiCompletionResponse> = response.try_into()?;
    Ok(CompletionResponse {
        choice: normalized.choice,
        usage: normalized.usage,
        raw_response: raw,
        message_id: normalized.message_id,
    })
}

fn validate_stream_content_type(content_type: Option<&str>) -> Result<(), CompletionError> {
    let Some(content_type) = content_type else {
        return Ok(());
    };
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    if media_type.eq_ignore_ascii_case("text/event-stream") {
        Ok(())
    } else {
        Err(CompletionError::ResponseError(format!(
            "expected an OpenAI SSE response, but received `{media_type}`"
        )))
    }
}
