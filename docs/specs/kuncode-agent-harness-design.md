# KunCode Agent Harness 总体设计

## 1. 文档职责

本文档定义 KunCode 的总体设计。

KunCode 的定位很明确：它不是发明一套新的 agent 机制，而是把 `shareAI-lab/learn-claude-code` 展示的 coding agent harness patterns，用 Rust 实现为一个模型无关、类型化、可审计、可扩展的生产级 runtime。

本文档回答：

1. KunCode 要实现什么。
2. KunCode 如何运作。
3. `learn-claude-code` 的哪些机制被吸收。
4. KunCode 在工程化上增加哪些边界。
5. MVP 应该实现哪些最小垂直切片。

本文档不回答：

1. 具体 Rust 模块文件名。
2. 每个 phase 的任务拆分。
3. 具体 provider 配置字段。
4. UI 文案和交互细节。
5. 压缩、redaction、权限阈值等实现参数。

这些细节由 MVP 设计文档和开发计划确定。

## 2. 项目定位

KunCode 是一个 Rust-native coding agent harness runtime。

它的目标不是让模型更聪明，而是给模型一个可靠的软件工程运行环境：

1. 模型负责推理、规划和决定下一步动作。
2. KunCode 负责提供工具、上下文、权限、执行环境、状态和审计。
3. 用户通过 CLI/TUI/API 提交目标、确认权限、查看过程和结果。

KunCode 的核心价值是工程化：

1. 用 Rust 类型系统表达 harness 协议。
2. 用结构化事件记录运行事实。
3. 用 workspace 和 execution lane 控制文件与命令边界。
4. 用 capability 和 policy 控制工具可见性与执行权限。
5. 用 context rebuild 管理模型每轮看到的工作集。
6. 用 artifact 保存长输出和可回读证据。
7. 用 completion gate 防止模型自称完成。

## 3. 与 learn-claude-code 的关系

`shareAI-lab/learn-claude-code` 是 KunCode 的主要机制参考。

该仓库把 coding agent harness 拆成一组逐步叠加的机制：

1. `s01`: agent loop，模型请求工具，runtime 执行并回填结果。
2. `s02`: tool dispatch，新增工具只增加 handler，不改 loop。
3. `s03`: TodoWrite，复杂任务需要显式步骤和 stale todo reminder。
4. `s04`: subagent，子任务使用独立 context。
5. `s05`: skill loading，知识按需加载，而不是全塞进 system prompt。
6. `s06`: context compact，micro compact、auto compact、manual compact。
7. `s07`: task system，任务持久化并支持依赖。
8. `s08`: background tasks，慢操作后台运行并通过 notification 回注。
9. `s09-s11`: teams，teammate、mailbox、协议化 request-response、auto-claim。
10. `s12`: worktree isolation，task 是控制平面，worktree 是执行平面。

KunCode 不复制这些 Python 教学代码。KunCode 做的是生产化转换：

1. dispatch map -> typed Tool registry。
2. TodoWrite -> TaskBoard + ReminderBlock + completion gate。
3. skill loading -> SkillIndexBlock + load_skill + SkillBlock。
4. compact -> ContextSet rebuild + invariant blocks + artifact references。
5. background notification queue -> Job events + event-to-context bridge。
6. mailbox JSONL -> typed MailboxMessage。
7. worktree isolation -> ExecutionLane。
8. teaching event stream -> EventEnvelope + schema version + durable event log。
9. ad hoc permission checks -> CapabilitySet + PolicyDecision。
10. shell/file helpers -> Workspace boundary + ToolRuntime。

因此，KunCode 的“创新点”不是新的 agent 理论，而是把这些 harness patterns 做成可维护、可测试、可恢复、可审计的 Rust runtime。

## 4. 一句话架构

KunCode 的主路径是：

```text
UserGoal
  -> Run
  -> LeadAgent
  -> ContextSet
  -> ModelRequest
  -> ModelResponse
  -> ToolRequest
  -> Validate
  -> CapabilityCheck
  -> PolicyDecision
  -> ToolExecution
  -> ToolResult
  -> EventLog / Artifact / TaskBoard
  -> next ContextSet
  -> FinalReport
```

