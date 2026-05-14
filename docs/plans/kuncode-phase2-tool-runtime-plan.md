# KunCode Phase 2 Tool Runtime 详细开发计划

## 1. 文档职责

本文档细化 MVP 开发计划中的 Phase 2：`Tool Runtime 与基础工具`。

它不替代总体设计文档，也不改写 MVP 总计划。本文只回答 Phase 2 怎么落地：

1. `kuncode-tools` 的类型、模块、执行顺序和测试边界。
2. 需要在 `kuncode-core` / `kuncode-events` 补齐的最小共享类型。
3. 7 个基础工具的实现顺序和验收测试。

Phase 2 入口仍以 [kuncode-mvp-development-plan.md](kuncode-mvp-development-plan.md) 为准：Phase 1 已完成。

## 2. Phase 2 目标

Phase 2 要交付一个可被后续 AgentLoop 调用的工具运行层：

```text
ToolRequest
  -> ToolRuntime
  -> schema validation
  -> capability gate
  -> tool.started
  -> Tool::execute
  -> tool.completed / tool.failed / tool.cancelled
  -> ToolResult / ToolError
```

目标边界：

1. 工具协议稳定，后续 provider/tool schema 映射和 agent loop 不需要重写工具接口。
2. 所有文件路径必须经过 `kuncode-workspace`。
3. 所有工具结果必须结构化，`summary` 必须不超过 200 字符。
4. 工具生命周期必须进入 `EventLog`。
5. 长输出必须通过 `ArtifactStore` 落盘，inline 只保留摘要和短片段。

## 3. 范围

Phase 2 包含：

1. `kuncode-core`：补 `ToolEffect`、`ToolCapability`。
2. `kuncode-events`：补 tool lifecycle event kinds。
3. `kuncode-tools`：实现工具协议、runtime、schema 校验、capability gate。
4. 基础工具：
   - `read_file`
   - `search`
   - `write_file`
   - `apply_patch`
   - `exec_argv`
   - `git_status`
   - `git_diff`

Phase 2 不包含：

1. `task_update`。它依赖 TaskBoard，等 TaskBoard 状态文件和 ContextBuilder 消费规则明确后再接入。
2. 完整 `kuncode-policy`。Phase 2 只做最小 capability gate；`RunMode`、Ask、profile、`UserInteraction` 留到 Phase 5。
3. AgentLoop 集成。Phase 4 再把 ToolRuntime 接入 runtime。
4. Provider tool schema 映射。Phase 3 再处理 provider request/response。

## 4. 关键设计决定

### 4.1 Capability 先于 Policy

Phase 2 需要测试 capability deny，但完整 policy 在 Phase 5。

因此 Phase 2 的规则是：

1. `ToolCapability` 表示 agent 当前能请求哪些工具类别。
2. `ToolRuntime` 只检查工具 descriptor 的 `default_capabilities` 是否和当前 granted capabilities 有交集。
3. risk flags 只记录，不做 Ask/Deny/Allow 决策。
4. 高风险命令的 Ask 行为留给 Phase 5 policy。

### 4.2 EventLog 记录事实，不记录完整输出

tool events payload 记录工具名、request id、状态、摘要和错误分类。

长 stdout/stderr、长文件内容、patch 细节不直接塞进 event payload；需要持久化时写 artifact，然后在 event/tool result 中引用 artifact id。

### 4.3 Artifact source event

`ArtifactStore::save` 和 `ArtifactStore::save_file` 需要 `source_event_id`。Phase 2 约定：

1. `ToolRuntime` 先创建并 emit `tool.started` envelope。
2. `ToolRuntime` 把该 envelope 的 `event_id` 放入 `ToolContext.source_event_id`。
3. 工具执行期间创建的 artifact 使用这个 `source_event_id`。
4. 内存中已有的小内容可以用 `save(&[u8])`；已经 spill 到临时文件或来自文件系统的大内容必须用 `save_file(...)` 流式保存并计算 sha256。

这样 artifact 能稳定回溯到触发它的 tool request。

## 5. Core 类型补齐

