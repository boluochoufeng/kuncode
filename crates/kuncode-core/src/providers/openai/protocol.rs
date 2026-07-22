//! OpenAI Chat Completions wire DTOs and domain mappings.

use serde::{Deserialize, Serialize};

use crate::{
    completion::{self, AssistantContent, CompletionError, message},
    json_utils,
    non_empty_vec::NonEmptyVec,
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "role", rename_all = "lowercase")]
pub(crate) enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(default, deserialize_with = "json_utils::null_or_default")]
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        refusal: Option<String>,
        #[serde(
            default,
            deserialize_with = "json_utils::null_or_vec",
            skip_serializing_if = "Vec::is_empty"
        )]
        tool_calls: Vec<ToolCall>,
    },
    #[serde(rename = "tool")]
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

impl From<message::Message> for Vec<Message> {
    fn from(value: message::Message) -> Self {
        match value {
            message::Message::System { content } => vec![Message::System { content }],
            message::Message::User { content } => {
                let mut messages = Vec::with_capacity(content.len());
                let mut text = Vec::new();
                for block in content {
                    match block {
                        message::UserContent::Text(value) => text.push(value.text()),
                        message::UserContent::ToolResult(result) => {
                            messages.push(Message::from(result));
                        }
                    }
                }
                if !text.is_empty() {
                    messages.push(Message::User {
                        content: text.join("\n"),
                    });
                }
                messages
            }
            message::Message::Assistant { id: _, content } => {
                let mut text = String::new();
                let mut refusal = String::new();
                let mut tool_calls = Vec::new();
                for block in content {
                    match block {
                        message::AssistantContent::Text(value) => text.push_str(value.text_ref()),
                        message::AssistantContent::Refusal(value) => {
                            refusal.push_str(value.text_ref());
                        }
                        message::AssistantContent::ToolCall(call) => {
                            tool_calls.push(ToolCall::from(call));
                        }
                        // Chat Completions does not accept replayed reasoning text.
                        message::AssistantContent::Reasoning(_) => {}
                    }
                }
                vec![Message::Assistant {
                    content: text,
                    refusal: (!refusal.is_empty()).then_some(refusal),
                    tool_calls,
                }]
            }
        }
    }
}