核心心智模型：

```text
Run 管目标
Agent 管推理者
ContextSet 管模型看到什么
ToolRequest 管模型想做什么
CapabilitySet 管模型能请求什么
Policy 管这次能不能做
Workspace/Lane 管在哪里做
ToolResult 管做完看到什么
EventLog 管事实如何解释
TaskBoard 管进度和证据
CompletionGate 管能不能说完成
```

## 5. 端到端运行机制

以用户输入“修复 cargo test 失败，并总结改动”为例。

### 5.1 创建 Run 和 Lead Agent

用户输入进入 CLI。CLI 不直接调用模型，也不直接执行命令，而是把目标提交给 runtime。

Runtime 创建：

```text
Run {
  run_id
  user_goal
  workspace_id
  default_lane_id
  status: Running
}
```

然后创建默认 `Lead` agent：

```text
Agent {
  agent_id
  role: Lead
  capability_set: MVP_LOCAL
  lane_id: default_lane_id
}
```

初始化事件：

```text
run.started
agent.created
user.goal_received
```

### 5.2 构建 ContextSet

模型不会收到完整仓库、完整历史或所有日志。Context Engine 为当前 turn 构建预算内 `ContextSet`：

```text
SystemBlock       // KunCode 固定规则
IdentityBlock     // 当前 agent、role、lane、permission mode
GoalBlock         // 用户目标
TaskBlock         // 当前步骤和阻塞项
SkillIndexBlock   // 可用知识源，例如 KUNCODE.md、README
DiffBlock         // 当前 workspace 修改摘要
```

ContextSet 是模型当前看到的工作集，不是事实数据库。

### 5.3 调用模型

Provider Adapter 把 ContextSet 渲染成 provider request。

模型可能返回自然语言，也可能请求工具：

```text
exec_argv(["cargo", "test"])
read_file("src/main.rs")
search("fn parse")
```

这些只是请求，还没有被信任，也没有被执行。

### 5.4 校验、权限和执行

每个 ToolRequest 必须经过 runtime：

```text
ToolRequest
  -> tool exists?
  -> input schema valid?
  -> capability allows?
  -> lane valid?
  -> policy allow/deny/ask?
  -> execute
```

例如 `cargo test` 会形成：

```text
PolicyInput {
  tool_name: exec_argv
  effects: [ExecuteProcess]
  lane_id: main_workspace
  input_summary: "cargo test"
  risk_flags: [long_running]
}
```

Policy 返回：

```text
Allow
Deny(reason)
Ask(prompt, choices)
```

工具执行后产生结构化结果：

```text
ToolResult {
  ok
  summary
  inline_content
  content_ref
  error
  metadata
}
```

长 stdout/stderr 不无限塞回模型，而是摘要加 artifact reference。

### 5.5 记录事实

运行过程进入 EventLog：

```text
tool.requested
policy.allowed
tool.started
tool.failed
artifact.created
verification.failed
```

EventLog 是审计事实解释。CLI 渲染、测试断言、恢复、benchmark 都消费这套事件协议。

### 5.6 重建下一轮 ContextSet

下一轮不是简单 append 全部聊天历史。Context Engine 重新收集当前事实：

```text
SystemBlock
IdentityBlock
GoalBlock
TaskBlock
CommandBlock(cargo test failed, key lines)
FileBlock(relevant source)
DiffBlock(current diff)
SummaryBlock(optional)
```

这允许 KunCode 保持长期任务上下文稳定，而不是让模型被旧输出、旧搜索结果和过期失败信息淹没。

### 5.7 修改、验证和完成

模型请求读文件、搜索、patch、再次测试。

所有写操作都通过 Workspace boundary：

1. 路径不能越出 workspace。
2. symlink 不能越界。
3. 写入要记录事件。
4. diff 要能作为 evidence。

当模型最终返回完成时，KunCode 不直接相信它。CompletionGate 检查：

