//! DeepSeek `/chat/completions` wire protocol (DeepSeek / OpenAI-compatible JSON).
//!
//! These types are **wire DTOs** that translate between the
//! provider-agnostic domain types in [`crate::completion`] and DeepSeek's HTTP
//! JSON. The overall shape - role-tagged messages, tool-use modeling,
//! reasoning fields, and stringified JSON in `function.arguments` - follows
//! the design used by `rig-core`'s DeepSeek provider.
//!
//! Mapping directions:
//! - outbound: [`completion::CompletionRequest`] -> [`DeepSeekCompletionRequest`];
//!   [`message::Message`] -> `Vec<Message>`.
//! - inbound: [`DeepSeekCompletionResponse`] -> [`completion::CompletionResponse`].

use serde::{Deserialize, Serialize};

use crate::{
    completion::{self, AssistantContent, CompletionError, message},
    json_utils,
    non_empty_vec::NonEmptyVec,
};

pub(crate) mod streaming;

/// DeepSeek wire message serialized by `role`.
///
/// This is flatter than the domain-side [`message::Message`]: `content` is a
/// plain `String`, tool calls live directly on assistant messages, and tool
/// results are standalone `tool` role messages. The outbound conversion from
/// [`message::Message`] performs that flattening.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// System prompt.
    System {
        /// Prompt text.
        content: String,
        /// Optional speaker name accepted by OpenAI-compatible APIs.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// User input; tool results are represented by [`Message::ToolResult`].
    User {
        /// User text content.
        content: String,
        /// Optional speaker name accepted by OpenAI-compatible APIs.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// Assistant output: visible text plus optional tool calls and reasoning.
    Assistant {
        /// Visible assistant text.
        content: String,
        /// Optional speaker name accepted by OpenAI-compatible APIs.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        // DeepSeek beta prefix-completion mode continues generation from the
        // supplied assistant prefix.
        #[serde(skip_serializing_if = "Option::is_none")]
        prefix: Option<bool>,
        // DeepSeek may omit this field or send null when no tool call exists.
        // `default` covers missing keys, `null_or_vec` covers explicit null,
        // and both normalize to an empty Vec. `skip_serializing_if` is outbound
        // only, avoiding empty arrays without weakening inbound tolerance.
        #[serde(
            default,
            deserialize_with = "json_utils::null_or_vec",
            skip_serializing_if = "Vec::is_empty"
        )]
        tool_calls: Vec<ToolCall>,
        // Present when parsing responses and intentionally preserved outbound:
        // v4 thinking mode requires replaying reasoning_content verbatim after
        // tool calls, otherwise the API returns 400. Non-tool follow-up turns
        // ignore it.
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },

    /// Tool execution result correlated by `tool_call_id`.
    #[serde(rename = "tool")]
    ToolResult {
        /// Identifier of the assistant tool call this result answers.
        tool_call_id: String,
        /// Tool output text.
        content: String,
    },
}

