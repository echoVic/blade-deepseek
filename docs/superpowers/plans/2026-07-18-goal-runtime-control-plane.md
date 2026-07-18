# Goal Runtime Control Plane Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make model-visible goal tools executable through the owned runtime, stop deterministic goal control failures before automatic continuation can loop, clear inactive goal context, and publish the verified patch as GitHub and npm release `v0.2.46`.

**Architecture:** `ThreadTurnToolMode::Goal` remains the single turn capability that drives both provider schema visibility and runtime execution. Goal tools become runtime-special control-plane operations executed on the runtime actor path with the persistent session id and live extension stores; no goal operation crosses the normal-tool OS-thread boundary or depends on thread-local state. Tool dispatch returns an explicit turn disposition so invalid model arguments remain recoverable while missing runtime capabilities and persistence failures end the turn; RuntimeHost atomically stalls an active goal after a non-successful goal generation.

**Tech Stack:** Rust workspace (`orca-core`, `orca-tools`, `orca-runtime`, `orca-tui`), JSONL history, Mock and real DeepSeek provider harnesses, Node.js release helpers, GitHub Actions, npm.

---

## Structural Problem And Evidence

`HostedTurnRequest::with_goal_tools(true)` becomes `ThreadTurnToolMode::Goal`, and
`AgentToolPolicyContext::goal_mode()` correctly exposes `get_goal`, `create_goal`,
and `update_goal` to the provider. The same policy is present in
`RuntimeStepSnapshot`, but `RuntimeNormalToolTurnRequest` drops it. The router
therefore classifies goal tools as ordinary tools and sends them through
`RuntimeToolCallRuntime::execute_normal`, which starts the dedicated
`orca-normal-tool` OS thread.

`orca-tools/update_goal.rs` tries to recover the missing runtime owner through a
thread-local `GOAL_HANDLER`. Commit `7ad993322` deleted the only production
`with_goal_handler` call site while moving TUI execution into RuntimeHost.
Commit `9d6d4d432` later made the normal-tool thread boundary explicit, so even
reinstalling the handler around a TUI call would not propagate it to execution.

Ordinary tool failures intentionally remain model-recoverable. Consequently the
missing goal handler returns a failed tool result, the model keeps sampling, the
outer turn can finish `success`, the persisted goal remains `active`, and the
TUI starts another continuation. The current stall detector only counts turns
using fewer than 500 tokens; repeated failed calls consume far more and reset
the streak.

The incident session made 463 real `update_goal` calls, completed 120 automatic
continuations, and left the goal active after 318,271,748 accounted tokens. This
is a control-plane ownership failure, not a model retry-policy defect.

## Target Ownership And Module Boundaries

- `orca-tools/update_goal.rs` owns only argument normalization, parsing, and
  model-facing result formatting. It has no session store, runtime extension,
  callback, TLS, or worker ownership.
- `AgentToolPolicyContext` remains the source of truth for Goal Mode. The policy
  that selects the provider schema is carried into `RuntimeNormalToolTurnRequest`,
  `ToolExecutionContext`, and `RuntimeToolInvocationContext`.
- `RuntimeSpecialToolDispatch` owns `GetGoal`, `CreateGoal`, and `UpdateGoal`
  classification. The router executes them before the normal-tool worker using
  `TaskRegistry::session_id()`, `GoalStore`, and the live thread extension store.
- `RuntimeToolDispatchOutput` owns whether the model may continue after a tool
  result. `ContinueModel` is used for invalid JSON, invalid status transitions,
  an unfinished create request, and ordinary tool failures. `StopTurn` is used
  when a visible goal tool lacks its persistent session/live extension context
  or GoalStore I/O fails.
- `RuntimeHost` owns system reaction to a failed goal generation. After usage
  accounting it atomically changes an `active` goal to `stalled`; it never
  overwrites a goal already paused, blocked, completed, or budget limited.
- `Conversation::replace_goal_state` owns both replacement and removal of the
  volatile goal block. TUI slash commands and continuation exits clear it when
  the goal becomes inactive.

## TUI User Value

- `update_goal({"status":"complete"})` and `blocked` persist on the first
  valid call and stop automatic continuation.
- A deterministic runtime wiring or persistence failure ends one turn and
  leaves a visible recoverable `stalled` goal instead of spending tokens
  indefinitely.
