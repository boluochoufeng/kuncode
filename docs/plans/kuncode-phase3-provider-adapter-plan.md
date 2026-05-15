# KunCode Phase 3 Provider Adapter 详细开发计划

## 1. 文档职责

本文档细化 MVP 开发计划中的 Phase 3：`Provider Adapter 与真实 Provider`。

它只回答 Phase 3 怎么落地：

1. `kuncode-provider` 的协议类型、trait、错误分类和 crate 边界。
2. `DeepSeekProvider` 的官方 DeepSeek Chat Completion API 请求/响应映射、重试、取消和 secret 处理。
3. `OpenAiCompatibleProvider` 的通用 OpenAI-compatible Chat Completions 请求/响应映射，用于只提供兼容接口的 provider。
4. 用本地 HTTP fixture 驱动真实 provider adapter 的确定性测试边界。
5. Phase 4 AgentLoop 集成前必须稳定下来的 provider-neutral contract。

Phase 3 入口：Phase 2 Tool Runtime 与基础工具完成。

## 2. Phase 3 目标

Phase 3 要交付一个 provider-neutral 调用层：

```text
ModelRequest
  -> ProviderAdapter
  -> provider-specific request
  -> provider response
  -> ModelResponse / ModelError
```

目标边界：

1. 后续 AgentLoop 只依赖 `ProviderAdapter`，不直接知道 DeepSeek 官方 wire format 或通用 OpenAI-compatible wire format。
2. DeepSeekProvider 能通过本地 HTTP fixture 覆盖文本回复、tool call、thinking metadata、429/5xx 重试和错误分类。
3. OpenAiCompatibleProvider 能通过本地 HTTP fixture 覆盖文本回复、tool call、429/5xx 重试和错误分类。
4. Phase 4 集成测试使用真实 provider adapter + wiremock fixture，不引入单独的模拟 provider。
5. provider request 默认不落盘；secret 不能进入 EventLog、Artifact、metadata 或普通 Debug 输出。
6. MVP 关闭 streaming；response 一次性返回。

## 3. 范围

Phase 3 包含：

1. `kuncode-provider`：`ProviderAdapter` trait、provider-neutral request/response/error 类型。
2. `ProviderCapabilities`：tool calling、context window、streaming、token usage 等能力标记。
3. `DeepSeekProvider`：一等 DeepSeek adapter，调用 DeepSeek 官方 Chat Completion API，保留 DeepSeek-specific 行为入口。
4. `OpenAiCompatibleProvider`：通用 OpenAI-compatible adapter，用于只提供兼容接口的 provider。
5. 可复用的 Chat Completions wire helper；它是内部实现细节，不取代 DeepSeekProvider / OpenAiCompatibleProvider 的 public 类型边界。
6. 重试策略：429 和 5xx 有限重试，其他错误按分类直接返回。
7. wiremock fixture 测试和可选真实 smoke test。

Phase 3 不包含：

1. ContextBuilder。Phase 4 负责把 `ContextSet` 渲染成 `ModelRequest.messages`。
2. AgentLoop。Phase 4 负责循环调用 provider 和 tools。
3. Policy Ask/Allow/Deny。Phase 5 接入。
4. Streaming response。MVP 统一 `stream: false`。
5. 多 provider 路由、fallback provider chain、成本预算硬限制。

## 4. 关键设计决定

### 4.1 Provider crate 边界

`kuncode-provider` 可以依赖：

1. `kuncode-core`
2. `serde` / `serde_json`
3. `tokio` / `tokio-util`
4. `reqwest`
5. 测试中的 `wiremock`

`kuncode-provider` 不依赖：

1. `kuncode-workspace`
2. `kuncode-tools`
3. `kuncode-runtime`
4. `kuncode-context`
5. `kuncode-events`

工具 schema 在 provider-neutral 层用 `ModelToolSpec` 表示。Phase 4 或调用方负责把 `ToolDescriptor` 转成 `ModelToolSpec`，避免 provider crate 反向依赖 tools。

### 4.2 Capability 缺失的处理

MVP 的 tool loop 依赖 provider tool calling。因此：

1. `tool_calling = false` 时，不能运行带工具的 agent loop，返回 `ModelError::CapabilityMissing`。
2. `supports_streaming = false` 可降级，因为 Phase 3/4 默认不使用 streaming。
3. `reports_token_usage = false` 可降级为 unknown，成本预算暂不参与 hard stop。
4. context window 不足时，Phase 4 ContextBuilder 负责降预算或拒绝构建请求。

### 4.3 Secret 与 debug dump

默认不保存完整 provider request/response。