impl From<message::Message> for Vec<Message> {
    /// Flattens one domain message into one or more wire messages.
    ///
    /// A user message may expand because embedded tool results must become
    /// standalone `tool` role messages. Assistant content is merged in the
    /// opposite direction: text blocks are concatenated, reasoning is replayed
    /// through `reasoning_content`, and tool calls are flattened. DeepSeek has
    /// no message-level id, so the domain assistant `id` is discarded.
    fn from(value: message::Message) -> Self {
        match value {
            message::Message::System { content } => vec![Message::System {
                content,
                name: None,
            }],
            message::Message::User { content: contents } => {
                let mut messages = Vec::with_capacity(contents.len());
                let mut user_messages = vec![];
                for content in contents.into_iter() {
                    match content {
                        message::UserContent::Text(text) => {
                            user_messages.push(text.text());
                        }
                        message::UserContent::ToolResult(tool_result) => {
                            messages.push(Message::from(tool_result));
                        }
                    }
                }

                let user_message = user_messages.join("\n");
                if !user_message.is_empty() {
                    messages.push(Message::User {
                        content: user_message,
                        name: None,
                    });
                }

                messages
            }
            message::Message::Assistant { id: _, content } => {
                let mut text_content = String::new();
                let mut tool_calls = vec![];
                let mut reasoning_content = String::new();
                for assistant_content in content {
                    match assistant_content {
                        message::AssistantContent::Text(text) => {
                            text_content.push_str(text.text_ref());
                        }
                        message::AssistantContent::ToolCall(tool_call) => {
                            tool_calls.push(ToolCall::from(tool_call));
                        }
                        message::AssistantContent::Reasoning(reasoning) => {
                            reasoning_content.push_str(
                                reasoning
                                    .content
                                    .iter()
                                    .filter_map(|content| match content {
                                        message::ReasoningContent::Text { text, signature: _ } => {
                                            Some(text.as_str())
                                        }
                                        message::ReasoningContent::Summary(summary) => {
                                            Some(summary.as_str())
                                        }
                                        message::ReasoningContent::Redacted { data } => {
                                            Some(data.as_str())
                                        }
                                        message::ReasoningContent::Encrypted(_) => None,
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                                    .as_str(),
                            );
                        }
                    }
                }

                let reasoning = if reasoning_content.is_empty() {
                    None
                } else {
                    Some(reasoning_content)
                };

                vec![Message::Assistant {
                    content: text_content,
                    name: None,
                    prefix: None,
                    tool_calls,
                    reasoning_content: reasoning,
                }]
            }
        }
    }
}

impl From<message::ToolResult> for Message {
    fn from(value: message::ToolResult) -> Self {
        // Join every result block so multi-block tool output is not truncated.
        let content = value
            .content
            .iter()
            .map(|c| match c {
                message::ToolResultContent::Text(text) => text.text_ref(),
            })
            .collect::<Vec<_>>()
            .join("\n");

        Message::ToolResult {
            tool_call_id: value.id,
            content,
        }
    }
}

/// Tool call emitted by an assistant message in DeepSeek's wire shape.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ToolCall {
    /// Provider tool-call identifier used by subsequent tool results.
    pub id: String,
    // Position within parallel calls. DeepSeek includes it in responses and
    // streaming chunks; request replay uses array order instead, so outbound
    // conversion fills 0 and inbound projection does not read it.
    pub index: usize,
    /// Tool-call kind; currently always [`ToolType::Function`].
    pub r#type: ToolType,
    /// Function name and arguments.
    pub function: Function,
}

impl From<message::ToolCall> for ToolCall {
    fn from(value: message::ToolCall) -> Self {
        Self {
            id: value.id,
            index: 0,
            r#type: ToolType::Function,
            function: Function {
                name: value.function.name,
                arguments: value.function.arguments,
            },
        }
    }
}

/// Tool kind; DeepSeek currently exposes only `function`.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
    /// Function-style tool call.
    #[default]
    Function,
}

/// Function name and arguments in a DeepSeek tool call.
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Function {
    /// Function name selected by the model.
    pub name: String,
    // Wire arguments are stringified JSON. The domain-side
    // ToolFunction::arguments is a structured serde_json::Value, so this
    // adapter bridges the two field types at the protocol boundary.
    #[serde(with = "json_utils::stringified_json")]
    pub arguments: serde_json::Value,
}

/// Function-style tool exposed to the model in a request.
///
/// The wire object wraps the domain definition as
/// `{ "type": "function", "function": { name, description, parameters } }`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolDefinition {
    /// Wire discriminator required by the OpenAI-compatible schema.
    pub r#type: String,
    /// Domain-level function metadata.
    pub function: completion::ToolDefinition,
}

impl From<completion::ToolDefinition> for ToolDefinition {
    fn from(value: completion::ToolDefinition) -> Self {
        Self {
            r#type: "function".to_string(),
            function: value,
        }
    }
}