- Malformed model arguments still return a precise tool error and allow the
  model to correct itself in the same turn.
- Paused, blocked, complete, cleared, or failed goals do not leave stale Goal
  Mode instructions in later ordinary turns.

## External Compatibility

- Keep `get_goal`, `create_goal`, and `update_goal` names, JSON schemas, tool
  result messages, `complete|blocked` model transition rules, and direct-call
  failure outside Goal Mode.
- Keep CLI flags, slash commands, TUI keys, server methods, JSONL event names,
  history record shapes, goal database shape, npm package names, and native
  archive layout unchanged.
- Goal tools remain a TUI recorded-history feature. RuntimeHost rejects a hosted
  request that asks to expose goal tools without a persistent session before a
  provider call can observe that schema.
- Ordinary failed Bash, MCP, external, workflow, and file tools retain existing
  model-recovery behavior.

### Task 1: RED Runtime Goal Execution And Admission Tests

**Files:**
- Modify: `crates/orca-runtime/src/controller.rs`
- Modify: `crates/orca-runtime/tests/runtime_host.rs`
- Modify: `crates/orca-runtime/src/goals.rs`

- [x] **Step 1: Add a hosted runtime goal-tool execution test**

Create a recorded `RuntimeThread`, persist an active goal, execute one non-goal
tool to establish live progress, then feed a provider continuation containing:

```rust
ToolRequest {
    id: "goal-update-1".to_string(),
    name: ToolName::UpdateGoal,
    action: ActionKind::Read,
    target: None,
    raw_arguments: Some(r#"{"status":"complete"}"#.to_string()),
}
```

Run the request with `ThreadTurnToolMode::Goal` and assert `RunStatus::Success`
plus persisted `ThreadGoalStatus::Complete`.

- [x] **Step 2: Add a missing goal runtime context terminal test**

Run the same provider continuation through a history-disabled thread in Goal
Mode. Assert the turn returns `RunStatus::Failed`, records one matching failed
tool result, and does not continue to a successful assistant terminal.

- [x] **Step 3: Add RuntimeHost admission coverage**

Start a history-disabled RuntimeHost thread and submit
`HostedTurnRequest::new("goal").with_goal_tools(true)`. Assert admission returns
`RuntimeHostError::ThreadStartFailed` before its executor is called.

- [x] **Step 4: Add atomic active-to-stalled store coverage**

Create one goal per status, call `GoalStore::stall_if_active`, and assert active
becomes stalled while paused, blocked, complete, and budget-limited records
remain byte-for-byte unchanged except for no write occurring.

- [x] **Step 5: Run RED tests**

```bash
cargo test -p orca-runtime hosted_goal_tool --lib -- --nocapture
cargo test -p orca-runtime --test runtime_host goal_tools_require_persistent_session -- --nocapture
cargo test -p orca-runtime stall_if_active --lib -- --nocapture
```

Expected: failures demonstrate TLS-backed goal execution, missing admission
validation, and absent atomic stall behavior.

### Task 2: Pure Goal Tool Protocol And Runtime-Special Dispatch

**Files:**
- Modify: `crates/orca-tools/src/update_goal.rs`
- Modify: `crates/orca-runtime/src/runtime_special.rs`
- Modify: `crates/orca-runtime/src/tool_router.rs`
- Modify: `crates/orca-runtime/src/tool_execution.rs`
- Modify: `crates/orca-runtime/src/tool_turn.rs`

- [x] **Step 1: Remove implicit goal ownership from orca-tools**

Delete `GOAL_HANDLER`, `GoalHandler`, `with_goal_handler`, and `dispatch`. Export
pure helpers with this responsibility:

```rust
pub fn parse_operation(request: &ToolRequest) -> Result<GoalToolOperation, String>;
pub fn completed_result(request: &ToolRequest, goal: Option<&ThreadGoal>) -> ToolResult;
pub fn unavailable_result(request: &ToolRequest) -> ToolResult;
```

Keep the normal registry executor deterministic outside runtime Goal Mode.

- [x] **Step 2: Add goal variants to special dispatch**

Extend `RuntimeSpecialToolDispatch` with `GetGoal`, `CreateGoal`, and
`UpdateGoal`. Implement a runtime goal executor that:

