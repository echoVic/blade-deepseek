# Persistent Goal Mode Design

> Historical design record. The thread-local execution approach in this file
> was removed by the v0.2.46 runtime control-plane redesign. See
> `docs/goal-mode.md` and
> `docs/reports/2026-07-18-goal-runtime-control-plane-incident.md` for the
> current contract.

## Goal

Implement an OpenAI Codex-style `/goal` mode for Orca: goals are persisted per TUI conversation session, can be viewed and controlled with slash commands, continue automatically between turns while active, and can be completed or blocked by the agent through a tool.

## References

- OpenAI Codex source reference: `/tmp/openai-codex-reference/codex-rs/tui/src/goal_display.rs`, `/tmp/openai-codex-reference/codex-rs/tui/src/app/thread_goal_actions.rs`, `/tmp/openai-codex-reference/codex-rs/protocol/src/protocol.rs`.
- Local loop reference: `/Users/qingyun/Documents/GitHub/package 3/src/skills/bundled/loop.ts` and `/Users/qingyun/Documents/GitHub/package 3/src/screens/REPL.tsx`.
- Orca implementation points: `crates/orca-tui/src/commands/mod.rs`, `crates/orca-tui/src/app.rs`, `crates/orca-tui/src/bridge.rs`, `crates/orca-tools/src/registry.rs`, `crates/orca-runtime/src/agent_common.rs`, `crates/orca-runtime/src/history.rs`.

## User-Facing Behavior

`/goal <objective>` creates or replaces the current session goal, marks it active, persists it, and immediately submits the objective as the next user task. If there is already an unfinished goal, Orca replaces it directly and reports that replacement; this keeps the first implementation non-modal while preserving the important Codex semantics.

`/goal` shows the current persisted goal summary: status, objective, elapsed time, tokens used, optional token budget, and available commands.

`/goal clear` deletes the persisted goal for the current session. `/goal pause` marks it paused. `/goal resume` marks it active and starts continuation if the session is idle. `/goal edit <objective>` updates the objective; if the edited goal was complete or budget-limited, it becomes active again.

Goal mode continues automatically after each successful agent turn when the persisted goal is still active. The continuation prompt is internal and asks the agent to continue the goal, report progress, and call `update_goal` when it is complete or blocked. It stops on pause, clear, blocked, complete, budget-limited, approval-required, error, cancellation, or max continuation turns.

## Data Model

Create `orca_core::goal_types`:

- `ThreadGoalStatus`: `Active`, `Paused`, `Blocked`, `UsageLimited`, `BudgetLimited`, `Complete`.
- `ThreadGoal`: `session_id`, `objective`, `status`, `token_budget`, `tokens_used`, `time_used_seconds`, `created_at`, `updated_at`.
- `GoalUpdate`: optional `objective`, optional `status`, optional `token_budget`.
- `MAX_THREAD_GOAL_OBJECTIVE_CHARS = 4000`.

Goals are keyed by Orca history session id. This mirrors Codex thread goals while fitting Orca's existing session history model.

## Persistence

Create `crates/orca-runtime/src/goals.rs` with a small store at `${ORCA_HOME}/goals_1.json`, falling back to `~/.orca/goals_1.json`. The store writes atomically through a temporary file and rename. JSON keeps dependencies low and can later be replaced by SQLite behind the same API.

Required API:

- `goals_db_path() -> PathBuf`
- `GoalStore::load_default()`
- `get(session_id) -> io::Result<Option<ThreadGoal>>`
- `replace(session_id, objective, status, token_budget) -> io::Result<ThreadGoal>`
- `update(session_id, GoalUpdate) -> io::Result<Option<ThreadGoal>>`
- `clear(session_id) -> io::Result<bool>`
- `account_usage(session_id, tokens_delta, elapsed_delta) -> io::Result<Option<ThreadGoal>>`

## Tooling

Add an `update_goal` built-in tool. Arguments:

```json
{
  "status": "complete|blocked|active|paused",
  "objective": "optional replacement objective",
  "reason": "optional short explanation"
}
```

The tool updates the current session goal through a thread-local goal context installed by the TUI bridge while a goal turn is running. Outside goal mode, it returns a failed tool result explaining that no active goal context is available.

The tool output includes the updated status and compact usage summary. This gives the model direct feedback that the loop will stop when appropriate.

## Runtime Instructions

When a goal is active, prepend pinned context to the conversation:

```text
## Goal Mode
The active goal is: <objective>
Continue working until the goal is complete or genuinely blocked. When complete, call update_goal with status "complete". When blocked, call update_goal with status "blocked" and explain the blocker. Do not mark complete just because one turn ended.
```

This mirrors Codex's goal extension behavior without moving Orca to an app-server protocol.

## TUI Integration

`TuiConversationSession` exposes the history session id so slash commands can bind goals to persisted sessions. If history is disabled, `/goal` returns an error explaining that persistent goals require recorded history.

`AppState` caches the current goal for display and slash command responses. The worker thread owns the canonical persisted updates through `UserAction` messages:

- `GoalShow`
- `GoalSet(String)`
- `GoalEdit(String)`
- `GoalClear`
- `GoalPause`
- `GoalResume`

The worker sends `TuiEvent::GoalUpdated`, `TuiEvent::GoalCleared`, and `TuiEvent::GoalStatus` back to the UI. Auto-continuation happens in the worker thread after `run_agent_for_tui` returns, using the persisted goal status.

## Limits And Safety

- A single `/goal` run has a maximum of 64 automatic continuations.
- Continuation stops on `ApprovalRequired`, errors, cancellation, or budget exhaustion.
- Token and elapsed accounting is best effort. Tokens use the existing `CostTracker` totals difference per turn; elapsed time is wall-clock seconds for each agent turn.
- Terminal statuses `Complete` and `BudgetLimited` are not downgraded by pause or block updates.

## Testing

Tests cover:

- Slash parsing for `/goal`, `/goal clear`, `/goal pause`, `/goal resume`, `/goal edit <objective>`.
- Goal objective validation and status labels.
- Persistent store create, update, clear, reload, usage accounting, and terminal status preservation.
- `update_goal` tool failure without context and success with context.
- TUI worker helper decisions: active goals continue, paused/blocked/complete goals stop.

## Out Of Scope

- Codex app-server protocol and listener notifications.
- Attachment materialization for goal objectives.
- Interactive replacement confirmation UI.
- SQLite migration. The storage API is designed so this can be swapped in later.