1. 是否有文件变更证据。
2. 是否有验证命令和结果。
3. 是否有未完成 TaskStep。
4. 如果没有验证，最终报告是否说明原因。
5. 是否还有权限阻塞或用户输入阻塞。

通过后，Run 才进入 `Completed`，并生成最终报告。

## 6. 核心原语

### 6.1 Run

一次用户目标的执行实例。

```text
Run {
  run_id
  user_goal
  root_agent_id
  workspace_id
  default_lane_id
  status
  budgets
  created_at
  ended_at
}
```

状态：

1. `Running`
2. `Completed`
3. `Failed`
4. `Blocked`
5. `Cancelled`
6. `BudgetExceeded`

Budgets 不是单个数字，而是一组停止条件：

1. `max_turns`: 最大模型轮次。
2. `max_wall_time`: 最大运行时间。
3. `max_tool_calls`: 最大工具调用数。
4. `max_context_tokens`: 单轮 active context 上限。
5. `max_model_tokens`: 总模型 token 预算。
6. `max_cost`: 可选成本预算。

设计取舍：预算采用“任一硬限制触发即停止”的保守策略。原因是 agent loop 的失败模式通常是无限修正、无限工具调用或上下文膨胀；让预算成为多个独立 hard stop，比把它们折算成一个总分更容易解释和测试。MVP 至少实现 `max_turns`、`max_wall_time` 和 `max_context_tokens`；成本预算可以等 provider token/cost metadata 稳定后再启用。

### 6.2 Agent

一个模型执行者。Subagent、teammate、reviewer 都是不同 role 的 Agent。

```text
Agent {
  agent_id
  run_id
  role
  provider_profile
  capability_set
  context_policy
  lane_id
  mailbox_id
  status
}
```

Role 示例：

1. `Lead`: 默认主 agent。
2. `Explorer`: 只读探索。
3. `Worker`: 可写实现。
4. `Verifier`: 执行测试和诊断。
5. `Reviewer`: 只读审查。

Role 不直接授权。Role 选择 capability set，policy 仍然逐次裁决。

### 6.3 Turn

一次模型调用和随后工具结果的集合。

```text
Turn {
  turn_id
  run_id
  agent_id
  context_set_id
  model_request_meta
  model_response
  tool_requests
  tool_results
  finish_signal
}
```

完整 provider request 默认不落盘。必须记录 request meta：provider、model、消息数量、工具数量、估算 token、context_set_id。

### 6.4 ToolRequest 和 ToolResult

```text
ToolRequest {
  request_id
  tool_name
  input
  requested_by_agent
  lane_id
  capability
  risk_flags
}

ToolResult {
  request_id
  ok
  summary
  inline_content
  content_ref
  error
  metadata
}
```

工具失败必须保留错误分类和可读摘要，不能只返回普通字符串。

### 6.5 EventEnvelope

```text
EventEnvelope {
  schema_version
  event_id
  run_id
  agent_id
  turn_id
  timestamp
  kind
  payload
}
```

`kind` 是事件类型。`payload` 只放事件数据，不重复包一层 kind。

Reader 遇到未知事件类型必须保留 envelope 信息并给出诊断。Event log 损坏时必须报告损坏位置，不能静默跳过。

## 7. Runtime 组件

### 7.1 Runtime Kernel

Runtime Kernel 负责调度主流程：

1. 创建 Run 和 Agent。
2. 驱动 agent loop。
3. 调用 Context Engine。
4. 调用 Provider Adapter。
5. 转交 ToolRequest 给 ToolRuntime。
6. 写 EventLog。
7. 更新 TaskBoard。
8. 生成 FinalReport。

### 7.2 Context Engine

Context Engine 每轮重新构建 ContextSet。它从 EventLog、Workspace、Artifact、TaskBoard 和 Job/Mailbox notification 中取事实，再按预算渲染给模型。

它不只是压缩器，而是模型观察世界的窗口。

### 7.3 ToolRuntime

ToolRuntime 负责：

