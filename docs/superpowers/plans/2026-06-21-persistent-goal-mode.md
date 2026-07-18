# Persistent Goal Mode Implementation Plan

> Historical implementation plan. Its thread-local Goal tool path was removed
> in v0.2.46; current execution ownership is defined by
> `docs/superpowers/plans/2026-07-18-goal-runtime-control-plane.md`.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build an OpenAI Codex-style persistent `/goal` mode for Orca TUI sessions.

**Architecture:** Add goal data types in `orca-core`, a JSON-backed persistent goal store in `orca-runtime`, an `update_goal` tool in `orca-tools`, and TUI worker actions that bind goals to recorded history session ids. The TUI worker auto-continues active goals after each completed turn and stops when persisted status changes to paused, blocked, complete, budget-limited, or usage-limited.

**Tech Stack:** Rust 2024 workspace, serde/serde_json, existing Orca history/session writer, existing TUI mpsc event loop, cargo tests.

---

## File Map

- Create `crates/orca-core/src/goal_types.rs`: shared goal status, goal record, update request, validation helpers.
- Modify `crates/orca-core/src/lib.rs`: export `goal_types`.
- Create `crates/orca-runtime/src/goals.rs`: JSON persistent store keyed by session id.
- Modify `crates/orca-runtime/src/lib.rs`: export `goals`.
- Create `crates/orca-tools/src/update_goal.rs`: parser, thread-local context, tool executor.
- Modify `crates/orca-tools/src/registry.rs`: register `update_goal`.
- Modify `crates/orca-tools/src/lib.rs`: export `update_goal`.
- Modify `crates/orca-core/src/tool_types.rs`: add `ToolName::UpdateGoal`.
- Modify `crates/orca-provider/src/tool_schema.rs`: include the new tool through registry-based schema.
- Modify `crates/orca-runtime/src/agent_common.rs`: append goal-mode instructions when active goal context is present.
- Modify `crates/orca-tui/src/commands/mod.rs`: parse `/goal` commands.
- Modify `crates/orca-tui/src/types.rs`: add goal events/actions and cached state.
- Modify `crates/orca-tui/src/app.rs`: handle slash goal commands, worker actions, auto-continuation.
- Modify `crates/orca-tui/src/bridge.rs`: expose session id, install update-goal context while turns run, account usage/time, return turn status.

## Task 1: Core Goal Types

**Files:**
- Create: `crates/orca-core/src/goal_types.rs`
- Modify: `crates/orca-core/src/lib.rs`

- [ ] Write failing tests in `goal_types.rs` for objective validation, status labels, terminal status behavior, and compact token formatting.
- [ ] Run `cargo test -p orca-core goal_types -- --nocapture`; expect unresolved module or failing tests.
- [ ] Implement `ThreadGoalStatus`, `ThreadGoal`, `GoalUpdate`, `validate_thread_goal_objective`, `goal_status_label`, `format_goal_elapsed_seconds`, `format_tokens_compact`, and `goal_usage_summary`.
- [ ] Export the module from `orca-core/src/lib.rs`.
- [ ] Run `cargo test -p orca-core goal_types -- --nocapture`; expect pass.

## Task 2: Persistent Goal Store

**Files:**
- Create: `crates/orca-runtime/src/goals.rs`
- Modify: `crates/orca-runtime/src/lib.rs`

- [ ] Write failing tests using `tempfile` for `replace`, `get`, `update`, `clear`, reload from disk, usage accounting, and preserving terminal `Complete`/`BudgetLimited` statuses.
- [ ] Run `cargo test -p orca-runtime goals -- --nocapture`; expect unresolved module or failing tests.
- [ ] Implement `GoalStore` with JSON map persistence, atomic temp-file rename, `goals_db_path`, and test-only `with_path`.
- [ ] Export the module from `orca-runtime/src/lib.rs`.
- [ ] Run `cargo test -p orca-runtime goals -- --nocapture`; expect pass.

## Task 3: Update Goal Tool

**Files:**
- Create: `crates/orca-tools/src/update_goal.rs`
- Modify: `crates/orca-tools/src/lib.rs`
- Modify: `crates/orca-tools/src/registry.rs`
- Modify: `crates/orca-core/src/tool_types.rs`

