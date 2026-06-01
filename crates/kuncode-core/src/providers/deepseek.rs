//! DeepSeek HTTP client and completion model implementation.
//!
//! [`DeepSeekClient`] owns authentication and the underlying `reqwest` client.
//! [`DeepSeekCompletionModel`] implements [`CompletionModel`]. The wire mapping
//! between [`crate::completion`] domain types and DeepSeek JSON lives in the
//! private `protocol` module.

use std::env::VarError;
use std::time::Duration;

use thiserror::Error;

use crate::{
    completion::{CompletionError, CompletionModel},
    json_utils,
    providers::deepseek::protocol::{DeepSeekCompletionRequest, DeepSeekCompletionResponse},
};

pub(crate) mod protocol;

const DEEPSEEK_API_BASE_URL: &str = "https://api.deepseek.com";
#[cfg(test)]
const DEEPSEEK_V4_FLASH: &str = "deepseek-v4-flash";

/// Errors produced while constructing a DeepSeek client.
#[derive(Debug, Error)]
pub enum Error {
    /// The underlying `reqwest` client could not be built.
    #[error("Http client error: {0}")]
    ClientError(#[from] Box<dyn std::error::Error + Send + Sync + 'static>),

    /// Required environment variable was missing or invalid.
    #[error("environment variable `{name}` is not set or is invalid")]
    EnvironmentVariable {
        /// Environment variable name that was read.
        name: String,
        #[source]
        /// Original environment lookup error.
        source: VarError,
    },
}

/// Authenticated DeepSeek HTTP client shared by model handles.
///
/// The client uses finite request/connect timeouts so provider calls cannot
/// hang indefinitely.
#[derive(Clone)]
pub struct DeepSeekClient {
    http_client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl DeepSeekClient {
    /// Builds a client from an API key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::ClientError`] if the HTTP client cannot be configured.
    pub fn new(api_key: impl Into<String>) -> Result<Self, Error> {
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(360))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|err| Error::ClientError(err.into()))?;
        Ok(Self {
            http_client,
            api_key: api_key.into(),
            base_url: DEEPSEEK_API_BASE_URL.to_string(),
        })
    }

    /// Builds a client from the `DEEPSEEK_API_KEY` environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EnvironmentVariable`] if the variable is missing or not
    /// valid Unicode, and [`Error::ClientError`] if the HTTP client cannot be
    /// configured.
    pub fn from_env() -> Result<Self, Error> {
        let api_key =
            std::env::var("DEEPSEEK_API_KEY").map_err(|err| Error::EnvironmentVariable {
                name: "DEEPSEEK_API_KEY".to_string(),
                source: err,
            })?;

        Self::new(api_key)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http_client
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
    }
}

/// Completion model handle bound to a DeepSeek model id.
#[derive(Clone)]
pub struct DeepSeekCompletionModel {
    client: DeepSeekClient,
    model: String,
}