1. API key 可以来自环境变量，也可以来自配置文件中的 inline secret。
2. provider 配置使用显式 credential source，例如 `api_key_env = "DEEPSEEK_API_KEY"` 或 `api_key = "..."`；同时存在时环境变量优先，避免本地 secret 被仓库配置覆盖。
3. 配置文件中的 inline secret 必须包在专用 secret 类型里；`Debug` / `Display` / serde 输出默认 redacted。
4. 普通 `ModelRequestMeta` 不包含 message 正文、tool argument 原文、环境变量值或配置文件 secret。
5. 后续 CLI 的 `--debug-dump-provider-request` 必须显式开启，并写到 run debug 目录；Phase 3 只预留类型边界，不默认实现 dump。

### 4.4 取消语义

`ProviderAdapter::call(request, cancel)` 必须监听 `CancellationToken`。

1. cancel 先到则返回 `ModelError::Cancelled`。
2. HTTP future 被 drop 后由 reqwest 尽力 abort。
3. cancel 不保证 provider 端停止计费；这点留到成本预算启用后再补充 metadata。

### 4.5 DeepSeek 与 OpenAI-compatible 分离

DeepSeek 官方 API 支持 tool calls，并在 Chat Completion API 中提供 `tools`、`tool_choice`、assistant `tool_calls`、thinking/reasoning 相关字段和 strict tool beta。KunCode 因此把 DeepSeek 作为一等 provider，而不是把它只配置成通用 OpenAI-compatible provider 的一个实例。

1. `DeepSeekProvider` 负责 DeepSeek 官方 API 的默认值、模型能力、thinking/reasoning metadata、strict tool beta 等 DeepSeek-specific 行为。
2. `OpenAiCompatibleProvider` 负责“只提供 OpenAI-compatible Chat Completions 表面”的第三方 provider，不默认暴露 DeepSeek-specific 扩展。
3. 两者可以共享私有 wire helper，降低 JSON shape 重复；共享层不能泄漏成 public contract，避免后续 DeepSeek 官方字段被通用 adapter 吞掉。
4. 配置层用不同 `kind` 区分：`deepseek` 与 `openai_compatible`。这让 doctor、smoke test、错误信息和 capability 报告能准确说明当前 provider 类型。

## 5. Provider 协议类型

### 5.1 ProviderAdapter

```rust
#[async_trait]
pub trait ProviderAdapter: Send + Sync {
    fn capabilities(&self) -> ProviderCapabilities;

    async fn call(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ModelError>;
}
```

### 5.2 ProviderCapabilities

字段：

1. `provider: String`
2. `model: String`
3. `tool_calling: bool`
4. `max_context_tokens: u32`
5. `supports_streaming: bool`
6. `reports_token_usage: bool`
7. `max_tool_calls_per_response: Option<u32>`

### 5.3 ModelRequest

字段：

1. `request_id: ModelRequestId`
2. `model: String`
3. `messages: Vec<ModelMessage>`
4. `tools: Vec<ModelToolSpec>`
5. `tool_choice: ModelToolChoice`
6. `temperature: Option<f32>`
7. `max_output_tokens: Option<u32>`
8. `metadata: ModelRequestMeta`

`ModelRequestId` 可以先放在 `kuncode-provider` 内；如果后续 EventLog 需要稳定引用，再提升到 `kuncode-core`。

### 5.4 ModelMessage

字段：

1. `role: ModelRole`：`System` / `User` / `Assistant` / `Tool`
2. `content: String`
3. `tool_call_id: Option<String>`
4. `name: Option<String>`

MVP 先使用纯文本 content。多模态 content part 留到后续。

### 5.5 ModelToolSpec

字段：

1. `name: String`
2. `description: String`
3. `input_schema: serde_json::Value`

约束：

1. `name` 和 `description` 由调用方保证来自已校验 descriptor。
2. `input_schema` 原样映射到 Chat Completions tool/function 的 `parameters`。
3. provider adapter 不重新校验 JSON Schema，只做 wire format 映射。

### 5.6 ModelResponse

字段：

1. `message: Option<AssistantMessage>`
2. `tool_calls: Vec<ModelToolCall>`
3. `finish_reason: ModelFinishReason`
4. `usage: Option<TokenUsage>`
5. `metadata: ModelResponseMeta`

约束：

1. 文本回复和 tool calls 可同时存在，但 Phase 4 AgentLoop 必须先处理 tool calls。
2. `tool_calls` 的 arguments 必须解析为 `serde_json::Value`；无法解析返回 `ModelError::Decode`。
3. provider 返回多个 tool calls 时，MVP 保留顺序，Phase 4 按顺序串行执行。