1. 工具注册。
2. schema 校验。
3. capability check。
4. policy 调用。
5. execution lane 解析。
6. 工具执行。
7. ToolResult 结构化。

Agent loop 不应该为每个工具写分支。

### 7.4 Policy Engine

Policy Engine 只做决策，不执行工具，不读写终端。

用户交互通过 `UserInteraction` 完成。CLI、TUI、API、测试和 non-interactive 模式共享同一 contract。

### 7.5 Workspace Manager

Workspace Manager 是所有文件和 cwd 访问的入口。文件工具、search、patch、git helper、shell cwd 都必须通过它或 ExecutionLane 校验。

### 7.6 执行模型和并发

MVP 采用单进程 Rust async runtime。Agent loop、provider 调用、event sink、policy、UserInteraction 和 tool execute 都按 async 边界设计。能使用 runtime 原生 async API 的地方优先使用 async API：普通文件读写走 async 文件接口，shell/git 子进程走 async process 接口。需要注意的是，不同 runtime 对文件 IO 的实现可能仍依赖后台 blocking pool；如果使用同步库、CPU 密集逻辑、`git2` 这类 blocking API，或没有合适 async wrapper 的操作，必须显式放入受控 blocking pool 或专用线程池，不能直接阻塞 executor。具体 runtime 选型（例如 Tokio）由 MVP 设计文档确定。

MVP 不实现 parallel tool calls。即使 provider 一次返回多个 tool request，runtime 也按顺序校验、授权和执行；因此 MVP 中 `max_parallel_requests` 固定为 `1`。长期并发可以在不改变 tool 协议的前提下引入：

1. 只读工具可以并行。
2. 同一 ExecutionLane 的写操作串行。
3. 不同 worktree lane 的写操作可以并行。
4. PolicyDecision 仍然逐个 ToolRequest 产生。

设计取舍：先选择单进程 async，而不是多进程 worker，是为了降低 MVP 的状态同步和恢复复杂度，同时让取消、timeout、provider IO、event 写入共享同一调度模型。需要跨线程或后台任务持有的 trait object 应满足 `Send + Sync`；具体 runtime crate 和 blocking pool 配置由 MVP 设计文档确定。

## 8. Tool、Capability 和 Policy

### 8.1 Tool 定义

```text
Tool {
  name
  description
  input_schema
  output_schema
  effects
  default_capability
  execute(input, ToolContext) -> ToolResult
}
```

`effects` 描述工具可能影响：

1. `ReadWorkspace`
2. `WriteWorkspace`
3. `ExecuteProcess`
4. `Network`
5. `ReadArtifact`
6. `WriteArtifact`
7. `ModifyTaskBoard`
8. `SendMailbox`
9. `Destructive`
10. `AskUser`

### 8.2 CapabilitySet

CapabilitySet 控制 agent 能看到和请求哪些工具。

```text
CapabilitySet {
  allowed_tools
  allowed_effects
  default_lane
  max_parallel_requests
  budget_overrides
}
```

示例：

1. `Explore`: read/search/git diff。
2. `Edit`: read/search/patch/write。
3. `Verify`: shell test/build/git status。
4. `Lead`: task、mailbox、delegation、summary。
5. `FullLocal`: 本地手动模式的完整能力。

CapabilitySet 是工具可见性和默认限制。Policy 是运行时授权。二者不能合并。

### 8.3 PolicyDecision

```text
PolicyInput {
  run_id
  agent_id
  tool_name
  effects
  lane_id
  input_summary
  risk_flags
  mode
}

PolicyDecision {
  Allow
  Deny(reason)
  Ask(prompt, choices)
}
```

Non-interactive 模式不能阻塞等待用户输入。如果 policy 结果是 `Ask`，runtime 必须按模式转换为 `Deny` 或受控 `Allow`，并写入事件。

KunCode 至少区分四种运行模式：

1. `interactive`: 默认模式，`Ask` 通过 UserInteraction 询问用户。
2. `non_interactive`: 自动化模式，`Ask` 转为 `Deny`。
3. `yes`: 用户显式信任当前 run，低风险 `Ask` 可以转为 `Allow`，destructive/network 仍可单独限制。
4. `dry_run`: 不执行会改变 workspace、网络或外部状态的工具，只记录将要执行的请求。

