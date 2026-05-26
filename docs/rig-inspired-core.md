# Kuncode Core 当前设计说明

这份文档记录当前 `kuncode-core` 的实现形状。当前实现基本参考 `rig-core` 的 completion/message/request 分层：`core` 先定义 provider 无关的数据结构和模型 trait，后续 DeepSeek provider 再把这些结构映射到 DeepSeek/OpenAI-compatible HTTP JSON。

当前范围只覆盖 `kuncode-core`。`kuncode-agent`、`kuncode-deepseek`、`kuncode-tools` 还没有正式接入。

## 当前模块

```text
crates/kuncode-core/src/
  lib.rs
  completion.rs
  completion/
    message.rs
    request.rs
```

模块关系：

```text
lib.rs
  -> completion
       -> message
       -> request
```

`lib.rs` 当前只公开：

```rust
pub mod completion;
```

也就是说，外部使用时路径是：

```rust
kuncode_core::completion::message::Message
kuncode_core::completion::request::CompletionRequest
```

后续如果 API 稳定，可以在 `completion.rs` 或 `lib.rs` 做更短的 re-export。

## completion.rs

`completion.rs` 是 completion 子模块入口。

当前职责：

- 声明 `message` 模块
- 声明 `request` 模块
- 定义 `CompletionError`

当前结构：

```rust
pub mod message;
pub mod request;

pub enum CompletionError {
    JsonError(serde_json::Error),
}
```

`CompletionError` 目前只有 JSON 错误，足够支撑早期 serde/mapping 实验。后续接 DeepSeek HTTP client 时，需要扩展：

```rust
Provider(String)
InvalidRequest(String)
InvalidResponse(String)
```

不要在 `kuncode-core` 里直接依赖 `reqwest::Error`。HTTP 错误应该在 provider crate 中转换成 core error。

## completion/message.rs

`message.rs` 定义模型对话历史和 tool-use content block。

当前核心类型：

```rust
pub enum Message {
    System { content: String },
    User { content: Vec<UserContent> },
    Assistant {
        id: Option<String>,
        content: Vec<AssistantContent>,
    },
}
```

serde 表示：

```rust
#[serde(tag = "role", rename_all = "lowercase")]
```

因此序列化后会带 `role` 字段：

```json
{ "role": "system", "content": "..." }
```

或者：

```json
{
  "role": "assistant",
  "id": null,
  "content": [...]
}
```

## Message 变体

### `Message::System`

表示系统提示词。

当前构造函数：

```rust
Message::system("You are a coding agent")
```

系统提示词应该作为 history 中的第一条 message，而不是单独维护 `preamble` 字段。

### `Message::User`

表示用户输入，也承载 tool result。

当前结构：

```rust
User {
    content: Vec<UserContent>,
}
```

`Vec<UserContent>` 代表一条 user message 可以包含多个 content block。当前没有强制非空，因此调用方需要避免构造空 `content`。

当前构造函数：

```rust
Message::user("hello")
Message::tool_result("result_id", "file content")
```

注意：当前实现参考 Rig 风格，tool result 放在 `UserContent::ToolResult` 中，而不是单独做 `Message::Tool` 变体。后续映射到 DeepSeek/OpenAI-compatible API 时，provider 层可以把它转换成 `role: "tool"` 的 API message。

### `Message::Assistant`

表示模型输出。

当前结构：

```rust
Assistant {
    id: Option<String>,
    content: Vec<AssistantContent>,
}
```

`id` 用来承载 provider 返回的 assistant message id。不是所有 provider 都有这个字段，所以是 `Option<String>`。

当前构造函数：

```rust
Message::assistant("done")
```

如果模型请求工具调用，应该通过 `AssistantContent::ToolCall` 放入 `content`。

## UserContent

当前结构：

```rust
pub enum UserContent {
    Text(Text),
    ToolResult(ToolResult),
}
```

serde 表示：

```rust
#[serde(tag = "type", rename_all = "lowercase")]
```

### `UserContent::Text`

普通用户文本。

构造函数：

```rust
UserContent::text("fix this bug")
```

### `UserContent::ToolResult`

工具执行结果。

当前结构：

```rust
pub struct ToolResult {
    pub id: String,
    pub call_id: Option<String>,
    pub content: Vec<ToolResultContent>,
}
```

