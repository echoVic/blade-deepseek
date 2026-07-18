# Goal Runtime Control Plane Incident

Date: 2026-07-18

Status: resolved in v0.2.46

## Summary

A recorded TUI Goal session repeatedly called `update_goal`, received
`goal tools are only available while goal mode is active`, and still completed
enough outer turns to trigger automatic continuation. The model could see the
Goal tool schema, but the execution worker could not reach the persistent Goal
owner. The goal remained `active`, so the TUI kept submitting continuation
turns.

This was a runtime control-plane ownership defect. It was not caused by
DeepSeek ignoring the Goal instructions, and it was not primarily a retry
prompt problem.

## Impact

The incident session was:

```text
~/.orca/sessions/2026/07/17/
session-2026-07-17T16-23-19-7e264ef2-a449-47be-b3f4-cd0b7bd2d9dd.jsonl
```

Structured JSONL and `goals_1.json` evidence showed:

| Measure | Value |
|---------|-------|
| `update_goal` requested events | 463 |
| failed `update_goal` completed events | 463 |
| completed outer sessions | 120 |
| unique persisted turn ids | 121 |
| final Goal status | `active` |
| Goal-accounted tokens | 318,271,748 |
| Goal active time | 18,750 seconds (5h 12m 30s) |
| final session estimated cost | $18.693824488 |

The screenshot's model reasoning estimated roughly 642 failures. That number
was model-generated text, not telemetry. The event journal count of 463
requested and 463 failed calls is authoritative.

The final session usage row contained 327,031,530 input tokens, 1,006,215
output tokens, and 318,261,632 cache tokens. It is a session aggregate and is
therefore distinct from the Goal ledger's 318,271,748 tokens.

## Failure Chain

```text
ThreadTurnToolMode::Goal
  -> provider advertises get_goal/create_goal/update_goal
  -> runtime execution request loses the Goal capability
  -> router classifies update_goal as an ordinary tool
  -> RuntimeToolCallRuntime starts orca-normal-tool
  -> worker has an empty thread-local GOAL_HANDLER
  -> failed tool result is treated as model-recoverable
  -> model retries and the outer turn can still return success
  -> persisted Goal stays active
  -> TUI submits the next automatic continuation
```

The existing low-progress stall detector did not protect this path. It detects
successful continuation turns that each add fewer than 500 tokens. These turns
spent far more tokens on repeated reasoning and tool calls, and the deterministic
control failure was hidden inside an outer success.

## Why Thread-Local State Failed

Rust `thread_local!` provides one independent value per OS thread. It is useful
for thread-bound caches, render state, or telemetry that is meaningful only on
that thread. It is not session-local, actor-local, or async-task-local.

The Goal callback was installed, when it was installed at all, on the TUI
execution thread. Normal tool execution later ran on a dedicated
`orca-normal-tool` OS thread, which had a different empty TLS slot. TLS does not
automatically propagate across `std::thread::spawn`, `spawn_blocking`, worker
pools, or async task migration.

Commit `7ad993322` removed the last production `with_goal_handler` scope while
migrating TUI execution into RuntimeHost. Commit `9d6d4d432` then made the
normal-tool worker boundary explicit. Reinstalling the callback only around the
outer TUI call would still not have transferred it to the worker.

## Systemic Assessment

The observed incident was Goal-specific, but the defect class was systemic:

1. Model-visible schema and executable runtime capability were derived through
   different paths.
2. A persistent control-plane operation tried to recover its owner from ambient
   thread state.
3. A deterministic control-plane failure inherited ordinary data-plane tool
   retry behavior.
4. Inactive persistent state did not have one typed cleanup path for volatile
   prompt context.

Any context-scoped tool could repeat this failure if it advertises a schema
without carrying the same turn capability to execution. The v0.2.46 invariant
is therefore general: schema visibility implies an executable, explicitly
owned runtime context.

## Reference Implementations

The local Codex, Claude Code, and Grok Build checkouts use different APIs but
the same ownership pattern:

- Codex builds model-visible specs and `ToolRegistry` from the same planned
  runtimes. `ToolCallRuntime` explicitly retains `Session`, `StepContext`,
  cancellation, and the turn tracker.
- Claude Code passes `ToolUseContext` into every `Tool.call`. That context owns
  the current tools, `AbortController`, app/session state, and interaction
  capabilities.
- Grok Build uses `ListToolsContext` for listing and `ToolCallContext` for
  execution. Typed extensions carry `SessionContext`, `Cancellation`, cwd, and
  other capabilities, while the stream contract requires one terminal.

None relies on OS thread identity to locate persistent session control state.

## Resolution

v0.2.46 makes Goal execution runtime-owned:

- `ThreadTurnToolMode::Goal` now propagates as `goal_mode` from provider policy
  through the step snapshot to each invocation.
- `get_goal`, `create_goal`, and `update_goal` are runtime-special dispatches
  executed before readonly batching and normal-worker execution.
- The executor uses the recorded session id, live extension stores, and
  `GoalStore` directly.
- `orca-tools/update_goal.rs` owns only parsing and model-facing result
  formatting.
- Invalid model arguments return `ContinueModel`; missing runtime capability or
  persistence failure returns `StopTurn` after recording exactly one result.
- RuntimeHost rejects Goal tools without a persistent session before provider
  sampling.
- A failed tracked Goal generation atomically changes only `active` to
  `stalled` and clears the volatile Goal block.
- Pause, clear, complete, blocked, budget limit, stall, and ordinary turns clear
  stale Goal context.

## Deletion Audit

The release removes:

- `thread_local! GOAL_HANDLER`
- the `GoalHandler` callback type
- `with_goal_handler` and TLS installation tests
- production Goal execution through the normal-tool worker
- string-only Goal context replacement that could not represent absence
- the path where a deterministic Goal control failure could finish the outer
  turn successfully and leave automatic continuation eligible

Historical design and implementation-plan documents may still mention the
removed TLS approach as provenance. They are not current runtime contracts.

## Regression Gates

The release includes focused runtime/tool/TUI tests, a lifecycle contract that
keeps Goal tools out of readonly and normal workers, and a billed DeepSeek
release-harness case. The real case executes `task_list`, calls `update_goal`
exactly once, persists `complete`, and verifies zero eligible continuations:

```text
Goal Mode real API e2e verified:
status=complete
non_goal_tools=1
update_goal_calls=1
continuations=0
```