设计取舍：把 mode 放进 PolicyInput，而不是散落在 CLI flag 判断中，是为了让同一个 policy contract 可用于 CLI、CI、测试和未来 API。`yes` 不是无条件 root 权限；它只是改变 Ask 的默认处理，高风险 effect 仍由 policy profile 决定。

## 9. Shell 和 Git

Shell 是工具，不是逃生口。

KunCode 支持两类 shell：

1. `exec_argv`: 直接执行 argv，适合测试、构建、格式化、git helper 等结构化命令。
2. `shell_command`: 通过 shell 解释字符串，适合用户明确需要管道、重定向、glob、环境变量或复合命令的场景。

默认优先 `exec_argv`。`shell_command` 必须有更清晰的展示、更严格的 policy 和更保守的 risk flags。

所有 shell 执行必须：

1. 绑定 ExecutionLane。
2. 限制 cwd。
3. 设置 timeout。
4. 捕获 exit code、stdout、stderr、duration。
5. 截断或 artifact 化长输出。
6. 进入 EventLog。

Git 不应只是任意 shell 别名。常用 git 操作应优先提供受控 helper，例如 status、diff、apply、branch/worktree 查询。高风险 git 操作进入 policy。

## 10. Workspace 和 ExecutionLane

### 10.1 Workspace

Workspace 是代码目录安全边界，不保存 KunCode runtime 状态。

Workspace Manager 负责：

1. canonicalize path。
2. 拒绝绝对路径越界。
3. 拒绝 `..` 越界。
4. 拒绝 symlink 越界。
5. 区分读路径和写路径策略。
6. 限制大文件、二进制和非 UTF-8 文件。
7. 为 search 和 git pathspec 提供统一过滤规则。

### 10.2 ExecutionLane

ExecutionLane 是工具实际执行的位置。

```text
ExecutionLane {
  lane_id
  kind
  root_path
  workspace_id
  task_id
  status
}
```

Lane kind：

1. `MainWorkspace`
2. `GitWorktree`
3. `ReadOnlySnapshot`
4. `FutureRemoteSandbox`

MVP 只需要 `MainWorkspace`。长期多 agent 和并行写入依赖 `GitWorktree`。

### 10.3 Worktree closeout

Worktree lane 结束时必须 closeout：

1. `keep`: 保留 worktree 供用户检查。
2. `remove`: 删除 worktree。
3. `promote`: 合并、复制或生成 patch，具体实现期定义。

closeout 决策必须进入 EventLog。绑定 task 未完成时，不允许静默删除 lane。

## 11. ContextSet 和 Compact

### 11.1 ContextBlock

Context block 类型：

1. `SystemBlock`: harness 固定规则。
2. `IdentityBlock`: agent role、lane、permission mode。
3. `GoalBlock`: 用户目标。
4. `TaskBlock`: task board 和 active steps。
5. `ReminderBlock`: stale task、权限阻塞、需要验证等提醒。
6. `SkillIndexBlock`: 可加载知识源列表。
7. `SkillBlock`: 已加载 skill 或文档内容。
8. `FileBlock`: 文件片段。
9. `SearchBlock`: 搜索结果。
10. `CommandBlock`: 命令摘要和关键输出。
11. `DiffBlock`: 当前修改摘要。
12. `NotificationBlock`: 后台 job、mailbox、重要事件通知。
13. `SummaryBlock`: 压缩摘要。

### 11.2 每轮重建

每次模型调用前都重建 ContextSet：

1. 固定加入 SystemBlock、IdentityBlock、GoalBlock。
2. 刷新 task state、diff、最近验证结果。
3. 注入未消费的重要 event notification。
4. 加入最近关键 tool results。
5. 按预算选择 file/search/command blocks。
6. 必要时用 summary 和 artifact reference 替换长内容。

ContextSet 可以有损，但不能丢掉这些不变量：

