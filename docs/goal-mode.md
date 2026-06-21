# Persistent Goal Mode

Orca's TUI supports Codex-style persistent goals through `/goal`. A goal is a long-running objective attached to the current saved conversation session.

## Commands

```text
/goal                         # show current goal summary
/goal <objective>             # create or replace the active goal
/goal edit <objective>        # update objective and reactivate terminal goals
/goal pause                   # stop automatic continuation
/goal resume                  # reactivate and continue
/goal clear                   # delete this session's goal
```

`/goal <objective>` creates the session if needed, persists the objective, marks it `active`, and immediately submits the objective to the agent. If the goal remains active after a successful turn, Orca submits an internal continuation prompt and keeps working.

## Persistence

Goals are stored by session id in:

- `$ORCA_HOME/goals_1.json` when `ORCA_HOME` is set
- `~/.orca/goals_1.json` otherwise

Persistent goals require recorded history so there is a stable session id. TUI sessions started with history disabled cannot use `/goal`.

## Statuses

| Status | Meaning |
|--------|---------|
| `active` | Orca should continue automatically after successful turns |
| `paused` | User stopped automatic continuation; `/goal resume` can restart |
| `blocked` | Agent reported a blocker that needs user input or an external change |
| `usage_limited` | Orca stopped after the continuation cap |
| `budget_limited` | Token budget was reached |
| `complete` | Agent reported the objective is finished |

Terminal statuses are not downgraded by pause/block operations.

## Agent Tool

Goal mode exposes `update_goal` to the model while a persistent goal turn is running:

```json
{"status":"complete"}
{"status":"blocked","reason":"waiting for credentials"}
{"status":"active","objective":"revised objective"}
```

The tool is intentionally scoped to goal turns. Outside goal mode it fails with a clear message instead of creating hidden state.

## Continuation Rules

Automatic continuation stops when:

- the goal status is no longer `active`
- the current turn fails, is interrupted, or needs approval
- the goal is cleared
- the continuation cap is reached
- cost or token budget checks stop the session

Before each active turn, Orca injects a single pinned goal context block. The block is replaced between turns, so long-running goals do not accumulate duplicate instructions.

## Implementation Notes

- Shared types live in `crates/orca-core/src/goal_types.rs`.
- Persistence lives in `crates/orca-runtime/src/goals.rs`.
- The model-facing tool lives in `crates/orca-tools/src/update_goal.rs`.
- TUI slash commands and continuation live in `crates/orca-tui/src/app.rs` and `crates/orca-tui/src/bridge.rs`.
