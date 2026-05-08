# Kuncode Agent Harness 总体设计

## 1. 文档职责

本文档描述 Kuncode 的长期 harness 架构、运行机制、核心边界和稳定协议。它不是 MVP 开发计划，也不负责列出每个阶段的具体文件改动。

文档分层规则：

1. 总体设计文档定义长期机制、概念边界和稳定协议名。
2. MVP 设计文档定义 MVP 阶段的 Rust 实现名、模块名和文件落点。
3. MVP 开发计划和 Phase 计划只拆任务，不重新发明架构名或实现名。
4. 如果开发中需要新增或修改实现名，先更新 MVP 设计文档，再同步计划文档。

本文参考 `shareAI-lab/learn-claude-code` 中的 harness 工程实践，但不照搬其教程结构或代码。Kuncode 采用 Rust 原生 runtime、结构化事件、明确权限边界和可审计 session 状态来实现同类机制。

## 2. 设计目标

Kuncode 是一个开源、模型无关、可审计、可扩展的 Coding Agent harness。它的目标不是让模型“更聪明”，而是给模型一个可靠的软件工程运行环境。

核心目标：

1. 支持真实仓库中的开发任务：读代码、搜索、编辑、运行命令、运行测试、解释结果、总结变更。
2. 提供稳定的 agent loop：每一轮有输入、有模型响应、有工具调用、有工具结果、有状态转移、有退出条件。
3. 提供高质量工具协议：工具小而清晰，输入输出结构化，错误结构化，权限可控。
4. 管理上下文生命周期：避免只靠不断增长的聊天历史，支持 typed context blocks、预算裁剪和压缩摘要。
5. 提供事件溯源：所有关键动作进入 event stream，用于 CLI 渲染、JSONL 持久化、测试断言、恢复、审计和 benchmark。
6. 支持工程验证闭环：没有验证就不声称完成；无法验证时最终报告必须说明原因。
7. 支持长期演进：后续可以加入 skills、子 agent、后台任务、mailbox、worktree 隔离、TUI、IDE、benchmark。

非目标：

1. 不训练模型。
2. 不默认执行高风险命令、删除文件、重置 git、推送远端。
3. 不把某个现有产品的 UI 或文案逐字复刻。
4. 不在单 agent loop 稳定前引入复杂多 agent 编排。

## 3. 借鉴原则

`learn-claude-code` 的关键价值不是某段 Python 代码，而是逐层增加 harness 机制的方式。Kuncode 采用其中的实践原则：

1. 最小循环不变：用户输入、模型响应、工具执行、工具结果回填，直到模型不再请求工具。
2. 工具扩展不改循环：新增工具应该注册到工具表，而不是改 agent loop 主体。
3. 先计划再执行：复杂任务需要可见步骤和状态，不应只靠模型自由发挥。
4. 知识按需加载：项目规则、skills、文档等应在需要时进入上下文，而不是全部塞进 system prompt。
5. 上下文必须压缩：旧工具结果、长输出、历史对话必须有分层压缩策略。
6. 状态要持久化：长任务、恢复、审计和多 agent 协作都不能只依赖内存会话。
7. 执行目录要隔离：长期需要把任务目标和执行目录分开，worktree 是后续多任务隔离的基础。

这些原则会转化为 Kuncode 的 Rust runtime、事件协议、workspace 安全层和 session 状态模型。

## 4. 总体架构

```text
CLI / TUI / API
      |
Interface Renderer / UserInteraction
      |
Session Manager
      |
Agent Loop Engine
      |
+----------------------+----------------------+
| Context Manager      | Tool Runtime         |
| Task State           | Workspace Manager    |
| Event Stream         | Permission Engine    |
| Prompt Renderer      | Provider Adapter     |
+----------------------+----------------------+
      |                       |
      |                       +--> Event Consumers
      |                            - CLI renderer
      |                            - Event log writer
      |                            - Test memory sink
      |                            - Future TUI/API/IDE
      |
      +--> Model Providers / Workspace / Git / Shell
```

模块边界：

