//! Runtime-selected chat model used when provider choice comes from configuration.

use serde_json::Value;

use crate::{
    completion::{
        CompletionError, CompletionModel, CompletionRequest, CompletionResponse, CompletionStream,
    },
    providers::{
        deepseek::{DeepSeekClient, DeepSeekCompletionModel},
        openai::{OpenAiClient, OpenAiCompletionModel},
    },
};

/// Provider client selected by project configuration.
#[derive(Clone)]
pub enum AnyChatClient {
    /// Native DeepSeek protocol behavior.
    DeepSeek(DeepSeekClient),
    /// OpenAI-compatible Chat Completions behavior.
    OpenAi(OpenAiClient),
}

/// Model handle that keeps the agent runtime independent of provider choice.
#[derive(Clone)]
pub enum AnyChatCompletionModel {
    /// Native DeepSeek model.
    DeepSeek(DeepSeekCompletionModel),
    /// OpenAI-compatible model.
    OpenAi(OpenAiCompletionModel),
}

impl CompletionModel for AnyChatCompletionModel {
    type Response = Value;
    type Client = AnyChatClient;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self {
        let model = model.into();
        match client {
            AnyChatClient::DeepSeek(client) => {
                Self::DeepSeek(DeepSeekCompletionModel::make(client, model))
            }
            AnyChatClient::OpenAi(client) => {
                Self::OpenAi(OpenAiCompletionModel::make(client, model))
            }
        }
    }

    /// `raw_response` semantics differ by branch and are best-effort only: the
    /// OpenAI branch passes the server's original JSON through verbatim, while
    /// the DeepSeek branch re-serializes its typed DTO (unmodeled fields are
    /// dropped). Callers may rely on it being valid JSON, not on it being
    /// byte-faithful; nothing in the runtime consumes it today.
    async fn completion(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse<Self::Response>, CompletionError> {
        match self {
            Self::DeepSeek(model) => {
                let response = model.completion(request).await?;
                let raw_response = serde_json::to_value(response.raw_response)?;
                Ok(CompletionResponse {
                    choice: response.choice,
                    usage: response.usage,
                    raw_response,
                    message_id: response.message_id,
                })
            }
            Self::OpenAi(model) => model.completion(request).await,
        }
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionStream, CompletionError> {
        match self {
            Self::DeepSeek(model) => model.stream(request).await,
            Self::OpenAi(model) => model.stream(request).await,
        }
    }
}