```rust
match operation {
    GoalToolOperation::Get => store.get(session_id),
    GoalToolOperation::Create { objective, token_budget } => {
        match store.get(session_id)? {
            Some(goal) if goal.status.should_continue() => Ok(None),
            Some(goal) if !goal.status.is_terminal() => Ok(None),
            _ => store
                .replace(session_id, &objective, ThreadGoalStatus::Active, token_budget)
                .map(Some),
        }
    }
    GoalToolOperation::Update(update) => {
        validate_goal_terminal_update_against_extensions(&update, thread_store)?;
        store.update(session_id, update)
    }
}
```

Parsing/policy errors return `ContinueModel`; missing visible capability,
session/live extension state, and store I/O return `StopTurn`.

- [x] **Step 3: Carry the schema capability through execution**

Add `goal_mode: bool` to `RuntimeNormalToolTurnRequest`,
`ToolExecutionContext`, and `RuntimeToolInvocationContext`; populate it from
`tool_policy.is_goal_mode()` at the existing step snapshot boundary.

- [x] **Step 4: Propagate explicit turn disposition**

Change router dispatch to return:

```rust
pub(crate) struct RuntimeToolDispatchOutput {
    pub result: ToolResult,
    pub disposition: RuntimeToolTurnDisposition,
}
```

Carry the disposition through `ToolExecutionCompletion`. Record every tool
result exactly once, then return `ToolTurnOutcome::Return { status: Failed }`
only for `StopTurn`.

- [x] **Step 5: Run GREEN and compatibility tests**

```bash
cargo test -p orca-tools update_goal --lib
cargo test -p orca-runtime hosted_goal_tool --lib
cargo test -p orca-runtime tool_turn::tests --lib
cargo test -p orca-runtime runtime_tool_call::tests --lib
```

Expected: goal execution passes without entering `orca-normal-tool`; ordinary
failed-tool continuation tests remain green.

### Task 3: RuntimeHost Failure State And Volatile Context Cleanup

**Files:**
- Modify: `crates/orca-runtime/src/goals.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-core/src/conversation.rs`
- Modify: `crates/orca-runtime/src/session.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [x] **Step 1: Reject impossible hosted goal capability**

In idle StartTurn admission, reject `request.allows_goal_tools()` when
`state.thread.session().session_id()` is absent. Do this before task creation or
generation spawn.

- [x] **Step 2: Stall active goal on failed hosted generation**

After accounting goal usage, inspect the executed terminal. For a tracked Goal
Mode generation whose status is not `Success`, call `stall_if_active`. Do not
change non-active states and do not relabel successful turns containing
recoverable ordinary tool errors.

- [x] **Step 3: Add typed goal context removal**

Change the replacement path to accept `Option<String>`:

```rust
pub fn replace_goal_state(&mut self, content: Option<String>) {
    self.volatile.goal = content
        .filter(|text| !text.trim().is_empty())
        .map(|text| format!("[Goal state]\n{text}"));
}
```

Carry `Option<String>` through `InteractiveSession` and
`RuntimeThreadMutation::ReplaceGoalContext`.

- [x] **Step 4: Clear context at every inactive transition**

Clear the live context after `/goal pause`, `/goal clear`, completed/blocked
tool updates, stalled turn failure, and before a turn for which no active goal
was loaded. Keep active context when waiting for owned workflows.

- [x] **Step 5: Verify continuation stop behavior**

```bash
cargo test -p orca-core replace_goal_state --lib
cargo test -p orca-runtime goal --lib
cargo test -p orca-runtime --test runtime_host goal -- --nocapture
cargo test -p orca-tui goal --lib -- --test-threads=1
```

Expected: deterministic control failure stops after one failed turn, persists
`stalled`, and inactive snapshots contain no volatile goal block.

### Task 4: Full Verification And Real DeepSeek Regression

**Files:**
- Modify: `scripts/release/real-api-e2e.mjs`
- Modify: `scripts/release/test-real-api-e2e.mjs`

- [x] **Step 1: Add a release-harness Goal Mode case**

Run a recorded TUI-compatible goal sequence against real DeepSeek that performs
at least one non-goal tool, calls `update_goal` once, and verifies the goal
record becomes `complete|blocked` without a second automatic continuation. Use
an isolated `ORCA_HOME` and a bounded budget.

- [x] **Step 2: Test the release-harness parser and fixtures**

```bash
node scripts/release/test-real-api-e2e.mjs
```

- [x] **Step 3: Run focused and complete local gates**

```bash
cargo fmt --all -- --check
cargo test -p orca-tools -p orca-runtime -p orca-tui --lib -- --test-threads=1
cargo test --workspace --all-targets -- --test-threads=1
cargo clippy --workspace --all-targets
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
npm --prefix site run build
npm --prefix site run check:seo
git diff --check
```

- [x] **Step 4: Run the real API gate**

```bash
node scripts/release/real-api-e2e.mjs --max-budget 0.02 --timeout-ms 300000
```

Expected: provider, CLI, history, server, and new Goal Mode cases all pass with
the configured DeepSeek credentials.

### Task 5: Documentation And Patch Release

**Files:**
- Modify: `docs/goal-mode.md`
- Modify: `docs/tools-comparison.md`
- Modify: `docs/production-roadmap.md`
- Create: `docs/reports/2026-07-18-goal-runtime-control-plane-incident.md`
- Create: `docs/releases/v0.2.46.md`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `npm/orca/package.json`
- Modify: `site/src/shared.ts`
- Modify: `site/src/changelog/Changelog.tsx`

- [x] **Step 1: Document the incident and invariant**

Record the 463-call/120-continuation evidence, regression commits, TLS boundary,
explicit runtime owner, recoverable/control-plane error distinction, context
cleanup, and deletion audit.

- [x] **Step 2: Update product and architecture docs**

Document that schema visibility implies executable runtime context, goal tools
never run as normal workers, control failures stall once, and ordinary tool
errors remain recoverable.

- [x] **Step 3: Prepare v0.2.46 metadata**

Bump all version sources to `0.2.46`, add release notes, and update both website
languages and release list. Run `cargo check` to align `Cargo.lock`.

- [x] **Step 4: Re-run release gates on final metadata**

Repeat Task 4 Step 3 after the version and docs changes.

- [ ] **Step 5: Commit, merge, tag, and publish**

Create semantic commits for the plan, runtime fix, verification/docs, and
release metadata. Rebase latest `origin/main`, merge the verified branch into
local `main`, rerun the complete gate, push `main`, create and push `v0.2.46`.

- [ ] **Step 6: Monitor and verify public artifacts**

Wait for the tag-triggered release workflow to complete, then run:

```bash
node scripts/release/verify-published.mjs \
  --version 0.2.46 \
  --repo echoVic/blade-deepseek \
  --package @blade-ai/orca \
  --bin orca