1. `agent`: session、turn、agent loop、task state、finish reason。
2. `interface`: CLI renderer、TUI/API adapter、user interaction、event sinks。
3. `events`: runtime 对外暴露过程的纯事件协议。
4. `event_log`: event log writer、reader、JSONL 持久化。
5. `context`: context blocks、预算、压缩、prompt rendering。
6. `model`: provider 抽象、消息格式、tool call 格式。
7. `tools`: tool trait、tool registry、tool runtime、内置工具。
8. `workspace`: workspace root、路径安全、文件读写、git 访问。
9. `policy`: 权限等级、确认策略、命令限制。
10. `config`: 配置文件、环境变量、CLI 参数解析。

依赖方向：

```text
CLI/interface
  -> config
  -> agent
  -> events

agent
  -> context
  -> model
  -> tools
  -> policy
  -> events

tools
  -> workspace
  -> policy
  -> events

interface/event sinks
  -> events

event_log
  -> events
```

核心约束：

1. Agent loop 不直接打印用户可见输出。
2. Tool runtime 不直接绕过 workspace 访问本地路径。
3. Provider adapter 不知道 workspace、CLI 或 event log 的具体实现。
4. Renderer、event log、测试 sink、未来 TUI/API 都消费同一套 `EventEnvelope`。
5. `events` 是纯协议层，不依赖 renderer、event sink、JSONL writer、CLI 或 workspace。
6. Event sink 和 event log writer 是 `EventEnvelope` 的消费者，不是事件协议的拥有者。

## 5. Agent Loop

### 5.1 最小循环

Kuncode 的 agent loop 保持一个清晰的不变式：

```text
user goal
  -> build context
  -> model request
  -> model response
  -> if tool calls:
       validate + execute tools
       append tool results to context
       continue
     else:
       finish
```

循环退出条件不是“跑完一段代码”，而是明确的 finish reason：

1. `Completed`: 任务完成，并有足够证据。
2. `Failed`: 工具、模型或验证失败，无法继续。
3. `BlockedByPermission`: 需要用户授权但无法获得。
4. `NeedsUserInput`: 缺少必要信息，不能安全假设。
5. `BudgetExceeded`: 达到 turn、token、时间或成本预算。
6. `Cancelled`: 用户取消。

取消必须是 cooperative：agent loop、provider 调用、tool execution 和后台任务共享同一个取消 token。长时间操作必须定期观察取消状态；shell 子进程和 HTTP 请求在取消时应尽快终止。

### 5.2 Turn 数据

每一轮 turn 至少包含：

```text
Turn {
  turn_id
  active_context
  rendered_prompt
  model_request_meta
  model_response
  tool_calls
  tool_results
  state_delta
  events
}
```

Turn 必须进入 event stream。完整 provider request 可以受 secret 和体积限制不落盘，但 request meta 必须记录，例如 provider、model、消息数量、工具数量、估算 token。

### 5.3 Tool call 回填

模型返回 tool calls 后，runtime 执行：

1. 解析 tool name 和 input。
2. 查找 tool registry。
3. 校验 input schema。
4. 计算 permission level。
5. 请求 policy 决策。
6. 执行工具。
7. 生成结构化 `ToolResult`。
8. 发布 tool events。
9. 把 tool result 转成下一轮 context 的输入。

工具失败不能变成普通字符串吞掉，必须保留错误分类和可读摘要。

### 5.4 Agent loop 不随工具增长而膨胀

新增工具只应增加：

1. tool definition。
2. input schema。
3. permission classification。
4. handler implementation。
5. tests。

Agent loop 主体不应为每个工具写特殊分支。特殊行为应通过 tool runtime、policy 或 context renderer 的规则表达。

## 6. Tool Runtime

### 6.1 工具协议

工具协议必须结构化：

```text
Tool {
  name
  description
  input_schema
  permission(input) -> PolicyInput
  execute(input, context, cancel) -> ToolResult
}
```

`ToolResult` 至少包含：

```text
ToolResult {
  ok
  summary
  content
  error
  metadata
}
```

`summary` 用于回填 active context；`content` 可以进入 event log 或 artifact；`error` 用于模型修正、agent 诊断和最终报告。

