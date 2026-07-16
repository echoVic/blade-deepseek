# Goal 续跑停止机制重设计 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 删除 `MAX_GOAL_CONTINUATIONS` 硬编码轮次上限，改为"运行时 token 记账 + 预算停 + 无进展（stall）检测"双刹车，并新增可恢复的 `Stalled` goal 状态。

**Architecture:** 三层改动——`orca-core` 加 `ThreadGoalStatus::Stalled` 状态；`orca-runtime` 在 hosted host 的 generation 闭包里接通 `tracks_goal_usage` 标志、调 `GoalStore::account_usage()` 记账（超预算由既有逻辑翻转 `BudgetLimited`）；`orca-tui` 的 `run_hosted_goal_turns` 删掉轮次上限、加"连续 3 个续跑 turn token 增量 < 500 即置 Stalled"检测。

**Tech Stack:** Rust workspace（crates: orca-core / orca-runtime / orca-tui / orca-tools），cargo test。

**Spec:** `docs/superpowers/specs/2026-07-16-goal-continuation-stop-redesign-design.md`

---

### Task 1: 新增 `ThreadGoalStatus::Stalled` 状态

**Files:**
- Modify: `crates/orca-core/src/goal_types.rs`（enum、label、tests）
- Modify: `crates/orca-tools/src/update_goal.rs:234-242`（`goal_status_word` 补 match 臂）
- Modify: `crates/orca-tui/src/ui.rs:167-173`（状态颜色补 match 臂）

背景：`ThreadGoalStatus` 是普通 enum，多处 `match` 是穷尽匹配，加变体后编译器会强制列出所有需要补臂的位置。`#[serde(rename_all = "snake_case")]` 会自动给出 `"stalled"` 序列化值。

- [ ] **Step 1: 在 `goal_types.rs` 测试模块中写失败测试**

在 `crates/orca-core/src/goal_types.rs` 的 `#[cfg(test)] mod tests` 末尾追加：

```rust
    #[test]
    fn stalled_status_is_recoverable_and_stops_continuation() {
        assert!(!ThreadGoalStatus::Stalled.is_terminal());
        assert!(!ThreadGoalStatus::Stalled.should_continue());
        assert_eq!(goal_status_label(ThreadGoalStatus::Stalled), "stalled");
        let json = serde_json::to_string(&ThreadGoalStatus::Stalled).unwrap();
        assert_eq!(json, "\"stalled\"");
        let parsed: ThreadGoalStatus = serde_json::from_str("\"stalled\"").unwrap();
        assert_eq!(parsed, ThreadGoalStatus::Stalled);
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p orca-core stalled_status -- --nocapture`
Expected: 编译错误 `no variant named 'Stalled'`（编译失败即本步的"失败"）

- [ ] **Step 3: 实现 Stalled 变体与所有 match 臂**

`crates/orca-core/src/goal_types.rs` enum 中在 `Blocked,` 之后加：

```rust
    Stalled,
```

同文件 `goal_status_label` 中在 `Blocked` 臂之后加：

```rust
        ThreadGoalStatus::Stalled => "stalled",
```

`is_terminal()` 与 `should_continue()` 使用 `matches!` 白名单，`Stalled` 不在其中，天然满足"非终止、不续跑"，无需改动。

`crates/orca-tools/src/update_goal.rs` `goal_status_word` 的 `Blocked` 臂后加：

```rust
        ThreadGoalStatus::Stalled => "stalled",
```

`crates/orca-tui/src/ui.rs` 状态颜色 match 中，把 `UsageLimited | BudgetLimited` 臂扩为：

```rust
        ThreadGoalStatus::UsageLimited
        | ThreadGoalStatus::BudgetLimited
        | ThreadGoalStatus::Stalled => theme.warning,
```

- [ ] **Step 4: 编译全 workspace 找漏网 match，运行测试**

Run: `cargo check --workspace 2>&1 | head -30`
Expected: 无错误。若报 non-exhaustive match，按语义补臂（凡与 UsageLimited 同组处理即可）。

Run: `cargo test -p orca-core goal_types && cargo test -p orca-tools update_goal`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add crates/orca-core/src/goal_types.rs crates/orca-tools/src/update_goal.rs crates/orca-tui/src/ui.rs
git commit -m "feat(core): add recoverable Stalled goal status"
```

---

### Task 2: 运行时接通 goal 用量记账

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs`（`spawn_generation` 闭包内记账）
- Test: `crates/orca-runtime/tests/runtime_host.rs`

