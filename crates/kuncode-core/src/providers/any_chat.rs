//! Runtime-selected chat model used when provider choice comes from configuration.

use serde_json::Value;

use crate::{
    completion::{
        CompletionError, CompletionModel, CompletionRequest, CompletionResponse, CompletionStream,
    },
    providers::{
        deepseek::{DeepSeekClient, DeepSeekCompletionModel},
        openai_compatible::{OpenAiCompatibleClient, OpenAiCompatibleCompletionModel},
    },
};

/// Provider client selected by project configuration.
#[derive(Clone)]
pub enum AnyChatClient {
    /// Native DeepSeek protocol behavior.
    DeepSeek(DeepSeekClient),
    /// OpenAI-compatible Chat Completions behavior.
    OpenAiCompatible(OpenAiCompatibleClient),
}

/// Model handle that keeps the agent runtime independent of provider choice.
#[derive(Clone)]
pub enum AnyChatCompletionModel {
    /// Native DeepSeek model.
    DeepSeek(DeepSeekCompletionModel),
    /// OpenAI-compatible model.
    OpenAiCompatible(OpenAiCompatibleCompletionModel),
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
            AnyChatClient::OpenAiCompatible(client) => {
                Self::OpenAiCompatible(OpenAiCompatibleCompletionModel::make(client, model))
            }
        }
    }

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
            Self::OpenAiCompatible(model) => model.completion(request).await,
        }
    }

    async fn stream(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionStream, CompletionError> {
        match self {
            Self::DeepSeek(model) => model.stream(request).await,
            Self::OpenAiCompatible(model) => model.stream(request).await,
        }
    }
}