### 6.2 内置工具族

长期内置工具族：

1. File read/list/search。
2. Patch/write。
3. Shell。
4. Git status/diff。
5. Project knowledge/skills。
6. Compact。
7. Ask user。
8. Future background task。
9. Future worktree/task tools。

### 6.3 Shell 不是逃生口

Shell 是一个受控工具，不是绕过 harness 的万能接口：

1. 输入必须是 argv，而不是任意 shell 字符串。
2. cwd 必须通过 workspace 校验。
3. 命令必须有 timeout。
4. stdout/stderr 必须截断和摘要。
5. 高风险命令必须进入 permission policy。
6. exit code、stdout、stderr、duration 必须进入 event stream。

## 7. Permission 和 User Interaction

Permission engine 负责把工具请求转成决策，不直接读写终端。

权限等级：

1. `Read`: 读文件、列目录、git status、git diff。
2. `Write`: apply patch、写文件。
3. `Execute`: shell、测试命令、构建命令。
4. `Network`: 下载依赖、访问远端 API。
5. `Destructive`: 删除、重置、清理、覆盖历史。
6. `Interaction`: 询问用户。

决策来源：

1. 静态 policy。
2. CLI flags，例如 non-interactive 或 yes mode。
3. 用户确认。
4. Future team/company policy profile。

Permission decision 至少包含：

1. `Allow`: 允许执行。
2. `Deny`: 拒绝执行，并给出可记录的原因。
3. `Ask`: 需要用户确认。

Policy input 不能只是 permission level。它必须包含足够上下文供 policy 判断风险：

1. tool name。
2. permission level。
3. workspace/worktree cwd。
4. tool input 摘要。
5. risk flags，例如 destructive、network、writes_outside_workspace、uses_secret、long_running。
6. non-interactive/yes mode。

Non-interactive 模式不能阻塞等待用户输入。如果 policy 结果是 `Ask`，runtime 必须根据模式转换为 `Deny` 或受控 `Allow`，并把决策写入 event stream。

需要用户输入时，runtime 通过 `UserInteraction` 请求，不直接 `stdin.read_line()`。

## 8. Task State

### 8.1 为什么需要 TaskState

复杂任务不能只靠聊天历史。Kuncode 需要显式任务状态来回答：

1. 当前在做什么。
2. 已经完成了哪些步骤。
3. 每个步骤有什么证据。
4. 还有哪些开放问题。
5. 为什么可以结束。

### 8.2 TaskStep

`TaskStep` 是可验证的工程步骤，不是纯 UI todo。

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

约束：

1. 复杂任务应先形成步骤列表。
2. 同一时间默认只有一个 step 处于 `InProgress`。
3. 执行某步前先标记 `InProgress`。
4. 完成某步后立即标记 `Completed`。
5. 失败必须记录证据。
6. 仍有必要步骤未完成时，agent 不应以 `Completed` 结束。

### 8.3 与模型计划的关系

模型可以提出计划，但 runtime 不能完全相信模型自报进度。模型生成的计划必须进入 `TaskState`，后续工具结果、diff、验证命令和 event log 共同构成证据链。

## 9. Context 生命周期

### 9.1 Context 不是 transcript

Kuncode 区分：

1. Transcript: 原始对话、模型响应、工具结果的完整历史。
2. Event log: 可审计的结构化事件流。
3. Active context: 当前 turn 送入模型的预算内上下文。
4. Artifacts: 体积大或二进制内容的外部文件。

Active context 可以被裁剪和压缩；event log 和 artifacts 负责保留可追溯信息。

Context rendering is lossy。也就是说，模型每一轮看到的 active context 不是完整事实记录，而是当前预算内的工作集。Kuncode 的一致性边界必须明确：

1. Event log 是审计事实源，记录 runtime 已经发生过什么。
2. Workspace 是执行事实源，记录代码和 git 状态当前是什么。
3. Artifacts 保存不适合直接进入 active context 的长输出和大内容。
4. Active context 只服务下一轮模型推理，不能作为唯一事实来源。
5. 最终报告不能只依赖摘要，必须结合当前 workspace 状态和验证证据。

