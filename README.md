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
- `kuncode-agent`：Agent 运行时、工具调度、权限、Hook、Todo、会话持久化和上下文压缩。
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

项目会自动读取当前目录下的 `.env`，因此也可以将变量写入本地 `.env`：

```dotenv
DEEPSEEK_API_KEY=your-api-key
```

使用 OpenAI 官方接口时，在用户目录的 `~/.kuncode/providers.json` 配置：

```json
{
  "defaultProfile": "openai",
  "profiles": {
    "openai": {
      "provider": "openai",
      "apiKeyEnv": "OPENAI_API_KEY",
      "model": "your-openai-model",
      "maxTokens": 16384
    }
  }
}
```

并设置对应环境变量：

```bash
export OPENAI_API_KEY="your-api-key"
```

兼容 OpenAI Chat Completions 的服务可以在 Profile 中增加 `baseUrl` 和
`headers`。`baseUrl` 支持服务根地址或完整 `/chat/completions` endpoint；
`apiKeyEnv` 为空时不发送 `Authorization`。

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

- `KUNCODE_MODEL` 可以覆盖配置文件中的模型名称；`DEEPSEEK_MODEL` 作为兼容别名保留。
- Provider 配置优先级为 CLI `--profile` / `--model` > 可信项目配置 >
  用户 Profile > 内置 DeepSeek 默认值。
- 未使用 `--trust-project` 时，项目中的 `profile`、`provider`、`name`、
  `baseUrl`、`apiKeyEnv`、`headers` 和 `maxTokens` 不会覆盖用户配置。
- `model.provider` 支持 `deepseek` 和 `openai`；自定义 endpoint 和 headers
  仅适用于 `openai` 协议。
- 内置模型配置包括 `deepseek-v4-pro` 和 `deepseek-v4-flash`。
- 非内置模型启用上下文压缩时，需要显式设置 `compaction.contextLimit`。
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