设计要点：记账放在 `spawn_generation`（`runtime_host.rs:2209`）的 `spawn_blocking` 闭包内、`usage_delta` 计算处（当前 2252-2261 行附近）。该闭包先于 `finish_generation` 与 terminal 发布完成，前端 `operation.wait()` 返回时记账必然已落库——满足 spec 的顺序约束，无需碰 `finish_generation`。

- [ ] **Step 1: 给测试 executor 加可记录 usage 的行为**

`crates/orca-runtime/tests/runtime_host.rs` 中找到 `enum TestBehavior`（`ScriptedExecutor` 上方），追加变体：

```rust
    RecordUsage {
        input_tokens: u64,
        output_tokens: u64,
        status: RunStatus,
    },
```

`impl ThreadOperationExecutor for ScriptedExecutor` 的 `match behavior` 中追加臂：

```rust
            TestBehavior::RecordUsage {
                input_tokens,
                output_tokens,
                status,
            } => {
                thread
                    .session_mut()
                    .cost_tracker_mut()
                    .add_usage(orca_core::provider_types::Usage {
                        input_tokens,
                        output_tokens,
                        cache_tokens: 0,
                    });
                Ok(status.into())
            }
```

- [ ] **Step 2: 写失败的记账集成测试**

同文件追加（注意：goal 记账要求 session 有 id，所以 `history_mode` 用 `Record` 且 `ORCA_HOME` 指向临时目录；用静态锁避免并行测试的环境变量竞争）：

```rust
static ORCA_HOME_TEST_LOCK: Mutex<()> = Mutex::new(());

fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
    // edition 2024: set_var/remove_var are unsafe; serialize via lock like
    // orca-tui's test_support::lock_process_env.
    let _guard = ORCA_HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let home = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("ORCA_HOME");
    unsafe {
        std::env::set_var("ORCA_HOME", home.path());
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(home.path())));
    unsafe {
        if let Some(previous) = previous {
            std::env::set_var("ORCA_HOME", previous);
        } else {
            std::env::remove_var("ORCA_HOME");
        }
    }
    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[test]
fn turn_with_goal_usage_tracking_accounts_tokens_and_flips_budget_limited() {
    with_orca_home(|_home| {
        let executor = Arc::new(ScriptedExecutor::new([
            TestBehavior::RecordUsage {
                input_tokens: 200,
                output_tokens: 100,
                status: RunStatus::Success,
            },
            TestBehavior::RecordUsage {
                input_tokens: 400,
                output_tokens: 200,
                status: RunStatus::Success,
            },
        ]));
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(executor).expect("start runtime host");
        let mut config = test_config(cwd.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let thread = host
            .start_thread(config, "goal accounting test")
            .expect("start runtime thread");
        let session_id = thread
            .session_id()
            .expect("recorded session id")
            .to_string();
        let mut store = orca_runtime::goals::GoalStore::load_default();
        store
            .replace(
                &session_id,
                "account my usage",
                orca_core::goal_types::ThreadGoalStatus::Active,
                Some(700),
            )
            .unwrap();

        let writer = SharedWriter::default();
        let first = thread
            .start_turn(
                HostedTurnRequest::new("turn one").with_goal_usage_tracking(true),
                writer.clone(),
            )
            .expect("start first turn");
        assert_eq!(
            first.wait_timeout(TEST_TIMEOUT).expect("first terminal").outcome(),
            &OperationOutcome::Completed(RunStatus::Success)
        );
        let after_first = store.get(&session_id).unwrap().unwrap();
        assert_eq!(after_first.tokens_used, 300);
        assert_eq!(
            after_first.status,
            orca_core::goal_types::ThreadGoalStatus::Active
        );

        let second = thread
            .start_turn(
                HostedTurnRequest::new("turn two").with_goal_usage_tracking(true),
                writer.clone(),
            )
            .expect("start second turn");
        assert_eq!(
            second.wait_timeout(TEST_TIMEOUT).expect("second terminal").outcome(),
            &OperationOutcome::Completed(RunStatus::Success)
        );
        let after_second = store.get(&session_id).unwrap().unwrap();
        assert_eq!(after_second.tokens_used, 900);
        assert_eq!(
            after_second.status,
            orca_core::goal_types::ThreadGoalStatus::BudgetLimited
        );

        host.shutdown().expect("shutdown runtime host");
    });
}

#[test]
fn turn_without_goal_usage_tracking_does_not_account() {
    with_orca_home(|_home| {
        let executor = Arc::new(ScriptedExecutor::new([TestBehavior::RecordUsage {
            input_tokens: 200,
            output_tokens: 100,
            status: RunStatus::Success,
        }]));
        let cwd = tempfile::tempdir().unwrap();
        let host = RuntimeHost::start_with_executor(executor).expect("start runtime host");
        let mut config = test_config(cwd.path().to_path_buf());
        config.history_mode = HistoryMode::Record;
        let thread = host
            .start_thread(config, "no accounting test")
            .expect("start runtime thread");
        let session_id = thread
            .session_id()
            .expect("recorded session id")
            .to_string();
        let mut store = orca_runtime::goals::GoalStore::load_default();
        store
            .replace(
                &session_id,
                "do not account",
                orca_core::goal_types::ThreadGoalStatus::Active,
                None,
            )
            .unwrap();

        let writer = SharedWriter::default();
        let turn = thread
            .start_turn(HostedTurnRequest::new("turn"), writer.clone())
            .expect("start turn");
        assert_eq!(
            turn.wait_timeout(TEST_TIMEOUT).expect("terminal").outcome(),
            &OperationOutcome::Completed(RunStatus::Success)
        );
        assert_eq!(store.get(&session_id).unwrap().unwrap().tokens_used, 0);

        host.shutdown().expect("shutdown runtime host");
    });
}
```