在 `crates/kuncode-core/src/` 新增：

```text
effect.rs
capability.rs
```

并在 `lib.rs` re-export。

### 5.1 ToolEffect

```text
#[serde(rename_all = "snake_case")]
enum ToolEffect {
    ReadWorkspace,
    WriteWorkspace,
    ExecuteProcess,
    ModifyTaskBoard,
}
```

### 5.2 ToolCapability

```text
#[serde(rename_all = "snake_case")]
enum ToolCapability {
    Explore,
    Verify,
    Edit,
    Lead,
}
```

### 5.3 Core 测试

新增或扩展 core tests：

1. `ToolEffect` serde wire format。
2. `ToolCapability` serde wire format。
3. MVP 开发计划 §8 工具清单对应的 effect/capability truth table。

## 6. EventKind 扩展

在 `crates/kuncode-events/src/envelope.rs` 的 `EventKind` 增加：

1. `tool.started`
2. `tool.completed`
3. `tool.failed`
4. `tool.cancelled`

### 6.1 Tool Event Payload

Phase 2 payload 使用 `serde_json::Value`，字段约定如下。

`tool.started`：

```json
{
  "tool_request_id": "...",
  "tool_name": "read_file",
  "effects": ["read_workspace"],
  "risk_flags": []
}
```

`tool.completed`：

```json
{
  "tool_request_id": "...",
  "tool_name": "read_file",
  "summary": "read src/lib.rs (1024 bytes)",
  "content_ref": null
}
```

`tool.failed`：

```json
{
  "tool_request_id": "...",
  "tool_name": "read_file",
  "error_kind": "workspace",
  "summary": "path escapes workspace root"
}
```

`tool.cancelled`：

```json
{
  "tool_request_id": "...",
  "tool_name": "exec_argv",
  "summary": "cancelled"
}
```

### 6.2 Event 测试

1. `EventKind::as_str()` 覆盖新增 kind。
2. `EventKind::from_wire()` 覆盖新增 kind。
3. `EventLogReader` corruption / unknown kind 测试不回归。

## 7. kuncode-tools 模块布局

建议结构：

```text
crates/kuncode-tools/src/
  lib.rs
  capability.rs
  context.rs
  descriptor.rs
  error.rs
  input.rs
  result.rs
  runtime.rs
  schema.rs
  tools/
    mod.rs
    read_file.rs
    search.rs
    write_file.rs
    apply_patch.rs
    exec_argv.rs
    git_status.rs
    git_diff.rs
```

`lib.rs` 只做模块声明和 public re-export，不堆实现。

## 8. kuncode-tools 依赖

`crates/kuncode-tools/Cargo.toml` 增加：

正式依赖：

1. `kuncode-core`
2. `kuncode-workspace`
3. `kuncode-events`
4. `async-trait`
5. `serde`
6. `serde_json`
7. `jsonschema`
8. `thiserror`
9. `tokio`
10. `tokio-util`
11. `regex`
12. `ignore`
13. `patch`

dev 依赖：

1. `tempfile`
2. `futures-util`

## 9. 工具协议

### 9.1 Tool trait

```text
#[async_trait]
trait Tool {
    fn descriptor(&self) -> &ToolDescriptor;
    async fn execute(
        &self,
        input: ToolInput,
        ctx: ToolContext<'_>,
    ) -> Result<ToolResult, ToolError>;
}
```

工具实现只负责执行本工具逻辑，不负责：

1. schema 校验。
2. capability gate。
3. 发 `tool.started`。
4. 发 `tool.completed` / `tool.failed` / `tool.cancelled`。

这些都由 `ToolRuntime` 负责。

### 9.2 ToolDescriptor

字段：

1. `name: String`
2. `description: String`
3. `input_schema: serde_json::Value`
4. `output_schema: Option<serde_json::Value>`
5. `effects: Vec<ToolEffect>`
6. `default_capabilities: Vec<ToolCapability>`
7. `risk_flags: Vec<RiskFlag>`

注册时必须校验：

1. `name` 非空。
2. `description` 非空。
3. `effects` 非空。
4. `default_capabilities` 非空。
5. `input_schema` 可被 `jsonschema` 编译。