/// DeepSeek token usage payload.
///
/// In addition to OpenAI-compatible fields, DeepSeek reports cache hit/miss
/// prompt tokens.
///
/// `#[serde(default)]`: token accounting is best-effort, and streaming usage
/// frames may carry a subset of fields. A missing sub-field defaults to zero
/// rather than failing the whole response/chunk parse.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Usage {
    /// Generated output tokens.
    pub completion_tokens: u32,
    /// Prompt/context input tokens.
    pub prompt_tokens: u32,
    /// Prompt tokens served from context cache.
    pub prompt_cache_hit_tokens: u32,
    /// Prompt tokens not served from context cache.
    pub prompt_cache_miss_tokens: u32,
    /// Provider-reported total token count.
    pub total_tokens: u32,
    /// Optional output-token breakdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokenDetails>,
    /// Optional prompt-token breakdown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// Output-token breakdown.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompletionTokenDetails {
    /// Tokens spent in the model's reasoning/thinking phase.
    pub reasoning_tokens: u32,
}

/// Prompt-token breakdown.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptTokensDetails {
    /// Input tokens served from context cache.
    pub cached_tokens: u32,
}

impl From<Usage> for completion::Usage {
    /// Projects DeepSeek usage into domain [`completion::Usage`].
    ///
    /// DeepSeek has no Anthropic-style "cache creation" accounting, so
    /// `cache_creation_input_tokens` is always zero.
    fn from(value: Usage) -> Self {
        completion::Usage {
            input_tokens: value.prompt_tokens as u64,
            output_tokens: value.completion_tokens as u64,
            total_tokens: value.total_tokens as u64,
            cached_input_tokens: value
                .prompt_tokens_details
                .map(|p| p.cached_tokens)
                .unwrap_or(0) as u64,
            cache_creation_input_tokens: 0,
            reasoning_tokens: value
                .completion_tokens_details
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0) as u64,
        }
    }
}

/// One candidate response.
///
/// DeepSeek currently returns one candidate (`n = 1`). `finish_reason` is not
/// normalized yet; the agent loop can branch on whether the projected choice
/// contains tool calls, and callers that care about truncation can inspect the
/// raw [`DeepSeekCompletionResponse`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    /// Provider stop reason string.
    pub finish_reason: String,
    /// Candidate index in the provider response.
    pub index: usize,
    /// Assistant message payload.
    pub message: Message,
    /// Optional logprob data when requested through provider-specific params.
    pub logprobs: Option<serde_json::Value>,
}

/// DeepSeek `/chat/completions` response body.
///
/// `id` identifies the completion call itself (the `chatcmpl-...` style id),
/// not a message. It is therefore not mapped to
/// [`completion::CompletionResponse::message_id`], which represents
/// OpenAI-Responses-style message ids. Callers that need this call id can read
/// it from `raw_response`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeepSeekCompletionResponse {
    /// Provider completion-call id.
    pub id: String,
    /// Candidate responses; DeepSeek normally returns one.
    pub choices: Vec<Choice>,
    /// Provider creation timestamp.
    pub created: u64,
    /// Model id that served the request.
    pub model: String,
    /// Provider backend fingerprint.
    pub system_fingerprint: String,
    /// OpenAI-compatible object type.
    pub object: String,
    /// Token accounting for the call.
    pub usage: Usage,
}

impl TryFrom<DeepSeekCompletionResponse>
    for completion::CompletionResponse<DeepSeekCompletionResponse>
{
    type Error = completion::CompletionError;

    /// Projects the first DeepSeek choice into domain assistant content.
    ///
    /// # Errors
    ///
    /// Returns [`CompletionError::ResponseError`] if the response has no
    /// choices, the first choice is not an assistant message, or the projected
    /// assistant content is empty and rejected by [`NonEmptyVec`].
    fn try_from(response: DeepSeekCompletionResponse) -> Result<Self, Self::Error> {
        // DeepSeek currently supports n=1, so the first choice is the only one.
        let choice = response.choices.first().ok_or_else(|| {
            CompletionError::ResponseError("Response didn't contain any choice".to_string())
        })?;

        let content = match &choice.message {
            Message::Assistant {
                content,
                name: _,
                prefix: _,
                tool_calls,
                reasoning_content,
            } => {
                let mut content = if content.trim().is_empty() {
                    vec![]
                } else {
                    vec![completion::AssistantContent::text(content)]
                };
                content.extend(tool_calls.iter().map(|tool_call| {
                    completion::AssistantContent::tool_call(
                        &tool_call.id,
                        &tool_call.function.name,
                        tool_call.function.arguments.clone(),
                    )
                }));

                if let Some(reasoning_content) = reasoning_content {
                    content.push(completion::AssistantContent::reasoning(reasoning_content));
                }
                Ok(content)
            }
            _ => Err(CompletionError::ResponseError(
                "Response didn't contain a valid message".to_string(),
            )),
        }?;

        let choice = NonEmptyVec::<AssistantContent>::try_from(content).map_err(|err| {
            CompletionError::ResponseError(format!(
                "Response didn't contain any assistant message: {}",
                err
            ))
        })?;

        Ok(completion::CompletionResponse {
            choice,
            usage: response.usage.clone().into(),
            raw_response: response,
            message_id: None,
        })
    }
}

