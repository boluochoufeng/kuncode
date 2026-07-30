//! OpenAI Chat Completions provider for official and compatible endpoints.

use std::{env::VarError, time::Duration};

use reqwest::{
    Url,
    header::{
        AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, InvalidHeaderName,
        InvalidHeaderValue,
    },
};
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
    /// The configured service URL is not valid.
    #[error("invalid OpenAI-compatible endpoint: {0}")]
    Url(#[from] url::ParseError),
    /// A configured header name is invalid.
    #[error("invalid provider header name: {0}")]
    HeaderName(#[from] InvalidHeaderName),
    /// A configured header value is invalid.
    #[error("invalid provider header value: {0}")]
    HeaderValue(#[from] InvalidHeaderValue),
    /// Authentication remains tied to the selected credential environment variable.
    #[error("custom provider headers must not include `Authorization`")]
    AuthorizationHeader,
}

/// Chat Completions client with official defaults and optional user configuration.
#[derive(Clone)]
pub struct OpenAiClient {
    http_client: reqwest::Client,
    api_key: String,
    endpoint: Url,
    headers: HeaderMap,
}

impl OpenAiClient {
    /// Builds a client for the fixed official OpenAI endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`enum@Error`] when the HTTP client cannot be configured.
    pub fn new(api_key: impl Into<String>) -> Result<Self, Error> {
        Self::with_endpoint(api_key, OPENAI_COMPLETIONS_URL, std::iter::empty())
    }

    /// Builds an OpenAI-protocol client for a user-controlled service endpoint.
    ///
    /// `base_url` may identify either the service root or the full
    /// `/chat/completions` endpoint. Query parameters are preserved.
    ///
    /// # Errors
    ///
    /// Returns [`enum@Error`] for an invalid URL, invalid or reserved header,
    /// or HTTP client configuration failure.
    pub fn with_endpoint(
        api_key: impl Into<String>,
        base_url: &str,
        headers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, Error> {
        let endpoint = completion_endpoint(base_url)?;
        let mut header_map = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::try_from(name.trim())?;
            if name == AUTHORIZATION {
                return Err(Error::AuthorizationHeader);
            }
            header_map.insert(name, HeaderValue::try_from(value)?);
        }
        let http_client = reqwest::Client::builder()
            .read_timeout(READ_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        Ok(Self {
            http_client,
            api_key: api_key.into(),
            endpoint,
            headers: header_map,
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
        let request = self
            .http_client
            .post(self.endpoint.clone())
            .headers(self.headers.clone());
        if self.api_key.is_empty() {
            request
        } else {
            request.bearer_auth(&self.api_key)
        }
    }
}

fn completion_endpoint(base_url: &str) -> Result<Url, Error> {
    let mut url = Url::parse(base_url.trim())?;
    if !url
        .path()
        .trim_end_matches('/')
        .ends_with("/chat/completions")
    {
        let path = format!("{}/chat/completions", url.path().trim_end_matches('/'));
        url.set_path(&path);
    }
    Ok(url)
}

/// Completion model for OpenAI-protocol Chat Completions APIs.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_normalization_preserves_query_parameters() {
        let endpoint = completion_endpoint("https://gateway.example/v1?tenant=test")
            .expect("valid service root");

        assert_eq!(endpoint.path(), "/v1/chat/completions");
        assert_eq!(endpoint.query(), Some("tenant=test"));
    }

    #[test]
    fn full_endpoint_with_query_is_not_modified() {
        let endpoint = completion_endpoint(
            "https://gateway.example/v1/chat/completions?api-version=2026-01-01",
        )
        .expect("valid full endpoint");

        assert_eq!(endpoint.path(), "/v1/chat/completions");
        assert_eq!(endpoint.query(), Some("api-version=2026-01-01"));
    }

    #[test]
    fn custom_authorization_header_is_rejected() {
        let result = OpenAiClient::with_endpoint(
            "key",
            "https://gateway.example/v1",
            [("Authorization".to_string(), "Bearer other".to_string())],
        );

        assert!(matches!(result, Err(Error::AuthorizationHeader)));
    }
}