字段含义：

- `id`：tool result 自己的 id。
- `call_id`：可选的 tool call id，用于关联之前 assistant 发出的工具调用。
- `content`：工具结果内容块。

`call_id` 使用：

```rust
#[serde(skip_serializing_if = "Option::is_none")]
```

没有值时不会序列化。

当前 `ToolResultContent`：

```rust
pub enum ToolResultContent {
    Text(Text),
}
```

也就是说第一版只支持文本工具结果。

## ToolChoice

当前结构：

```rust
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Specific { function_names: Vec<String> },
}
```

含义：

- `Auto`：模型自行决定是否调用工具。
- `None`：禁止工具调用。
- `Required`：要求模型必须调用工具。
- `Specific`：限制模型只能调用指定函数名。

默认值是 `Auto`。

这个类型目前放在 `message.rs` 中，因为它和 tool-use message 结构一起复制自 Rig 风格。后续如果 completion request 继续增长，可以考虑移动到 `request.rs` 或单独 `tool.rs`。

## AssistantContent

当前结构：

```rust
pub enum AssistantContent {
    Text(Text),
    ToolCall(ToolCall),
    Reasoning(Reasoning),
}
```

serde 表示：

```rust
#[serde(untagged)]
```

这表示序列化时不会额外加 `type` 字段，而是按内部结构自然展开。这样更接近 Rig 的通用 content block 思路，但 provider 映射时要注意不同 content block 的 JSON 形状不能冲突。

### `AssistantContent::Text`

普通 assistant 文本。

构造函数：

```rust
AssistantContent::text("done")
```

### `AssistantContent::ToolCall`

模型请求执行工具。

当前构造函数：

```rust
AssistantContent::tool_call(
    "tool_call_id",
    "read_file",
    serde_json::json!({ "path": "src/main.rs" }),
)
```

或者：

```rust
AssistantContent::tool_call_with_call_id(
    "id",
    "provider_call_id",
    "read_file",
    serde_json::json!({ "path": "src/main.rs" }),
)
```

### `AssistantContent::Reasoning`

模型 reasoning 内容。

当前构造函数：

```rust
AssistantContent::reasoning("I need to inspect the file first")
```

DeepSeek thinking mode 或其它 provider 的 reasoning/thinking 内容，可以先映射到这里。具体是否暴露给 agent loop，需要后续 provider 层决定。

## Text

当前结构：

```rust
pub struct Text(String);
```

它是文本 newtype，而不是直接到处使用 `String`。

作用：

- 保持 content block 的结构一致。
- 方便后续给文本块增加 metadata。
- 提供 `text()` 访问内部字符串。

当前支持：

```rust
impl<T> From<T> for Text
where
    T: Into<String>,
```

所以可以从 `&str` 或 `String` 直接转换。

## ToolCall

当前结构：

```rust
pub struct ToolCall {
    pub id: String,
    pub call_id: Option<String>,
    pub function: ToolFunction,
    pub signature: Option<String>,
    pub addtional_params: Option<serde_json::Value>,
}
```

字段含义：

- `id`：Kuncode 内部或 provider 返回的 tool call id。
- `call_id`：可选 provider call id，用于兼容某些 API 的调用关联字段。
- `function`：工具函数名和参数。
- `signature`：可选签名字段，为 reasoning/tool-use 校验类 provider 预留。
- `addtional_params`：额外 provider 参数。当前字段名是实现现状；如果 API 尚未稳定，建议后续改成 `additional_params`。

当前构造：

```rust
ToolCall::new(id, ToolFunction { name, arguments })
ToolCall::new(...).with_call_id(call_id)
```

## ToolFunction

当前结构：

```rust
pub struct ToolFunction {
    pub name: String,
    pub arguments: serde_json::Value,
}
```

含义：

- `name`：工具名，例如 `read_file`。
- `arguments`：工具参数 JSON。

`arguments` 用 `serde_json::Value`，是为了让 core 不依赖具体工具参数类型。真正执行工具时，`kuncode-tools` 再根据工具名反序列化成具体参数结构。

## Reasoning

当前结构：

```rust
pub struct Reasoning {
    pub id: Option<String>,
    pub content: Vec<ReasoningContent>,
}
```

支持的 content：

