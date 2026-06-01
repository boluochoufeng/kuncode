# kuncode-core 设计说明

`kuncode-core` 是 kuncode 的领域模型与 provider 抽象层。它先定义一套 **provider 无关**的对话 / 请求 / 响应类型和 `CompletionModel` trait，再由具体 provider（当前是 DeepSeek）把这些类型映射到该 provider 的 HTTP JSON。

本文讲分层、类型关系、wire 映射和关键设计决策的「为什么」；逐字段细节看代码里的 doc 注释（已按 [AGENTS.md](../AGENTS.md) 规范写齐）。

## 设计渊源：rig-core

整个 completion 层的形状**参考 `rig-core` 的 `completion` / `message` / `request` 分层**，核心理念有三条贯穿全文：

1. **领域类型与 wire DTO 分离**：core 只放 provider 无关的「模型对话长什么样」；具体 provider 的 JSON 形状是另一套类型，靠 `From` / `TryFrom` 桥接。
2. **响应表达「模型返回了什么」，而非「为什么停」**：`CompletionResponse` 给的是一组 assistant content，不强行抽象各家差异巨大的 stop reason。
3. **content block 模型**：一条消息可以含多个内容块（文本 / 工具调用 / 工具结果 / reasoning），用统一的 enum 表达，provider 层负责拍平到各自的 wire 形状。

DeepSeek provider 的协议类型（`providers::deepseek::protocol`）同样参考了 rig-core 的 DeepSeek provider 建模方式。

## 模块布局

```text
crates/kuncode-core/src/
  lib.rs
  non_empty_vec.rs        # NonEmptyVec<T>：静态保证非空
  json_utils.rs           # merge / stringified_json / null_or_vec
  completion.rs           # 抽象层入口 + CompletionError
  completion/
    message.rs            # 对话消息与 content block
    request.rs            # 请求/响应/CompletionModel trait/Builder
    streaming.rs          # 流式事件类型
  providers.rs            # provider 模块入口
  providers/
    deepseek.rs           # DeepSeekClient / DeepSeekCompletionModel
    deepseek/
      protocol.rs         # DeepSeek wire DTO 与双向映射
```

`lib.rs` 公开 `completion` / `json_utils` / `non_empty_vec` / `providers` 四个模块。`completion.rs` 把常用类型 re-export 到 `completion::` 一层（如 `kuncode_core::completion::{Message, CompletionRequest, StreamEvent}`），避免深路径。

## 依赖方向

workspace 内是 `cli → agent → core` 单向依赖。

provider 实现放在 `kuncode-core` 内（`providers::deepseek`），而非单独 crate，因此 `kuncode-core` **直接依赖 `reqwest`**，`CompletionError` 里也有 `HttpError(#[from] reqwest::Error)`。这是有意取舍：provider 与领域类型同 crate、改起来更顺；代价是 core 的公开 API 名义上耦合了 reqwest（pre-1.0）。若将来要把 core 当稳定库对外发布，再评估是否引入 `HttpClient` trait 把传输层抽出去——当前单 app、单 provider，不值得提前抽象。

## completion 抽象层

### message.rs —— 对话与 content block

`Message` 是 provider 无关的一轮对话，按 `role` 标签序列化：

```rust
pub enum Message {
    System { content: String },
    User { content: NonEmptyVec<UserContent> },
    Assistant { id: Option<String>, content: NonEmptyVec<AssistantContent> },
}
```

- user / assistant 的 content 是 **`NonEmptyVec`**——一条消息至少一个 content block，用类型挡掉空消息。
- `UserContent = Text | ToolResult`：工具结果走 user 角色里的 `ToolResult`（rig 风格），不单独做 `Message::Tool` 变体；provider 层再映射成 wire 上的 `role: "tool"`。
- `AssistantContent = Text | ToolCall | Reasoning`，`#[serde(untagged)]`——多数 provider 不返回 `type` 判别字段，靠形状区分。
- `ToolChoice = Auto | None | Required | Specific { function_name }`，默认 `Auto`。
- `ToolCall { id, call_id, function, signature, additional_params }`：`id` 是主关联 id，`call_id` 是次级 provider id（给 OpenAI Responses 那种同时有两者的 API）。`ToolResult.id` 对应「被回应的 tool_call id」。
- `Reasoning` / `ReasoningContent` 为思考型模型预留，支持 `Text(带签名) | Encrypted | Redacted | Summary`，覆盖各 provider 的 reasoning 回传约定。