impl From<message::ToolResult> for Message {
    fn from(value: message::ToolResult) -> Self {
        let content = value
            .content
            .iter()
            .map(|block| match block {
                message::ToolResultContent::Text(text) => text.text_ref(),
            })
            .collect::<Vec<_>>()
            .join("\n");
        Self::ToolResult {
            tool_call_id: value.call_id.unwrap_or(value.id),
            content,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub(crate) struct ToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: ToolType,
    function: Function,
}

impl From<message::ToolCall> for ToolCall {
    fn from(value: message::ToolCall) -> Self {
        Self {
            id: value.call_id.unwrap_or(value.id),
            kind: ToolType::Function,
            function: Function {
                name: value.function.name,
                arguments: value.function.arguments,
            },
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
enum ToolType {
    #[default]
    Function,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
struct Function {
    name: String,
    #[serde(with = "json_utils::stringified_json")]
    arguments: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ToolDefinition {
    #[serde(rename = "type")]
    kind: ToolType,
    function: completion::ToolDefinition,
}

impl From<completion::ToolDefinition> for ToolDefinition {
    fn from(function: completion::ToolDefinition) -> Self {
        Self {
            kind: ToolType::Function,
            function,
        }
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct OpenAiCompletionRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

impl TryFrom<completion::CompletionRequest> for OpenAiCompletionRequest {
    type Error = CompletionError;

    fn try_from(request: completion::CompletionRequest) -> Result<Self, Self::Error> {
        let model = request.model.ok_or_else(|| {
            CompletionError::RequestError("OpenAI request is missing a model ID".to_string())
        })?;
        let messages = request
            .chat_history
            .into_iter()
            .flat_map(Vec::<Message>::from)
            .collect();
        Ok(Self {
            model,
            messages,
            tools: request
                .tools
                .into_iter()
                .map(ToolDefinition::from)
                .collect(),
            tool_choice: request.tool_choice.map(ToolChoice::from),
            max_completion_tokens: request.max_tokens.map(|value| value as u32),
            temperature: request.temperature,
            top_p: request.top_p,
            stop: request.stop.filter(|value| !value.is_empty()),
            reasoning_effort: request.reasoning.map(ReasoningEffort::from),
            response_format: request.output_schema.map(ResponseFormat::json_schema),
            stream: None,
            stream_options: None,
        })
    }
}

impl OpenAiCompletionRequest {
    pub(crate) fn into_streaming(mut self) -> Self {
        self.stream = Some(true);
        self.stream_options = Some(StreamOptions {
            include_usage: true,
        });
        self
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
}

impl From<completion::ReasoningEffort> for ReasoningEffort {
    fn from(value: completion::ReasoningEffort) -> Self {
        match value {
            completion::ReasoningEffort::Off => Self::None,
            completion::ReasoningEffort::Minimal => Self::Minimal,
            completion::ReasoningEffort::Low => Self::Low,
            completion::ReasoningEffort::Medium => Self::Medium,
            completion::ReasoningEffort::High => Self::High,
            completion::ReasoningEffort::Xhigh => Self::Xhigh,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseFormat {
    JsonSchema { json_schema: JsonSchema },
}

impl ResponseFormat {
    fn json_schema(schema: serde_json::Value) -> Self {
        Self::JsonSchema {
            json_schema: JsonSchema {
                name: "kuncode_output",
                schema,
                strict: true,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonSchema {
    name: &'static str,
    schema: serde_json::Value,
    strict: bool,
}

#[derive(Debug, Serialize)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum ToolChoice {
    None,
    Auto,
    Required,
    #[serde(untagged)]
    Function(ToolChoiceFunction),
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", content = "function", rename_all = "lowercase")]
enum ToolChoiceFunction {
    Function { name: String },
}

impl From<message::ToolChoice> for ToolChoice {
    fn from(value: message::ToolChoice) -> Self {
        match value {
            message::ToolChoice::None => Self::None,
            message::ToolChoice::Auto => Self::Auto,
            message::ToolChoice::Required => Self::Required,
            message::ToolChoice::Specific { function_name } => {
                Self::Function(ToolChoiceFunction::Function {
                    name: function_name,
                })
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OpenAiCompletionResponse {
    pub(crate) id: String,
    pub(crate) choices: Vec<Choice>,
    pub(crate) created: u64,
    pub(crate) model: String,
    pub(crate) object: String,
    #[serde(default)]
    pub(crate) system_fingerprint: Option<String>,
    #[serde(default)]
    pub(crate) usage: Usage,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Choice {
    finish_reason: String,
    index: usize,
    message: Message,
    logprobs: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct Usage {
    completion_tokens: u32,
    prompt_tokens: u32,
    total_tokens: u32,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokenDetails>,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokenDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PromptTokenDetails {
    #[serde(default)]
    cached_tokens: u32,
}

impl From<Usage> for completion::Usage {
    fn from(value: Usage) -> Self {
        Self {
            input_tokens: u64::from(value.prompt_tokens),
            output_tokens: u64::from(value.completion_tokens),
            total_tokens: u64::from(value.total_tokens),
            cached_input_tokens: value
                .prompt_tokens_details
                .map_or(0, |details| u64::from(details.cached_tokens)),
            cache_creation_input_tokens: 0,
            reasoning_tokens: value
                .completion_tokens_details
                .map_or(0, |details| u64::from(details.reasoning_tokens)),
        }
    }
}

impl TryFrom<OpenAiCompletionResponse>
    for completion::CompletionResponse<OpenAiCompletionResponse>
{
    type Error = CompletionError;

    fn try_from(response: OpenAiCompletionResponse) -> Result<Self, Self::Error> {
        let choice = response.choices.first().ok_or_else(|| {
            CompletionError::ResponseError("OpenAI response contained no choices".to_string())
        })?;
        let Message::Assistant {
            content,
            refusal,
            tool_calls,
        } = &choice.message
        else {
            return Err(CompletionError::ResponseError(
                "OpenAI response did not contain an assistant message".to_string(),
            ));
        };
        let mut blocks = Vec::new();
        if !content.trim().is_empty() {
            blocks.push(AssistantContent::text(content));
        }
        blocks.extend(tool_calls.iter().map(|call| {
            AssistantContent::tool_call(
                &call.id,
                &call.function.name,
                call.function.arguments.clone(),
            )
        }));
        if let Some(refusal) = refusal.as_ref().filter(|value| !value.is_empty()) {
            blocks.push(AssistantContent::refusal(refusal));
        }
        let blocks = NonEmptyVec::try_from(blocks).map_err(|error| {
            CompletionError::ResponseError(format!(
                "OpenAI response contained no assistant content: {error}"
            ))
        })?;
        Ok(completion::CompletionResponse {
            choice: blocks,
            usage: response.usage.clone().into(),
            raw_response: response,
            message_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{CompletionRequestBuilder, Message as DomainMessage, ReasoningEffort};

    #[test]
    fn maps_openai_specific_request_fields() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
            "additionalProperties": false
        });
        let request = CompletionRequestBuilder::new(DomainMessage::user("test"))
            .model("gpt-test")
            .max_tokens(Some(512))
            .reasoning(Some(ReasoningEffort::Off))
            .output_schema(Some(schema.clone()))
            .build();
        let wire = OpenAiCompletionRequest::try_from(request).expect("wire request");
        let json = serde_json::to_value(wire).expect("serialize request");

        assert_eq!(json["max_completion_tokens"], 512);
        assert!(json.get("max_tokens").is_none());
        assert_eq!(json["reasoning_effort"], "none");
        assert_eq!(json["response_format"]["type"], "json_schema");
        assert_eq!(json["response_format"]["json_schema"]["strict"], true);
        assert_eq!(json["response_format"]["json_schema"]["schema"], schema);
    }

    #[test]
    fn preserves_refusal_as_assistant_content() {
        let response: OpenAiCompletionResponse = serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "finish_reason": "stop",
                "index": 0,
                "message": {"role": "assistant", "content": null, "refusal": "Cannot comply"},
                "logprobs": null
            }],
            "created": 1,
            "model": "gpt-test",
            "object": "chat.completion",
            "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
        }))
        .expect("response fixture");
        let normalized: completion::CompletionResponse<_> =
            response.try_into().expect("normalize response");

        assert!(matches!(
            normalized.choice.first(),
            AssistantContent::Refusal(value) if value.text_ref() == "Cannot comply"
        ));
    }
}