### 9.3 ToolInput

字段：

1. `request_id: ToolRequestId`
2. `name: String`
3. `payload: serde_json::Value`

Phase 2 不新增完整 `ToolRequest` 领域模型；`ToolInput` 先作为 runtime 调用边界。Phase 4 如果引入 `ToolRequest`，应复用 `ToolRequestId` 和 payload 结构。

### 9.4 ToolResult

字段：

1. `summary: String`
2. `inline_content: Option<String>`
3. `content_ref: Option<ArtifactId>`
4. `metadata: serde_json::Value`

约束：

1. `summary` 不超过 200 字符。
2. `inline_content` 不放无限长输出。
3. 如果输出被写入 artifact，必须设置 `content_ref`。
4. metadata 必须是对象或 `null`，不放大块正文。

### 9.5 ToolError

分类：

1. `UnknownTool`
2. `InvalidInput`
3. `CapabilityDenied`
4. `Workspace`
5. `Io`
6. `Process`
7. `Timeout`
8. `Cancelled`
9. `Artifact`
10. `ResultTooLarge`
11. `Internal`

错误必须保留：

1. 机器可匹配分类。
2. 用户可读 summary。
3. 源错误字符串或路径等诊断信息。

## 10. ToolContext

`ToolContext` 至少包含：

1. `run_id: RunId`
2. `agent_id: Option<AgentId>`
3. `turn_id: Option<TurnId>`
4. `source_event_id: EventId`
5. `workspace: &Workspace`
6. `lane: &ExecutionLane`
7. `event_sink: EventSinkHandle`
8. `artifact_store: Option<&(dyn ArtifactStore + Send + Sync)>`
9. `cancel_token: tokio_util::sync::CancellationToken`
10. `limits: ToolLimits`

`event_sink` 放进 context 是为了允许后续工具发更细的子事件；Phase 2 的基础 lifecycle event 仍由 `ToolRuntime` 统一发。

### 10.1 ToolLimits

字段：

1. `max_inline_output_bytes`
2. `max_stdout_bytes`
3. `max_stderr_bytes`
4. `default_timeout_ms`
5. `max_timeout_ms`

建议默认值：

1. `max_inline_output_bytes = 32 KiB`
2. `max_stdout_bytes = 256 KiB`
3. `max_stderr_bytes = 256 KiB`
4. `default_timeout_ms = 120000`
5. `max_timeout_ms = 600000`

这些默认值可先写在 `kuncode-tools`，后续配置系统落地后再迁移。

## 11. ToolRuntime

### 11.1 注册阶段

`ToolRuntime::register(tool)` 必须：

1. 拒绝重复 tool name。
2. 校验 descriptor 必填字段。
3. 编译并缓存 input schema。
4. 保存工具对象。

当前实现内部使用私有 `ToolRegistry`：`Vec<Entry>` 保存注册顺序，`HashMap<String, usize>` 做名称索引。`descriptors()` 按注册顺序稳定输出，`execute` / `descriptor(name)` 仍按 name 查找。

### 11.2 执行阶段

一次执行顺序固定为：

1. 查找工具名；不存在返回 `ToolError::UnknownTool`。
2. schema 校验；失败返回 `ToolError::InvalidInput`。
3. capability gate；失败返回 `ToolError::CapabilityDenied`。
4. 构造 `tool.started` envelope 并 emit。
5. 把 `tool.started.event_id` 作为 `ToolContext.source_event_id`。
6. 调用 `Tool::execute`。
7. 如果 cancel token 已触发或工具返回 `Cancelled`，emit `tool.cancelled`。
8. 如果工具返回其他错误，emit `tool.failed`。
9. 如果工具成功，校验 `summary <= 200`。
10. 成功 emit `tool.completed`。
11. 返回 `ToolResult` 或 `ToolError`。

如果工具返回 `Ok(result)` 时 cancel token 已触发，runtime 有意丢弃成功 payload，返回 `ToolError::Cancelled` 并记录 `tool.cancelled`。如果 terminal lifecycle event emit 失败，返回的 internal error 必须保留原始工具错误摘要，避免排查时只看到 emit 失败。