### request.rs —— 请求、响应、模型 trait

```rust
pub struct CompletionRequest {
    pub model: Option<String>,            // None → 用模型自身的 id
    pub chat_history: NonEmptyVec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<u64>,
    pub stop: Option<Vec<String>>,
    pub reasoning: Option<ReasoningEffort>,
    pub tool_choice: Option<ToolChoice>,
    pub additional_params: Option<serde_json::Value>,  // provider 专有参数，merge 进出站 body
    pub output_schema: Option<serde_json::Value>,
}
```

由 `CompletionRequestBuilder` 构造：`new(prompt)` 接住「最后一条消息」，`build()` 把它追加到 history 末尾，从而**保证 `chat_history` 永不为空**，与 builder 方法调用顺序无关。

`ReasoningEffort` 是中立档位 `Off / Minimal / Low / Medium / High / Xhigh`，词汇对齐 OpenAI 的粒度；各 provider 把它近似到自己的原生标度。`None` = 用模型默认，`Off` = 显式关思考。

```rust
pub struct CompletionResponse<T> {
    pub choice: NonEmptyVec<AssistantContent>,  // 模型这轮返回的内容
    pub usage: Usage,
    pub raw_response: T,                          // provider 原始响应，逃生口
    pub message_id: Option<String>,
}
```

`Usage` 统计六类 token（input/output/total/cached_input/cache_creation_input/reasoning），实现 `Add`/`AddAssign`，便于跨多轮或流式分片累加。

`CompletionModel` 是每个 provider 要实现的 trait，用 `-> impl Future` 而非 `async_trait`（静态分发、无 boxed future、无 trait object 要求）：

```rust
pub trait CompletionModel: Clone + Send + Sync {
    type Response: Send + Sync + Serialize + DeserializeOwned;
    type Client;
    fn make(client: &Self::Client, model: impl Into<String>) -> Self;
    fn completion(&self, request: CompletionRequest)
        -> impl Future<Output = Result<CompletionResponse<Self::Response>, CompletionError>> + Send;
    fn stream(&self, request: CompletionRequest)
        -> impl Future<Output = Result<CompletionStream, CompletionError>> + Send;
}
```

`ToolDefinition { name, description, parameters }` 是函数式工具（本地工具）；`ProviderToolDefinition { kind, config }` 是 provider 内置工具（如 web_search），形状由 provider 定。

### streaming.rs —— 流式事件

非流式一次返回整段；流式按 token 增量 emit 事件，coding agent 需要它实时渲染：

```rust
pub enum StreamEvent {
    TextDelta(String),         // 可见回答增量
    ReasoningDelta(String),    // 思考增量，单独通道渲染
    ToolCallStart { index, id, name },   // 工具调用一开始就预告
    Completed { content: NonEmptyVec<AssistantContent>, usage: Usage, finish_reason: FinishReason },
}
pub type CompletionStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, CompletionError>> + Send>>;
```

`Completed` 是成功流的终止事件，携带**已拼装好**的完整内容（等价于非流式返回）。`FinishReason = Stop | Length | ToolCalls | ContentFilter | Other(String)`。取消即 drop 流。

> 流式类型已定义，但 provider 的 `stream()` 尚未实现（`todo!()`）。

### CompletionError

```rust
pub enum CompletionError {
    JsonError(#[from] serde_json::Error),   // (反)序列化失败
    ResponseError(String),
    RequestError(String),
    HttpError(#[from] reqwest::Error),      // 传输/读流失败
    ApiError { status: u16, message: String },  // 非 2xx，带原始响应体
}
```