- [ ] Write failing tests for parsing `{"status":"complete"}`, rejecting unsupported statuses, failing without a goal context, and updating a temp store with a goal context.
- [ ] Run `cargo test -p orca-tools update_goal -- --nocapture`; expect unresolved module or failing tests.
- [ ] Add `ToolName::UpdateGoal` and `as_str/from_str` mapping.
- [ ] Implement thread-local `GoalToolContext`, `with_goal_context`, `parse_args`, and `execute`.
- [ ] Register `update_goal` as a built-in read/action tool with schema containing `status`, `objective`, and `reason`.
- [ ] Run `cargo test -p orca-tools update_goal -- --nocapture`; expect pass.

## Task 4: Goal Instructions And Tool Schema

**Files:**
- Modify: `crates/orca-runtime/src/agent_common.rs`
- Modify: `crates/orca-provider/src/tool_schema.rs`

- [ ] Write failing tests asserting active goal instructions mention the objective and `update_goal`.
- [ ] Run `cargo test -p orca-runtime agent_common -- --nocapture`; expect fail.
- [ ] Add a small goal instruction formatter and append it when a goal is supplied by the caller.
- [ ] Ensure provider schema includes `update_goal` through the built-in registry.
- [ ] Run `cargo test -p orca-runtime agent_common -- --nocapture` and `cargo test -p orca-provider tool_schema -- --nocapture`; expect pass.

## Task 5: Slash Commands And State

**Files:**
- Modify: `crates/orca-tui/src/commands/mod.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [ ] Write failing tests for `/goal`, `/goal build thing`, `/goal clear`, `/goal pause`, `/goal resume`, `/goal edit better thing`, and usage errors for `/goal edit`.
- [ ] Run `cargo test -p orca-tui commands -- --nocapture`; expect fail.
- [ ] Add `SlashCommand::Goal(GoalSlashCommand)` and command list entry.
- [ ] Add `UserAction` variants for show/set/edit/clear/pause/resume and `TuiEvent` variants for updated/cleared/status/error.
- [ ] Run `cargo test -p orca-tui commands -- --nocapture`; expect pass.

## Task 6: TUI Session Binding And Auto-Continuation

**Files:**
- Modify: `crates/orca-tui/src/bridge.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] Write failing unit tests for pure helpers: active goals continue, paused/blocked/complete goals stop, continuation prompt includes objective, replacing a goal submits initial objective.
- [ ] Run `cargo test -p orca-tui goal -- --nocapture`; expect fail.
- [ ] Store and expose the `session_id` from `SessionWriter` metadata in `TuiConversationSession`; return an error for goal commands when history is disabled.
- [ ] Have the worker process goal actions with `GoalStore::load_default()`, emit goal events, and submit initial/continuation prompts.
- [ ] Wrap `run_agent_for_tui` calls with `orca_tools::update_goal::with_goal_context` when a goal is active.
- [ ] Account elapsed seconds and token deltas after each goal turn.
- [ ] Stop auto-continuation after 64 automatic goal continuations.
- [ ] Run `cargo test -p orca-tui goal -- --nocapture`; expect pass.

## Task 7: Integration Verification

**Files:**
- Add or modify: `tests/goal_contract.rs`

- [ ] Write an integration test that uses fixture provider mode to start a recorded session, set a goal through a helper if direct TUI automation is not available, reload the store, and verify persistence.
- [ ] Run `cargo test --test goal_contract -- --nocapture`; expect fail before wiring, pass after.
- [ ] Run targeted tests:
  - `cargo test -p orca-core goal_types -- --nocapture`
  - `cargo test -p orca-runtime goals -- --nocapture`
  - `cargo test -p orca-tools update_goal -- --nocapture`
  - `cargo test -p orca-tui commands -- --nocapture`
  - `cargo test -p orca-tui goal -- --nocapture`
- [ ] Run full verification: `cargo test`.

## Self-Review

Spec coverage: the plan covers data model, persistence, update tool, runtime instructions, TUI slash commands, auto-continuation, accounting, and verification. Out-of-scope Codex app-server protocol, attachments, interactive replacement, and SQLite are intentionally excluded by the design.

Placeholder scan: no TBD/TODO/fill-in placeholders remain.

Type consistency: `ThreadGoalStatus`, `ThreadGoal`, `GoalUpdate`, `GoalStore`, `GoalSlashCommand`, `UserAction`, and `TuiEvent` names are used consistently.