### 5.7 ModelError

分类：

1. `InvalidRequest`
2. `CapabilityMissing`
3. `Authentication`
4. `RateLimited`
5. `Transient`
6. `Server`
7. `Timeout`
8. `Cancelled`
9. `Transport`
10. `Decode`
11. `ExhaustedRetries`
12. `Internal`

每个错误必须提供：

1. 机器可匹配 kind。
2. 短 summary。
3. provider name / model name。
4. 可安全显示的 diagnostic。不得包含 API key、Authorization header 或完整 request body。

## 6. Fixture 驱动的真实 Provider 测试

Phase 3 不实现单独的模拟 provider。确定性测试通过 `wiremock` 启动本地 HTTP server，让真实 adapter 调用本地 fixture：

1. `DeepSeekProvider` 使用 DeepSeek 官方 Chat Completion wire shape。
2. `OpenAiCompatibleProvider` 使用通用 OpenAI-compatible Chat Completions wire shape。

两类 fixture 可以复用大部分 JSON 样例，但测试文件要分开，避免 DeepSeek-specific 字段意外变成通用 adapter contract。

### 6.1 Fixture 格式

fixture 使用 provider 的真实 Chat Completions 响应 JSON，而不是 KunCode 自定义脚本。

```json
{
  "id": "chatcmpl-test",
  "object": "chat.completion",
  "choices": [
    {
      "index": 0,
      "message": {
        "role": "assistant",
        "content": "I will inspect the file.",
        "tool_calls": [
          {
            "id": "call_1",
            "type": "function",
            "function": {
              "name": "read_file",
              "arguments": "{\"path\":\"src/lib.rs\"}"
            }
          }
        ]
      },
      "finish_reason": "tool_calls"
    }
  ],
  "usage": {
    "prompt_tokens": 10,
    "completion_tokens": 5,
    "total_tokens": 15
  }
}
```

### 6.2 行为

1. 测试构造真实 provider adapter，把 `base_url` 指向 wiremock server。
2. wiremock 断言 HTTP method、path、Authorization header、`stream = false`、tools wire shape。
3. wiremock 返回真实 provider wire JSON，adapter 负责解析成 `ModelResponse`。
4. 429/5xx 通过 wiremock response sequence 覆盖 retry。
5. delay/cancel 测试通过 wiremock delayed response 或挂起 response 覆盖。

### 6.3 测试

1. 文本 response 映射正确。
2. tool call response arguments 解析为 JSON。
3. request tools wire shape 与 `ModelToolSpec` 一致。
4. 429/5xx retry sequence 正确。
5. delayed response 期间 cancel 返回 `Cancelled`。
6. DeepSeek fixture 覆盖 `reasoning_content` / reasoning token usage metadata。
7. OpenAI-compatible fixture 不接受 DeepSeek-only 配置字段。

## 7. DeepSeekProvider

### 7.1 配置

字段：

1. `base_url`
2. `model`
3. `api_key_env`
4. `api_key`
5. `timeout_ms`
6. `max_retries`
7. `initial_backoff_ms`
8. `max_backoff_ms`
9. `thinking`
10. `strict_tools`

默认：

1. `base_url = "https://api.deepseek.com"`
2. `api_key_env = "DEEPSEEK_API_KEY"`
3. `api_key = None`
4. `timeout_ms = 120000`
5. `max_retries = 2`
6. `initial_backoff_ms = 250`
7. `max_backoff_ms = 2000`
8. `thinking = None`，表示不覆盖 provider/model 默认值。
9. `strict_tools = false`；若启用，需要使用 DeepSeek beta base URL 并遵守 DeepSeek strict schema 限制。

credential 解析顺序：

1. 如果 `api_key_env` 指向的环境变量存在且非空，使用环境变量值。
2. 否则如果 `api_key` 配置存在且非空，使用配置值。
3. 否则返回 `ModelError::Authentication`，summary 只说明缺少 credential source，不输出 secret。

### 7.2 请求映射

DeepSeek 官方 Chat Completion payload：

1. `model`
2. `messages`
3. `tools`：`[{ "type": "function", "function": { name, description, parameters } }]`
4. `tool_choice`
5. `temperature`
6. `max_tokens`
7. `stream = false`
8. `thinking`：仅当配置显式指定时发送。

当 `strict_tools = true` 时，DeepSeekProvider 在每个 function 定义上附加 `strict = true`。如果未来 DeepSeek strict schema 与 KunCode tool schema 不完全兼容，adapter 必须返回 `ModelError::InvalidRequest`，不能静默降级。

Authorization header 使用 `Bearer <api_key>`，不得进入 Debug、error summary 或 fixture assertion message。

