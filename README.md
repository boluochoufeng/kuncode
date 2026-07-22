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

TUI 中按 `Enter` 提交，`Ctrl+J` 插入换行，`PageUp` / `PageDown` 浏览历史，
运行中按 `Ctrl+C` 取消。终端设置了 `NO_COLOR` 时界面自动使用无颜色样式；移除该
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
    "name": "deepseek-v4-pro",
    "maxTokens": 65536
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

- `DEEPSEEK_MODEL` 可以覆盖配置文件中的模型名称。
- 内置模型配置包括 `deepseek-v4-pro` 和 `deepseek-v4-flash`。
- `compaction.mode` 支持 `disabled`、`shadow` 和 `enabled`，默认是 `disabled`。
- `shadow` 只计算和报告压缩候选，不替换当前上下文。
- `enabled` 会在达到预算阈值时执行压缩，并要求会话持久化状态保持健康。

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
