//! DeepSeek `/chat/completions` 的线上协议层（DeepSeek / OpenAI-compatible JSON）。
//!
//! 本模块的类型是 **wire DTO**，负责在 [`crate::completion`] 里 provider 无关的
//! 领域类型与 DeepSeek 的 HTTP JSON 之间双向映射。整体形状——消息按 `role` 标签、
//! tool-use 的建模方式、reasoning 字段、`function.arguments` 用 stringified JSON
//! 等——**参考 `rig-core` 的 DeepSeek provider 设计**。
//!
//! 映射方向：
//! - 出站：[`completion::CompletionRequest`] → [`DeepSeekCompletionRequest`]；
//!   [`message::Message`] → `Vec<Message>`。
//! - 入站：[`DeepSeekCompletionResponse`] → [`completion::CompletionResponse`]。

use serde::{Deserialize, Serialize};

use crate::{
    completion::{self, AssistantContent, CompletionError, message},
    json_utils,
    non_empty_vec::NonEmptyVec,
};

/// 一条 DeepSeek wire 消息，按 `role` 标签序列化（`system` / `user` / `assistant` / `tool`）。
///
/// 与领域侧 [`message::Message`] 的关键区别是**结构更扁**：`content` 是纯 `String`、
/// 工具调用平铺在 assistant 上、工具结果是独立的 `tool` 角色消息。领域侧那套
/// 「一条消息含多个 content block」的模型由 [`From<message::Message>`](Self) 在出站时
/// 拍扁成这套形状。
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    /// 系统提示。
    System {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// 用户输入。工具结果不走这里，而是独立的 [`Message::ToolResult`]。
    User {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },

    /// 模型输出：文本 + 可选的并行 `tool_calls` + 可选 `reasoning_content`。
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        // DeepSeek beta「续写」：让 assistant 从给定前缀继续生成。
        #[serde(skip_serializing_if = "Option::is_none")]
        prefix: Option<bool>,
        // DeepSeek 在无工具调用时可能省略该字段、也可能发成 null：
        // default 兜「键缺失」，null_or_vec 兜「值为 null」，二者都归一成空 vec。
        // skip_serializing_if 只管出站（不发空数组），与入站容错互不影响。
        #[serde(
            default,
            deserialize_with = "json_utils::null_or_vec",
            skip_serializing_if = "Vec::is_empty"
        )]
        tool_calls: Vec<ToolCall>,
        // 入站(解析响应)有值；出站也保留——v4 thinking mode 在工具调用的后续轮要求
        // 把 reasoning_content 原样回传，否则 400（非工具调用轮回传则被忽略）。
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
    },

    /// 工具执行结果。wire 上是 `role: "tool"`，靠 `tool_call_id` 关联发起它的调用。
    #[serde(rename = "tool")]
    ToolResult {
        tool_call_id: String,
        content: String,
    },
}

impl From<message::Message> for Vec<Message> {
    /// 把一条领域消息拍扁成若干条 wire 消息。
    ///
    /// 返回 `Vec` 是因为单条领域消息可能展开成多条：user 消息里夹带的工具结果会被
    /// 拆成独立的 `tool` 角色消息。assistant 则相反——多个 content block 合并成单条：
    /// 文本拼接、reasoning 拼接进 `reasoning_content`、tool_calls 平铺。领域侧 assistant
    /// 的 `id` 对 DeepSeek 无意义（它没有消息级 id），丢弃。
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
        // 拼接所有内容块（旧实现只取首块，多块时丢后续）。
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

/// assistant 发起的工具调用（wire 形状）。
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct ToolCall {
    pub id: String,
    // 并行调用里的序号。DeepSeek 响应/流式里带它；回传请求时并不被 schema 使用
    // （位置靠数组顺序表达），故出站固定填 0。入站解析也不读它。
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

/// 工具类型。DeepSeek 目前只有 `function` 一种。
#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Clone)]
#[serde(rename_all = "lowercase")]
pub enum ToolType {
    #[default]
    Function,
}

/// 被调用的函数名与参数。
#[derive(Debug, Serialize, Deserialize, PartialEq, Clone)]
pub struct Function {
    pub name: String,
    // arguments 在 wire 上是一段 **stringified JSON**（字符串里再套 JSON），用
    // stringified_json 在序列化/反序列化时转换；领域侧 ToolFunction.arguments 是
    // 直接的 serde_json::Value，两边字段类型对齐靠这层转换桥接。
    #[serde(with = "json_utils::stringified_json")]
    pub arguments: serde_json::Value,
}

/// 请求里提供给模型的函数式工具。
///
/// wire 上包成 `{ "type": "function", "function": { name, description, parameters } }`，
/// 内层直接复用领域 [`completion::ToolDescriptor`]。
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

/// DeepSeek 的 token 用量。
///
/// 除 OpenAI 通用字段外，带 DeepSeek 专有的 `prompt_cache_hit_tokens` /
/// `prompt_cache_miss_tokens`（缓存命中/未命中的输入 token）。
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

/// completion 用量细分；`reasoning_tokens` 为思考阶段消耗的 token。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CompletionTokenDetails {
    pub reasoning_tokens: u32,
}

/// prompt 用量细分；`cached_tokens` 为命中上下文缓存的输入 token。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptTokensDetails {
    pub cached_tokens: u32,
}