### 7.3 响应映射

1. 读取第一个 choice。
2. `message.content` 映射到 `AssistantMessage.content`。
3. `message.tool_calls[].function.name` 映射到 `ModelToolCall.name`。
4. `message.tool_calls[].function.arguments` 必须 JSON parse 成 `serde_json::Value`。
5. `finish_reason` 映射到 `ModelFinishReason`：`stop` / `tool_calls` / `length` / `content_filter` / `unknown`。
6. `usage.prompt_tokens`、`usage.completion_tokens`、`usage.total_tokens` 映射到 `TokenUsage`。
7. `message.reasoning_content` 映射到 `ModelResponseMeta` 的 provider-specific metadata，不进入 assistant user-visible content。
8. `usage.completion_tokens_details.reasoning_tokens` 若存在，记录到 token usage 扩展字段或 metadata，供 Phase 7 回看是否需要 first-class 成本预算字段。

DeepSeek thinking mode 的多轮要求必须在 adapter 边界内显式建模：如果后续请求需要携带上一轮 assistant 的 `reasoning_content`，应通过 provider-specific message metadata 保存和回放，而不是把 reasoning 文本混入普通 `content`。

### 7.4 错误与重试

重试表：

| 条件 | 分类 | 重试 |
| --- | --- | --- |
| HTTP 400 | `InvalidRequest` | 否 |
| HTTP 401 / 403 | `Authentication` | 否 |
| HTTP 408 / request timeout | `Timeout` | 是 |
| HTTP 429 | `RateLimited` | 是 |
| HTTP 5xx | `Server` | 是 |
| network/connect error | `Transport` / `Transient` | 是 |
| invalid JSON response | `Decode` | 否 |
| tool arguments 非 JSON | `Decode` | 否 |

重试耗尽返回 `ModelError::ExhaustedRetries`，metadata 保留最后一个安全错误摘要和 attempt count。

## 8. OpenAiCompatibleProvider

### 8.1 配置

字段：

1. `provider_name`
2. `base_url`
3. `model`
4. `api_key_env`
5. `api_key`
6. `timeout_ms`
7. `max_retries`
8. `initial_backoff_ms`
9. `max_backoff_ms`

默认：

1. `provider_name = "openai_compatible"`
2. `api_key_env = None`，调用方必须显式配置环境变量名或 inline secret。
3. `api_key = None`
4. `timeout_ms = 120000`
5. `max_retries = 2`
6. `initial_backoff_ms = 250`
7. `max_backoff_ms = 2000`

credential 解析顺序与 DeepSeekProvider 相同：环境变量优先，其次配置文件 inline secret，最后返回 `ModelError::Authentication`。

### 8.2 请求映射

通用 OpenAI-compatible Chat Completions payload：

1. `model`
2. `messages`
3. `tools`
4. `tool_choice`
5. `temperature`
6. `max_tokens`
7. `stream = false`

OpenAiCompatibleProvider 不发送 DeepSeek-only 字段，例如 `thinking`、DeepSeek strict beta 语义或 DeepSeek-specific reasoning replay 字段。若配置层传入这些字段，应在解析配置时拒绝，避免用户误以为通用 provider 支持 DeepSeek 行为。

### 8.3 响应映射

1. 读取第一个 choice。
2. `message.content` 映射到 `AssistantMessage.content`。
3. `message.tool_calls[].function.name` 映射到 `ModelToolCall.name`。
4. `message.tool_calls[].function.arguments` 必须 JSON parse 成 `serde_json::Value`。
5. `finish_reason` 映射到 `ModelFinishReason`。
6. 标准 `usage` 字段映射到 `TokenUsage`；未知扩展字段不参与 provider-neutral contract。

### 8.4 错误与重试

复用 DeepSeekProvider 的默认 HTTP 分类表。provider-specific 错误 JSON 只提取安全 summary 和 status code；不要把原始 request body、Authorization header 或 secret 写入错误。

## 9. 模块组织

建议文件：

```text
crates/kuncode-provider/src/
  lib.rs
  adapter.rs
  error.rs
  request.rs
  response.rs
  chat_completions_wire.rs
  deepseek.rs
  openai_compatible.rs
  retry.rs
  redaction.rs

crates/kuncode-provider/tests/
  deepseek_fixture.rs
  openai_compatible_fixture.rs
  redaction.rs
  smoke_deepseek.rs
```

`smoke_deepseek.rs` 必须默认 ignored 或 feature-gated，不进入普通 CI。

## 10. Cargo 依赖

普通依赖：