排查约定：`HttpError` = 网络/读 body 失败；`JsonError` = body 拿到但 schema 不匹配；`ApiError` = 服务端非 2xx。三者区分清楚，定位时先看类别。`status` 暂用 `u16`（是否升级成 `http::StatusCode` 留待需要重试/限流逻辑时再定）。

## 支撑类型

- **`NonEmptyVec<T: Clone>`**：内部 `Vec`，构造时校验非空；`Deref` 到 `[T]`、有 `iter/first/push/into_vec`；自定义 `Serialize`/`Deserialize`（反序列化拒绝空序列，把不变量挡在数据边界）。用于 `chat_history`、`choice`、各 content 列表。
- **`json_utils`**：
  - `merge(a, b)`：浅合并 JSON object，`b` 同名键覆盖 `a`；把 `additional_params` 叠加进出站 body。
  - `stringified_json`：把 `Value` 序列化成**字符串**（DeepSeek 的 `function.arguments` 是 stringified JSON）。
  - `null_or_vec`：反序列化一个 provider 可能发成 `null` 的 `Vec<T>`，`null` 归一成空 vec；配合字段上的 `#[serde(default)]`，同时覆盖「键缺失」与「值为 null」。

## DeepSeek provider

### DeepSeekClient

持有 `reqwest::Client` / `api_key` / `base_url`。`new` 构造时设了 `timeout(360s)` + `connect_timeout(10s)`（避免请求永久挂起）；`from_env` 读 `DEEPSEEK_API_KEY`；`post(path)` 返回一个已带好 base URL + `Bearer` 鉴权的 `RequestBuilder`，body 与响应消费方式交给调用方，`completion` 与 `stream` 共用。

### DeepSeekCompletionModel —— completion 流程

`completion()` 已真实验证通过（含工具调用往返）：

1. `model` 兜底：`request.model` 为 `None` 时回退到模型自身 id。
2. 取出 `additional_params`（`try_from` 会消费 request）。
3. `CompletionRequest` → `DeepSeekCompletionRequest`（`TryFrom`）→ 序列化成 `Value` → `merge` 叠加 `additional_params`（用户键覆盖）。
4. `post("/chat/completions").json(&body).send()`。
5. 非 2xx → 读 body 文本 → `ApiError { status, message }`。
6. 2xx → 读 bytes → `serde_json::from_slice`（解析失败归 `JsonError`）→ `CompletionResponse::try_from`。

`stream()` 尚未实现。

### protocol.rs —— wire DTO 与映射

DeepSeek / OpenAI-compatible 的线上结构与 core 类型之间的双向映射：

- **出站**：`From<message::Message> for Vec<Message>`（单条领域消息可拆成多条 wire 消息——user 里夹带的 tool 结果拆出来；assistant 多 content-block 合并成单条，**reasoning 拼回 `reasoning_content` 并回传**——见下方 reasoning 回传规则）、`From<completion::ToolDefinition>`、`TryFrom<CompletionRequest> for DeepSeekCompletionRequest`。
- **入站**：`TryFrom<DeepSeekCompletionResponse>`（取 `choices[0]`，把 content / tool_calls / reasoning_content 还原成 `AssistantContent`）。
- `tool_calls` 字段用 `default + deserialize_with = "null_or_vec" + skip_serializing_if`，容忍「缺失 / null / 数组」三态。
- `function.arguments` 用 `stringified_json` 解回 `Value`。

**reasoning 档位的拆分**是关键映射：core 的中立 `ReasoningEffort` 一拆为二 → DeepSeek 的 `thinking`（开关）+ `reasoning_effort`（强度，只有 `high/max`）。`Minimal/Low/Medium/High` 塌缩到 `high`，`Xhigh → max`，`Off → thinking disabled`。开启思考时主动清掉 `temperature/top_p`（DeepSeek 思考模式忽略它们），与将来 OpenAI/Anthropic「开思考即清采样参数」保持同一范式。