### 11.3 Capability Gate

Phase 2 的 gate 规则：

```text
allowed = descriptor.default_capabilities intersects granted_capabilities
```

`granted_capabilities` 由执行调用方传入 runtime，例如：

```text
ToolRuntime::execute(input, ctx, granted_capabilities)
```

后续 Phase 5 policy 接入后，capability gate 仍保留；policy 在 capability gate 后执行。

## 12. 工具实现细节

### 12.1 read_file

输入：

```json
{ "path": "src/lib.rs", "offset": 1, "limit": 200 }
```

行为：

1. `path` 必须是字符串。
2. `offset` / `limit` 可选；任一存在时返回带行号的范围读取，`offset` 从 1 开始，默认 `limit = 200`。
3. 使用 `Workspace::resolve_read_file`。
4. 使用 canonical workspace path 读 UTF-8 文本。
5. `WorkspaceError::{PathEscape,SymlinkEscape,TooLarge,Binary,NotFound}` 映射为 `ToolError::Workspace`。
6. 内容超 `max_inline_output_bytes` 时 inline 截断，metadata 记录 `truncated: true`。
7. 范围读取超出 EOF 时返回空 inline，`end_line: null`、`returned_lines: 0`、`range_truncated: false`，summary 不生成倒置行号。

结果：

1. 全文件读取 `summary = "read <relative-path> (<bytes> bytes)"`。
2. 范围读取 `summary = "read <relative-path> lines <start>-<end> of <total> (<selected_bytes> selected bytes, <bytes> file bytes)"`；空范围使用 `read <path> from line <offset> (0 lines of <total>)`。
3. `inline_content = Some(text_or_truncated_text)`。
4. metadata 包含 relative path、bytes、selected_bytes、truncated、range_truncated、line_numbered、start_line、end_line、returned_lines、total_lines。

### 12.2 search

输入：

```json
{
  "query": "fn parse",
  "path": "src",
  "max_results": 50
}
```

行为：

1. `query` 必须是字符串。
2. `path` 可选；存在时必须在 workspace 内。
3. 优先调用 `rg`，按 stdout 行流式读取。
4. `rg` 不存在时 fallback 到 `ignore` walk + `regex`。
5. 必须跳过 `.git`、`target`、`node_modules`、`.venv`。
6. 超过 `max_results` 后停止收集并终止 `rg`。
7. 达到 `ToolLimits.max_inline_output_bytes` 后停止收集并终止 `rg`。
8. fallback 后端在 blocking task 中按行读取文件，不把整个文件读入内存，也不阻塞 async executor worker。
9. 单条 snippet UTF-8 安全截断；metadata 记录 `snippet_truncated`。
10. search artifact 只保存已选中的结果列表，不保存整个仓库的无限匹配集合。

结果：

1. summary 记录 query 和 match 数量。
2. inline content 是紧凑 match 列表：`path:line:snippet`。
3. metadata 包含 `truncated`、`inline_truncated`、`snippet_truncated`、`content_ref`、`backend`。

### 12.3 git_status

输入：

```json
{}
```

行为：

1. 固定执行 `git status --short`。
2. cwd 是 workspace root。
3. 使用 `ToolLimits.default_timeout_ms`。
4. 不接受任意 git 参数。
5. stdout/stderr 使用 bounded streaming capture。
6. `changed_files` 从完整 captured stdout 统计，即使 inline 已截断也必须准确。

结果：

1. summary 记录 changed file count。
2. inline content 是原始 short status，必要时截断。
3. metadata 记录 `changed_files`、`stdout_bytes`、`truncated`、`duration_ms`。

### 12.4 git_diff

输入：

```json
{ "path": "src/lib.rs" }
```

`path` 可选。

行为：

1. 固定执行 `git diff`。
2. 如果有 path，必须通过 workspace 校验后作为 pathspec。
3. stdout/stderr 使用 bounded streaming capture。
4. stdout 过长时写 artifact。

结果：