Kuncode 不在每一轮把完整 transcript、完整工具输出和完整 artifact 都塞回模型。原因是：

1. 工具输出会快速膨胀，导致成本、延迟和失败概率上升。
2. 旧失败、旧搜索结果和旧文件片段可能已经过期，会干扰当前判断。
3. 模型上下文窗口不是数据库；噪声越多，越容易漏掉当前关键事实。
4. 每轮重复发送完整历史会扩大 secret 和敏感内容暴露面。

正确边界是：完整事实保存在 event log、workspace 和 artifacts；模型每轮只接收当前任务需要的最小充分工作集。需要原文时，通过工具或 artifact reference 回读。

### 9.2 ContextBlock

长期 context block 类型：

1. `SystemBlock`: harness 固定行为和约束。
2. `ProjectBlock`: 项目说明、`KUNCODE.md`、style guide。
3. `TaskBlock`: user goal、task state、open questions。
4. `FileBlock`: 文件片段。
5. `SearchBlock`: 搜索结果。
6. `CommandBlock`: 命令、退出码、摘要、关键输出和可选 artifact reference。
7. `DiffBlock`: 当前修改摘要和关键 diff。
8. `MemoryBlock`: 持久记忆或用户偏好。
9. `SummaryBlock`: 压缩摘要、compact reason 和可选 artifact reference。

长期可以为 block 增加 event references、retention policy、importance 等元数据。但这些属于增强能力，不是 MVP 第一版必须实现的前置条件。

### 9.3 分层压缩策略

借鉴 context compact 的优秀实践，Kuncode 采用每轮轻量管理和阈值触发重压缩结合的策略，而不是每轮塞完整历史，也不是等上下文爆掉后才开始处理。

Every turn: active context rebuild

每次 agent loop 准备调用模型前都要重建 active context：

1. 收集 system constraints、user goal、task state。
2. 刷新当前 workspace 事实，例如 diff 摘要和最近验证结果。
3. 加入最近关键 tool results。
4. 对长输出执行 inline-or-reference 规则。
5. 按优先级和预算选择 blocks。

这一步是轻量整理，不生成大摘要，不改变 event log。

Layer 1: tool result micro compact

1. 每轮渲染前可以执行。
2. 保留最近关键 tool results。
3. 旧的长 tool results 替换为短摘要或 artifact/event reference。
4. 不改变 event log。

Layer 2: threshold auto compact

1. 当 active context 超过预算阈值时触发。
2. 保存完整可追溯信息到 event log/artifacts。
3. 生成 `SummaryBlock`。
4. 重建 active context。

Layer 3: manual compact

1. 模型或用户可以显式请求 compact。
2. 使用与 auto compact 相同的结构化摘要机制。
3. 事件流记录 compact reason 和结果。

MVP 可以先用规则摘要，后续再引入 model-assisted compact。

压缩必须遵守不可丢失规则。下面内容不能被普通 compact 吞掉：

1. 用户当前目标。
2. 当前 task state 和未解决问题。
3. 最近一次失败验证的命令、退出码和摘要。
4. 当前 workspace 修改摘要，例如 git diff summary。
5. 用户明确给出的约束。
6. 权限拒绝或需要用户输入的阻塞状态。

长命令输出、长搜索结果和旧工具结果可以被摘要。Runtime 不尝试判断“摘要是否语义充分”，而是使用保守白名单判断“是否可以安全不保存原文”。只要输出失败、被截断、超过内联阈值、包含诊断标记、搜索命中超过阈值，或没有完整进入 ContextBlock，就必须写入 artifact，并在 block 中保留 reference。

### 9.4 Context 渲染

Prompt renderer 从 blocks 生成 provider-specific model request。Provider adapter 可以转换格式，但不能改变 block 语义。

渲染前应尽量刷新当前事实，而不是完全相信旧摘要。对 coding task，优先刷新 task state、当前 diff、最近验证结果；必要时重新读取相关文件片段。

渲染顺序优先级：

1. System constraints。
2. User goal。
3. Task state。
4. Critical evidence。
5. Current files/diff/search results。
6. Recent tool results。
7. Summaries。

