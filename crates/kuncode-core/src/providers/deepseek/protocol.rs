use serde::{Deserialize, Serialize};

use crate::{
    completion::{self, AssistantContent, CompletionError, message},
    json_utils,
    non_empty_vec::NonEmptyVec,
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    User {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        prefix: Option<bool>,
        #[serde(
            default,
            deserialize_with = "json_utils::null_or_vec",
            skip_serializing_if = "Vec::is_empty"
        )]
        tool_calls: Vec<ToolCall>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
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
                    tool_calls: tool_calls,
                    reasoning_content: reasoning,
                }]
            }
        }
    }
}

impl From<message::ToolResult> for Message {
    fn from(value: message::ToolResult) -> Self {
        let content = match value.content.first().clone() {
            message::ToolResultContent::Text(text) => text.text(),
        };

        Message::ToolResult {
            tool_call_id: value.id,
            content: content,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ToolCall {
    pub id: String,
    pub index: usize,
    pub r#type: ToolType,
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

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
    #[default]
    Function,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Function {
    pub name: String,
    // 和completion::message::ToolFunction的字段类型对齐
    #[serde(with = "json_utils::stringified_json")]
    pub arguments: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolDescriptor {
    pub r#type: String,
    pub function: completion::ToolDescriptor,
}

impl From<completion::ToolDescriptor> for ToolDescriptor {
    fn from(value: completion::ToolDescriptor) -> Self {
        Self {
            r#type: "function".to_string(),
            function: value,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Usage {
    pub completion_tokens: u32,
    pub prompt_tokens: u32,
    pub prompt_cache_hit_tokens: u32,
    pub prompt_cache_miss_tokens: u32,
    pub total_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens_details: Option<CompletionTokenDetails>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompletionTokenDetails {
    pub reasoning_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptTokensDetails {
    pub cached_tokens: u32,
}

impl Into<completion::Usage> for Usage {
    fn into(self) -> completion::Usage {
        completion::Usage {
            input_tokens: self.prompt_tokens as u64,
            output_tokens: self.completion_tokens as u64,
            total_tokens: self.total_tokens as u64,
            cached_input_tokens: self
                .prompt_tokens_details
                .map(|p| p.cached_tokens)
                .unwrap_or(0) as u64,
            cache_creation_input_tokens: 0,
            reasoning_tokens: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub finish_reason: String,
    pub index: usize,
    pub message: Message,
    pub logprobs: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeepSeekCompletionResponse {
    pub id: String,
    pub choices: Vec<Choice>,
    pub created: u64,
    pub model: String,
    pub system_fingerprint: String,
    pub object: String,
    pub usage: Usage,
}

impl TryFrom<DeepSeekCompletionResponse>
    for completion::CompletionResponse<DeepSeekCompletionResponse>
{
    type Error = completion::CompletionError;

    fn try_from(response: DeepSeekCompletionResponse) -> Result<Self, Self::Error> {
        // 当前DeepSeek 仅支持 n=1，也就是返回一个choice
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
                content.extend(tool_calls.into_iter().map(|tool_call| {
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
                err.to_string()
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

/// DeepSeek `/chat/completions` 的请求体。
///
/// **注意：本结构体不是 DeepSeek 线上格式的完整契约，这是有意取舍。** 它只建模
/// 两类字段：
///
/// - **映射组**：由 `TryFrom<CompletionRequest>` 从通用一等字段填入。其中
///   `thinking` + `reasoning_effort` 由中立的 `reasoning` 档位一拆为二而来。
/// - **控制组**：由调用的是 `completion()` 还是流式方法决定，不接受外部传入。
///
/// 其余 DeepSeek 专有且少用的参数（如 `logprobs`、`top_logprobs`、`user_id`）
/// **刻意不在此声明**；要用它们，由调用方塞进 `CompletionRequest::additional_params`，
/// 在 `completion()` 把本结构体序列化成 JSON 后用 `json_utils::merge` 叠加进去
/// （用户提供的同名键会覆盖这里的值）抵达 API。
///
/// 因此放弃了两点“struct 即 wire 契约”的好处：
/// - 仅看本 struct **无法**得知 DeepSeek 支持的全部参数；
/// - **无法**仅凭本类型对“发出去的精确 JSON”做强类型断言——真正的请求体是
///   序列化结果再 merge 之后的 `serde_json::Value`。
#[derive(Debug, Serialize, Deserialize)]
pub struct DeepSeekCompletionRequest {
    // —— 映射组：从 CompletionRequest 的一等字段而来 ——
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Stop>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<ToolChoice>,
    // thinking + reasoning_effort：由中立 reasoning 档位拆出（见 TryFrom）。
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<Thinking>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<ReasoningEffort>,
    // response_format：DeepSeek 仅支持 json_object，不支持传 schema；可考虑从 output_schema 派生。
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<ResponseFormat>,
    // —— 控制组：由调用方法（completion / stream）决定，不接受外部传入 ——
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
            .map(|msg| Vec::<Message>::from(msg))
            .collect::<Vec<Vec<Message>>>()
            .into_iter()
            .flatten()
            .collect();

        let tool_choice = req.tool_choice.map(ToolChoice::from);

        // 把reasoning 档位拆成 DeepSeek 的两个 wire 字段：
        // thinking 开关 + reasoning_effort 强度。DeepSeek 只有 high/max 两档，
        // minimal/low/medium/high 一律塌缩到 high，xhigh→max。
        use completion::ReasoningEffort as Eff;
        let (thinking, reasoning_effort) = match req.reasoning {
            None => (None, None),
            Some(Eff::Off) => (Some(Thinking::Disabled), None),
            Some(Eff::Minimal | Eff::Low | Eff::Medium | Eff::High) => {
                (Some(Thinking::Enabled), Some(ReasoningEffort::High))
            }
            Some(Eff::Xhigh) => (Some(Thinking::Enabled), Some(ReasoningEffort::Max)),
        };

        // 思考模式下 DeepSeek 会忽略 temperature/top_p（设了也不生效）。这里主动
        // 清掉它们，与将来 OpenAI/Anthropic（设了会直接报错）保持同一处理范式：
        // “开启思考即清采样参数”，差别只在别家不清会 400。
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
            messages: chat_history,
            max_tokens: req.max_tokens.map(|t| t as u32),
            temperature,
            top_p,
            // 空列表视作未设置，避免发出 "stop": []
            stop: req.stop.filter(|s| !s.is_empty()).map(Stop::Multi),
            tools: req.tools.into_iter().map(ToolDescriptor::from).collect(),
            tool_choice,
            stream: None,
            stream_options: None,
            thinking,
            reasoning_effort,
            response_format: None,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Thinking {
    Enabled,
    Disabled,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    High,
    Max,
}

#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    #[default]
    Text,
    JsonObject,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Stop {
    Single(String),
    Multi(Vec<String>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamOptions {
    include_usage: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    None,
    Auto,
    Required,
    #[serde(untagged)]
    Function(ToolChoiceFunction),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "function", rename_all = "lowercase")]
pub enum ToolChoiceFunction {
    Function { name: String },
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