/// DeepSeek `/chat/completions` request body.
///
/// This intentionally is not the full DeepSeek wire contract. It models only
/// two groups of fields:
///
/// - **mapped fields** populated from provider-agnostic request fields by
///   `TryFrom<CompletionRequest>`. `thinking` and `reasoning_effort` are split
///   from the neutral `reasoning` effort.
/// - **control fields** chosen by whether the caller invokes `completion()` or
///   the streaming path, not by external input.
///
/// Less common DeepSeek-specific parameters such as `logprobs`,
/// `top_logprobs`, or `user_id` are intentionally left out. Callers can place
/// them in [`completion::CompletionRequest::additional_params`], which is
/// shallow-merged into the serialized request body before dispatch; caller
/// keys win on collision.
///
/// This trades away two conveniences: the struct alone does not reveal every
/// DeepSeek parameter, and the exact outgoing JSON cannot be asserted solely
/// from this type because the final payload is the serialized value after
/// `additional_params` are merged in.
#[derive(Debug, Serialize, Deserialize)]
pub struct DeepSeekCompletionRequest {
    // Mapped fields from CompletionRequest.
    model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDefinition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Stop>,
    // Split from the neutral reasoning effort in TryFrom.
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
    // DeepSeek supports only json_object, not full schema transport.
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    // Control fields chosen by the calling method (completion / stream).
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

impl TryFrom<completion::CompletionRequest> for DeepSeekCompletionRequest {
    type Error = completion::CompletionError;
    fn try_from(req: completion::CompletionRequest) -> Result<Self, Self::Error> {
        if req.output_schema.is_some() {
            tracing::warn!("DeepSeek dosen't support structed outputs");
        }

        let model = req.model.ok_or_else(|| {
            CompletionError::RequestError("Invalid request: lacking of model ID".to_string())
        })?;

        let chat_history: Vec<Message> = req
            .chat_history
            .into_iter()
            .flat_map(Vec::<Message>::from)
            .collect();

        let tool_choice = req.tool_choice.map(ToolChoice::from);

        // Split the neutral reasoning effort into DeepSeek's two wire fields:
        // the thinking toggle and reasoning_effort strength. DeepSeek exposes
        // only high/max, so minimal/low/medium/high collapse to high and xhigh
        // maps to max.
        use completion::ReasoningEffort as Eff;
        let (thinking, reasoning_effort) = match req.reasoning {
            None => (None, None),
            Some(Eff::Off) => (Some(Thinking::Disabled), None),
            Some(Eff::Minimal | Eff::Low | Eff::Medium | Eff::High) => {
                (Some(Thinking::Enabled), Some(ReasoningEffort::High))
            }
            Some(Eff::Xhigh) => (Some(Thinking::Enabled), Some(ReasoningEffort::Max)),
        };

        // DeepSeek ignores temperature/top_p when thinking is enabled. Dropping
        // them here keeps the provider behavior aligned with APIs that reject
        // sampling parameters in reasoning mode.
        let (temperature, top_p) = if matches!(thinking, Some(Thinking::Enabled)) {
            if req.temperature.is_some() || req.top_p.is_some() {
                tracing::warn!(
                    "dropping temperature/top_p: DeepSeek ignores them when thinking is enabled"
                );
            }
            (None, None)
        } else {
            (req.temperature, req.top_p)
        };

        Ok(Self {
            model,
            tools: req.tools.into_iter().map(ToolDefinition::from).collect(),
            tool_choice,
            messages: chat_history,
            max_tokens: req.max_tokens.map(|t| t as u32),
            temperature,
            top_p,
            // Treat an empty list as unset to avoid sending "stop": [].
            stop: req.stop.filter(|s| !s.is_empty()).map(Stop::Multi),
            stream: None,
            stream_options: None,
            thinking,
            reasoning_effort,
            response_format: None,
        })
    }
}

impl DeepSeekCompletionRequest {
    /// Marks this request as streaming, asking the provider to include token
    /// usage in the final SSE frame. The completion path leaves both unset.
    pub(crate) fn into_streaming(mut self) -> Self {
        self.stream = Some(true);
        self.stream_options = Some(StreamOptions {
            include_usage: true,
        });
        self
    }
}

/// Thinking toggle serialized as `thinking.type`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Thinking {
    /// Enable DeepSeek thinking mode.
    Enabled,
    /// Disable DeepSeek thinking mode.
    Disabled,
}