1. 当前用户目标。
2. agent 身份和 role。
3. 当前 lane。
4. permission mode。
5. active task 和 open blockers。
6. 最近失败验证。
7. 当前 workspace 修改摘要。
8. 用户明确约束。

设计取舍：KunCode 选择每轮重建 ContextSet，是为了避免模型基于过期文件片段、旧搜索结果或已失效验证继续推理。代价是 prompt cache 命中率可能下降。为降低成本，renderer 应保持静态前缀稳定：SystemBlock、provider tool schema、固定项目规则和 SkillIndexBlock 的排序尽量不变；动态 blocks 放在后部。MVP 接受部分 cache miss 换取上下文正确性，但必须在 render meta 中记录 block 顺序、token 估算和被裁剪内容，便于后续优化 cache 策略。

### 11.3 Compact

Compact 分三层：

1. Micro compact: 每轮可执行，清理旧长 tool results，保留摘要或引用。
2. Auto compact: 超过预算阈值触发，生成 SummaryBlock。
3. Manual compact: 用户或模型请求触发。

Compact 后必须重新注入 identity、goal、task、lane、permission mode 等 invariant blocks。不能只依赖摘要让 agent 记住自己是谁、在哪个目录、在做什么。

### 11.4 Skill 两阶段加载

KunCode 不把所有项目知识塞进 system prompt。

两阶段机制：

1. `SkillIndexBlock`: 暴露 skill name、description、trigger、source。
2. `load_skill` 工具：按需加载完整 skill，产生 `SkillBlock`。

MVP 可以先把 `KUNCODE.md`、README 和 docs index 视作项目知识源，完整 skill loader 后续实现。

## 12. TaskBoard 和 CompletionGate

### 12.1 TaskBoard

TaskBoard 表示 run 内的进度状态。

```text
TaskStep {
  id
  title
  status
  rationale
  evidence
  related_files
  verification
}
```

状态：

1. `Pending`
2. `InProgress`
3. `Completed`
4. `Failed`
5. `Skipped`
6. `Blocked`

复杂任务应有步骤。默认同一 agent 同一时间只应有一个 in-progress step。

### 12.2 Stale reminder

吸收 `learn-claude-code` 的 todo nag 思路：当存在 open step，且连续 N 轮没有 step 状态变化时，Context Engine 注入 `ReminderBlock`。

Reminder 不改变任务状态，只提示模型：

1. 更新当前步骤。
2. 记录阻塞原因。
3. 标记验证结果。
4. 如无需继续，解释完成证据。

N 是实现期参数。

### 12.3 CompletionGate

最终报告必须通过完成门禁：

1. 只读分析任务需要读取、搜索或推理依据。
2. 写入任务需要 diff 或文件变更依据。
3. 修复、测试、构建、格式化任务需要验证命令、时间和结果。
4. 用户要求跳过验证时，最终报告必须说明未验证。
5. 验证无法运行时，最终报告必须说明原因和剩余风险。
6. 模型自称完成不是证据。

## 13. EventLog、Artifact 和 Storage

MVP 的 EventLog 存储格式采用 JSONL。长期可以增加 SQLite index 或迁移到 SQLite 作为主存储，但事件协议本身不绑定某一种物理存储。

设计取舍：JSONL 的优势是实现成本低、append-only 语义清楚、容易 `tail`、容易人工检查，也方便在早期快速写 golden tests。它的弱点是随机读、范围查询和损坏恢复不如 SQLite。KunCode 因此把 JSONL 定位为 MVP 的 durable log，而不是永远唯一的查询层；reader 必须能报告损坏行号和 event id，未来可以在 JSONL 之上建立 SQLite index。

### 13.1 Event categories

长期事件类别：

1. `run.*`
2. `agent.*`
3. `turn.*`
4. `context.*`
5. `model.*`
6. `tool.*`
7. `policy.*`
8. `workspace.*`
9. `lane.*`
10. `task.*`
11. `job.*`
12. `mailbox.*`
13. `artifact.*`
14. `final_report.*`