1. summary 记录 diff bytes 和是否 truncated/artifact。
2. inline content 是短 diff 或摘要。
3. content_ref 指向 artifact id，如果落盘。

### 12.5 write_file

输入：

```json
{
  "path": "src/new.rs",
  "content": "..."
}
```

行为：

1. 使用 `Workspace::resolve_write_path`。
2. 父目录必须存在且在 workspace 内。
3. 只写 UTF-8 文本。
4. 不创建父目录。

结果：

1. summary 记录 path 和 bytes。
2. metadata 记录 relative path、bytes。

### 12.6 apply_patch

输入：

```json
{ "patch": "..." }
```

行为：

1. MVP 只支持文本 unified diff。
2. 每个目标文件最终都必须经过 workspace 校验。
3. 不支持 rename。
4. 不支持 binary patch。
5. 不支持越界 path。
6. 不支持删除。
7. 同一次 patch 不允许 duplicate target。
8. 两阶段执行：先解析、规范化路径、读取当前内容并计算所有目标的新内容；全部验证通过后才写入。
9. 验证阶段任何错误不得修改 workspace。
10. 写入阶段如果发生 IO 错误，尽力 rollback 已写文件：已有文件写回原内容，新建文件删除；rollback 失败返回带诊断的 internal/io 错误。
11. 修改已有文件时保留原有行尾；CRLF 或混合行尾不得被静默规范化为 LF。
12. hunk 必须按 old range 升序且不能重叠；倒序或重叠 hunk 返回 `ToolError::InvalidInput`。
13. context mismatch、duplicate target、new-file patch 中出现 context/remove 等 patch 语义错误返回 `ToolError::InvalidInput`。

结果：

1. summary 记录 touched file count。
2. metadata 记录 touched relative paths。

### 12.7 exec_argv

输入：

```json
{
  "argv": ["cargo", "test"],
  "cwd": ".",
  "timeout_ms": 120000
}
```

行为：

1. `argv` 必须是非空字符串数组。
2. 禁止 shell string。
3. `cwd` 可选；必须在 workspace/lane 内。
4. 使用 `tokio::process::Command`。
5. Unix spawn 时创建独立 process group；timeout/cancel 后 kill 整个 process group 并 wait。
6. Windows 暂保留 direct-child kill fallback。
7. stdout/stderr 使用 bounded streaming capture：内存只保留 inline 前缀、真实 byte counter 和 truncated flag，超过阈值后 spill 到临时文件。
8. stdout 或 stderr 任一超阈值时写完整 combined artifact，格式为 `stdout:\n...\nstderr:\n...`。
9. trusted command 判断只设置 `RiskFlag::UntrustedCommand`，不做 Ask。

结果：

1. summary 记录 exit status、stdout/stderr 大小、是否 artifact。
2. inline content 包含短 stdout/stderr。
3. metadata 记录 argv、cwd、exit code、duration、真实 stdout/stderr byte count、truncated、trusted_command、artifact。
4. `stdout_truncated` / `stderr_truncated` 只表示对应 stream 被截断；combined inline 被截断时用 `inline_truncated` 表示，不把两个 stream 同时标为 truncated。

## 13. 测试计划

### 13.1 测试文件

```text
crates/kuncode-tools/tests/runtime.rs
crates/kuncode-tools/tests/read_file.rs
crates/kuncode-tools/tests/search.rs
crates/kuncode-tools/tests/write_file.rs
crates/kuncode-tools/tests/apply_patch.rs
crates/kuncode-tools/tests/exec_argv.rs
crates/kuncode-tools/tests/git.rs
```

### 13.2 每个工具必须覆盖

1. happy path。
2. capability deny。
3. error classification。

### 13.3 Runtime 测试

1. duplicate registration rejected。
2. unknown tool rejected。
3. invalid schema rejected。
4. capability deny 在 execute 前发生。
5. `tool.started` 在执行前写入。
6. 成功时事件链为 `tool.started` -> `tool.completed`。
7. 失败时事件链为 `tool.started` -> `tool.failed`。
8. cancel 时事件链为 `tool.started` -> `tool.cancelled`。
9. summary 超 200 字符返回 `ToolError::ResultTooLarge`。