```rust
pub enum ReasoningContent {
    Text {
        text: String,
        signature: Option<String>,
    },
    Encrypted(String),
    Redacted { data: String },
    Summary(String),
}
```

含义：

- `Text`：普通 reasoning 文本，可带签名。
- `Encrypted`：加密 reasoning 内容。
- `Redacted`：被 provider 脱敏的 reasoning。
- `Summary`：reasoning 摘要。

当前构造函数：

```rust
Reasoning::new("...")
Reasoning::new_with_signature("...", Some(signature))
Reasoning::redacted(data)
Reasoning::encrypted(data)
Reasoning::summaries(vec![...])
Reasoning::multi(vec![...])
```

这部分明显是为 reasoning/thinking provider 预留的。DeepSeek thinking mode 接入时，provider 层应该把 API 中的 reasoning 字段映射到 `Reasoning`。

## completion/request.rs

`request.rs` 定义模型 trait、completion request、completion response 和工具描述。

## CompletionModel

当前 trait：

```rust
pub trait CompletionModel: Clone + Send + Sync {
    type Response: Send + Sync + Serialize + DeserializeOwned;
    type Client;

    fn make(client: &Self::Client, model: impl Into<String>) -> Self;

    fn completion(
        &self,
        request: CompletionRequest,
    ) -> impl Future<Output = Result<CompletionResponse<Self::Response>, CompletionError>>;
}
```

含义：

- `CompletionModel`：一个可以执行 completion 请求的模型。
- `Response`：provider 原始响应类型。
- `Client`：provider client 类型，例如未来的 `DeepSeekClient`。
- `make`：从 client 和 model name 构造具体模型对象。
- `completion`：执行一次非流式 completion 请求。

这里使用 `fn -> impl Future`，而不是 `async_trait`。好处是：

- 静态分发。
- 不需要 boxed future。
- 不要求 trait object。
- 更接近 Rig-style 核心 trait 设计。

如果以后需要 `Box<dyn CompletionModel>` 或运行时动态切换 provider，再考虑 `async_trait` 或 object-safe wrapper。

## CompletionRequest

当前结构：

```rust
pub struct CompletionRequest {
    pub model: Option<String>,
    pub chat_history: Vec<Message>,
    pub tools: Vec<ToolDescriptor>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub output_schema: Option<serde_json::Value>,
}
```

字段含义：

- `model`：可选模型覆盖。如果为 `None`，使用 `CompletionModel` 自身的默认模型。
- `chat_history`：完整历史消息，包括 system/user/assistant/tool result content。
- `tools`：本次请求提供给模型的工具列表。
- `temperature`：采样温度。
- `max_tokens`：最大输出 token 数。
- `tool_choice`：工具调用策略。
- `output_schema`：结构化输出 schema 预留。

当前没有 `stream` 字段，说明第一版 completion 只考虑非流式。

## CompletionResponse

当前结构：

```rust
pub struct CompletionResponse<T> {
    pub raw_response: T,
}
```

这说明当前 response 还没有标准化 assistant content 和 usage，只保留 provider 原始响应。

后续实现 agent loop 前，建议补充：

```rust
pub struct CompletionResponse<T> {
    pub choice: OneOrMany<AssistantContent>,
    pub usage: Usage,
    pub raw_response: T,
    pub message_id: Option<String>,
}
```

这和 Rig core 的方向一致：core response 表达“模型返回了什么”，也就是一组 assistant content，而不是表达“provider 为什么停止”。如果 provider 返回了 stop reason、length、content filter 等信息，先保留在 `raw_response` 中。

因此当前不建议在 core 层添加统一的 `FinishReason`。原因是不同 provider 的停止原因语义差异很大，强行抽象会丢细节，最后通常只能落到 `Other(String)`。对 agent loop 来说，第一优先级是检查 `choice` 里有没有 `AssistantContent::ToolCall`：

```rust
let has_tool_call = response
    .choice
    .iter()
    .any(|content| matches!(content, AssistantContent::ToolCall(_)));
```

如果有 tool call，就执行工具并继续下一轮；如果没有，就把文本内容作为最终输出。长度耗尽、内容过滤等 provider-specific 情况，后续可以从 `raw_response` 或 provider 层错误中处理。

## ToolDescriptor

当前结构：

