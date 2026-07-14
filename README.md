# kuncode

`kuncode` 是一个使用 Rust 编写、运行在终端中的编码 Agent。项目参考
[`learn-claude-code`](https://github.com/shareAI-lab/learn-claude-code) 的 Harness
Engineering 思路：模型负责判断下一步做什么，Harness 负责提供工具、上下文、权限边界、持久化和用户界面。

当前版本为 `0.1.0`，使用 DeepSeek 模型，提供一次性命令行执行和交互式 TUI 两种使用方式。

## 工作区结构

```text
kuncode-cli ──▶ kuncode-agent ──▶ kuncode-core ──▶ DeepSeek API
    │                 │                 │
    │                 │                 └─ 消息、Completion、流式协议、Provider
    │                 └─ Agent Loop、工具、权限、会话、压缩与编排
    └─ 参数、配置、审批、一次性输出与 TUI
```

- `kuncode-core`：Provider-neutral 的消息与 Completion 抽象，以及 DeepSeek Provider。
- `kuncode-agent`：Agent 运行时、工具调度、权限、Hook、Todo、会话持久化和上下文压缩。
- `kuncode-cli`：命令行参数、项目配置、终端审批、普通输出和交互式 TUI。

## 环境要求

- Rust stable，项目使用 Rust 2024 edition。
- DeepSeek API Key。
- 支持 ANSI 终端；交互模式需要 stdin 和 stdout 都连接到真实终端。

## 快速开始

设置 API Key：

```bash
export DEEPSEEK_API_KEY="your-api-key"
```

项目会自动读取当前目录下的 `.env`，因此也可以将变量写入本地 `.env`：

```dotenv
DEEPSEEK_API_KEY=your-api-key
```

一次性执行任务：

```bash
cargo run -p kuncode-cli -- "分析当前项目并运行测试"
```

启动交互式 TUI：

```bash
cargo run -p kuncode-cli
```

构建 release 二进制：

```bash
cargo build --release -p kuncode-cli
./target/release/kuncode --help
```

## 权限控制

工具执行前会经过权限策略与用户审批。CLI 支持追加 allow、ask 和 deny 规则：

```bash
cargo run -p kuncode-cli -- \
  --allow 'Read' \
  --allow 'Bash(cargo *)' \
  --ask 'Edit(.env)' \
  --deny 'Bash(curl *)' \
  "检查并修复项目"
```

支持三种权限模式：

- `default`：读取默认放行，写入和命令执行默认询问。
- `accept-edits`：自动接受普通文件编辑，显式 ask/deny 规则仍然生效。
- `bypass`：跳过普通审批，但不能绕过 deny 规则。

通过 `--mode <MODE>` 选择模式，例如：

```bash
cargo run -p kuncode-cli -- --mode accept-edits "整理代码格式"
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
    "name": "deepseek-v4-pro",
    "maxTokens": 65536
  },
  "agent": {
    "maxIterations": 50,
    "todoReminderInterval": 3
  },
  "compaction": {
    "mode": "enabled"
  }
}
```

补充说明：

- `DEEPSEEK_MODEL` 可以覆盖配置文件中的模型名称。
- 内置模型配置包括 `deepseek-v4-pro` 和 `deepseek-v4-flash`。
- `compaction.mode` 支持 `disabled`、`shadow` 和 `enabled`，默认是 `disabled`。
- `shadow` 只计算和报告压缩候选，不替换当前上下文。
- `enabled` 会在达到预算阈值时执行压缩，并要求会话持久化状态保持健康。

## 工具

CLI 默认向模型注册以下工具：

- `bash`：在 workspace 根目录执行 Shell 命令，并限制输出大小。
- `read_file`：按行读取文件，支持分页和 UTF-8 安全截断。
- `write_file`：在 workspace 内写入新内容。
- `edit_file`：精确替换唯一匹配的文本。
- `glob`：搜索 workspace 文件，默认遵守 `.gitignore`。
- `todo_write`：维护当前会话的结构化任务计划。

所有工具都通过同一个注册表、参数校验、权限门和结果封装进入 Agent Loop。

## 会话与上下文压缩

每次运行会尝试创建 SQLite 会话 journal。模型消息和工具结果写入 journal，为未来的恢复、分叉和长期记忆提供权威历史。

启用上下文压缩后，运行时会：

1. 计算完整请求的 token 预算。
2. 保持工具调用与结果的协议原子性。
3. 保护最近上下文和当前用户请求。
4. 将旧的大型工具结果归档为 content-addressed artifact。
5. 对安全的工具结果做确定性裁剪。
6. 必要时生成并校验结构化语义摘要。
7. 原子提交 checkpoint 后再替换内存中的 Active Context。

持久化或一致性校验失败时，系统会拒绝危险的有损压缩，而不是继续使用无法证明来源的上下文。

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

## 已实现功能

- 基于流式 Completion 的单 Agent 工具循环，支持文本、推理内容和工具调用增量。
- DeepSeek Provider、模型能力配置，以及针对连接、408、429 和部分 5xx 的指数退避重试。
- Bash、文件读写、精确编辑、Glob 和 TodoWrite 工具。
- workspace 路径约束、参数校验、allow/ask/deny 规则、会话授权和终端审批。
- UserPromptSubmit、PreToolUse、PostToolUse、Stop 四类 Hook 运行时接缝。
- 结构化 Todo、计划提醒、Observer 事件和实时 TUI 展示。
- 一次性 CLI、交互式 TUI、流式预览和 Ctrl-C 取消。
- SQLite 会话 journal、artifact、checkpoint 与一致性校验基础设施。
- 多阶段自动上下文压缩，包括 artifact spill、结果裁剪、语义摘要、CAS 和失败关闭。
- 运行时拼装的 Identity、Environment 和 Tools 系统提示区块。

## 待实现功能

- 将 Hook 注册与外部命令 Hook 接入 CLI 项目配置。
- 会话 list、resume、fork 和 export 命令及运行时恢复流程。
- Skill 扫描、索引和按需加载。
- 独立上下文的 Subagent，以及权限冒泡、取消和结果回传。
- 从完整 journal 选择、抽取、合并并按需加载的长期 Memory。
- 带依赖关系、owner 和 claim/complete 生命周期的持久 Task System。
- 后台任务、完成通知和可选的 Cron 调度。
- Agent Team、异步 Mailbox、团队协议、自治任务认领和 worktree 隔离。
- MCP 客户端、动态工具发现以及外部能力的权限治理。
- `Retry-After`、上下文过长错误后的反应式压缩和更完整的错误恢复策略。
