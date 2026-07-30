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
                let mut tool_calls = Vec::new();
                for block in content {
                    match block {
                        message::AssistantContent::Text(value) => text.push_str(value.text_ref()),
                        message::AssistantContent::ToolCall(call) => {
                            tool_calls.push(ToolCall::from(call));
                        }
                        // Chat Completions does not accept replayed reasoning text.
                        message::AssistantContent::Reasoning(_) => {}
                    }
                }
                vec![Message::Assistant {
                    content: text,
                    // Inbound-only: refusals are flattened to text on receipt,
                    // so replayed history never carries the field.
                    refusal: None,
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
        let tools = request
            .tools
            .into_iter()
            .map(ToolDefinition::from)
            .collect::<Vec<_>>();
        let tool_choice = match request.tool_choice {
            // OpenAI already defaults to `none` when no tools are present.
            Some(message::ToolChoice::None) if tools.is_empty() => None,
            value => value.map(ToolChoice::from),
        };
        let capabilities = ModelCapabilities::for_model(&model);
        let reasoning_effort = request
            .reasoning
            .and_then(|value| ReasoningEffort::from_domain(value, capabilities));
        let temperature = if capabilities.requires_default_temperature {
            None
        } else {
            request.temperature
        };
        Ok(Self {
            model,
            messages,
            tools,
            tool_choice,
            max_completion_tokens: request
                .max_tokens
                .map(|value| {
                    u32::try_from(value).map_err(|_| {
                        CompletionError::RequestError(format!(
                            "max_tokens {value} exceeds the OpenAI u32 wire range"
                        ))
                    })
                })
                .transpose()?,
            temperature,
            top_p: request.top_p,
            stop: request.stop.filter(|value| !value.is_empty()),
            reasoning_effort,
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

#[derive(Clone, Copy)]
struct ModelCapabilities {
    supports_none_reasoning_effort: bool,
    requires_default_temperature: bool,
}

impl ModelCapabilities {
    fn for_model(model: &str) -> Self {
        let is_gpt_5 =
            model == "gpt-5" || model.starts_with("gpt-5-") || model.starts_with("gpt-5.");
        let is_o_series = ["o1", "o3", "o4"].iter().any(|family| {
            model == *family
                || model
                    .strip_prefix(family)
                    .is_some_and(|suffix| suffix.starts_with('-'))
        });
        Self {
            supports_none_reasoning_effort: supports_none_reasoning_effort(model),
            requires_default_temperature: is_gpt_5 || is_o_series,
        }
    }
}

impl ReasoningEffort {
    /// Maps explicit disablement to `none` only for model families that support
    /// it. Older reasoning and non-reasoning models reject that wire value, so
    /// omission is the compatible fallback for them.
    ///
    /// [`Off`]: completion::ReasoningEffort::Off
    fn from_domain(
        value: completion::ReasoningEffort,
        capabilities: ModelCapabilities,
    ) -> Option<Self> {
        match value {
            completion::ReasoningEffort::Off if capabilities.supports_none_reasoning_effort => {
                Some(Self::None)
            }
            completion::ReasoningEffort::Off => None,
            completion::ReasoningEffort::Minimal => Some(Self::Minimal),
            completion::ReasoningEffort::Low => Some(Self::Low),
            completion::ReasoningEffort::Medium => Some(Self::Medium),
            completion::ReasoningEffort::High => Some(Self::High),
            completion::ReasoningEffort::Xhigh => Some(Self::Xhigh),
        }
    }
}

fn supports_none_reasoning_effort(model: &str) -> bool {
    model
        .strip_prefix("gpt-5.")
        .and_then(|suffix| suffix.split('-').next())
        .and_then(|minor| minor.parse::<u16>().ok())
        .is_some_and(|minor| minor >= 1)
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseFormat {
    JsonSchema { json_schema: JsonSchema },
}

impl ResponseFormat {
    /// `strict: false` deliberately: strict mode accepts only a narrow schema
    /// subset (every property `required`, no `$schema`/`format`, ...), and the
    /// schemas callers pass here are plain `schemars` output that violates it —
    /// OpenAI would 400 on the request. Non-strict still steers generation with
    /// the schema; callers validate the parsed result themselves.
    fn json_schema(schema: serde_json::Value) -> Self {
        Self::JsonSchema {
            json_schema: JsonSchema {
                name: "kuncode_output",
                schema,
                strict: false,
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
        let mut answer = String::new();
        if !content.trim().is_empty() {
            answer.push_str(content);
        }
        // A refusal is OpenAI's wire spelling for "the answer text is a
        // decline"; the agent has no refusal-aware branching, so it flattens to
        // ordinary text. The verbatim field survives in `raw_response`.
        if let Some(refusal) = refusal.as_ref().filter(|value| !value.is_empty()) {
            answer.push_str(refusal);
        }
        if !answer.is_empty() {
            blocks.push(AssistantContent::text(answer));
        }
        blocks.extend(tool_calls.iter().map(|call| {
            AssistantContent::tool_call(
                &call.id,
                &call.function.name,
                call.function.arguments.clone(),
            )
        }));
        let blocks = NonEmptyVec::try_from(blocks).map_err(|error| {
            CompletionError::ResponseError(format!(
                "OpenAI response contained no assistant content: {error}"
            ))
        })?;
        Ok(completion::CompletionResponse {
            choice: blocks,
            usage: response.usage.clone().into(),
            raw_response: response,
            // `id` is the completion-call id (`chatcmpl-...`), not a message id;
            // mirror the DeepSeek mapping and leave it in `raw_response` only.
            message_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        completion::{
            CompletionRequestBuilder, Message as DomainMessage, ReasoningEffort,
            ToolChoice as DomainToolChoice, ToolDefinition as DomainToolDefinition, ToolResult,
            ToolResultContent, UserContent,
        },
        non_empty_vec::NonEmptyVec,
    };

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
            .reasoning(Some(ReasoningEffort::Medium))
            .output_schema(Some(schema.clone()))
            .build();
        let wire = OpenAiCompletionRequest::try_from(request).expect("wire request");
        let json = serde_json::to_value(wire).expect("serialize request");

        assert_eq!(json["max_completion_tokens"], 512);
        assert!(json.get("max_tokens").is_none());
        assert_eq!(json["reasoning_effort"], "medium");
        assert_eq!(json["response_format"]["type"], "json_schema");
        // Strict mode rejects plain schemars output (optional fields, $schema,
        // integer formats) with a 400 — the request must never opt in.
        assert_eq!(json["response_format"]["json_schema"]["strict"], false);
        assert_eq!(json["response_format"]["json_schema"]["schema"], schema);
    }

    #[test]
    fn reasoning_effort_off_maps_to_none_when_the_model_supports_it() {
        let request = CompletionRequestBuilder::new(DomainMessage::user("test"))
            .model("gpt-5.1")
            .temperature(Some(0.0))
            .reasoning(Some(ReasoningEffort::Off))
            .build();
        let wire = OpenAiCompletionRequest::try_from(request).expect("wire request");
        let json = serde_json::to_value(wire).expect("serialize request");

        assert_eq!(json["reasoning_effort"], "none");
        assert!(json.get("temperature").is_none());
    }

    #[test]
    fn reasoning_effort_off_is_omitted_for_models_without_none_support() {
        let request = CompletionRequestBuilder::new(DomainMessage::user("test"))
            .model("gpt-4o")
            .temperature(Some(0.0))
            .reasoning(Some(ReasoningEffort::Off))
            .build();
        let wire = OpenAiCompletionRequest::try_from(request).expect("wire request");
        let json = serde_json::to_value(wire).expect("serialize request");

        assert!(json.get("reasoning_effort").is_none());
        assert_eq!(json["temperature"], 0.0);
    }

    #[test]
    fn reasoning_model_omits_non_default_temperature() {
        let request = CompletionRequestBuilder::new(DomainMessage::user("test"))
            .model("o3")
            .temperature(Some(0.0))
            .build();
        let wire = OpenAiCompletionRequest::try_from(request).expect("wire request");
        let json = serde_json::to_value(wire).expect("serialize request");

        assert!(json.get("temperature").is_none());
    }

    #[test]
    fn no_tools_omits_redundant_none_tool_choice() {
        let request = CompletionRequestBuilder::new(DomainMessage::user("test"))
            .model("gpt-test")
            .tool_choice(Some(DomainToolChoice::None))
            .build();
        let wire = OpenAiCompletionRequest::try_from(request).expect("wire request");
        let json = serde_json::to_value(wire).expect("serialize request");

        assert!(json.get("tools").is_none());
        assert!(json.get("tool_choice").is_none());
    }

    #[test]
    fn tool_definition_and_specific_choice_use_openai_wire_shapes() {
        let request = CompletionRequestBuilder::new(DomainMessage::user("test"))
            .model("gpt-test")
            .tool(DomainToolDefinition {
                name: "lookup".to_string(),
                description: "Look up a value".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": { "key": { "type": "string" } },
                    "required": ["key"]
                }),
            })
            .tool_choice(Some(DomainToolChoice::Specific {
                function_name: "lookup".to_string(),
            }))
            .build();
        let wire = OpenAiCompletionRequest::try_from(request).expect("wire request");
        let json = serde_json::to_value(wire).expect("serialize request");

        assert_eq!(
            json["tools"],
            serde_json::json!([{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "description": "Look up a value",
                    "parameters": {
                        "type": "object",
                        "properties": { "key": { "type": "string" } },
                        "required": ["key"]
                    }
                }
            }])
        );
        assert_eq!(
            json["tool_choice"],
            serde_json::json!({
                "type": "function",
                "function": { "name": "lookup" }
            })
        );
    }

    #[test]
    fn mixed_user_content_emits_tool_results_before_joined_text() {
        let domain = DomainMessage::User {
            content: NonEmptyVec::from_first_rest(
                UserContent::text("first"),
                vec![
                    UserContent::ToolResult(ToolResult {
                        id: "call_1".to_string(),
                        call_id: None,
                        content: NonEmptyVec::new(ToolResultContent::text("tool output")),
                    }),
                    UserContent::text("second"),
                ],
            ),
        };

        let wire = Vec::<Message>::from(domain);

        assert_eq!(wire.len(), 2);
        assert!(matches!(
            &wire[0],
            Message::ToolResult { tool_call_id, content }
                if tool_call_id == "call_1" && content == "tool output"
        ));
        assert!(matches!(
            &wire[1],
            Message::User { content } if content == "first\nsecond"
        ));
    }

    #[test]
    fn pure_tool_call_assistant_turn_serializes_empty_content() {
        let domain = DomainMessage::Assistant {
            id: None,
            content: NonEmptyVec::from_first_rest(
                AssistantContent::tool_call(
                    "call_1",
                    "lookup",
                    serde_json::json!({"key": "value"}),
                ),
                vec![AssistantContent::reasoning("not replayed")],
            ),
        };

        let wire = Vec::<Message>::from(domain);
        let json = serde_json::to_value(&wire).expect("serialize messages");

        assert_eq!(
            json,
            serde_json::json!([{
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {
                        "name": "lookup",
                        "arguments": "{\"key\":\"value\"}"
                    }
                }]
            }])
        );
    }

    #[test]
    fn oversized_max_tokens_is_a_request_error_not_a_wrap() {
        let request = CompletionRequestBuilder::new(DomainMessage::user("test"))
            .model("gpt-test")
            .max_tokens(Some(u64::from(u32::MAX) + 1))
            .build();

        assert!(matches!(
            OpenAiCompletionRequest::try_from(request),
            Err(CompletionError::RequestError(_))
        ));
    }

    #[test]
    fn refusal_flattens_to_assistant_text() {
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

        // The wire refusal reaches the agent as ordinary text — the domain has
        // no refusal-aware consumer — while `raw_response` keeps the original.
        assert!(matches!(
            normalized.choice.first(),
            AssistantContent::Text(value) if value.text_ref() == "Cannot comply"
        ));
    }

    #[test]
    fn content_and_refusal_flatten_to_one_text_block() {
        let response: OpenAiCompletionResponse = serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "finish_reason": "stop",
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Cannot ",
                    "refusal": "comply"
                },
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

        assert_eq!(normalized.choice.len(), 1);
        assert!(matches!(
            normalized.choice.first(),
            AssistantContent::Text(value) if value.text_ref() == "Cannot comply"
        ));
    }

    #[test]
    fn tool_calls_map_with_ids_names_and_parsed_arguments() {
        let response: OpenAiCompletionResponse = serde_json::from_value(serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "finish_reason": "tool_calls",
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"NYC\"}"}
                    }]
                },
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

        let call = match normalized.choice.first() {
            AssistantContent::ToolCall(call) => call,
            other => panic!("expected a tool call, got {other:?}"),
        };
        assert_eq!(call.id, "call_9");
        assert_eq!(call.function.name, "get_weather");
        assert_eq!(call.function.arguments, serde_json::json!({"city": "NYC"}));
    }
}