impl CompletionModel for DeepSeekCompletionModel {
    type Response = DeepSeekCompletionResponse;
    type Client = DeepSeekClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        Self {
            client: client.clone(),
            model: model.into(),
        }
    }

    async fn completion(
        &self,
        request: crate::completion::CompletionRequest,
    ) -> Result<
        crate::completion::CompletionResponse<Self::Response>,
        crate::completion::CompletionError,
    > {
        let mut request = request;
        request.model.get_or_insert_with(|| self.model.clone());

        let extra_params = request.additional_params.take();
        let request = DeepSeekCompletionRequest::try_from(request)?;
        let mut body = serde_json::to_value(&request)?;
        if let Some(extra) = extra_params {
            body = json_utils::merge(body, extra);
        }

        let response = self
            .client
            .post("/chat/completions")
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let message = response.text().await.unwrap_or_default();
            return Err(CompletionError::ApiError {
                status: status.as_u16(),
                message,
            });
        }

        let response_body = response.bytes().await?;
        let resp: DeepSeekCompletionResponse = serde_json::from_slice(&response_body)?;
        resp.try_into()
    }

    async fn stream(
        &self,
        _request: crate::completion::CompletionRequest,
    ) -> Result<crate::completion::CompletionStream, crate::completion::CompletionError> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{AssistantContent, CompletionRequestBuilder, Message, ToolDescriptor};

    #[tokio::test]
    #[ignore = "hits the real DeepSeek API; requires DEEPSEEK_API_KEY"]
    async fn completion_smoke() {
        // Load DEEPSEEK_API_KEY from the repository's .env when present.
        dotenvy::dotenv().ok();

        let client = DeepSeekClient::from_env().expect("DEEPSEEK_API_KEY is not set");
        let model = DeepSeekCompletionModel::make(&client, DEEPSEEK_V4_FLASH);

        let request =
            CompletionRequestBuilder::new(Message::user("How strong are you at Chinese chess?"))
                .max_tokens(Some(1024))
                .build();

        let response = model
            .completion(request)
            .await
            .expect("completion request failed");

        println!("choice = {:#?}", response.choice);
        println!("usage  = {:?}", response.usage);

        // A real round trip should report token usage; this guards against an
        // empty response path being mistaken for a successful API call.
        assert!(
            response.usage.total_tokens > 0,
            "usage was empty; the request may not have reached the API"
        );
    }

    /// Verifies the tool-call path end to end: tool request, local execution,
    /// tool result replay, and final model answer.
    #[tokio::test]
    #[ignore = "hits the real DeepSeek API; requires DEEPSEEK_API_KEY"]
    async fn tool_call_round_trip() {
        dotenvy::dotenv().ok();

        let client = DeepSeekClient::from_env().expect("DEEPSEEK_API_KEY is not set");
        let model = DeepSeekCompletionModel::make(&client, DEEPSEEK_V4_FLASH);

        // A function the model cannot answer from context alone.
        let weather_tool = ToolDescriptor {
            name: "get_weather".to_string(),
            description: "Look up the current weather for a city".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "city": { "type": "string", "description": "City name, such as Beijing" }
                },
                "required": ["city"]
            }),
        };

        // Round 1: ask the question and expect a tool call.
        let user_prompt = Message::user("北京天气怎么样");
        let round1 = CompletionRequestBuilder::new(user_prompt.clone())
            .tool(weather_tool.clone())
            .max_tokens(Some(1024))
            .build();

        let resp1 = model.completion(round1).await.expect("round1 failed");
        println!("== round1 choice ==\n{:#?}", resp1.choice);

        let tool_call = resp1
            .choice
            .iter()
            .find_map(|c| match c {
                AssistantContent::ToolCall(tc) => Some(tc.clone()),
                _ => None,
            })
            .expect("model did not emit a tool_call; use tool_choice=Required if needed");

        assert_eq!(tool_call.function.name, "get_weather");
        let city = tool_call
            .function
            .arguments
            .get("city")
            .and_then(|v| v.as_str())
            .expect("tool_call arguments are missing city");
        println!("== parsed tool call == name=get_weather city={city}");

        // Execute the local tool with a deterministic response.
        let tool_output = serde_json::json!({
            "city": city,
            "temperature": "22 C",
            "condition": "sunny"
        })
        .to_string();

        // Round 2: replay assistant(tool_call) plus the tool result and expect
        // the model to summarize from that result. build() appends the prompt
        // last, so the final order is [user, assistant(tool_call), tool_result].
        let assistant_turn = Message::Assistant {
            id: resp1.message_id.clone(),
            content: resp1.choice.clone(),
        };
        let result_turn = Message::tool_result(tool_call.id.clone(), tool_output);

        let round2 = CompletionRequestBuilder::new(result_turn)
            .messages([user_prompt, assistant_turn])
            .tool(weather_tool)
            .max_tokens(Some(1024))
            .build();

        let resp2 = model.completion(round2).await.expect("round2 failed");
        println!("== round2 choice ==\n{:#?}", resp2.choice);

        let final_text: String = resp2
            .choice
            .iter()
            .filter_map(|c| match c {
                AssistantContent::Text(t) => Some(t.text_ref()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        println!("== final answer ==\n{final_text}");

        // The path is wired correctly if round 2 produces non-empty text from
        // the tool result.
        assert!(!final_text.trim().is_empty(), "round 2 produced no text");
    }
}