## 10. Event Stream 和持久化

### 10.1 事件驱动

Agent runtime、tool runtime、provider adapter、permission engine 都发布结构化事件。CLI renderer、event log writer、测试 sink、未来 TUI/API 都消费同一 `EventEnvelope` 事件流。

事件流解决三个问题：

1. 用户能看到 agent 在做什么。
2. 测试可以断言结构化行为，而不是脆弱 stdout。
3. 崩溃后能从持久化状态调试或恢复。

### 10.2 EventEnvelope

```text
EventEnvelope {
  schema_version
  id
  session_id
  turn_id
  timestamp
  kind
  data
}
```

`kind` 是事件类型；`data` 只放 payload 字段，不重复包一层 kind。

事件协议需要版本或兼容策略。长期要求：

1. Event log append-only。
2. 新增 event kind 必须向后兼容已有 reader。
3. 删除或重命名 event kind 必须通过 schema migration 或兼容 reader 处理。
4. Reader 遇到未知 event kind 应能保留 envelope 元信息并给出明确诊断。
5. Event log 损坏时，reader 应报告损坏位置；resume/replay 可以选择停止、跳过或进入只读诊断模式，但不能静默丢事件。

### 10.3 事件类别

长期事件类别：

1. Session lifecycle: started、ended、failed。
2. User input: goal received、permission requested、user response。
3. Context: rendered、compacted、budget exceeded。
4. Model: request started、response received、provider failed。
5. Tool: requested、completed、failed。
6. Workspace: file read、patch applied、git diff observed。
7. Task state: step added、step status changed、evidence recorded。
8. Verification: command started、passed、failed。
9. Final report。

### 10.4 Secret 策略

Event log 是 redacted audit source，不是所有原文的无条件完整副本。Event log 不能记录：

1. API key。
2. provider secret。
3. 完整敏感 provider request。
4. 用户明确标记为敏感的内容。

可以记录：

1. provider name。
2. model name。
3. request message count。
4. tool count。
5. estimated token/char count。
6. redacted config state。

Artifact 是长输出、大内容、二进制内容或不适合内联 active context 的外部记录。Artifact 是 Kuncode home/session 的一等持久化对象，不应只是裸文件路径。

Artifact 边界：

1. Artifact 必须有 stable id，ContextBlock 和 event 通过 id 引用它。
2. Artifact metadata 至少表达 kind、size、content hash、redaction state、source event 或 source tool call。
3. Artifact 原文写入前必须经过 redaction policy。
4. Provider secret 永不落盘；完整 provider request 默认不落 event log，也不落 artifact，除非用户显式开启调试并完成 redaction。
5. Artifact reader 工具必须受 permission 和 workspace/session 边界约束。
6. Artifact 清理策略不能破坏 event log 的可解释性；清理后仍应保留 metadata 和明确的 missing artifact 诊断。

## 11. Session、Kuncode Home 和 Workspace

### 11.1 三类路径

Kuncode 区分三类路径：

1. Workspace: agent 操作的代码目录。
2. Kuncode home: Kuncode 自己的配置、session、日志、cache、artifacts。
3. Future worktree root: 多任务隔离执行目录。

Workspace 不承担 session 状态存储。Kuncode home 不参与工具文件操作。Future worktree 是 workspace 的隔离执行变体。

### 11.2 Kuncode Home

Kuncode home 保存：

```text
$KUNCODE_HOME/
  config.toml
  sessions/
    <session-id>/
      events.jsonl
      artifacts.jsonl
      artifacts/
      future metadata
  cache/
  future task state
```

默认可以是用户级目录，例如 `~/.kuncode`。项目说明文件 `KUNCODE.md` 可以放在 workspace 内，随代码版本管理。

### 11.3 Workspace

Workspace 是工具接触本地代码的唯一入口：

1. 拒绝绝对路径越界。
2. 拒绝 `..` 越界。
3. 读路径和写路径都必须拒绝 symlink 越界。
4. 默认跳过 `.git`、`target`、`node_modules`、`.venv`。
5. 大文件和非 UTF-8 内容必须拒绝或摘要化。
6. git helper 只暴露受控命令。