/// DeepSeek reasoning strength after neutral effort levels are collapsed.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    /// DeepSeek's lower available reasoning strength.
    High,
    /// DeepSeek's maximum reasoning strength.
    Max,
}

/// Output format accepted by DeepSeek.
///
/// DeepSeek supports `text` and `json_object`; it does not accept a JSON
/// Schema payload.
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    /// Plain text output.
    #[default]
    Text,
    /// JSON object mode.
    JsonObject,
}

/// Stop sequence, either a single string or multiple strings.
///
/// `untagged` preserves the two DeepSeek-supported wire shapes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Stop {
    /// One stop sequence.
    Single(String),
    /// Multiple stop sequences.
    Multi(Vec<String>),
}

/// Streaming options.
#[derive(Debug, Serialize, Deserialize)]
pub struct StreamOptions {
    /// Include usage accounting in the final stream frame.
    include_usage: bool,
}

/// Tool-call policy.
///
/// The `Function` variant serializes as
/// `{ "type": "function", "function": { "name": ... } }`; the other variants
/// serialize as `"none"`, `"auto"`, or `"required"`.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    /// Disable tool calls.
    None,
    /// Let the model decide whether to call tools.
    Auto,
    /// Require at least one tool call.
    Required,
    /// Require a specific function.
    #[serde(untagged)]
    Function(ToolChoiceFunction),
}

/// Specific function the model must call.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "function", rename_all = "lowercase")]
pub enum ToolChoiceFunction {
    /// Function constraint payload.
    Function {
        /// Required function name.
        name: String,
    },
}