1. `async-trait.workspace = true`
2. `kuncode-core = { path = "../kuncode-core" }`
3. `reqwest.workspace = true`
4. `serde.workspace = true`
5. `serde_json.workspace = true`
6. `thiserror.workspace = true`
7. `tokio.workspace = true`
8. `tokio-util.workspace = true`
9. `uuid.workspace = true`（如 `ModelRequestId` 使用 uuid newtype）

dev-dependencies：

1. `wiremock.workspace = true`

不新增 YAML 依赖；fixture 使用 provider 真实 Chat Completions JSON。

## 11. 实施顺序

建议按以下提交顺序推进：

1. `kuncode-provider` 增加依赖、模块骨架和 re-export。
2. 实现 provider-neutral request/response/capability/error 类型和 serde 测试。
3. 实现 `ProviderAdapter` trait。
4. 实现 retry classifier 和 redaction helper。
5. 实现私有 `chat_completions_wire` helper，覆盖共享 JSON shape，不暴露为 public API。
6. 实现 DeepSeekProvider 配置、request builder 和 response parser。
7. 实现 OpenAiCompatibleProvider 配置、request builder 和 response parser。
8. 增加 wiremock fixture：文本回复、tool call、429 retry、5xx retry、auth error、decode error、cancel。
9. 增加 DeepSeek reasoning metadata / strict tool beta 的 fixture 覆盖。
10. 增加 secret redaction regression tests。
11. 增加 feature-gated DeepSeek smoke test。
12. 同步 `progress.md` 的 Phase 3 状态。

## 12. 测试计划

### 12.1 Unit tests

1. `ProviderCapabilities` serde/wire shape。
2. `ModelRequest` / `ModelResponse` round trip。
3. `ModelError` kind 和 summary。
4. retry classifier truth table。
5. redaction helper 不泄露 API key。

### 12.2 DeepSeek fixture tests

1. 成功文本回复。
2. tool call 回复，arguments JSON parse。
3. 429 按配置重试后成功。
4. 5xx 按配置重试后成功。
5. 401 映射 Authentication 且不重试。
6. invalid JSON 映射 Decode。
7. Authorization header 正确发送但不出现在错误/Debug 字符串。
8. delayed response 期间 cancel 返回 `Cancelled`。
9. `reasoning_content` 和 reasoning token usage 进入 metadata，不混入普通 content。
10. strict tool beta 启用时 request function 带 `strict = true`。

### 12.3 OpenAI-compatible fixture tests

1. 成功文本回复。
2. tool call 回复，arguments JSON parse。
3. request tools wire shape 与通用 OpenAI-compatible 预期一致。
4. 429 / 5xx retry 行为与 retry classifier 一致。
5. DeepSeek-only 配置字段被拒绝或不进入请求体。

### 12.4 Smoke test

本地手动运行：

```bash
DEEPSEEK_API_KEY=... cargo test -p kuncode-provider --features deepseek-smoke --test smoke_deepseek -- --ignored
```

要求：

1. 能完成一次文本回复。
2. 能完成一次 tool call 回复解析。
3. 记录模型名、时间、是否返回 usage；不记录 request/response 全文或 secret。

## 13. 验收命令

Phase 3 完成前必须通过：

```bash
cargo fmt --all -- --check
cargo test -p kuncode-provider
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

`cargo deny check` 依赖本机安装 `cargo-deny`；未安装时记录环境缺失。

## 14. 出口条件

Phase 3 完成必须满足：

1. `ProviderAdapter`、`ModelRequest`、`ModelResponse`、`ModelError` public API 稳定到可供 Phase 4 AgentLoop 使用。
2. DeepSeekProvider 通过 wiremock fixture 覆盖成功、tool call、429/5xx retry、auth error、decode error 和 cancel。
3. OpenAiCompatibleProvider 通过 wiremock fixture 覆盖成功、tool call、429/5xx retry、auth error、decode error 和 cancel。
4. Phase 4 集成测试可以复用真实 provider adapter + wiremock fixture，而不是依赖单独的模拟 provider。
5. provider secret 不进入 Debug、error summary、metadata 或测试输出。
6. cancel token 能中断 DeepSeek 和 OpenAI-compatible HTTP call。
7. 不支持 tool calling 时返回 `CapabilityMissing`，不静默降级到无工具 agent loop。
8. 普通 CI 不依赖真实 DeepSeek 网络/API key。

## 15. 暂不解决的问题

1. Streaming response。
2. 多 provider 路由和 fallback provider chain。
3. 成本预算 hard stop。
4. reasoning / thinking token 独立计费。
5. provider request 的安全 redacted dump。
6. 非 Chat Completions provider 的自定义协议。