impl Into<completion::Usage> for Usage {
    /// 收敛成领域 [`completion::Usage`]。DeepSeek 无 Anthropic 那种 "cache creation"
    /// 概念，故 `cache_creation_input_tokens` 恒为 0。
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
            reasoning_tokens: self
                .completion_tokens_details
                .map(|d| d.reasoning_tokens)
                .unwrap_or(0) as u64,
        }
    }
}

/// 一个候选回复。DeepSeek 只返回一个（n=1）。
///
/// `finish_reason` 暂不映射到领域层——agent loop 靠 `choice` 里有没有 tool_call 来
/// 决定是否继续，截断等情况需要时从原始响应取（见 [`DeepSeekCompletionResponse`]）。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Choice {
    pub finish_reason: String,
    pub index: usize,
    pub message: Message,
    pub logprobs: Option<serde_json::Value>,
}

/// DeepSeek `/chat/completions` 的响应体。
///
/// `id` 是**这次 completion 调用**的 id（`chatcmpl-…` 那类），不是消息级 id，故
/// **不**映射到领域 [`completion::CompletionResponse::message_id`]（那个表示 OpenAI
/// Responses 风格的逐消息 id）；需要这个调用 id 时从 `raw_response` 取。
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

    /// 取唯一 choice（DeepSeek n=1），把 assistant 的 content / tool_calls /
    /// reasoning_content 还原成领域 [`AssistantContent`]。
    ///
    /// # Errors
    ///
    /// 响应没有 choice、choice 不是 assistant 消息，或还原后内容为空（被
    /// [`NonEmptyVec`] 拒绝）时返回 [`CompletionError::ResponseError`]。
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

/// 思考开关（wire 上是 `thinking.type`）。
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Thinking {
    Enabled,
    Disabled,
}

/// DeepSeek 的思考强度，只有两档；中立的 [`completion::ReasoningEffort`] 会塌缩到这里。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningEffort {
    High,
    Max,
}

/// 输出格式。DeepSeek 仅支持 `text` / `json_object`（不支持传 JSON Schema）。
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseFormat {
    #[default]
    Text,
    JsonObject,
}

/// 停止序列：单个字符串或一组字符串。`untagged` 让两种形态都能直接序列化。
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Stop {
    Single(String),
    Multi(Vec<String>),
}

/// 流式选项；`include_usage` 让流的最后一帧带上 usage 统计。
#[derive(Debug, Serialize, Deserialize)]
pub struct StreamOptions {
    include_usage: bool,
}

/// 工具调用策略。
///
/// `Function` 变体 `untagged` 成 `{ "type": "function", "function": { "name": ... } }`，
/// 其余三个简单变体序列化成字符串 `"none"` / `"auto"` / `"required"`。
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolChoice {
    None,
    Auto,
    Required,
    #[serde(untagged)]
    Function(ToolChoiceFunction),
}

/// 指定模型必须调用的具体函数。
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

#[cfg(test)]
mod tests {
    use super::Message;
    use crate::completion::message as dm;
    use crate::non_empty_vec::NonEmptyVec;

    /// 一条 user 消息混含 Text / ToolResult 时：tool 结果**排在前**（wire 要求 tool
    /// 消息紧跟带 tool_calls 的 assistant 之后，中间不能插 user 文本），user 文本合并
    /// 后随其后。
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

        assert_eq!(wire.len(), 2, "tool 一条 + 合并文本一条");
        match &wire[0] {
            Message::ToolResult {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id.as_str(), "call_1");
                assert_eq!(content.as_str(), "r");
            }
            other => panic!("[0] 期望 ToolResult，得到 {other:?}"),
        }
        match &wire[1] {
            // 文本块按出现次序合并（"a" 与 "b" 之间隔着 tool 结果，仍合并成一条）
            Message::User { content, .. } => assert_eq!(content.as_str(), "a\nb"),
            other => panic!("[1] 期望 User，得到 {other:?}"),
        }
    }

    /// ToolResult 含多个内容块时，出站应拼接**全部**块，而非只取首块。
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
            other => panic!("期望 ToolResult，得到 {other:?}"),
        }
    }

    /// assistant 的 reasoning 出站应拼回 `reasoning_content`（v4 工具轮要求回传）。
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
            other => panic!("期望 Assistant，得到 {other:?}"),
        }
    }
}
