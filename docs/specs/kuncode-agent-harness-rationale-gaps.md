# KunCode Agent Harness 设计决策 Rationale 缺口

## 文档职责

本文档跟踪 [kuncode-agent-harness-design.md](kuncode-agent-harness-design.md) 中**作为结论陈述但未说明为什么这样选**的关键决策。

每一条都带一个状态标签：

- `[已解决]`：总体设计文档已补齐 rationale，链接到具体位置。
- `[MVP 已选定]`：MVP 开发计划选定了一条实现路径以降低第一版的不确定性，长期方案仍未定；该选择不反向约束总体设计，可在 MVP 文档内修订。
- `[仍未解决]`：尚未在任何文档中给出取舍理由。

目的：

1. 让"已经写在哪份文档里"一眼可查。
2. 把"需要决定"和"已经决定但未解释"区分开。
3. 给后续 phase review 提供 backlog。

本文档不重新设计 harness，只标记 rationale 位置。

## 总览

| # | 项目 | 状态 |
|---|---|---|
| 1 | EventLog 用 JSONL | 已解决 |
| 2 | KunCode home 放 `$KUNCODE_HOME` | MVP 已选定 |
| 3 | Artifact 必须经 id 引用 | 仍未解决 |
| 4 | 完整 provider request 默认不落盘 | MVP 已选定 |
| 5 | ContextSet 每轮完全重建 | 已解决 |
| 6 | Compact 分三层 | MVP 已选定 |
| 7 | Skill 两阶段加载 | MVP 已选定 |
| 8 | EventEnvelope `kind` + `payload` 而非 tagged union | MVP 已选定 |
| 9 | EventLog vs ContextSet 对账 | MVP 已选定 |
| 10 | Tool `effects` 固定枚举 10 项 | MVP 已选定 |
| 11 | `exec_argv` 默认优先 `shell_command` | MVP 已选定 |
| 12 | MVP 只有 `MainWorkspace` | 仍未解决 |
| 13 | Worktree closeout `promote` 留空 | MVP 已选定 |
| 14 | `Run.budgets` 内容 | 已解决 |
| 15 | 并发模型 | 已解决 |
| 16 | `max_parallel_requests` 与 MVP 冲突 | 已解决 |
| 17 | Provider capability fallback | 已解决 |
| 18 | Reasoning / thinking token 位置 | MVP 已选定 |
| 19 | Streaming 暂缓但 cancel 必须支持 | 已解决 |
| 20 | Policy Ask 在 non-interactive 下的转换 | 已解决 |
| 21 | Capability set 五种预设边界 | 仍未解决 |

## 一、存储与持久化

### 1. EventLog 用 JSONL

状态：`[已解决]`