### 13.4 exec_argv 额外测试

1. command success。
2. command not found。
3. timeout。
4. cancel 后 5 秒内进程被回收。
5. long stdout 落 artifact。
6. long stderr 落 artifact。
7. cwd 越界被拒绝。
8. Unix-only：timeout/cancel 能回收 shell 启动的子进程树。
9. long stdout/stderr artifact 保存完整 combined 输出，metadata byte count 是真实输出大小。

### 13.5 Artifact 测试

1. stdout/stderr 超阈值时生成 artifact 文件。
2. `artifacts.jsonl` 可读回 metadata。
3. artifact `source_event_id` 等于对应 `tool.started` event id。
4. `save_file` 可保存大文件，sha256、size、source_event_id 正确。

### 13.6 search 额外测试

1. 单条超长匹配被 snippet cap 截断。
2. 多条匹配导致总 inline 超限时设置 `inline_truncated`，并生成有效 artifact 引用。
3. `rg` 后端达到 `max_results` 或 inline 限制后停止读取并终止进程。
4. fallback 后端在 async 工具中不会直接跑同步文件 IO。

### 13.7 apply_patch 额外测试

1. 多文件 patch 中后续 hunk context mismatch 时，前面文件保持原样。
2. duplicate target 在任何写入前失败。
3. 新建文件 + 后续失败时不留下新文件。
4. CRLF 文件应用 patch 后保留 CRLF。
5. 倒序 hunk 被拒绝且不写入。

### 13.8 read_file 额外测试

1. `offset` 超过 EOF 返回空 inline、`end_line: null`、`returned_lines: 0`。
2. 空范围 summary 不出现倒置行号。
3. 范围读取 metadata 记录 `selected_bytes`，summary 不把整文件大小伪装成切片大小。

## 14. 实施顺序

建议按以下提交顺序推进：

1. `kuncode-core` 增加 `ToolEffect` / `ToolCapability` 和测试。
2. `kuncode-events` 增加 tool lifecycle event kinds 和测试。
3. `kuncode-tools` 增加 Cargo 依赖、模块骨架、协议类型。
4. 实现 `ToolRuntime` register / schema validation / capability gate。
5. 实现 runtime event emission 和 runtime tests。
6. 实现 `read_file`。
7. 实现 `git_status` / `git_diff`。
8. 实现 `search`。
9. 实现 `write_file`。
10. 实现 `apply_patch`。
11. 实现 `exec_argv`。
12. 补齐 artifact、timeout、cancel 测试。

## 15. 验收命令

Phase 2 完成前必须通过：

```bash
cargo fmt --all -- --check
cargo test -p kuncode-core
cargo test -p kuncode-events
cargo test -p kuncode-tools
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check
```

`cargo deny check` 依赖本机安装 `cargo-deny`；未安装时记录环境缺失。

## 16. 出口条件

Phase 2 完成必须满足：

1. 7 个基础工具均可通过 `ToolRuntime` 执行。
2. 每个工具至少覆盖 happy path、capability deny、error classification。
3. `ToolRuntime` 覆盖 unknown tool、duplicate register、schema invalid、capability deny、event order。
4. `exec_argv` 支持 timeout、cancel kill、long output artifact。
5. 所有工具结果 summary 不超过 200 字符。
6. 所有 path/cwd 访问经过 `Workspace` / `ExecutionLane`。
7. tool lifecycle events 可从 `events.jsonl` 读回。
8. stdout/stderr artifact metadata 可从 `artifacts.jsonl` 读回。
9. `search`、`exec_argv` 和 git helper 不使用无界内存保存外部进程输出。
10. `apply_patch` 验证错误必须零写入，写入错误必须尽力 rollback。

## 17. 暂不解决的问题

1. 完整 policy mode 和 Ask 交互。
2. TaskBoard 和 `task_update`。
3. Provider-specific tool schema 渲染。
4. AgentLoop 中 tool call 的循环集成。
5. 多 lane / worktree。
6. plugin 工具加载。
