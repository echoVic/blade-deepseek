# Goal 自动续跑停止机制重设计（去除 MAX_GOAL_CONTINUATIONS）

- 日期：2026-07-16
- 状态：已批准（设计），待实现
- 范围：`orca-core`（goal 状态机）、`orca-runtime`（用量记账）、`orca-tui`（停止策略）、`docs/goal-mode.md`

## 背景与问题

goal 模式下 `run_hosted_goal_turns`（`crates/orca-tui/src/app.rs`）在每个成功 turn 后自动注入 continuation prompt 续跑，唯一的防失控护栏是硬编码 `MAX_GOAL_CONTINUATIONS: usize = 64`：数到 64 就把 goal 置为 `UsageLimited` 并停止。

问题：

1. **轮次计数不是进展信号。** 正常长任务会被误伤（64 次不够），原地打转的任务却要烧满 64 次才停。参考实现（OpenAI codex、Anthropic Claude Code）都不用默认轮次上限作主刹车：codex 靠 token 触顶自动压缩兜底；Claude Code 交互模式无上限，token-budget 自动续跑用"预算 90% + 递减收益（连续 3 次续跑 token 增量 < 500 即停）"。
2. **既有预算链路是断的。** `GoalStore::account_usage()`（`crates/orca-runtime/src/goals.rs:272`）会累加 `tokens_used` 并在超 `token_budget` 时置 `BudgetLimited`，但生产代码从未调用它；`HostedTurnRequest::with_goal_usage_tracking(true)` 标志被设置（`app.rs`）却无人消费。`token_budget` 目前是摆设。

## 决策（已与用户确认）

- 主刹车 = **token 预算 + 无进展检测**，删除 `MAX_GOAL_CONTINUATIONS`。
- 无预算 = 无上限（用户在场可随时中断，对齐 Claude Code 交互模式）。不设默认预算。
- 无进展信号 = **每个续跑 turn 的 tokens_used 增量低于阈值**。
- 无进展停下时进入**新增状态 `Stalled`**（非终止，可 `/goal resume`）。
- 记账**下沉到运行时**（方案 B）：`orca-runtime` 的 hosted host 在 generation 结束处统一记账，任何前端设置 `with_goal_usage_tracking(true)` 即自动获得预算保护；停止策略保留在 TUI 续跑循环。

## 设计

### 1. 状态机（`crates/orca-core/src/goal_types.rs`）

新增 `ThreadGoalStatus::Stalled`（serde 值 `stalled`）：

| 属性 | 值 |
|---|---|
| `is_terminal()` | false（可恢复） |
| `should_continue()` | false（自动续跑停止） |
| `goal_status_label` | `"stalled"` |
| TUI 颜色（`ui.rs`） | warning（与 UsageLimited 同组） |

- `update_goal` 工具约束不变：模型只能设 `complete`/`blocked`；`Stalled` 仅由系统设置。
- `UsageLimited` 保留（兼容旧 `goals_1.json` 数据），但不再有代码路径产生它。

### 2. 运行时记账（`crates/orca-runtime/src/runtime_host.rs`）

接通 `tracks_goal_usage` 标志：

- generation spawn 时记录 `Instant::now()`；`finish_generation()` 已持有本次 generation 的 `usage_delta`（`runtime_host.rs` 现有 `OperationTaskResult.usage_delta`）。
- 在把 `usage_delta` 累加进 `usage_ledger` 的同一处：若 `active.request.tracks_goal_usage()` 且能取到线程 session_id，调用 `GoalStore::load_default().account_usage(session_id, usage_delta.total_tokens(), elapsed_secs)`。
- 超预算翻转由 `account_usage` 现有逻辑完成（`tokens_used >= budget` 且 Active → `BudgetLimited`），运行时不加额外判断。
- 记账 IO 失败：记日志/忽略，不影响 turn 结果。
- **顺序约束**：记账必须发生在前端可观察到 turn 完成之前（即 `operation.wait()` 返回前），否则 TUI 在 turn 后重读 goal 会拿到未更新的 `tokens_used`。实现时需验证 `finish_generation` 与 terminal 发布的先后关系；若存在竞争，把记账挪到 turn 任务闭包内（`usage_after` 计算处）。

### 3. TUI 停止策略（`crates/orca-tui/src/app.rs` `run_hosted_goal_turns`）

- **删除** `MAX_GOAL_CONTINUATIONS` 常量与 `continuation > MAX_GOAL_CONTINUATIONS` 分支。continuation 编号仅保留用于 prompt 文案 `[Goal continuation #N]`。
- **预算停**：turn 后重读 goal，若运行时记账已翻转为 `BudgetLimited`，现有 `!goal.status.should_continue()` 分支自然停止并播报，无需新代码。
- **无进展停**（新增，仅对自动续跑 turn 计数）：
  - 常量：`STALL_TOKEN_DELTA_THRESHOLD: i64 = 500`（对齐 Claude Code DIMINISHING_THRESHOLD）、`STALL_TURN_LIMIT: u32 = 3`。
  - 循环维护 `stall_streak: u32`。每 turn 前记 `tokens_before = goal.tokens_used`，turn 后重读得 `tokens_after`；`tokens_after - tokens_before < STALL_TOKEN_DELTA_THRESHOLD` → `stall_streak += 1`，否则清零。
  - `stall_streak >= STALL_TURN_LIMIT` → 置 goal 为 `Stalled`，发 Notice：`Goal auto-continuation stopped because the last 3 turns made no measurable progress. Use /goal resume to continue.`，break。
- `/goal resume`：现有路径 `update_goal_status_for_session(Active)` 对 Stalled 直接可用；循环重启后 `stall_streak` 归零。

### 边界情况

- **记账缺失（provider 无 usage 数据 / 记账失败）**：token 增量恒 0，3 个续跑 turn 后以 `Stalled` 停下。这是有意的安全取向——观测不到进展时宁可停下交还用户，也不无限跑。
- **手动首 turn 不计 stall**：`stall_streak` 是循环局部变量，从第一个 continuation 起算。
- **workflow 等待 / 非 success turn**：现有分支先于 stall 判定 break，行为不变。

### 4. 文档

`docs/goal-mode.md` 同步更新：删除 continuation cap 的描述（第 63、86-91 行附近），补充 `stalled` 状态行与停止条件（预算耗尽、无进展、用户暂停、模型 complete/blocked）。

## 测试计划

- `goal_types.rs`：`Stalled` 的 `is_terminal`/`should_continue`/label/serde 往返。
- `goals.rs`：`account_usage` → `BudgetLimited` 已有测试覆盖，不动。
- `runtime_host.rs`：新增——`with_goal_usage_tracking(true)` 的 turn 完成后 GoalStore `tokens_used` 增加（`with_orca_home` 隔离）；不带标志不记账。
- `app.rs`：改写现有 continuation-limit 测试为 stall 测试——3 个零增量续跑 turn 后收到 `GoalStatus(Stalled)` 与对应 Notice；`BudgetLimited` 翻转后循环停止。

## 参考

- codex：`core/src/session/turn.rs`（无轮次上限；token 90% 触发 auto-compact 兜底；UsageLimitReached 抛错即停）
- Claude Code：`src/query.ts`（交互模式无 maxTurns）、`src/query/tokenBudget.ts`（`COMPLETION_THRESHOLD = 0.9`、`DIMINISHING_THRESHOLD = 500`、连续 3 次无进展早停）