### 13.2 Artifact

Artifact 保存长输出、大文件、二进制内容、完整命令日志和不适合进入 active context 的内容。

```text
Artifact {
  artifact_id
  run_id
  kind
  size
  content_hash
  redaction_state
  source_event_id
  source_tool_request_id
}
```

Artifact 必须通过 id 被 event 和 context block 引用，不能只暴露裸文件路径。

### 13.3 Secret 策略

Provider secret、API key、认证 token 和已知 secret 环境变量不得落入 EventLog 或 Artifact。完整 provider request 默认不落盘。

普通工具输出和源码片段需要经过 redaction policy，但未知 secret 的识别只能是 best effort，不能作为绝对安全能力承诺。

### 13.4 KunCode Home

KunCode home 保存 runtime 状态，不写入 workspace。

```text
$KUNCODE_HOME/
  config.toml
  runs/
    <run-id>/
      events.jsonl
      artifacts.jsonl
      artifacts/
      metadata.json
      taskboard.json  // Phase 4 预留，Phase 1 不创建
  cache/
```

Phase 1 的实际落地只创建 `events.jsonl`、`artifacts.jsonl`、`artifacts/` 和 `metadata.json`。`taskboard.json` 属于 TaskBoard 落地时的预留路径，不能由其他 crate 手写；run 目录文件布局通过 `RunDir` 统一封装。

项目知识文件可以放在 workspace，例如 `KUNCODE.md`，但 run 状态和 artifacts 不写入 workspace。

## 14. Jobs 和 Notification

`Job` 表示后台运行的长操作。

```text
Job {
  job_id
  run_id
  agent_id
  lane_id
  command_or_action
  status
  started_at
  finished_at
  result_ref
}
```

后台 job 不直接修改 messages。流程是：

1. job started 进入 EventLog。
2. job completed/failed 进入 EventLog。
3. Context Engine 每轮扫描未消费的重要 job event。
4. 需要时注入 `NotificationBlock`。
5. 被注入后记录 consumed marker 或 last_seen event id。

这把 `learn-claude-code` 的 notification queue 提升为 event-to-context bridge。

## 15. Mailbox 和多 Agent

多 agent 不是 MVP 前置条件，但架构应从第一天允许多个 Agent。

```text
MailboxMessage {
  message_id
  run_id
  from_agent
  to_agent
  message_type
  correlation_id
  payload
  created_at
}
```

长期 message types：

1. `message`
2. `broadcast`
3. `task_claim`
4. `plan_request`
5. `plan_response`
6. `handoff`
7. `shutdown_request`
8. `shutdown_response`

Mailbox 是 agent 间通信协议。EventLog 是审计协议。二者可以互相引用，但不能混成一个概念。

Subagent 是短生命周期 Agent。Teammate 是长生命周期 Agent。它们共享 Agent 原语，只是 role、capability、context policy 和 mailbox 行为不同。

## 16. Provider Adapter

Provider Adapter 隔离模型 API 差异。

Provider capability：

1. tool calling。
2. structured output。
3. streaming。
4. max context tokens。
5. parallel tool calls。
6. cost/token metadata。
7. reasoning controls。

Provider capability 不匹配时采用显式 fallback 策略：

1. 不支持 parallel tool calls：runtime 序列化执行。
2. 不支持 streaming：runtime 等完整响应，但 cancel 只能尽力 abort in-flight request。
3. 不支持 cost/token metadata：成本预算降级为 unknown，不参与 hard stop。
4. 不支持 reasoning controls：忽略该配置，不影响基础 agent loop。
5. 不支持 tool calling：不能运行 KunCode 的 tool loop，除非通过后续 text-to-tool shim 明确降级；MVP 直接报错。
6. 不支持足够 context window：Context Engine 必须降低预算或拒绝启动。

设计取舍：provider-neutral 不等于所有 provider 都能完整运行。KunCode 只对不破坏核心语义的能力做降级；会破坏 tool loop、context budget 或安全边界的能力缺失必须 hard fail。

Provider Adapter 负责：