注意：若 `RuntimeThreadHandle` 没有 `session_id()` 方法而只有 snapshot 路径，用 `thread.snapshot().expect("snapshot").session_id().expect("recorded session id").to_string()` 替代（`RuntimeThreadSnapshot::session_id()` 已存在，见 `runtime_host.rs:1010`）。

- [ ] **Step 3: 运行测试确认失败**

Run: `cargo test -p orca-runtime --test runtime_host turn_with_goal_usage_tracking -- --nocapture`
Expected: FAIL — `after_first.tokens_used` 为 0（记账代码尚未存在）

- [ ] **Step 4: 在 `spawn_generation` 闭包内实现记账**

`crates/orca-runtime/src/runtime_host.rs`，先在文件内（`spawn_generation` 附近）加辅助函数：

```rust
fn account_goal_usage_for_generation(
    state: &ThreadActorState,
    request: &HostedTurnRequest,
    usage_delta: UsageTotals,
    elapsed_secs: i64,
) {
    if !request.tracks_goal_usage() {
        return;
    }
    let Some(session_id) = state.thread.session().session_id() else {
        return;
    };
    if let Err(error) = crate::goals::GoalStore::load_default().account_usage(
        session_id,
        usage_delta.total_tokens() as i64,
        elapsed_secs,
    ) {
        tracing::warn!(%error, "failed to account goal usage");
    }
}
```

（若该文件不使用 `tracing`，改成 `let _ = ...;` 静默忽略，与文件现有风格一致。）

然后修改 `spawn_generation` 的闭包（当前 `runtime_host.rs:2228-2262`）。改动点：闭包开头记录起始时间；构造 `OperationTaskResult` 前先算 `usage_delta` 并记账：

```rust
        let join = tokio::task::spawn_blocking(move || {
            let mut state = state;
            let started_at = Instant::now();
            let usage_before = state.thread.session().aggregate_usage_totals();
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                run_hosted_operation(
                    executor.as_ref(),
                    &mut state.thread,
                    &mut state.events,
                    &task_request,
                    &task_context,
                    writer.as_mut(),
                    &task_cancel,
                )
            }));
            let outcome = match outcome {
                Ok(Ok(outcome)) => GenerationTaskOutcome::Executed(outcome),
                Ok(Err(error)) => GenerationTaskOutcome::ExecutionFailed {
                    kind: error.kind(),
                    message: error.to_string(),
                },
                Err(payload) => GenerationTaskOutcome::Panicked {
                    message: panic_message(payload),
                },
            };
            let usage_after = state.thread.session().aggregate_usage_totals();
            let usage_delta = subtract_usage_totals(
                usage_totals_delta(usage_before, usage_after),
                usage_credit,
            );
            account_goal_usage_for_generation(
                &state,
                &task_request,
                usage_delta,
                started_at.elapsed().as_secs() as i64,
            );
            OperationTaskResult {
                state,
                writer,
                outcome,
                usage_delta,
            }
        });
```

