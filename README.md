# kuncode

`kuncode` 是一个使用 Rust 编写、运行在终端中的编码 Agent。项目参考
[`learn-claude-code`](https://github.com/shareAI-lab/learn-claude-code) 的 Harness
Engineering 思路：模型负责判断下一步做什么，Harness 负责提供工具、上下文、权限边界、持久化和用户界面。

当前版本为 `0.1.0`，默认使用 DeepSeek，也支持 OpenAI Chat Completions，
提供一次性命令行执行和交互式 TUI 两种使用方式。

## 工作区结构

```text
kuncode-cli ──▶ kuncode-agent ──▶ kuncode-core ──▶ LLM API
    │                 │                 │
    │                 │                 └─ 消息、Completion、流式协议、Provider
    │                 └─ Agent Loop、工具、权限、会话、压缩与编排
    └─ 参数、配置、审批、一次性输出与 TUI
```

- `kuncode-core`：Provider-neutral 的消息与 Completion 抽象，以及 DeepSeek、OpenAI Provider。
- `kuncode-agent`：Agent 运行时、工具调度、权限、Hook、Todo、子代理、会话持久化和上下文压缩。
- `kuncode-cli`：命令行参数、项目配置、终端审批、普通输出和交互式 TUI。

## 环境要求

- Rust stable，项目使用 Rust 2024 edition。
- DeepSeek 或 OpenAI API Key。
- 支持 ANSI 终端；交互模式需要 stdin 和 stdout 都连接到真实终端。

## 快速开始

设置 API Key：

```bash
export DEEPSEEK_API_KEY="your-api-key"
```

KunCode 只读取启动进程已有的环境变量，不会自动加载工作区中的 `.env`。
请在 shell、终端配置或可信的进程管理器中注入密钥，避免未受信项目改变网络与运行时配置。

使用 OpenAI 官方接口时，在 `.kuncode/settings.json` 配置：

```json
{
  "model": {
    "provider": "openai",
    "name": "gpt-5.1",
    "maxTokens": 16384
  }
}
```

并设置对应环境变量：

```bash
export OPENAI_API_KEY="your-api-key"
```

一次性执行任务：

```bash
cargo run -p kuncode-cli -- "分析当前项目并运行测试"
```

启动交互式 TUI：

```bash
cargo run -p kuncode-cli
```

TUI 中按 `Enter` 提交，`Ctrl+J` 插入换行，`PageUp` / `PageDown` 浏览历史，
运行中按 `Ctrl+C` 取消。`Shift+Tab` 在轮次之间循环切换权限模式
（`default` → `accept-edits` → `plan`）；`bypass` 和 `dont-ask` 只能由启动时的
`--mode` 指定，不在循环里，避免一次误按就放开无人值守的边界。
当前模式显示在输入框正下方一行的左侧，旁边标着 `shift+tab`，终端太矮时这一行整体让位
给对话和计划面板。模式名按放权程度着色：`plan` 绿色（只读）、`default` 灰色（每一步都问）、
`accept-edits` 青色（改文件不再询问）、`dont-ask` 黄色（无人值守但默认拒绝）、`bypass`
红色（所有闸门关闭）；`NO_COLOR` 下由文字本身承担含义。底部状态栏右侧显示累计 token
与缓存命中率和当前模型（`in … · out … · cache …% · <模型>`），终端变窄时整段
丢弃，而不是把标签截断成半截。
输入 `/` 弹出命令列表，`↑` / `↓` 选择、`Tab` 补全、
`Enter` 执行；`/help` 列出可用命令，`/model` 弹出模型选择列表（`↑` / `↓`
选择、`Enter` 切换、`Esc` 取消），`/model <名称>` 直接切换到指定模型或
`modelProfiles` 中的 profile。可切换的候选在启动时一次性解析成注册表
（profile ∪ provider 内置模型 ∪ 启动模型），切换只查表、不重读配置文件，
名称不在注册表中会报错并列出全部可用项（provider 不变；`--resume`
恢复的会话仍使用启动时解析的模型）。`/compact`
立即压缩上下文，`/quit`（或输入 `exit`）退出。终端设置了 `NO_COLOR` 时界面自动使用无颜色样式；移除该
环境变量即可启用 ANSI 语义色。

构建 release 二进制：

```bash
cargo build --release -p kuncode-cli
./target/release/kuncode --help
```

## 项目配置

在项目根目录创建 `.kuncode/settings.json`。所有配置段都使用严格 schema，未知字段和无效值会在启动时直接报错。

```json
{
  "permissions": {
    "allow": ["Read", "Bash(cargo *)"],
    "ask": ["Edit(.env)"],
    "deny": ["Bash(curl *)"],
    "defaultMode": "default"
  },
  "model": {
    "provider": "deepseek",
    "name": "deepseek-v4-flash",
    "maxTokens": 65536
  },
  "modelProfiles": {
    "fast": { "name": "deepseek-v4-flash", "maxTokens": 32768 }
  },
  "agent": {
    "maxIterations": 50,
    "todoReminderInterval": 3
  },
  "logging": {
    "level": "info"
  },
  "compaction": {
    "mode": "enabled"
  }
}
```

补充说明：

- `--model <NAME>` 可以为单次运行指定模型名称，适用于任意 provider，优先级最高；
  `KUNCODE_MODEL` 可以覆盖配置文件中的模型名称；`DEEPSEEK_MODEL` 作为兼容别名保留。
- `model.provider` 支持 `deepseek` 和 `openai`；两者分别使用固定官方 endpoint，
  并读取 `DEEPSEEK_API_KEY` 或 `OPENAI_API_KEY`。
- 内置模型配置包括 `deepseek-v4-flash` 和 `deepseek-v4-pro`；未指定 `model.name`
  时，DeepSeek provider 默认使用 `deepseek-v4-flash`。
- `modelProfiles` 定义可按名字引用的具名模型档案（`name` 必填，`maxTokens` 与
  `provider` 可选，`provider` 默认取 `model.provider`）。profile 与内置模型、
  启动模型在启动时合并为模型注册表，供 `/model` 切换；同名时 profile 优先。
  每个条目的输出预算和压缩配置各自独立校验，无效的 profile 会在启动时报错并
  指明是哪一个；指向其他 provider 的 profile 本次运行不可切换，仅记录警告。
- 没有内置能力档案的模型，`model.maxTokens` 默认值为 `16384`；从旧版
  `32768` 默认值升级时，如配置了 `compaction.reservedOutput`，需同步调整或显式设置
  `model.maxTokens`。
- 非内置模型启用上下文压缩时，需要显式设置 `compaction.contextLimit`。
- `compaction.mode` 支持 `disabled`、`shadow` 和 `enabled`，默认是 `disabled`。
- `shadow` 只计算和报告压缩候选，不替换当前上下文。
- `enabled` 会在达到预算阈值时执行压缩，并要求会话持久化状态保持健康。
- TUI 的 `/compact` 可以在未达阈值时手动触发压缩，走同一条流水线和同样的安全门；
  只有 `enabled` 模式支持，且上下文已经低于 `compaction.targetRatio` 时会直接报告
  「无需压缩」而不去花一次总结调用。手动压缩在审计记录里的 reason 是 `manual`，
  不会伪装成越过了某个阈值。

## 项目指令文件

启动时 kuncode 会读取项目指令文件，作为系统提示的最后一段发给模型，用来约定这个
仓库的工程规范、命令和禁止事项。

只识别 `AGENTS.md` 这一个文件名，最多产生两份文档：

1. 用户全局：`~/.kuncode/AGENTS.md`。
2. 当前工作区根目录：`./AGENTS.md`。

两份都存在时全局在前、项目在后，越靠后越具体、优先级越高；项目指令整体高于内置的
身份提示。文件缺失、内容为空、无法读取或不是 UTF-8 都只记一条日志，不影响启动。单份文档超过
64 KiB 会在字符边界截断，并在提示中标明截断位置。

文档在启动时读取一次：系统提示是每次请求的缓存前缀，会话中途改动指令文件不应作废
整段对话的 KV 缓存，因此修改在下次启动后生效。

## 技能（load_skill 工具）

技能是某一类工作的可复用指令，放在技能目录下的 `SKILL.md` 里：

```text
~/.kuncode/skills/<name>/SKILL.md    # 用户全局
./.kuncode/skills/<name>/SKILL.md    # 当前工作区，同名时覆盖全局
```

文档可以带 frontmatter（`---` 包围的 `name:` 与 `description:` 两个字段，
其余字段忽略），缺失时回退到目录名和正文第一行。启动时扫描一次，系统提示只
携带目录（名字加一行描述，按名字排序保证缓存前缀稳定）；全文由模型在需要时
调用 `load_skill` 工具读入对话，因此每次请求只为索引付出 token，文档只在真正
用到的会话里出现。会话中途增删技能在下次启动生效。

加载一个技能就是读它的文件：权限走 `Read(<SKILL.md 路径>)`，默认放行、`plan`
模式下可用，也可以按路径用 deny 规则禁掉某个技能。单个文档超过 64 KiB 会截断
并在文中标注。子代理同样能加载技能。无效目录（缺 `SKILL.md`、空内容、非
UTF-8）只记日志跳过，不影响启动。

## 记忆（load_memory / write_memory 工具）

记忆是留给未来会话的信息：用户的持久偏好、值得吸取的纠正、仓库里查不到的项目
事实。每条记忆一个 Markdown 文件，按项目隔离存放在工作区之外：

```text
~/.kuncode/memory/<项目slug>/<名字>.md
```

文件带 frontmatter（`name`、`description`，可选 `type`，取值
`user` / `feedback` / `project` / `reference`），正文即记忆内容。文件名（也是记忆
名）只允许小写字母、数字和连字符，最长 64 字符——这同时锁死了写入路径。

保存是显式的：模型调用 `write_memory`，不做每轮自动抽取。写入是一次 `Edit`
权限检查，默认弹审批并带 diff 预览，`plan` 模式直接拒绝；路径在工作区外，
`accept-edits` 也不会自动放行。覆盖已有记忆前必须先用 `load_memory` 读过本会话
的当前内容（与 `write_file` 的防盲覆盖同一道门），因此修正记忆的方式是先读后
改写，而不是新建一条近似重复。

召回与技能同构：启动时扫描一次目录，系统提示只携带索引（名字加一行描述，按
名字排序），全文由模型在需要时调用 `load_memory` 读入对话，超过 64 KiB 截断并
标注。提示中明确记忆是背景信息而非指令，与当前请求冲突时以当前请求为准。会话
中途新写的记忆当场就能 load，下次启动进入索引。读取默认放行、`plan` 模式可用，
也可以按绝对路径用 deny 规则禁掉某条记忆。

记忆文件是普通文本，可以直接查看、编辑或删除；无效文件（非法文件名、空内容、
非 UTF-8）只记日志跳过。没有 home 目录时整个功能静默关闭，与会话持久化的降级
一致。

## 任务系统（create_task 等六个工具）

任务系统是跨会话的工作账本：任务落盘、可以互相依赖、认领后带 owner。它与
`todo_write` 分工明确——todo 是当前会话的注意力管理（内存中、整表覆写、会话结
束即消失），任务系统是跨会话的工作协调（可恢复、可依赖、可认领），两者并存。

每个任务一个 JSON 文件，按项目隔离存放在工作区之外：

```text
~/.kuncode/tasks/<项目slug>/<id>.json
```

字段为 `id`（`task_` + 8 位十六进制，创建时生成）、`subject`、`description`、
`status`（`pending` → 认领 → `in_progress` → 完成 → `completed`）、`owner` 和
`blockedBy`（前置任务 id 列表）。id 语法同时锁死了写入路径。

六个工具：`create_task` 新建（id 用排他创建落盘，先建全部节点、再用返回的 id
加边）；`update_task` 给 pending 且未认领的任务追加 `blockedBy`（拒绝自环和依
赖环）；`claim_task` 认领——所有前置任务 `completed` 才放行（引用的文件缺失视
为未完成）；`complete_task` 完成并列出因此解锁的任务；`get_task` / `list_tasks`
读取（列表省略 description）。

权限上，四个写工具共用无值命名空间 `TaskWrite`：默认放行（和 `todo_write` 一
样是高频记账操作），`plan` 模式直接拒绝（与 `TodoWrite` 相反——这是跨会话的磁
盘副作用，plan 轮次不能留痕），可在 settings 里用裸规则 `deny: ["TaskWrite"]`
整体关掉。两个读工具是普通 `Read`，默认放行、`plan` 可用、可按路径 deny。

启动时扫描一次任务店，存在未完成任务时系统提示中会出现一行计数
（`Task store: N open tasks (M claimable) ...`），提示模型用 `list_tasks`
恢复上下文；会话中途的变化通过工具查询，不进这一行。

并发范围：id 分配靠排他创建保证无冲突；认领与完成目前是朴素的读改写，跨进程
的原子认领属于后续多 agent 协作章节。任务文件是普通 JSON，可直接查看或手工修
改；损坏的文件记日志跳过。没有 home 目录时整个功能静默关闭。

## 子代理（task 工具）

模型可以调用 `task` 工具，把一个自包含的子任务委托给子代理：一个嵌套的 Agent Loop。
子代理与父循环共享工作区、权限规则、会话内已授予的许可和同一个审批通道，但工具列表
里没有 `task` 本身（杜绝无限委托）；只有最终报告作为工具结果返回父上下文，中间的探索
过程不会进入父对话，这也是它解决上下文膨胀的方式。

子代理的形态由 `agent_type` 参数选择，内置两种：

- `general`（默认）：从全新上下文启动，只带着委托的 prompt 开始工作。
- `fork`：从父对话在委托时刻的副本启动，适合依赖已有讨论内容的子任务
  （快照会裁掉正在执行中的那批工具调用，保证转录合法）。

也可以用文件定义自定义类型，放在 agents 目录下的平铺 `.md` 文件里：

```text
~/.kuncode/agents/<name>.md    # 用户全局
./.kuncode/agents/<name>.md    # 当前工作区，同名时覆盖全局
```

frontmatter 支持 `name`、`description`、`tools`（逗号分隔的工具白名单，只能收窄父
工具集，永远不含 `task`）和 `model`（模型注册表中的 profile 名或模型 id，委托给该
类型的子代理固定运行在这个模型及其输出预算上；名称未注册时告警并回退为继承父轮次
模型，`/model` 切换不影响已固定的类型）；正文作为附加指令追加在父系统提示之后，
环境、工具和项目指令段全部保留。`general` 和 `fork` 是保留名，占用它们的文件会被
跳过，它们始终运行在父轮次当前的模型上。类型列表在启动时扫描一次并渲染进 `task`
的工具描述（缓存前缀的一部分），中途增删下次启动生效。

边界与计量：

- 子代理继承发起时父会话的权限模式与本会话已批准的规则；`plan` 模式直接拒绝委托本身。
  委托这个动作默认放行，权限目标是 `Agent(<类型名>)`，因此可以按类型单独关掉，
  例如 `deny: ["Agent(fork)"]` 或 `deny: ["Agent(explore)"]`。子代理的每一次工具调用
  仍然逐条走同一套审批管线。
- 子代理消耗的 token 计入父轮次的用量统计，状态栏和退出报告里都能看到。
- 子代理会话不持久化、不参与上下文压缩；`Ctrl+C` 取消会传播进嵌套循环。
- 界面上委托显示为一行工具调用（`Task: <描述>`，非默认类型显示为
  `Task (<类型>): <描述>`），子代理自己的工具调用作为嵌套行实时列在这一行
  下方（TUI 与一次性输出都支持）。子代理的中间叙述、流式输出和它自己的
  任务计划不上屏——报告会整体返回，计划面板始终属于父循环。完整运行细节
  记录在日志文件中。

## 开发

提交前在 workspace 根目录运行：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace --all-targets
cargo test --workspace
```

公共 API 或文档注释发生变化时，额外运行：

```bash
cargo doc --workspace --no-deps
```