Workspace safety 不是文件工具自己的私有逻辑。所有文件读写、搜索、patch、git pathspec、shell cwd、future worktree cwd 都必须通过 workspace/worktree 边界校验。

读写策略必须分开：

1. 读路径要求目标存在、是允许读取的文件类型、没有越界。
2. 写路径允许新文件，但父目录和 symlink chain 不能越界。
3. Search 工具必须尊重 ignored path、大小限制和二进制/非 UTF-8 策略。
4. Shell cwd 必须是 workspace 或 worktree 内经过校验的目录。
5. Git helper 只暴露受控只读或受控写入操作，不能成为任意 shell 的别名。

### 11.4 Future Worktree Isolation

长期多任务能力需要把控制平面和执行平面分开：

1. Control plane: task state、session state、event log、mailbox。
2. Execution plane: workspace 或 worktree。

Worktree 隔离的长期模型：

```text
Task {
  id
  goal
  status
  assigned_agent
  worktree_id
}

Worktree {
  id
  path
  branch
  task_id
  status
}
```

绑定规则：

1. 创建 worktree 时绑定 task id。
2. task 进入 in progress。
3. shell/tool cwd 指向对应 worktree。
4. 收尾时可以 keep 或 remove worktree。
5. 生命周期事件进入 event stream。

MVP 不实现 worktree，但总体设计必须保留这条演进路径，避免把 session、workspace、task state 混成一个目录概念。

## 12. Provider Adapter

Provider adapter 负责隔离模型 API 差异。Agent loop 只依赖统一接口。

Provider capability：

1. tool calling。
2. structured output。
3. streaming。
4. max context tokens。
5. parallel tool calls。
6. cost/token metadata。

Provider adapter 负责：

1. 把 Kuncode model request 转成 provider payload。
2. 把 provider response 转成统一 model response。
3. 把 tool calls 转成统一 tool call 类型。
4. 把 provider error 转成 model error。
5. 避免 secret 进入 event log。
6. 接受取消 token，并在取消时 abort in-flight request 或尽快返回 cancelled error。

## 13. Skills 和项目知识

长期项目知识不能全部塞进 system prompt。Kuncode 应支持按需加载：

1. Workspace 内 `KUNCODE.md`。
2. 项目文档。
3. skill files。
4. API reference。
5. 用户偏好和团队规则。

加载方式：

1. 先让模型知道有哪些知识源可用。
2. 模型或 runtime 根据任务选择需要的知识。
3. 知识作为 context block 或 tool result 进入 active context。
4. 加载行为进入 event stream。

MVP 可以暂缓完整 skills loader，但 prompt/context 设计不能假设所有知识都在 system prompt 中。

## 14. Background Tasks 和 Teams

长期能力包括后台任务和多 agent 团队，但它们必须建立在稳定单 agent loop 之上。

Background task 机制：

1. 慢命令或长验证可以后台执行。
2. 后台任务有 id、status、started_at、finished_at。
3. 完成后向 event stream 注入 notification。
4. Agent 可以继续推理，不阻塞主循环。

Team 机制：

1. 每个 teammate 有独立 context/session。
2. 团队通信通过 mailbox 或 request-response protocol。
3. 任务分配、认领、完成都进入 task state 和 event stream。
4. Worktree isolation 是并行写入任务的前提。

这些不是 MVP 范围，但总体设计必须保证 event、task、workspace 边界能承载它们。

## 15. 错误处理

Kuncode 使用明确的错误边界：

1. 核心模块和 public trait 返回明确、可匹配的模块级错误。
2. CLI 入口和 command handler 可以汇总多种底层错误，并转成面向用户的诊断信息。
3. 不透明应用层错误不应出现在 agent loop、tool runtime、provider adapter、workspace、events、context、config 等核心 public API 中。
4. 如果后续需要跨组件统一错误，应保留原始领域分类，而不是退化成字符串或 catch-all。

领域错误：

