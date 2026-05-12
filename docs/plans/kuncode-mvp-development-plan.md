# KunCode MVP 开发计划

## 1. 文档职责

本文档定义 KunCode MVP 阶段的实现落点。

它消费两份输入：

1. [kuncode-agent-harness-design.md](../specs/kuncode-agent-harness-design.md)：长期架构、协议名和不变量。
2. [kuncode-agent-harness-rationale-gaps.md](../specs/kuncode-agent-harness-rationale-gaps.md)：尚未解决的设计取舍。

它回答：

1. MVP 实际要交付什么。
2. Rust crate、模块和关键类型如何落点。
3. Phase 如何切，每个 Phase 的入口/出口和验收。
4. 哪些已写在设计文档里、MVP 期可以直接照抄。
5. 哪些总体设计之外的实现选择在 MVP 期采用。

它不回答：

1. 长期机制设计——回到总体设计。
2. 未决取舍的最终方案——回到 rationale gaps。
3. UI 文案、prompt 模板和文档站点。

如果实施过程中需要新增或改写实现名，先改本文档，再同步任务列表，不在总体设计里发明实现名。

## 2. MVP 范围凝固

来源：总体设计 [§18](../specs/kuncode-agent-harness-design.md#L943-L983)。

### 2.1 必须交付的不变量

1. 单进程异步 runtime。
2. 单 `Lead` agent 的 agent loop。
3. `Run` / `Agent` / `Turn` / `ToolRequest` / `ToolResult` / `EventEnvelope` 类型化原语。
4. JSONL EventLog 与 Artifact 引用。
5. `Workspace` 路径边界 + `MainWorkspace` ExecutionLane。
6. `PolicyDecision` 与四种运行模式。
7. 每轮 ContextSet 重建，含 invariant blocks。
8. `ProviderAdapter` trait + `FakeProvider` + 一个真实 provider。
9. `CompletionGate` 与最终报告。
10. `Run.budgets` 至少包含 `max_turns`、`max_wall_time`、`max_context_tokens`。

### 2.2 推荐工具集合

1. `read_file`
2. `search`（ripgrep wrapper 或等价）
3. `write_file` / `apply_patch`
4. `exec_argv`（带 timeout 与输出截断）
5. `git_status` / `git_diff`
6. `task_update`（修改 TaskBoard 的工具形式）

### 2.3 MVP 暂缓

完整列表见总体设计 §18。本文档不允许把以下任何一项偷偷塞回 MVP：

1. 多 Agent / Mailbox。
2. 背景 jobs / notification 队列。
3. Worktree lane。
4. 完整 skill loader。
5. Streaming。
6. 并行工具调用。
7. Resume / replay。
8. 模型辅助 compact。

### 2.4 MVP 实现选择

下面这些是 MVP 的实现选择，用于降低第一版的不确定性。它们不反向约束总体设计；如果后续事实证明不合适，可以在本文档中修改，而不是改写总体架构。

1. 异步 runtime 采用 Tokio。
2. EventLog 物理存储采用 JSONL，单 writer task + `mpsc` 接收。
3. 真实 provider 采用 DeepSeek API 的 OpenAI-compatible Chat Completions 形态（理由：DeepSeek 是目标 provider，且其 API 支持 OpenAI-compatible tool calls，MVP 可以少写一层自定义协议适配）。
4. 工作目录探测：CLI 启动时显式传入 workspace root，未提供则使用 `cwd` 并写入 `run.started` 事件。
5. KunCode home 默认 `$XDG_STATE_HOME/kuncode`，缺省退回 `$HOME/.local/state/kuncode`；可由 `KUNCODE_HOME` 覆盖。
6. 配置文件格式：TOML。
7. CLI 默认 `non_interactive` 在 `stdin` 非 tty 时启用，否则 `interactive`。

剩余未决定的取舍仍记在 rationale gaps。

## 3. Rust crate 布局

MVP 用 Cargo workspace。单 crate 也能跑，但工作区让边界更清楚，也方便后续把 provider 和 storage 拆出独立发布。

```text
kuncode/
  Cargo.toml                 # workspace root
  crates/
    kuncode-core/            # 协议原语、错误分类、ID 类型
    kuncode-events/          # EventEnvelope、EventLog、artifact store、KunCode home
    kuncode-workspace/       # Workspace、ExecutionLane、路径安全
    kuncode-tools/           # Tool trait、ToolRuntime、内建工具
    kuncode-policy/          # PolicyDecision、modes、UserInteraction trait
    kuncode-context/         # ContextBlock、ContextSet builder、micro compact
    kuncode-provider/        # ProviderAdapter trait、FakeProvider、DeepSeek 实现
    kuncode-runtime/         # Runtime Kernel、agent loop、CompletionGate
    kuncode-cli/             # 二进制入口
  docs/
    specs/                   # 已有的设计文档
  examples/                  # 集成示例和 golden 用例素材
```

约束：

1. `kuncode-core` 不依赖其他 crate。
2. `kuncode-provider` 不依赖 `kuncode-workspace`、`kuncode-tools` 或 `kuncode-runtime`。
3. `kuncode-runtime` 只依赖纯协议 crate（core / events / workspace / tools / policy / context / provider），不依赖 CLI。
4. CLI 是唯一可以见 `clap`、终端 IO 和 stdout 渲染的地方。
5. 每个 crate 必须有自己的 unit test 文件夹；跨 crate 测试放 `crates/kuncode-runtime/tests/`。

依赖原则：核心 trait 用 `async_trait::async_trait`；时间用 `time` crate；ID 用 `uuid` v7（保留时间顺序）；序列化用 `serde` + `serde_json`；错误分类用 `thiserror`。

## 4. 关键类型与 trait

下列名是 MVP 阶段稳定的实现名。后续如果要改，必须改本文档而不是悄悄改代码。

### 4.1 ID 与时间

1. `RunId`、`AgentId`、`TurnId`、`EventId`、`ToolRequestId`、`ArtifactId`：均为 `Uuid` 的 newtype。
2. `Instant` 字段统一用 `time::OffsetDateTime` 序列化为 RFC3339。

### 4.2 Core 原语（`kuncode-core`）

1. `Run` / `RunStatus` / `Budgets`。
2. `Agent` / `AgentRole`。
3. `Turn` / `ModelRequestMeta`。
4. `ToolRequest` / `ToolResult` / `ToolError`。
5. `RiskFlag` 枚举。
6. `EventKind` 枚举（封闭）+ `EventEnvelope`：MVP 采用封闭 enum + `#[serde(tag="kind")]`，等扩展点稳定后再考虑放开。
7. 领域错误：`KuncodeError` 顶层 enum，按 §17 分类。

### 4.3 Events 与存储（`kuncode-events`）

1. `EventSink` trait：`async fn emit(&self, envelope: EventEnvelope) -> Result<()>`。
2. `JsonlEventSink`：单 writer task；`fn handle(&self) -> EventSinkHandle` 返回可 clone 的发送端。
3. `EventLogReader`：从 `events.jsonl` 流式读，遇损坏行返回 `EventLogError::Corrupted { offset, line, cause }` 但不中断后续读。
4. `ArtifactStore` trait + `FileArtifactStore`：保存内存 bytes 或流式保存已有文件到 `$KUNCODE_HOME/runs/<id>/artifacts/`，元数据写入 `artifacts.jsonl`。
5. `RunDir`：封装 `$KUNCODE_HOME/runs/<id>/` 下的文件布局，禁止其他 crate 直接拼路径。

### 4.4 Workspace（`kuncode-workspace`）

1. `Workspace`：root path + 配置（max file size、二进制策略等）。
2. `WorkspacePath`：canonicalized、保证在 workspace 内的 newtype。所有写工具入口必须接收 `WorkspacePath` 而非 `&Path`。
3. `ExecutionLane`、`LaneKind`（MVP 只有 `MainWorkspace`）。
4. `WorkspaceError`：`PathEscape`、`SymlinkEscape`、`TooLarge`、`Binary`、`NotFound`、`IoError`。

### 4.5 Tools（`kuncode-tools`）

1. `Tool` trait：

```text
#[async_trait]
trait Tool {
    fn descriptor(&self) -> &ToolDescriptor;
    async fn execute(&self, input: ToolInput, ctx: ToolContext) -> Result<ToolResult, ToolError>;
}
```

2. `ToolDescriptor`：`name`、`description`、`input_schema`、`output_schema`、`effects`、`default_capability`、`risk_flags`。
3. `ToolRuntime`：注册 + 校验 + capability check + policy 调用 + 执行。Agent loop 不直接调用 `Tool::execute`。
4. MVP 内建工具实现以独立模块：`read_file`、`search`、`write_file`、`apply_patch`、`exec_argv`、`git_status`、`git_diff`、`task_update`。
5. `ExecArgv` 走 `tokio::process::Command`；输出使用 bounded capture，超过阈值落 artifact，inline 留摘要。
6. `apply_patch` MVP 接受 unified diff 文本，使用 `patch` crate 或受控自实现；不允许 delete、rename、duplicate target 等高风险操作（留给后续），验证失败不得部分写入。

### 4.6 Policy（`kuncode-policy`）

1. `PolicyInput` / `PolicyDecision`。
2. `RunMode`：`Interactive` / `NonInteractive` / `Yes` / `DryRun`。
3. `PolicyEngine` trait + `DefaultPolicyEngine`：基于 effects + risk flags + mode 的查表实现。
4. `UserInteraction` trait：
   ```text
   async fn ask(&self, prompt: AskPrompt) -> AskResponse;
   ```
   MVP 提供 `NonInteractiveInteraction`（恒返回拒绝并写事件）和 `StdinInteraction`（CLI 用）。
5. Policy profile 配置写在 TOML，MVP 不做用户自定义 profile，只暴露内置 `local-default`。

### 4.7 Context（`kuncode-context`）

1. `ContextBlock` enum：13 种 block kind，序列化稳定。
2. `ContextSet`：有序 block 集合 + render meta。
3. `ContextBuilder`：每轮重建入口，输入是 `RunState`（agent、taskboard、recent events、recent tool results、diff snapshot），输出 `ContextSet`。
4. `MicroCompact`：MVP 唯一启用的 compact 层；auto compact 与 manual compact 留 trait 钩子但暂不实现。
5. 静态前缀稳定要求：`SystemBlock` → tool schemas → `SkillIndexBlock`（如果有）→ `IdentityBlock` → `GoalBlock` 必须顺序固定且内容稳定，便于 provider 端 prompt cache。

### 4.8 Provider（`kuncode-provider`）

1. `ProviderAdapter` trait：

```text
#[async_trait]
trait ProviderAdapter {
    fn capabilities(&self) -> ProviderCapabilities;
    async fn call(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelResponse, ModelError>;
}
```

2. `ProviderCapabilities`：MVP 必检字段 `tool_calling: bool`、`max_context_tokens: u32`、`supports_streaming: bool`、`reports_token_usage: bool`。
3. `FakeProvider`：脚本化响应，按 turn 输出预设序列；用于所有集成测试。
4. `DeepSeekProvider`：调用 DeepSeek OpenAI-compatible Chat Completions API；MVP 关闭 streaming；secret 从环境变量读取，不进 EventLog。
5. fallback 规则按总体设计 §16 实现；不支持 tool calling 直接 `ModelError::CapabilityMissing` 启动失败。

### 4.9 Runtime（`kuncode-runtime`）

1. `RuntimeKernel`：组合所有依赖，提供 `start_run(...) -> RunHandle`。
2. `AgentLoop`：单 agent 实现，循环顺序：build context → call provider → process tool requests → emit events → update task board → check budgets → check completion gate。
3. `CompletionGate`：按总体设计 §12.3 实现；规则用查表写，便于测试。
4. `FinalReportBuilder`：生成 `final_report.*` 事件 + Markdown 报告写入 `runs/<id>/report.md`。
5. `RunHandle`：暴露 cancel、wait、status，便于 CLI 或测试驱动。

### 4.10 CLI（`kuncode-cli`）

1. 子命令最小集：
   1. `kuncode run "<goal>"`：启动一次 run。
   2. `kuncode tail <run-id>`：尾随事件。
   3. `kuncode doctor`：检查依赖（git、ripgrep）、provider 凭据、KunCode home 可写。
   4. `kuncode config show`：打印有效配置（脱敏）。
2. 渲染层只消费 `EventEnvelope` 流，不直接访问 runtime 内部状态。

## 5. Phase 分解

每个 phase 都要交付可测试代码。"出口条件"是合并前必须满足的硬要求。

### Phase 0：仓库骨架

入口：当前 `src/main.rs` 空白。

任务：

1. 建立 workspace `Cargo.toml` 和 9 个空 crate。
2. 配置 `rustfmt`、`clippy`、`cargo deny`、CI（`cargo test --workspace`，`cargo clippy -- -D warnings`）。
3. `kuncode-core` 写入 ID newtype、`Budgets`、`RunStatus`、`AgentRole`、`RiskFlag`、`KuncodeError` 顶层 enum。
4. `kuncode-cli` 提供 `kuncode --version`。

出口：CI 绿；`kuncode --version` 可运行。

### Phase 1：Workspace 与事件

入口：Phase 0 完成。

任务：

1. `kuncode-workspace`：`Workspace`、`WorkspacePath` 安全构造、unit tests 覆盖 `..`、symlink、绝对路径越界、二进制、大文件。
2. `kuncode-events`：`EventEnvelope`、`EventKind` 封闭 enum、`JsonlEventSink`、`EventLogReader`、`ArtifactStore`。
3. KunCode home 布局落地：`runs/<id>/events.jsonl`、`artifacts.jsonl`、`artifacts/`、`metadata.json`。
4. 损坏测试：构造 truncated JSON 行、未知 `kind`、空文件，reader 必须分类报告。

出口：

1. Workspace 单元测试覆盖 §10.1 所有规则。
2. EventLog 可写、可读、可在损坏时仍读出后续行。
3. 文档：在 [kuncode-agent-harness-design.md](../specs/kuncode-agent-harness-design.md) §13 引用本 phase 的实际目录结构（如果有偏差，回设计文档调整）。

### Phase 2：Tool Runtime 与基础工具

入口：Phase 1 完成。

任务：

1. `kuncode-tools`：`Tool` trait、`ToolRuntime`、`ToolDescriptor`、JSON schema 校验（`jsonschema` crate）。
2. 实现内建工具：`read_file`、`search`（调 `rg`，若不可用回退到自实现 walk + regex）、`write_file`、`apply_patch`、`exec_argv`、`git_status`、`git_diff`。
3. `exec_argv`：timeout、cancel、Unix 进程树回收、stdout/stderr bounded capture，长度阈值后落 artifact。
4. 所有工具的输出 `summary` 字段必须在 200 字符内，便于 ContextSet 引用。

出口：

1. 每个工具至少 3 个 unit test：happy path、capability deny、错误分类。
2. `exec_argv` 取消测试：cancel 后 5 秒内进程被回收，事件链 `tool.started` → `tool.cancelled` 完整。

### Phase 3：Provider Adapter 与 FakeProvider

入口：Phase 2 完成。

任务：

1. `kuncode-provider`：trait、`ModelRequest`、`ModelResponse`、`ModelError` 分类、`CancellationToken`（用 `tokio_util::sync::CancellationToken`）。
2. `FakeProvider`：从脚本（YAML 或 JSON）读取响应序列，按 turn 索引输出；支持插入 tool 请求和最终 stop。
3. `DeepSeekProvider`：HTTP 客户端、OpenAI-compatible Chat Completions 请求/响应映射、错误分类、429 / 5xx 限次重试、token usage 写入 `ModelRequestMeta`。
4. fallback 决策表 + 单测。
5. 最小真实 provider smoke：通过 `--features deepseek-smoke` 运行一次 DeepSeek API 调用，验证认证、模型名、tool schema 和 tool call 响应解析；默认不在 PR CI 上运行。

出口：

1. FakeProvider 能驱动一个不真实联网的 agent loop dry run（与 Phase 4 集成测试一起验收）。
2. DeepSeekProvider 用录制的 HTTP fixture（`wiremock` 或 `mockito`）跑通成功路径、tool call 路径与 429 重试。
3. Secret 不出现在事件序列化输出中（regression test）。
4. 可选 deepseek-smoke 在本地至少跑通一次，并把结果记录到 Phase 记录中；失败不阻塞普通 CI，但阻塞进入 Phase 4。

### Phase 4：Context Engine 与 Agent Loop

入口：Phase 3 完成。

任务：

1. `kuncode-context`：13 种 `ContextBlock`、`ContextBuilder`、`MicroCompact`、`RenderMeta`（含 block 顺序、token 估算、被裁剪内容摘要）。
2. `kuncode-runtime`：`AgentLoop` 实现单 agent 循环，集成 ToolRuntime、Policy（stub）、EventSink、ContextBuilder、Provider。
3. Budget 触发：`max_turns`、`max_wall_time`、`max_context_tokens` 任一超出立刻进入 `BudgetExceeded`。
4. 状态机：`Running` → `Completed` / `Failed` / `Blocked` / `Cancelled` / `BudgetExceeded`。

出口：

1. 集成测试：FakeProvider 脚本驱动一次"读文件 → 修改 → 运行测试 → 报告"，事件序列与 golden 对齐。
2. Budget 超限测试：3 种 budget 都各有一个用例。
3. 验证 ContextSet 不变量：每轮都包含 SystemBlock、IdentityBlock、GoalBlock、TaskBlock。

### Phase 5：Policy、UserInteraction、Modes

入口：Phase 4 完成。

任务：

1. `kuncode-policy`：`DefaultPolicyEngine`、四种 `RunMode`、profile TOML 加载。
2. `UserInteraction`：`NonInteractiveInteraction`、`StdinInteraction`。
3. AgentLoop 接入 Policy；高风险 effect（`Destructive`、`Network`、`ExecuteProcess` + 未授信命令）必须经过 policy。
4. `DryRun` 模式：写工具与 `exec_argv` 不真正执行，但记录 `policy.dry_run_allowed` 事件。

出口：

1. 每种 mode 至少一个集成测试。
2. `Ask` 在 `non_interactive` 下必然转 `Deny` 并写事件。
3. `Yes` 模式下 destructive 仍然不会自动通过。

### Phase 6：CompletionGate 与 FinalReport

入口：Phase 5 完成。

任务：

1. `CompletionGate` 规则查表实现 + 单测。
2. `FinalReportBuilder`：根据 run 类型（只读 / 写入 / 修复）生成不同结构的最终报告。
3. `kuncode-cli`：`run` 命令在 run 结束后打印报告路径，并以非 0 退出码反映 `Failed` / `Blocked` / `BudgetExceeded`。

出口：

1. CompletionGate 拒绝"声称完成但无 diff"的 run；测试覆盖 6 条 §12.3 规则。
2. 最终报告含 Markdown 与机器可读 `final_report.completed` 事件 payload。

### Phase 7：DeepSeek 真实 E2E smoke

入口：Phase 6 完成。

任务：

1. CI 中保留可选 `--features deepseek-smoke` 的 smoke 测试，使用密钥环境变量；默认不在 PR CI 上运行。
2. 手动一次 end-to-end：在示例仓库跑通"修复一个故意失败的单测"。
3. 修复在真实模型上暴露的 prompt 渲染 bug、tool schema 兼容性、token usage 计算偏差。

出口：

1. Phase 7 评审记录一次 smoke 的 cost、turns、wall time。
2. 任何 smoke 期发现的协议偏差回归到 §4 的实现名定义。

### Phase 8：CLI 收尾与 doctor

入口：Phase 7 完成。

任务：

1. `doctor`：检查 `git`、`rg`、provider 凭据、KunCode home 可写、磁盘空间。
2. `config show` 输出 effective config 并脱敏。
3. `tail` 子命令：流式输出指定 run 的事件，支持过滤 `kind`。

出口：MVP candidate 标签可发布；以下 §12 验收清单全过。

## 6. EventLog 物理落地

### 6.1 写入策略

1. 单 writer task 持有 `tokio::fs::File`，其他组件通过 `mpsc::Sender<EventEnvelope>` 投递。
2. 每条事件 `serde_json::to_writer` + `\n`；行内不允许换行。
3. fsync 策略 MVP：每 N 条或每 K 毫秒一次；具体阈值放入配置，默认 N=16、K=200ms。`run.completed` / `run.failed` / `final_report.*` 强制 fsync。
4. writer task 退出时 drain 队列并最终 fsync；进程信号触发 graceful shutdown。

### 6.2 读取策略

1. `EventLogReader::stream()` 返回 `impl Stream<Item = Result<EventEnvelope, EventLogError>>`。
2. 行解析失败仅影响该行，offset 与 raw line 进入错误，stream 继续。
3. `EventLogReader::tail()` 用 `notify` crate 监听文件追加，CLI `tail` 子命令复用。

### 6.3 Artifact 落地

1. 文件命名 `<artifact_id>.bin`，内容不加压缩（MVP 简单优先）。
2. `artifacts.jsonl` 每行一条 `ArtifactRecord`：包含 size、sha256、kind、source event id。
3. 任何 `content_ref` 都是 `ArtifactId`，不允许暴露文件系统路径。

## 7. 配置与 KunCode home

### 7.1 配置文件

`$KUNCODE_HOME/config.toml` 示例字段：

```text
[runtime]
log_level = "info"

[provider.default]
kind = "deepseek"
model = "deepseek-v4-pro"
api_key_env = "DEEPSEEK_API_KEY"

[budgets.default]
max_turns = 40
max_wall_time_secs = 1800
max_context_tokens = 200000

[policy.default]
profile = "local-default"
mode = "interactive"

[storage]
fsync_every_n = 16
fsync_every_ms = 200
```

MVP 不实现 per-project 配置覆盖；workspace 内的 `KUNCODE.md` 视作 skill index 的一部分，不影响 runtime 配置。

### 7.2 优先级

1. CLI flag。
2. 环境变量（`KUNCODE_*` 前缀）。
3. `$KUNCODE_HOME/config.toml`。
4. 内建默认。

冲突时高优先级覆盖低优先级，启动时打一条 `config.resolved` 事件包含来源指示，便于 `doctor` 复现。

## 8. 内建工具清单

下表是 MVP 唯一允许内建的工具。新增工具进 MVP 必须先改本节。

| name | effects | capability set | risk flags |
|---|---|---|---|
| `read_file` | `ReadWorkspace` | `Explore` / `Edit` | — |
| `search` | `ReadWorkspace` | `Explore` / `Edit` | — |
| `write_file` | `WriteWorkspace` | `Edit` | `mutates_workspace` |
| `apply_patch` | `WriteWorkspace` | `Edit` | `mutates_workspace` |
| `exec_argv` | `ExecuteProcess` | `Verify` / `Edit` | `long_running`，命令未授信时 `untrusted_command` |
| `git_status` | `ReadWorkspace` | `Explore` / `Verify` | — |
| `git_diff` | `ReadWorkspace` | `Explore` / `Verify` | — |
| `task_update` | `ModifyTaskBoard` | `Lead` | — |

`exec_argv` 的"授信命令"白名单 MVP 写死：`cargo`、`go`, `python`, `npm`, `pnpm`, `bun`, `rustc`, `node`, `git`（仅子命令 status/diff/log/show/branch/worktree list），其他命令视为未授信，policy 默认 `Ask`。

## 9. 测试策略落地

凝固自总体设计 [§19](../specs/kuncode-agent-harness-design.md#L985-L1002)。

### 9.1 必须存在的测试套件

1. `kuncode-workspace/tests/path_safety.rs`：覆盖 §10.1 全部规则。
2. `kuncode-events/tests/jsonl_corruption.rs`：损坏行号、未知 kind、空文件。
3. `kuncode-tools/tests/*.rs`：每个工具 happy + deny + error。
4. `kuncode-provider/tests/deepseek_fixture.rs`：基于 `wiremock` 的录制 fixture。
5. `kuncode-runtime/tests/agent_loop_*.rs`：FakeProvider 驱动的端到端剧本。
6. `kuncode-runtime/tests/completion_gate.rs`：6 条规则各一例。
7. `kuncode-runtime/tests/budget_*.rs`：3 种 hard stop 各一例。

### 9.2 Golden

1. `examples/golden/context_render/`：固定 RunState → 固定 ContextSet 输出。
2. `examples/golden/event_stream/`：固定脚本 → 固定事件序列。

Golden 比较使用 `insta` snapshot；更新需 reviewer 显式批准。

### 9.3 Local benchmark

MVP 提供一个基准仓库（`examples/bench/fixit/`），目标是"修复故意失败的 `cargo test`"。基准记录：turns、wall time、tool calls、最终是否通过 CompletionGate。每个 MVP candidate tag 跑一次并归档到 `docs/bench/`（首次跑出来后再建该目录）。

## 10. 安全与 secret

1. provider key 通过环境变量读取，配置文件只保留环境变量名。
2. 序列化时 `ProviderProfile` 的 `api_key_env` 在事件中显示为 `"<env:NAME>"`。
3. `redact` MVP 实现为 best effort 字符串替换；规则：常见 token 前缀（`sk-`、`AKIA`、`ghp_` 等）+ 环境中标记为 secret 的变量值。
4. `--debug-dump-provider-request` CLI flag 启用时允许 raw 请求落到 `runs/<id>/debug/`，并显式打日志警告该 run 不应分享。

## 11. 开放问题（指向 rationale gaps）

MVP 期不解决，但要在 Phase 退出评审中显式确认未触发：

1. `$KUNCODE_HOME` 位置取舍（gap #2）：MVP 用 XDG，后续视用户反馈。
2. Provider request 默认不落盘（gap #4）：MVP 提供 debug flag，长期方案待定。
3. Compact 三层 rationale（gap #6）：MVP 只跑 micro，auto / manual 留 trait 钩子。
4. Skill 两阶段加载（gap #7）：MVP 只用 SkillIndexBlock 静态注入，不实现 `load_skill`。
5. EventEnvelope tagged enum vs string kind（gap #8）：MVP 用封闭 enum；如果出现外部插件需求再考虑放开。
6. EventLog vs ContextSet 对账规则（gap #9）：MVP 由 `ContextBuilder` 单向消费事件，没有反向校验；Phase 4 集成测试覆盖关键事件被引入 context 的情况。
7. Effects 扩展（gap #10）：MVP 封闭 enum；放开等到 Tool plugin 设计。
8. `exec_argv` vs `shell_command`（gap #11）：MVP 只暴露 `exec_argv`，`shell_command` 暂不进 capability set。
9. Worktree closeout `promote`（gap #13）：MVP 没有 worktree。
10. Reasoning / thinking token（gap #18）：MVP 仅记录到 `ModelRequestMeta`，不进入独立事件；要在 Phase 7 smoke 后回头确认是否需要 first-class 事件。

任何一条在实现中升级为阻塞性问题，必须改回 rationale gaps，并在本节附决策记录。

## 12. MVP 验收清单

合并到主线发布 MVP candidate tag 前，下列全部为 ✓：

1. `cargo test --workspace` 全过。
2. `cargo clippy --workspace --all-targets -- -D warnings` 全过。
3. FakeProvider 集成测试覆盖：读 → 改 → 测 → 报告的完整流程。
4. DeepSeek smoke 在示例仓库一次跑通；report.md 含 diff、验证命令与结果。
5. CompletionGate 6 条规则全部有测试。
6. EventLog 损坏恢复测试通过。
7. 路径安全 8 条规则全部有测试。
8. 4 种 RunMode 都有集成测试。
9. `doctor` 在缺 `git` / 缺 `rg` / 缺凭据三种情况都能给出可执行诊断。
10. 文档 [kuncode-agent-harness-design.md](../specs/kuncode-agent-harness-design.md) 与本文档之间的实现名一致；偏差全部消解。

任何一条不过，不发 MVP tag。

## 13. 不在 MVP 内的事项

下列禁止在 MVP 周期里"顺手做"。需要时单独立项。

1. TUI / IDE 集成。
2. 项目级 `.kuncode` 配置覆盖。
3. 自定义 policy profile 文件。
4. 自定义工具加载 / plugin。
5. Trace replay。
6. 多 provider 路由。
7. Cost 看板与计费导出。
8. 模型自动 compact。
9. Web UI。

清单变更走 PR，PR 描述里必须解释为什么不能等到 MVP 之后。