（原代码在 `OperationTaskResult` 字面量里内联计算 `usage_delta`，此处只是提前到局部变量，再传给记账函数。）确认文件顶部已有 `use std::time::Instant;`，没有则补。

- [ ] **Step 5: 运行测试确认通过**

Run: `cargo test -p orca-runtime --test runtime_host turn_with_goal_usage_tracking turn_without_goal_usage_tracking`
Expected: 2 passed

Run: `cargo test -p orca-runtime --test runtime_host`
Expected: 全部 PASS（确认没破坏既有 host 测试）

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/tests/runtime_host.rs
git commit -m "feat(runtime): account goal token usage per hosted generation"
```

---

### Task 3: TUI 删除轮次上限、新增 stall 检测

**Files:**
- Modify: `crates/orca-tui/src/app.rs`（`run_hosted_goal_turns`、常量、tests）

- [ ] **Step 1: 写失败的 stall 集成测试**

在 `crates/orca-tui/src/app.rs` 测试模块中（`empty_recorded_hosted_tui_goal_resume_restores_latest_active_goal` 附近），追加：

```rust
    #[test]
    fn goal_auto_continuation_stalls_after_three_no_progress_turns() {
        with_orca_home(|_home| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::unbounded();
            let (action_tx, action_rx) = mpsc::unbounded();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    run_hosted_tui_controller_for_test(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::GoalSet("stall detection goal".to_string()))
                .unwrap();

            // mock provider 不产生 usage，goal 一直 active：
            // 用户 turn 后应跑满 3 个零增量续跑 turn，然后置 Stalled 并停。
            let mut stalled_notice = false;
            let mut stalled_status = false;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while std::time::Instant::now() < deadline && !(stalled_notice && stalled_status) {
                match event_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(TuiEvent::Notice(message))
                        if message.contains("no measurable progress") =>
                    {
                        stalled_notice = true;
                    }
                    Ok(TuiEvent::GoalUpdated(goal))
                        if goal.status == orca_core::goal_types::ThreadGoalStatus::Stalled =>
                    {
                        stalled_status = true;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(stalled_notice, "missing stall notice");
            assert!(stalled_status, "missing Stalled goal update");
        });
    }
```

- [ ] **Step 2: 运行测试确认失败**

Run: `cargo test -p orca-tui goal_auto_continuation_stalls -- --nocapture`
Expected: FAIL（超时，收不到 stall 事件——旧代码要 64 次才停且置 UsageLimited）

- [ ] **Step 3: 实现 stall 检测并删除轮次上限**

`crates/orca-tui/src/app.rs`：

3a. 把 `const MAX_GOAL_CONTINUATIONS: usize = 64;`（4256 行附近）替换为：

```rust
const STALL_TOKEN_DELTA_THRESHOLD: i64 = 500;
const STALL_TURN_LIMIT: u32 = 3;
```

3b. 修改 `run_hosted_goal_turns`。循环前初始化（`let mut continuation = starting_continuation;` 之后）：

```rust
    let mut stall_streak: u32 = 0;
    let mut turn_was_continuation = starting_continuation > 0;
```

3c. 循环顶部，`active_goal` 加载之后、构造 `request` 之前，记录本 turn 前的用量：

```rust
        let tokens_before = active_goal
            .as_ref()
            .map(|goal| goal.tokens_used)
            .unwrap_or(0);
```

3d. workflow 通知分支（`submitted_turn = SubmittedTurn::workflow_notification(notification); continue;`）在 `continue` 前加：

```rust
            turn_was_continuation = false;
```

3e. 把原来的续跑上限块：

```rust
        continuation += 1;
        if continuation > MAX_GOAL_CONTINUATIONS {
            update_goal_status_for_session(
                Some(&session_id),
                orca_core::goal_types::ThreadGoalStatus::UsageLimited,
                event_tx,
            );
            let _ = event_tx.send(TuiEvent::Notice(
                "Goal auto-continuation stopped after reaching the continuation limit.".to_string(),
            ));
            break;
        }
        submitted_turn =
            SubmittedTurn::user(goal_continuation_prompt(&goal.objective, continuation));
```

替换为（`goal` 是 turn 后重读的记录，前一行的 `should_continue` 检查保持不变）：

```rust
        if turn_was_continuation {
            let delta = goal.tokens_used.saturating_sub(tokens_before);
            if delta < STALL_TOKEN_DELTA_THRESHOLD {
                stall_streak += 1;
            } else {
                stall_streak = 0;
            }
            if stall_streak >= STALL_TURN_LIMIT {
                update_goal_status_for_session(
                    Some(&session_id),
                    orca_core::goal_types::ThreadGoalStatus::Stalled,
                    event_tx,
                );
                let _ = event_tx.send(TuiEvent::Notice(format!(
                    "Goal auto-continuation stopped because the last {STALL_TURN_LIMIT} turns made no measurable progress. Use /goal resume to continue."
                )));
                break;
            }
        }
        continuation += 1;
        submitted_turn =
            SubmittedTurn::user(goal_continuation_prompt(&goal.objective, continuation));
        turn_was_continuation = true;
```

- [ ] **Step 4: 运行测试确认通过**

Run: `cargo test -p orca-tui goal_auto_continuation_stalls`
Expected: PASS

Run: `cargo test -p orca-tui`
Expected: 全部 PASS（现有 goal resume/pause/show 测试不受影响；若有测试断言 `UsageLimited` 由续跑上限产生，按新语义改为 `Stalled` 或删除）

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tui/src/app.rs
git commit -m "feat(tui): replace goal continuation cap with stall detection"
```

---

### Task 4: 文档同步与全量验证

**Files:**
- Modify: `docs/goal-mode.md`

- [ ] **Step 1: 更新 goal-mode.md 状态表**

`docs/goal-mode.md` 状态表（58-65 行附近），把：

```markdown
| `usage_limited` | Orca stopped after the continuation cap |
```

替换为：

```markdown
| `stalled` | Orca stopped after consecutive continuation turns made no measurable progress; `/goal resume` can restart |
| `usage_limited` | Legacy status from the removed continuation cap; kept for old records |
```

- [ ] **Step 2: 更新 Continuation Rules 一节**

把（86-92 行附近）：

```markdown
- the continuation cap is reached
- cost or token budget checks stop the session
```

替换为：

```markdown
- the goal token budget is exhausted (`budget_limited`)
- consecutive continuation turns make no measurable token progress (`stalled`)
- cost budget checks stop the session
```

并在该列表之后补充一段：

```markdown
There is no fixed continuation cap. Each hosted goal turn accounts its token
usage into the persisted goal record; a goal with a `token_budget` flips to
`budget_limited` when usage reaches the budget. Independently, if three
consecutive automatic continuation turns each add fewer than 500 tokens of
recorded usage, Orca marks the goal `stalled` and stops. Both states are
recoverable with `/goal resume`.
```

- [ ] **Step 3: 全量测试**

Run: `cargo test --workspace 2>&1 | tail -20`
Expected: 全部 PASS

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -10`
Expected: 无新增 warning/error

- [ ] **Step 4: Commit**

```bash
git add docs/goal-mode.md
git commit -m "docs(goal): document stall detection and budget stop, drop continuation cap"
```

---

## 验证清单（对照 spec）

- [ ] `MAX_GOAL_CONTINUATIONS` 在代码库中不再存在（`grep -rn MAX_GOAL_CONTINUATIONS crates/` 为空）
- [ ] `Stalled` 非终止、可 `/goal resume`（Task 1 测试）
- [ ] 带 `with_goal_usage_tracking(true)` 的 turn 会把 usage 落进 GoalStore；超预算翻转 `BudgetLimited`（Task 2 测试）
- [ ] 不带标志的 turn 不记账（Task 2 测试）
- [ ] 记账先于 terminal 发布（实现位置在 generation 闭包内，结构性保证）
- [ ] 3 个零增量续跑 turn 后置 `Stalled` 并停（Task 3 测试）
- [ ] 手动首 turn 不计 stall（`turn_was_continuation` 初始为 false，仅 GoalSet 路径）
- [ ] 文档不再提 continuation cap