1. 把 ContextSet 渲染结果转成 provider payload。
2. 把 provider response 转成统一 model response。
3. 把 tool calls 转成 ToolRequest。
4. 把 provider error 转成结构化 model error。
5. 避免 secret 进入 EventLog。
6. 支持 cancel token。

Provider Adapter 不知道 workspace、policy UI、event sink 或 artifact 文件布局。

## 17. 错误处理

领域错误：

1. `ModelError`
2. `ToolError`
3. `PolicyError`
4. `WorkspaceError`
5. `LaneError`
6. `ContextError`
7. `EventLogError`
8. `ArtifactError`
9. `TaskBoardError`
10. `ConfigError`

核心 public API 不返回未分类 catch-all 字符串。CLI 可以把领域错误转成用户可读诊断，但不能丢掉原始分类。

Agent 面对错误时：

1. 可恢复工具错误回填给模型。
2. 权限错误按 policy 和 interaction mode 处理。
3. provider 临时错误有限重试。
4. 连续重复失败停止并报告。
5. 验证失败进入诊断或最终风险报告。

## 18. MVP 收窄

MVP 目标是跑通一个可信的单 agent 垂直切片，不是实现全部长期机制。

MVP 必须保留的架构不变量：

1. 有 Run。
2. 有 Lead Agent。
3. 有最小 agent loop。
4. 有 ToolRequest/ToolResult 结构。
5. 有 EventEnvelope 和 JSONL EventLog。
6. 有 Workspace 和 MainWorkspace lane。
7. 有 PolicyDecision。
8. 有 ContextSet rebuild。
9. 有 Provider Adapter。
10. 有 CompletionGate。

MVP 推荐实现：

1. `FakeProvider` 用于 deterministic integration tests。
2. 一个真实 provider adapter 用于 smoke validation。
3. file read/search。
4. patch/write。
5. shell exec with timeout。
6. git status/diff。
7. minimal TaskBoard。
8. minimal artifact/reference。
9. non-interactive UserInteraction。
10. config/doctor。

MVP 暂缓：

1. 多 agent。
2. mailbox。
3. background jobs。
4. worktree lane。
5. full skills loader。
6. model-assisted compact。
7. resume/replay。
8. streaming。
9. parallel tool calls。

## 19. 测试策略

测试围绕原语边界，而不是 stdout。

1. Unit tests: workspace path safety、tool schema、policy decision、context budget、event schema。
2. Integration tests: fake provider 驱动 agent loop，验证工具执行、事件、context、completion gate。
3. Golden tests: context rendering、prompt rendering、compact summary。
4. Provider tests: HTTP mock 和真实 smoke validation。
5. Storage tests: event log 损坏、artifact 缺失、unknown event kind。
6. Local benchmark: 固定仓库、固定任务、固定验收命令。

验收原则：

1. 没有事件证据，不声称 runtime 做过某事。
2. 没有 workspace 校验，不允许访问本地路径。
3. 没有 policy decision，不执行高风险动作。
4. 没有 context budget，不允许长任务进入 agent loop。
5. 没有 completion evidence，不声称任务完成。

## 20. 设计结论

KunCode 的设计结论是：

```text
learn-claude-code 给出 harness mechanisms。
KunCode 给出 Rust-native production runtime。
```

KunCode 当前阶段的价值不是机制原创，而是把已经清晰的 coding agent harness patterns 做成一套工程上可靠的系统：

1. 类型化协议。
2. 稳定事件边界。
3. 可审计 session。
4. 可控工具执行。
5. 可恢复上下文生命周期。
6. provider-neutral 模型适配。
7. workspace/worktree execution boundary。
8. completion evidence discipline。

如果未来 KunCode 要形成自己的机制创新，应建立在这套 runtime 稳定之后，例如：

1. 更强的 local benchmark 和 trace replay。
2. 更细粒度的 policy profile。
3. 更好的 context selection algorithm。
4. 更可靠的 multi-agent scheduling。
5. 更严格的 workspace sandbox。

这些不应混入 MVP 的基础 harness 目标。