impl From<message::ToolChoice> for ToolChoice {
    fn from(value: message::ToolChoice) -> Self {
        match value {
            message::ToolChoice::None => Self::None,
            message::ToolChoice::Auto => Self::Auto,
            message::ToolChoice::Required => Self::Required,
            message::ToolChoice::Specific {
                function_name: function_names,
            } => Self::Function(ToolChoiceFunction::Function {
                name: function_names,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Message;
    use crate::completion::message as dm;
    use crate::non_empty_vec::NonEmptyVec;

    #[test]
    fn request_serializes_tool_metadata_before_messages() {
        let tool = crate::completion::ToolDefinition {
            name: "bash".to_string(),
            description: "Run a shell command".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                },
                "required": ["cmd"]
            }),
        };
        let request = crate::completion::CompletionRequestBuilder::new(dm::Message::user("hello"))
            .model("deepseek-test")
            .tool(tool)
            .tool_choice(Some(dm::ToolChoice::Auto))
            .build();

        let wire = super::DeepSeekCompletionRequest::try_from(request)
            .expect("request should map to wire");
        let json = serde_json::to_string(&wire).expect("wire request should serialize");

        let model = json.find("\"model\"").expect("model field");
        let tools = json.find("\"tools\"").expect("tools field");
        let tool_choice = json.find("\"tool_choice\"").expect("tool_choice field");
        let messages = json.find("\"messages\"").expect("messages field");

        assert!(model < tools);
        assert!(tools < tool_choice);
        assert!(tool_choice < messages);
    }

    #[test]
    fn request_mapping_preserves_flattened_message_order() {
        let messages = NonEmptyVec::from_first_rest(
            dm::Message::system("system"),
            vec![
                dm::Message::user("first"),
                dm::Message::User {
                    content: NonEmptyVec::from_first_rest(
                        dm::UserContent::ToolResult(dm::ToolResult {
                            id: "call_1".to_string(),
                            call_id: None,
                            content: NonEmptyVec::new(dm::ToolResultContent::text("tool output")),
                        }),
                        vec![dm::UserContent::text("after tool")],
                    ),
                },
                dm::Message::assistant("done"),
            ],
        );
        let request = crate::completion::CompletionRequestBuilder::from_messages(messages)
            .model("deepseek-test")
            .build();

        let wire = super::DeepSeekCompletionRequest::try_from(request)
            .expect("request should map to wire");

        assert_eq!(wire.messages.len(), 5);
        assert!(matches!(
            &wire.messages[0],
            Message::System { content, .. } if content == "system"
        ));
        assert!(matches!(
            &wire.messages[1],
            Message::User { content, .. } if content == "first"
        ));
        assert!(matches!(
            &wire.messages[2],
            Message::ToolResult {
                tool_call_id,
                content,
            } if tool_call_id == "call_1" && content == "tool output"
        ));
        assert!(matches!(
            &wire.messages[3],
            Message::User { content, .. } if content == "after tool"
        ));
        assert!(matches!(
            &wire.messages[4],
            Message::Assistant { content, .. } if content == "done"
        ));
    }

    /// A mixed user message emits tool results first, then merged user text.
    ///
    /// DeepSeek requires `tool` messages to immediately follow the assistant
    /// message that contained `tool_calls`; user text cannot sit in between.
    #[test]
    fn user_message_emits_tool_results_before_merged_text() {
        let domain = dm::Message::User {
            content: NonEmptyVec::from_first_rest(
                dm::UserContent::text("a"),
                vec![
                    dm::UserContent::ToolResult(dm::ToolResult {
                        id: "call_1".to_string(),
                        call_id: None,
                        content: NonEmptyVec::new(dm::ToolResultContent::text("r")),
                    }),
                    dm::UserContent::text("b"),
                ],
            ),
        };

        let wire = Vec::<Message>::from(domain);

        assert_eq!(wire.len(), 2, "one tool message plus one merged user text");
        match &wire[0] {
            Message::ToolResult {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id.as_str(), "call_1");
                assert_eq!(content.as_str(), "r");
            }
            other => panic!("[0] expected ToolResult, got {other:?}"),
        }
        match &wire[1] {
            // Text blocks are merged in their original order, even when a tool
            // result sits between them in the domain message.
            Message::User { content, .. } => assert_eq!(content.as_str(), "a\nb"),
            other => panic!("[1] expected User, got {other:?}"),
        }
    }

    /// Multi-block tool results are fully joined, not truncated to the first block.
    #[test]
    fn tool_result_joins_all_content_blocks() {
        let domain = dm::ToolResult {
            id: "call_1".to_string(),
            call_id: None,
            content: NonEmptyVec::from_first_rest(
                dm::ToolResultContent::text("line1"),
                vec![dm::ToolResultContent::text("line2")],
            ),
        };

        match Message::from(domain) {
            Message::ToolResult {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id.as_str(), "call_1");
                assert_eq!(content.as_str(), "line1\nline2");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }
    }

    /// Assistant reasoning is replayed through `reasoning_content`.
    #[test]
    fn assistant_reasoning_is_sent_back() {
        let domain = dm::Message::Assistant {
            id: None,
            content: NonEmptyVec::from_first_rest(
                dm::AssistantContent::text("answer"),
                vec![dm::AssistantContent::reasoning("my thoughts")],
            ),
        };

        let wire = Vec::<Message>::from(domain);

        assert_eq!(wire.len(), 1);
        match &wire[0] {
            Message::Assistant {
                content,
                reasoning_content,
                ..
            } => {
                assert_eq!(content.as_str(), "answer");
                assert_eq!(reasoning_content.as_deref(), Some("my thoughts"));
            }
            other => panic!("expected Assistant, got {other:?}"),
        }
    }
}
