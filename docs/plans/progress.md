# KunCode 开发进度

最近更新：2026-05-14 UTC

## 当前阶段

Phase 0 和 Phase 1 已完成。Phase 2 的 Tool Runtime、基础工具主体和 review 修复已实现。Phase 3 的 Provider Adapter 详细计划已落地，下一步可以开始实现 `kuncode-provider`。

## Phase 2已完成

1. 完成 `ToolRuntime` 的注册与执行主链路：descriptor 校验、schema 编译缓存、schema validation、capability gate、工具生命周期事件、`source_event_id` 透传，以及 `summary` 200 字符约束。
2. 实现 7 个内建工具：`read_file`、`search`、`write_file`、`apply_patch`、`exec_argv`、`git_status`、`git_diff`。
3. 增加工具公共 API：`Tool::risk_flags` 动态风险标记、`builtin_tools()`、`register_builtin_tools()`，并从 `kuncode-tools` 对外导出。
4. 增加 `Workspace::resolve_existing_path`，用于需要校验已存在路径的工具。
5. 补齐关键结构体字段和函数的 doc 注释，覆盖工具 descriptor、runtime、workspace path helper、内建工具入口等公共 API。
6. `ToolRuntime` 内部改为私有 `ToolRegistry`，用注册顺序 `Vec<Entry>` 加名称索引 `HashMap<String, usize>`，保持 descriptor 输出顺序稳定。
7. `read_file` 支持 `offset` / `limit` 范围读取，并修复 offset 超过 EOF 时的空结果 metadata；范围 summary 区分 selected bytes 和 file bytes。
8. `exec_argv`、`git_diff`、`git_status` 使用 bounded streaming capture；`exec_argv` 长 stdout/stderr 生成完整 combined artifact，`git_diff` 长 stdout 生成 artifact，`git_status` 只返回受限 inline 并从完整 capture 统计 changed file count。
9. Unix 下 timeout/cancel 使用 process group 回收进程树；Windows 暂保留 direct-child kill fallback。
10. `search` 对 `rg` 和 Rust fallback 都做 streaming/line-by-line 边界控制，fallback 同步 IO 放入 blocking task，snippet、inline、artifact 都只保存已选结果集合。
11. `apply_patch` 改为两阶段验证和写入，验证失败零写入，写入失败尽力 rollback；已有文件保留原行尾，hunk 顺序倒置、context mismatch 等 patch 语义错误归类为 `InvalidInput`。
12. 增加工具集成测试和共享测试 fixture，覆盖 capability deny、路径越界、截断、artifact、timeout、cancel、进程树回收、search 边界、apply_patch 原子性/行尾保留/倒序 hunk、git 非仓库错误、动态风险标记、内建 descriptor truth table 等 Phase 2 行为。

## 已验证

以下命令已在本轮 Phase 2 实现后通过：

```text
cargo fmt --all -- --check
cargo test -p kuncode-core
cargo test -p kuncode-events
cargo test -p kuncode-tools
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

`cargo deny check` 本轮未执行成功：本机未安装 `cargo-deny`。

## 文档整理

计划类文档统一放到 `docs/plans`：

1. `docs/plans/kuncode-mvp-development-plan.md`
2. `docs/plans/kuncode-phase2-tool-runtime-plan.md`
3. `docs/plans/kuncode-phase3-provider-adapter-plan.md`
4. `docs/plans/progress.md`

设计和取舍类文档保留在 `docs/specs`：

1. `docs/specs/kuncode-agent-harness-design.md`
2. `docs/specs/kuncode-agent-harness-rationale-gaps.md`

## 后续入口

1. Phase 3：按 `docs/plans/kuncode-phase3-provider-adapter-plan.md` 实现 `ProviderAdapter`、一等 `DeepSeekProvider` 和通用 `OpenAiCompatibleProvider`，测试使用 wiremock fixture。
2. Phase 4：接入 ContextBuilder 与最小 agent loop。
3. Phase 2 后续如有行为变更，先同步 `docs/plans/kuncode-phase2-tool-runtime-plan.md` 和本进度文件，再改代码。