1. Model error: provider 失败、限流、schema 错误。
2. Tool error: 工具输入非法、执行失败、超时。
3. Permission error: 策略阻止。
4. Workspace error: 路径越界、文件不存在、非 UTF-8。
5. Context error: 超预算、渲染失败。
6. Event/session error: 事件持久化、session 状态、恢复失败。
7. Config error: 配置解析、环境变量、CLI 参数错误。

Agent 面对错误时：

1. 可恢复工具错误：结构化返回模型，让模型修正。
2. 权限错误：请求用户或按 non-interactive policy 阻止。
3. provider 临时错误：有限重试。
4. 连续失败：停止并报告。
5. 验证失败：进入诊断或最终失败报告。

## 16. 测试和验收策略

测试围绕 harness 边界：

1. Unit tests: path safety、context budget、event schema、tool registry。
2. Integration tests: 临时仓库中运行 agent，验证文件修改、事件记录和权限行为。
3. Golden tests: prompt renderer、compact summary。
4. Provider tests: deterministic provider 和 HTTP mock。
5. Recovery tests: event log 损坏、session 缺失、artifact 缺失。
6. Local task benchmark: 固定模型、仓库状态和验收命令。

验收原则：

1. 没有事件证据，不声称 agent 做过某事。
2. 没有验证证据，不声称修改完成。
3. 没有 workspace 校验，不允许工具访问本地路径。
4. 没有权限决策，不执行高风险动作。
5. 没有上下文预算策略，不允许长任务进入 agent loop。

Completion evidence gate:

1. 只读分析任务完成时，必须有读取、搜索、诊断或推理所依据的 evidence。
2. 写入任务完成时，必须有 workspace diff evidence。
3. 修复、测试、构建、格式化相关任务完成时，必须有 fresh verification evidence。
4. 如果用户明确要求不验证，最终报告必须说明未验证。
5. 如果验证无法运行，最终报告必须说明失败原因、已完成工作和剩余风险。
6. Agent 不应把模型自称完成当作 completion evidence。

## 17. MVP 收窄原则

MVP 不实现所有长期机制，但不能违背长期架构。

MVP 必须实现：

1. 单 agent loop 的基础垂直切片。
2. Workspace 和 Kuncode home 分离。
3. Event stream、schema version 和 JSONL event log。
4. CLI renderer、event log、测试 sink 共享 `AgentEvent`。
5. 最小 artifact store，用于长输出、大内容和被截断工具结果。
6. Redaction 边界，确保 secret 不进入 event log 或 artifact。
7. Context 每轮 active context rebuild 和 inline-or-reference 规则。
8. `UserInteraction` 抽象的非交互实现。
9. Permission policy contract，至少包含 policy input 和 allow/deny/ask decision。
10. Completion evidence gate，防止仅凭模型自称完成。
11. `FakeProvider` 支撑 deterministic integration tests。
12. `OpenAICompatibleProvider` 支撑真实 LLM smoke validation。
13. 最小 provider/config/doctor。

MVP 可以暂缓：

1. 完整 TaskState 状态机。
2. 完整 context compact。
3. skills loader。
4. background task。
5. multi-agent teams。
6. worktree isolation。
7. resume/replay。
8. 完整 artifact 清理和迁移。
9. model-assisted compact。
10. streaming 和 parallel tool calls。

MVP 设计文档负责确定这些机制在 MVP 中的具体 Rust 名字和落地点。

## 18. 参考来源

本文参考 `shareAI-lab/learn-claude-code` 的 harness 工程实践，重点吸收其机制思想：

1. Agent loop: 固定循环、工具结果回填、stop reason 退出。
2. Tool use: 新工具注册到 dispatch/registry，不改循环主体。
3. Planning/Todo: 复杂任务需要显式步骤。
4. Skills/knowledge: 知识按需加载。
5. Context compact: 分层压缩而不是无限堆上下文。
6. Task persistence: 任务状态持久化。
7. Background tasks: 慢操作可后台执行并回注通知。
8. Teams/mailbox: 多 agent 需要通信协议。
9. Worktree isolation: 任务目标和执行目录分离。

Kuncode 不复制其 Python 教程实现，而是把这些实践转换为 Rust runtime、结构化事件、workspace 安全层和 session 持久化设计。