```

Verify GitHub Release assets, npm main/platform packages, npm tarball assets,
`npm exec --yes @blade-ai/orca@0.2.46 -- --version`, and Pages deployment.

## Acceptance Criteria

- A Goal Mode tool visible in the provider schema has an executable persistent
  session and live runtime extension context.
- Hosted `get_goal`, `create_goal`, and `update_goal` execute through runtime
  special dispatch and never through the normal-tool worker or TLS.
- One valid terminal `update_goal` persists `complete|blocked`; automatic
  continuation observes it and stops.
- Invalid goal JSON/status remains a recoverable model-facing result.
- Missing visible capability, live context, or GoalStore I/O ends the turn; an
  active goal becomes `stalled` without overwriting any concurrent terminal or
  user-controlled status.
- Ordinary failed tools preserve current model recovery semantics.
- Goal pause, clear, complete, blocked, budget limit, and runtime stall remove
  stale volatile Goal Mode instructions.
- Focused tests, complete serial workspace tests, Clippy, site/SEO, release
  helpers, npm staging, real DeepSeek regression, and diff checks pass.
- `v0.2.46` is public on GitHub and npm, the release workflow and Pages workflow
  pass, and the public verifier plus `npm exec` report `orca 0.2.46`.

## Final Deletion Targets

This work is incomplete until it deletes:

- `thread_local! GOAL_HANDLER`;
- the `GoalHandler` callback type and `with_goal_handler` scope;
- any production goal execution through `RuntimeNormalToolCallRuntime`;
- tests that install a TLS goal handler;
- string-only goal-context replacement that cannot represent absence;
- a continuation path where a deterministic goal control failure can finish the
  outer turn successfully and leave the automatic loop eligible to continue.

The existing token-delta stall detector remains as defense for genuine
low-progress successful turns. It is not the control-failure detector and must
not be used to mask missing runtime ownership.