```rust
pub struct ToolDescriptor {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}
```

含义：

- `name`：工具名。
- `description`：给模型看的工具描述。
- `parameters`：JSON schema，用于描述工具参数。

这是 provider 无关的工具定义。

DeepSeek/OpenAI-compatible provider 后续可以把它映射成：

```json
{
  "type": "function",
  "function": {
    "name": "...",
    "description": "...",
    "parameters": { ... }
  }
}
```

## ProviderToolDescriptor

当前结构：

```rust
pub struct ProviderToolDescriptor {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten, default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub config: serde_json::Map<String, serde_json::Value>,
}
```

它用于描述 provider-specific tool 配置。

例子：

```json
{
  "type": "web_search",
  "max_results": 5
}
```

与 `ToolDescriptor` 的区别：

- `ToolDescriptor` 是函数式工具描述，适合本地工具。
- `ProviderToolDescriptor` 是 provider 内置工具或特殊工具配置，形状由 provider 决定。

## 当前实现的交互方式

当前 core 层的预期使用方式：

```text
agent 构造 CompletionRequest
  |
  v
CompletionModel::completion(request)
  |
  v
provider model 实现
  |
  v
返回 CompletionResponse<ProviderRawResponse>
```

未来 DeepSeek provider 的职责：

```text
CompletionRequest
  -> DeepSeek/OpenAI-compatible request DTO
  -> HTTP request
  -> DeepSeek/OpenAI-compatible response DTO
  -> CompletionResponse<RawResponse>
```

未来 agent loop 的职责：

```text
1. 维护 chat_history: Vec<Message>
2. 调用 CompletionModel::completion
3. 从 CompletionResponse.choice 中取 assistant content
4. 如果 choice 中有 AssistantContent::ToolCall，调用 tools executor
5. 把 ToolResult 追加回 chat_history
6. 继续下一轮
```

不过当前 `CompletionResponse` 只有 `raw_response`，所以在进入 agent loop 前，需要先补上 `choice` 和 `usage` 这类 Rig-style 标准字段。

## 与 DeepSeek/OpenAI-compatible API 的映射建议

当前 core 类型不是 DeepSeek API DTO。映射应该放在未来的 `kuncode-deepseek` crate 中。

建议模块：

```text
crates/kuncode-deepseek/src/
  client.rs
  model.rs
  protocol.rs
```

`protocol.rs` 负责：

- `Message -> API message`
- `ToolDescriptor -> API function tool`
- `ProviderToolDescriptor -> API provider tool`
- API assistant tool call -> `AssistantContent::ToolCall`
- API reasoning content -> `AssistantContent::Reasoning`
- API tool result shape -> `UserContent::ToolResult`

不要让 `kuncode-agent` 直接处理 `choices`、`tool_calls[].function.arguments` 这类 API 字段。

## 后续待补

为了让这套 core 能支撑 Coding Agent，后续至少需要补：

1. `CompletionResponse` 标准化字段：`choice`、`usage`、`message_id`、`raw_response`。
2. `OneOrMany` 或 `NonEmptyVec`，用于保证 `choice` 和 message content 非空。
3. 更完整的 `CompletionError`。
4. `ToolDescriptor` 和 `ProviderToolDescriptor` 在 `CompletionRequest` 中的关系，目前 request 只有 `tools: Vec<ToolDescriptor>`。
5. `Text` 的公开构造方式，如果希望外部直接创建文本块。
6. `Vec<UserContent>` / `Vec<AssistantContent>` 是否要改成非空集合。
7. `ToolCall.addtional_params` 字段名是否修正。
8. DeepSeek provider DTO 和 mapping。
9. agent loop。
10. local tools executor。

暂时不需要补 core-level `FinishReason`。如果以后 Kuncode 的 agent loop 明确需要处理长度耗尽或内容过滤，再添加 Kuncode 自己的停止原因抽象；不要为了模仿 OpenAI/DeepSeek 的字段提前加到 core。

## 暂时不做

当前阶段先不要实现：

- streaming
- embeddings
- RAG
- vector store
- memory
- MCP
- subagent
- skill loading
- context compact
- worktree isolation
- multi-agent team

这些功能可以后续继续参考 Rig 或 Claude Code 教程加，但不属于当前 `kuncode-core` completion 雏形的核心路径。
