//! Runtime-selected chat model used when provider choice comes from configuration.

use serde_json::Value;

use crate::{
    completion::{
        CompletionError, CompletionModel, CompletionRequest, CompletionResponse, CompletionStream,
    },
    providers::{
        deepseek::{DeepSeekClient, DeepSeekCompletionModel, protocol::DeepSeekCompletionResponse},
        openai::{OpenAiClient, OpenAiCompletionModel},
    },
};

/// Provider client selected by project configuration.
#[derive(Clone)]
pub enum AnyChatClient {
    /// Native DeepSeek protocol behavior.
    DeepSeek(DeepSeekClient),
    /// Official OpenAI Chat Completions behavior.
    OpenAi(OpenAiClient),
}

/// Model handle that keeps the agent runtime independent of provider choice.
#[derive(Clone)]
pub enum AnyChatCompletionModel {
    /// Native DeepSeek model.
    DeepSeek(DeepSeekCompletionModel),
    /// Official OpenAI model.
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
                erase_deepseek_response(response)
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

fn erase_deepseek_response(
    response: CompletionResponse<DeepSeekCompletionResponse>,
) -> Result<CompletionResponse<Value>, CompletionError> {
    let raw_response = serde_json::to_value(response.raw_response)?;
    Ok(CompletionResponse {
        choice: response.choice,
        usage: response.usage,
        raw_response,
        message_id: response.message_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::AssistantContent;

    #[test]
    fn make_dispatches_to_the_selected_provider() {
        let deepseek = AnyChatClient::DeepSeek(
            DeepSeekClient::new("test-key").expect("build DeepSeek test client"),
        );
        let openai =
            AnyChatClient::OpenAi(OpenAiClient::new("test-key").expect("build OpenAI test client"));

        assert!(matches!(
            AnyChatCompletionModel::make(&deepseek, "deepseek-test"),
            AnyChatCompletionModel::DeepSeek(_)
        ));
        assert!(matches!(
            AnyChatCompletionModel::make(&openai, "gpt-test"),
            AnyChatCompletionModel::OpenAi(_)
        ));
    }

    #[test]
    fn deepseek_response_erasure_serializes_the_typed_raw_response() {
        let raw: DeepSeekCompletionResponse = serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "finish_reason": "stop",
                "index": 0,
                "message": {"role": "assistant", "content": "hello"},
                "logprobs": null
            }],
            "created": 1,
            "model": "deepseek-test",
            "system_fingerprint": "fp_test",
            "object": "chat.completion",
            "usage": {
                "prompt_tokens": 4,
                "completion_tokens": 2,
                "total_tokens": 6,
                "unmodeled_extension": true
            }
        }))
        .expect("DeepSeek response fixture");
        let typed: CompletionResponse<DeepSeekCompletionResponse> =
            raw.try_into().expect("normalize DeepSeek response");

        let erased = erase_deepseek_response(typed).expect("erase response type");

        assert!(matches!(
            erased.choice.first(),
            AssistantContent::Text(text) if text.text_ref() == "hello"
        ));
        assert_eq!(erased.usage.total_tokens, 6);
        assert_eq!(erased.raw_response["id"], "chatcmpl-test");
        assert!(
            erased.raw_response["usage"]
                .get("unmodeled_extension")
                .is_none(),
            "typed DeepSeek responses intentionally drop unmodeled fields"
        );
    }
}