位置：[kuncode-agent-harness-design.md L755-757](kuncode-agent-harness-design.md#L755-L757)

设计文档已说明 JSONL 是 MVP durable log，不是永远唯一查询层；reader 必须报告损坏行号和 event id，未来可以在 JSONL 之上建立 SQLite index。

MVP 实现策略见 [kuncode-mvp-development-plan.md §6](../plans/kuncode-mvp-development-plan.md)：单 writer task + `mpsc`，按阈值 fsync。

### 2. KunCode home 放 `$KUNCODE_HOME`，不写 workspace

状态：`[MVP 已选定]`

位置：[kuncode-agent-harness-design.md L803-820](kuncode-agent-harness-design.md#L803-L820)

未讨论的问题仍然存在：

1. 跨机器或跨 workspace clone 后 run 历史丢失。
2. 多 workspace 同名 run-id 冲突。
3. CI 环境 `$HOME` 不稳定。

MVP 选定的实现见 [kuncode-mvp-development-plan.md §2.4](../plans/kuncode-mvp-development-plan.md)：默认 `$XDG_STATE_HOME/kuncode`，缺省退回 `$HOME/.local/state/kuncode`，可由 `KUNCODE_HOME` 覆盖。

长期未决：是否需要 workspace 内 `.kuncode/` 作为可选 mirror。等用户反馈再回头看。

### 3. Artifact 必须经 id 引用，不暴露裸路径

状态：`[仍未解决]`

位置：[kuncode-agent-harness-design.md L795](kuncode-agent-harness-design.md#L795)

未说明原因。猜测是为了 redaction 和 GC，但用户用 `cat` 看日志的能力是否被牺牲？

MVP 期实现保留了 id-only 约束（见开发计划 §6.3），但 rationale 仍空。建议在 Phase 1 实施完后补一句"为何 id-only"。

### 4. 完整 provider request 默认不落盘

状态：`[MVP 已选定]`

位置：[kuncode-agent-harness-design.md L373](kuncode-agent-harness-design.md#L373), [L799](kuncode-agent-harness-design.md#L799)

MVP 选择见 [kuncode-mvp-development-plan.md §10](../plans/kuncode-mvp-development-plan.md)：provider secret 可来自环境变量或配置文件，但默认不落盘；提供 `--debug-dump-provider-request` 显式打开，dump 写到 `runs/<id>/debug/` 并在日志中警告该 run 不应分享。

长期未决：是否要把 redaction 后的"安全 dump"作为常态选项，便于 bug repro 而不需要 debug flag。

## 二、Context 与模型交互

### 5. ContextSet 每轮完全重建

状态：`[已解决]`

位置：[kuncode-agent-harness-design.md L677](kuncode-agent-harness-design.md#L677)

设计文档已说明：每轮重建是为避免模型基于过期片段推理，代价是 prompt cache 命中率下降。策略是静态前缀稳定（SystemBlock / tool schema / 项目规则 / SkillIndexBlock 排序固定），动态 block 放后部；render meta 必须记录 block 顺序、token 估算和被裁剪内容。

MVP 实现把 `IdentityBlock` 和 `GoalBlock` 也并入静态前缀（见开发计划 §4.7）。

未决衍生问题：是否在多个 cache breakpoint 上做实验。不影响 MVP。

### 6. Compact 分三层 micro / auto / manual

状态：`[MVP 已选定]`

位置：[kuncode-agent-harness-design.md L679-687](kuncode-agent-harness-design.md#L679-L687)

设计文档未解释为何是三层。

MVP 选择见 [kuncode-mvp-development-plan.md §4.7](../plans/kuncode-mvp-development-plan.md)：只实现 `MicroCompact`，auto / manual 留 trait 钩子。

长期未决：micro 每轮执行的成本与收益没量化；auto 触发阈值算法待定。

### 7. Skill 两阶段加载

状态：`[MVP 已选定]`

位置：[kuncode-agent-harness-design.md L689-698](kuncode-agent-harness-design.md#L689-L698)

设计文档未解释为何是 index + `load_skill`，而不是 just-in-time 自动注入。

MVP 选择见 [kuncode-mvp-development-plan.md §11](../plans/kuncode-mvp-development-plan.md)：只用 `SkillIndexBlock` 静态注入，不实现 `load_skill` 工具。

长期未决：等真实模型使用观察后再决定是 index+load 还是 keyword-triggered auto-inject。

## 三、协议与类型

### 8. EventEnvelope 的 `kind` + `payload` 而非 tagged union

状态：`[MVP 已选定]`

位置：[kuncode-agent-harness-design.md L401-418](kuncode-agent-harness-design.md#L401-L418)

MVP 选择见 [kuncode-mvp-development-plan.md §4.2](../plans/kuncode-mvp-development-plan.md)：采用封闭 Rust enum + `#[serde(tag="kind")]`，物理表示仍是 `kind` + `payload` JSON，但 Rust 端是类型安全的 tagged union。等扩展点稳定后再考虑放开为字符串 kind。

长期未决：第三方插件需要新增 event kind 时的扩展机制。

### 9. EventLog 是事实，ContextSet 是工作集，但二者如何对账未规定

状态：`[MVP 已选定]`

位置：[kuncode-agent-harness-design.md L169](kuncode-agent-harness-design.md#L169), [L247](kuncode-agent-harness-design.md#L247)

MVP 选择见 [kuncode-mvp-development-plan.md §11](../plans/kuncode-mvp-development-plan.md)：`ContextBuilder` 单向消费事件，没有反向校验；Phase 4 集成测试覆盖关键事件是否被引入 context 的若干用例。

长期未决：是否需要 invariant checker 强制要求"failed 事件必须出现在下一轮 context 里"。

### 10. Tool 的 `effects` 是固定枚举的 10 项

状态：`[MVP 已选定]`

位置：[kuncode-agent-harness-design.md L494-505](kuncode-agent-harness-design.md#L494-L505)

MVP 选择见 [kuncode-mvp-development-plan.md §11](../plans/kuncode-mvp-development-plan.md)：封闭 enum。

长期未决：放开等到 Tool plugin 设计；届时需要 namespacing 规则避免 effect 命名冲突。

## 四、执行模型

### 11. `exec_argv` 默认优先 `shell_command`

状态：`[MVP 已选定]`

位置：[kuncode-agent-harness-design.md L572](kuncode-agent-harness-design.md#L572)

MVP 选择见 [kuncode-mvp-development-plan.md §8](../plans/kuncode-mvp-development-plan.md)：MVP 只暴露 `exec_argv`，`shell_command` 暂不进 capability set。这等于强制了"优先 exec_argv"，模型没有第二个选项。

长期未决：未来引入 `shell_command` 时如何引导模型选择（prompt？policy？capability 默认值？）。

### 12. MVP 只有 `MainWorkspace`，多 agent 写并发依赖 `GitWorktree`

状态：`[仍未解决]`

位置：[kuncode-agent-harness-design.md L623](kuncode-agent-harness-design.md#L623)

设计文档说"长期多 agent 和并行写入依赖 `GitWorktree`"，但没说为何不优先 `ReadOnlySnapshot`——Snapshot 比 worktree 实现成本低很多，对 Explorer / Reviewer 角色就够用。

MVP 阶段没有 multi-agent，所以这条不阻塞，但下一阶段规划前必须先回答："Explorer / Reviewer 是用 Snapshot 还是 Worktree 跑？"

### 13. Worktree closeout `promote` 留空

状态：`[MVP 已选定]`

位置：[kuncode-agent-harness-design.md L627-633](kuncode-agent-harness-design.md#L627-L633)

MVP 无 worktree，无需 closeout（见 [kuncode-mvp-development-plan.md §11](../plans/kuncode-mvp-development-plan.md)）。

长期候选：cherry-pick / rebase / 生成 patch artifact。在 worktree lane 进入实施前必须三选一并写入设计文档。

## 五、并发与预算

### 14. `Run.budgets` 内容

状态：`[已解决]`

位置：[kuncode-agent-harness-design.md L316-325](kuncode-agent-harness-design.md#L316-L325)

设计文档已定义 6 个独立 hard stop（`max_turns` / `max_wall_time` / `max_tool_calls` / `max_context_tokens` / `max_model_tokens` / `max_cost`），策略是"任一触发即停止"。

MVP 至少实现前 3 项（见 [kuncode-mvp-development-plan.md §2.1](../plans/kuncode-mvp-development-plan.md)）。

### 15. 并发模型

状态：`[已解决]`

位置：[kuncode-agent-harness-design.md L465-476](kuncode-agent-harness-design.md#L465-L476)

设计文档已选定单进程 async runtime，async-first；sync 库 / CPU 密集 / 无 async wrapper 的操作显式入 blocking pool。

MVP 进一步冻结为 Tokio（见 [kuncode-mvp-development-plan.md §2.4](../plans/kuncode-mvp-development-plan.md)）。

### 16. `max_parallel_requests` 与 MVP 暂缓项冲突

状态：`[已解决]`

位置：[kuncode-agent-harness-design.md L469](kuncode-agent-harness-design.md#L469)

设计文档已说明 MVP 中 `max_parallel_requests = 1`，即使 provider 返回多个 tool request 也按顺序执行。冲突消解。

## 六、Provider 适配

### 17. Provider capability 不匹配时的 fallback 策略

状态：`[已解决]`

位置：[kuncode-agent-harness-design.md L896-905](kuncode-agent-harness-design.md#L896-L905)

设计文档已列 6 条 fallback 规则，并给出原则："不破坏核心语义的能力做降级；破坏 tool loop / context budget / 安全边界的能力缺失必须 hard fail"。

剩余小遗漏：cancel abort 后 provider 端可能仍计费——对成本 budget 语义有影响。这条不阻塞 MVP，等 cost budget 启用时再补。

### 18. Reasoning / thinking token 没有 first-class 位置

状态：`[MVP 已选定]`

位置：未规定。

MVP 选择见 [kuncode-mvp-development-plan.md §11](../plans/kuncode-mvp-development-plan.md)：仅记录到 `ModelRequestMeta`，不进入独立事件。Phase 7 真实 smoke 后回头确认是否需要 first-class 事件。

风险点：MVP 中 DeepSeekProvider 已涉及 thinking/reasoning metadata，后续其他 reasoner / extended thinking 形态也可能有独立计费字段。如果真实 smoke 发现 token usage 或 budget 语义需要 reasoning 独立计费，MVP 期会被迫加事件 schema。建议 Phase 7 完成立刻 review 该项。

### 19. Streaming 暂缓但 cancel token 必须支持

状态：`[已解决]`

位置：[kuncode-agent-harness-design.md L899](kuncode-agent-harness-design.md#L899)

设计文档已说明：不支持 streaming 时 runtime 等完整响应，cancel 只能尽力 abort in-flight request。

剩余小遗漏（与 #17 一致）：abort 后 provider 端是否计费没说，对 cost budget 有影响。

## 七、安全与权限

### 20. Policy Ask 在 non-interactive 下转 Deny 或 Allow

状态：`[已解决]`

位置：[kuncode-agent-harness-design.md L554-561](kuncode-agent-harness-design.md#L554-L561)

设计文档已定义 4 种模式（`interactive` / `non_interactive` / `yes` / `dry_run`）。mode 入 PolicyInput。

MVP 选择见 [kuncode-mvp-development-plan.md §2.4](../plans/kuncode-mvp-development-plan.md)：CLI 在 stdin 非 tty 时默认 `non_interactive`，否则 `interactive`。

剩余小遗漏：`dry_run` 模式下 `Ask` 的默认行为（Deny 还是"假设 Allow 并记录"）设计文档没说。建议 Phase 5 实施时一并写入。

### 21. Capability set 五种预设的边界

状态：`[仍未解决]`

位置：[kuncode-agent-harness-design.md L521-527](kuncode-agent-harness-design.md#L521-L527)

设计文档列了 `Explore` / `Edit` / `Verify` / `Lead` / `FullLocal` 五种，但没说：

1. 为什么是这五种。
2. Lead agent 同时需要"修改"和"运行测试"时，用 `Edit` 还是组合 `Edit + Verify`？
3. 组合规则是什么（并集？优先级？）。

MVP 阶段 `Lead` agent 用单一 capability set，由 [kuncode-mvp-development-plan.md §8](../plans/kuncode-mvp-development-plan.md) 的工具表隐式定义边界。但理论上的"组合"机制仍未澄清，在 multi-role 引入前必须回答。

## 八、剩余优先级

`[仍未解决]` 三项：

1. **#3 Artifact id-only 引用的理由**：Phase 1 实施后顺手补一句即可，成本极低。
2. **#12 MainWorkspace 优先 vs ReadOnlySnapshot 优先**：multi-agent 阶段前必须回答，否则会推迟一整个里程碑。
3. **#21 Capability set 组合规则**：multi-role / multi-agent 引入前必须回答。

`[MVP 已选定]` 中需要在 MVP 内回头 review 的：

1. **#18 reasoning / thinking token**：Phase 7 真实 smoke 完成后立刻 review。
2. **#20 `dry_run` 模式下 Ask 行为**：Phase 5 实施时一并写入。

其余 `[MVP 已选定]` 项可以等到 MVP 结束后批量处理。如果 MVP 期内某项选择被推翻，先改 [kuncode-mvp-development-plan.md](../plans/kuncode-mvp-development-plan.md)，再回到本文档调整该项状态。