**reasoning_content 必须回传**（v4 thinking mode，与 deepseek-reasoner 相反）：本 provider 只面向 **deepseek-v4**。按其 thinking mode 文档，**工具调用的后续轮必须把上一轮的 `reasoning_content` 原样回传，否则 API 返回 400**；非工具调用轮回传则被忽略。因此出站映射**无条件保留** reasoning（拼回 `reasoning_content`）。注意这与 `deepseek-reasoner`（禁止输入带 `reasoning_content`）规则相反——本 provider 不支持 reasoner，故按 v4 规则走。

`DeepSeekCompletionRequest` 刻意**不是** DeepSeek 全量契约：只建模「从 core 一等字段映射来的」和「由 completion/stream 决定的控制字段」；其余少用的 provider 专有参数靠 `additional_params` 在序列化后 merge 进去。

## 关键设计决策（「为什么」）

1. **provider 在 core 内、core 依赖 reqwest**：换 provider 与领域类型同 crate 的便利，接受名义上的 reqwest 耦合。
2. **不在 core response 加 `FinishReason`**：agent loop 的主分支（执行工具 vs 结束）靠**检查 `choice` 里有没有 `AssistantContent::ToolCall`**判断，不需要 finish_reason；`length` 截断这种边角从 `raw_response` 兜底。强行抽象各 provider 的停止原因会丢细节、最后多半落到 `Other(String)`。（**流式** `Completed` 仍带 `FinishReason`，因为流式拼装本就需要它。）
3. **`message_id` 与 DeepSeek `id` 不是一回事**：`message_id` 表示**消息级** id（OpenAI Responses 的 `msg_`/`rs_`，用于多轮 reasoning 项配对）；DeepSeek 顶层 `id` 是 **completion 调用 id**。DeepSeek 的 Chat Completions 没有消息级 id，故 `message_id` 保持 `None`，调用 id 需要时从 `raw_response.id` 取。
4. **`additional_params` 在序列化后 merge**：保证用户能覆盖任何映射出的字段，也让 DTO 不必穷举 provider 全量参数。
5. **`NonEmptyVec` 守不变量**：`chat_history`、`choice`、各 content 列表非空由类型保证，省掉运行时校验。
6. **`null_or_vec` + `default` 双保险**：response/stream 里 `tool_calls` 的「缺失/null」都归一成空 vec，下游永远拿到可直接遍历的 `Vec`。
7. **HTTP 超时显式设置**：`reqwest::Client::new()` 无默认超时会导致请求永久挂起，故 `new` 里显式配 timeout。

## 当前状态与下一步

已完成且验证：
- completion 抽象层（message / request / streaming 类型齐全）。
- DeepSeek `completion()` —— 真实 API 端到端通过，含**工具调用往返**（tool_call 解析、`role=tool` 回传、多轮拼接、reasoning 往返）。
- 测试在 `providers::deepseek::tests`，`#[ignore]` 防误触真实计费，靠 `.env`(dotenvy) 读 key。

待做（按依赖顺序）：
1. **DeepSeek `stream()`**：新增 SSE chunk 协议类型（delta 形态）+ 分片累积成 `StreamEvent`。
2. **`kuncode-agent`**：`Tool` trait + 注册表 + 工具调用循环（completion → 检测 ToolCall → 执行 → 回灌 → 收敛）。可先建在非流式 `completion()` 上。
3. **`kuncode-cli`**：参数/配置 → agent。

### 实现备注

- `From<message::Message>`：一条 user 消息混含文本与工具结果时，出站把 **tool 结果排在前**（wire 要求 tool 消息紧跟带 `tool_calls` 的 assistant 之后，中间不能插 user 文本，否则 400），user 文本合并后随其后。标准构造器不会把两者塞进同一条消息，故这只影响非标准混合输入。
- `From<message::ToolResult>` 会拼接**全部**内容块，不丢块。
- `Usage.reasoning_tokens` 从 `completion_tokens_details.reasoning_tokens` 映射（实测可得）；`wire ToolCall.index` 是用不到的残留字段（出站固定 0、入站不读），无需处理。

## 暂不做

streaming 之外的 embeddings / RAG / vector store / memory / MCP / subagent / skill / context compact / worktree / multi-agent —— 都不在当前 completion + agent loop 雏形的核心路径上，后续按需加。
