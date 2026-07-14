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

## Timing and Resume

The activity timer is cumulative while a goal is active:

```text
displayed time = persisted completed-turn time + current-turn elapsed time
```

Orca persists the current turn's elapsed time when that turn returns. Approval
and user-input waits inside the turn are therefore included. Time between
automatic continuations, time spent paused or in a terminal goal status, and
time while Orca is closed are excluded. A process crash can lose the unfinished
current-turn delta because that delta has not yet been added to the persisted
goal record.

`/goal resume` reactivates the existing record without resetting its objective,
token budget, tokens used, elapsed time, or `created_at`. Same-session resume
updates that record in place. If history restoration produces a different
session id, Orca writes the full record to the restored session and pauses the
source record in the same atomic store replacement.

Cross-session resume refuses to overwrite a goal that already exists for the
target session. Missing-source, collision, and persistence errors leave the
existing goal records unchanged; the TUI also keeps its prior session,
preloaded-history, and history-mode state until the goal-store update succeeds.
After restored history is loaded, Orca projects the preserved goal state before
the first `TurnStarted` event so the cumulative timer has its persisted base
from the first rendered frame.

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

## Agent Tools

Goal mode exposes three scoped tools to the model while a persistent goal turn is running:

```json
{}
{"objective":"ship release verification","token_budget":100000}
{"status":"complete"}
{"status":"blocked","reason":"waiting for credentials"}
```

`get_goal` reads the current goal. `create_goal` creates a new active goal only when no unfinished goal exists. `update_goal` is intentionally narrow: the model can only mark the current goal `complete` or `blocked`. Pause, resume, edit, clear, budget-limited, and usage-limited transitions remain user/system controlled.

The tools are intentionally scoped to goal turns. Outside goal mode they are not advertised to the model, and direct calls fail with a clear message instead of creating hidden state.

## Continuation Rules

Automatic continuation stops when:

- the goal status is no longer `active`
- the current turn fails, is interrupted, or needs approval
- the goal is cleared
- the continuation cap is reached
- cost or token budget checks stop the session

An unknown or malformed provider function name is recoverable inside the same
agent turn rather than being treated as a failed turn. Orca preserves the
provider call id, original name, and raw arguments without inventing a new tool
identity: configured external names remain `External` with their declared
action, unresolved `mcp__*` names remain `Mcp`, and other generic unknown names
become `External`.
Every unresolved request receives provisional `Read`, fails registry validation
as a matching tool result, and is sent back to DeepSeek for correction. Orca
never infers or executes `bash` from a command-shaped function name. Genuine
transport, provider, and quota failures still fail the turn and pause automatic
Goal continuation.

Before each active turn, Orca injects a single pinned goal context block. The block is replaced between turns, so long-running goals do not accumulate duplicate instructions.

## Shell Worker Cleanup On macOS

Goal turns use the same shell sandbox as ordinary Orca turns. The macOS
workspace-write and read-only Seatbelt profiles let a sandboxed process signal
itself and its own child processes. This is required by process managers such
as Vitest, Tinypool, and other test runners when they stop worker pools after a
test failure or shutdown.

The permission remains lineage-scoped: sandboxed commands cannot signal
unrelated processes or other processes that merely share the sandbox. Orca
also starts non-interactive shell commands in their own process group so
timeouts and explicit cancellation can clean up the command tree.

## Resource Ownership

Goal mode does not keep a separate in-memory copy of every subprocess output.
It uses the same runtime tool paths as ordinary turns. The integrated v0.2.22
candidate makes the ownership contract across those shared paths explicit:

- captured stdout and stderr are bounded at process ingress
- streamed shell output uses bounded retained storage
- external tools, verifier commands, MCP servers, workflows, async subagents,
  and server shells keep an owner responsible for terminal cleanup
- timeout, cancellation, setup failure, stdin EOF, and server shutdown either
  wait for owned children or transfer them to an explicit reaper

The candidate Unix behavior uses process groups so cleanup reaches descendants.
On non-Unix platforms, it guarantees only direct-child kill and wait; complete
descendant-tree parity remains follow-up work. These guarantees are candidate
scope and are not part of the published v0.2.21 contract until v0.2.22 ships.

The v0.2.18 memory incident was traced to sandboxed Vitest/Tinypool workers that
could not be signaled and survived their parent. v0.2.19 fixed that direct
Seatbelt policy defect. See
[`docs/reports/2026-07-12-goal-resource-incident.md`](reports/2026-07-12-goal-resource-incident.md)
for the evidence and the separate v0.2.22 defense-in-depth candidate scope.

## Implementation Notes

- Shared types live in `crates/orca-core/src/goal_types.rs`.
- Persistence lives in `crates/orca-runtime/src/goals.rs`.
- The model-facing goal tools live in `crates/orca-tools/src/update_goal.rs`.
- TUI slash commands and continuation live in `crates/orca-tui/src/app.rs` and `crates/orca-tui/src/bridge.rs`.
