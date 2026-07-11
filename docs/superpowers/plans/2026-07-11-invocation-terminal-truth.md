# Invocation Terminal Truth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `v0.2.17` so every accepted tool invocation has one truthful terminal result, interrupted work is distinguishable from failed or crash-unknown work, and tool interruption/replay behavior is registry metadata rather than name-based policy.

**Architecture:** Add canonical terminal and control semantics to `orca-core`, carry terminal metadata with each model-context tool message, and repair legacy missing results as deterministic `indeterminate` terminals without rewriting old JSONL. Runtime and TUI execution loops close the current invocation and every unstarted sibling before returning; adapters emit observed `cancelled` results only when cancellation and cleanup are known, while crash recovery remains conservative. Existing CLI, TUI, server methods, event names, and history records stay compatible through additive enum values and optional metadata.

**Tech Stack:** Rust 2024, Cargo tests, JSONL session history, DeepSeek chat completions, ratatui TUI, Node.js release verification, GitHub Actions, npm.

---

## Scope And Invariants

This plan implements P0.1 from `docs/reports/2026-07-11-codex-package3-runtime-refactor.md`.

1. `cancelled` means Orca observed cancellation and completed the adapter's cleanup contract. It does not imply side effects were rolled back.
2. `indeterminate` means execution may have started but no trustworthy terminal result survived. It must never be retried automatically.
3. A tool call that has not started when its turn stops receives a synthetic `cancelled` result explaining that it was not executed.
4. Legacy assistant tool calls without results receive deterministic `indeterminate` results in memory; the source JSONL is not rewritten.
5. `ReplaySemantics` independently determines whether a caller may retry a terminal or interrupted invocation.
6. No `DetachAndObserve` tool is enabled until `RuntimeHost` can atomically adopt and join it in P0.3/P1.2.
7. Every execution surface emits and persists exactly one result per assistant tool call before returning a terminal turn outcome.

## File Map

- `crates/orca-core/src/tool_types.rs`: canonical `ToolStatus`, `ToolResultKind`, `ToolTerminal`, `InterruptSemantics`, `ReplaySemantics`, and constructors.
- `crates/orca-core/src/conversation.rs`: terminal-aware tool messages and deterministic call/result normalization.
- `crates/orca-core/src/event_schema.rs`: additive terminal fields in `tool.call.completed` payloads.
- `crates/orca-core/src/thread_item_projection.rs`: persisted-message terminal metadata projection.
- `crates/orca-tools/src/registry.rs`: required control semantics for built-in, MCP, and external tools.
- `crates/orca-tools/src/bash.rs`: observed shell cancellation returns `cancelled`.
- `crates/orca-tools/src/external.rs`: observed external-process cancellation returns `cancelled`.
- `crates/orca-tools/src/lib.rs`: terminal closure helpers and registry control-policy lookup.
- `crates/orca-runtime/src/session.rs`: records terminal-aware tool messages once for live context and history.
- `crates/orca-runtime/src/history.rs`: resumes legacy incomplete calls as `indeterminate` rather than deleting them.
- `crates/orca-runtime/src/thread_store/types.rs`: round-trips optional terminal metadata in existing message records.
- `crates/orca-runtime/src/thread_store/projection.rs`: projects live and stored tool terminals identically.
- `crates/orca-runtime/src/lifecycle.rs`: maps cancelled pre-tool hooks and task lifecycle correctly.
- `crates/orca-runtime/src/tool_execution.rs`: maps terminal status to run status and extension outcomes.
- `crates/orca-runtime/src/tool_turn.rs`: closes every unstarted sibling before an early return.
- `crates/orca-runtime/src/runtime_readonly_tool_turn.rs`: threads cancellation into read-only batches without detaching workers.
- `crates/orca-runtime/src/runtime_user_input.rs`: user-cancelled interaction returns `cancelled`.
- `crates/orca-runtime/src/extension.rs`: distinguishes cancelled and indeterminate lifecycle outcomes.
- `crates/orca-runtime/src/goals.rs`: does not count cancelled/unstarted work as a completed tool attempt.
- `crates/orca-runtime/src/child_agent_loop_runner.rs`: maps child tool cancellation to a cancelled child turn.
- `crates/orca-tui/src/agent_runner.rs`: persists terminal-aware results and closes pending siblings in the legacy TUI loop.
- `crates/orca-tui/src/agent_tool_execution.rs`: preserves typed cancellation through TUI adapters.
- `crates/orca-tui/src/runtime_event_projection.rs`: keeps `cancelled` and `indeterminate` status text intact.
- `crates/orca-tui/src/types.rs`: renders cancelled and indeterminate tool rows as distinct terminal states.
- `tests/runtime_lifecycle_contract.rs`: canonical runtime terminal and sibling-closure behavior.
- `tests/thread_store_contract.rs`: live/stored projection and JSONL compatibility.
- `tests/session_server_contract.rs`: additive server event and thread item behavior.
- `tests/agent_loop_contract.rs`: headless turn cancellation leaves no unmatched tool calls.
- `scripts/release/real-api-e2e.mjs`: resumes a synthetic incomplete tool turn against DeepSeek.
- `scripts/release/test-real-api-e2e.mjs`: verifies the new real-API scenario is mandatory.
- `Cargo.toml`, `Cargo.lock`, `README.md`, `npm/orca/package.json`, `site/index.html`, `site/src/shared.ts`, `site/src/changelog/Changelog.tsx`: `v0.2.17` release metadata.
- `docs/production-roadmap.md`, `docs/releases/v0.2.17.md`: current architecture and release evidence.

### Task 1: Canonical Terminal And Control Types

**Files:**
- Modify: `crates/orca-core/src/tool_types.rs`
- Modify: `crates/orca-core/src/event_schema.rs`

- [ ] **Step 1: Write failing terminal-constructor and serialization tests**

Add focused tests that require these public shapes:

```rust
assert_eq!(
    ToolResult::cancelled(&request, "turn interrupted", None).status,
    ToolStatus::Cancelled,
);
assert_eq!(
    ToolResult::indeterminate(&request, "missing terminal result").kind,
    ToolResultKind::Indeterminate,
);
assert_eq!(ToolStatus::Cancelled.as_str(), "cancelled");
assert_eq!(ToolStatus::Indeterminate.as_str(), "indeterminate");
assert_eq!(
    serde_json::to_value(events.tool_call_completed(&cancelled)).unwrap()["payload"]["status"],
    "cancelled",
);
```

- [ ] **Step 2: Run the tests and verify RED**

Run:

```bash
cargo test -p orca-core tool_terminal -- --nocapture
```

Expected: compile failure because `Cancelled`, `Indeterminate`, and their constructors do not exist.

- [ ] **Step 3: Add canonical terminal/control types**

Implement:

```rust
pub enum InterruptSemantics {
    CooperativeCancel,
    WaitForTerminal,
    DetachAndObserve,
}

pub enum ReplaySemantics {
    SafeToRetry,
    IdempotentWithKey,
    IndeterminateAfterStart,
}

pub enum ToolTerminalSource {
    Observed,
    CompatibilityRepair,
}

pub struct ToolTerminal {
    pub status: ToolStatus,
    pub error: Option<String>,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    pub kind: ToolResultKind,
    pub source: ToolTerminalSource,
}
```

Add `Cancelled` and `Indeterminate` to both `ToolStatus` and `ToolResultKind`; add `ToolResult::cancelled`, `ToolResult::cancelled_before_start`, and `ToolResult::indeterminate`. Extend `ToolSpec` with required `interrupt_semantics` and `replay_semantics` fields. Keep serialized names snake_case and preserve all old names.

- [ ] **Step 4: Run core tests and verify GREEN**

Run:

```bash
cargo test -p orca-core tool_terminal -- --nocapture
cargo test -p orca-core completed_result_kinds_remain_completed_status -- --nocapture
```

Expected: all selected tests pass.

- [ ] **Step 5: Commit the canonical types**

```bash
git add crates/orca-core/src/tool_types.rs crates/orca-core/src/event_schema.rs
git commit -m "feat(core): model truthful tool terminals"
```

### Task 2: Registry-Owned Interrupt And Replay Policy

**Files:**
- Modify: `crates/orca-tools/src/registry.rs`
- Modify: `crates/orca-tools/src/lib.rs`

- [ ] **Step 1: Write failing registry policy tests**

Require the registry to expose policy by resolved tool identity:

```rust
let bash = default_tool_registry().resolve("bash").unwrap();
assert_eq!(bash.spec.interrupt_semantics, InterruptSemantics::CooperativeCancel);
assert_eq!(bash.spec.replay_semantics, ReplaySemantics::IndeterminateAfterStart);

let read = default_tool_registry().resolve("read_file").unwrap();
assert_eq!(read.spec.interrupt_semantics, InterruptSemantics::WaitForTerminal);
assert_eq!(read.spec.replay_semantics, ReplaySemantics::SafeToRetry);
```

Also assert that arbitrary external and MCP tools are `IndeterminateAfterStart`, and no registered tool uses `DetachAndObserve`.

- [ ] **Step 2: Run registry tests and verify RED**

Run:

```bash
cargo test -p orca-tools registry_control_semantics -- --nocapture
```

Expected: compile or assertion failure because registry specs do not populate control metadata.

- [ ] **Step 3: Populate conservative policy**

Set built-in defaults to `WaitForTerminal`; set replay to `SafeToRetry` only for capability sets that are read-only. Explicitly set bash to `CooperativeCancel`. Set external process and MCP proxy tools to `CooperativeCancel + IndeterminateAfterStart`. Expose a registry lookup helper used by runtime/TUI instead of matching names.

- [ ] **Step 4: Run registry tests and verify GREEN**

Run:

```bash
cargo test -p orca-tools registry_control_semantics -- --nocapture
cargo test -p orca-tools default_registry_exposes_builtin_tool_metadata -- --nocapture
```

Expected: all selected tests pass and no registered tool enables detach.

- [ ] **Step 5: Commit registry policy**

```bash
git add crates/orca-tools/src/registry.rs crates/orca-tools/src/lib.rs
git commit -m "feat(tools): declare interrupt and replay policy"
```

### Task 3: Terminal-Aware Conversation And Legacy Repair

**Files:**
- Modify: `crates/orca-core/src/conversation.rs`
- Modify: `crates/orca-provider/src/deepseek_http.rs`
- Modify: `crates/orca-provider/src/context.rs`

- [ ] **Step 1: Replace the drop test with failing repair tests**

Change the current incomplete-boundary expectation to require the assistant call and a synthesized result:

```rust
normalize_tool_boundaries(&mut messages);
assert_eq!(messages.len(), 3);
assert!(matches!(&messages[1], Message::Assistant { tool_calls, .. } if tool_calls.len() == 1));
assert!(matches!(
    &messages[2],
    Message::Tool { tool_call_id, terminal: Some(terminal), .. }
        if tool_call_id == "call_1"
            && terminal.status == ToolStatus::Indeterminate
            && terminal.source == ToolTerminalSource::CompatibilityRepair
));
```

Add a two-call test proving existing results are preserved, missing results are inserted in call order, duplicate/orphan results do not create a second terminal, and repeated normalization is byte-equivalent at the provider message layer.

- [ ] **Step 2: Run core/provider tests and verify RED**

Run:

```bash
cargo test -p orca-core normalize_tool_boundaries_repairs_missing_results -- --nocapture
cargo test -p orca-provider api_messages_repair_incomplete_tool_call_boundaries -- --nocapture
```

Expected: assertions fail because the assistant call is currently dropped.

- [ ] **Step 3: Carry optional terminal metadata on tool messages**

Extend `Message::Tool` with `terminal: Option<ToolTerminal>`. Keep `Conversation::add_tool_result(id, content)` as a legacy/test convenience with `None`; add `add_tool_result_with_terminal(&ToolResult, content)` for production writes. Rewrite normalization to preserve the assistant call, keep one matching result per call ID, synthesize deterministic `CompatibilityRepair` indeterminate results for missing IDs, and order results by the assistant call list.

- [ ] **Step 4: Verify DeepSeek replay and context compaction**

Run:

```bash
cargo test -p orca-core normalize_tool_boundaries -- --nocapture
cargo test -p orca-provider api_messages_repair_incomplete_tool_call_boundaries -- --nocapture
cargo test -p orca-provider context::tests -- --nocapture
```

Expected: repaired calls reach DeepSeek with one tool message per call; context compaction preserves terminal metadata when it clones or truncates tool messages.

- [ ] **Step 5: Commit conversation repair**

```bash
git add crates/orca-core/src/conversation.rs crates/orca-provider/src/deepseek_http.rs crates/orca-provider/src/context.rs
git commit -m "fix(history): repair missing tool terminals"
```

### Task 4: JSONL Round Trip And Projection Parity

**Files:**
- Modify: `crates/orca-core/src/thread_item_projection.rs`
- Modify: `crates/orca-runtime/src/thread_store/types.rs`
- Modify: `crates/orca-runtime/src/thread_store/writer.rs`
- Modify: `crates/orca-runtime/src/thread_store/projection.rs`
- Modify: `crates/orca-runtime/src/history.rs`
- Test: `tests/thread_store_contract.rs`

- [ ] **Step 1: Write failing round-trip and projection tests**

Persist cancelled and indeterminate tool messages, reload them, and require both live and stored item projection to include:

```json
{
  "status": "indeterminate",
  "terminalSource": "compatibility_repair",
  "error": "Tool invocation outcome is indeterminate because its terminal result was missing from recovered history. Inspect external state before retrying."
}
```

Add a resume test proving the original JSONL bytes are unchanged while the resumed conversation contains the synthetic terminal.

- [ ] **Step 2: Run storage tests and verify RED**

Run:

```bash
cargo test -p orca-runtime resume_repairs_incomplete_assistant_tool_call_turns -- --nocapture
cargo test --test thread_store_contract tool_terminal_metadata -- --nocapture
```

Expected: failure because `StoredMessage -> Message` discards status metadata and live projection cannot see it.

- [ ] **Step 3: Round-trip one canonical metadata object**

Keep the existing optional `status`, `error`, `exit_code`, and `truncated` fields in the JSONL shape, then add optional `kind` and `terminal_source` fields. Convert those flat stored fields to and from one `ToolTerminal` in the canonical `Message::Tool` model. Old records must still deserialize with `kind/source == None`; new records preserve the full terminal. Make live and stored projection call the same terminal-completion helper.

- [ ] **Step 4: Run storage and history tests and verify GREEN**

Run:

```bash
cargo test -p orca-runtime history::tests -- --nocapture
cargo test --test thread_store_contract -- --test-threads=1
```

Expected: all tests pass, legacy records remain readable, and source transcripts are not rewritten during resume.

- [ ] **Step 5: Commit storage parity**

```bash
git add crates/orca-core/src/thread_item_projection.rs crates/orca-runtime/src/thread_store crates/orca-runtime/src/history.rs tests/thread_store_contract.rs
git commit -m "feat(history): persist invocation terminal metadata"
```

### Task 5: Observed Cancellation At Tool Adapters

**Files:**
- Modify: `crates/orca-tools/src/bash.rs`
- Modify: `crates/orca-tools/src/external.rs`
- Modify: `crates/orca-tools/src/registry.rs`
- Modify: `crates/orca-runtime/src/lifecycle.rs`
- Modify: `crates/orca-runtime/src/runtime_user_input.rs`

- [ ] **Step 1: Write failing cancellation-status tests**

Update existing shell/external cancellation tests and add MCP/pre-hook/user-input tests requiring `ToolStatus::Cancelled`, `ToolResultKind::Cancelled`, and preserved partial output/error diagnostics.

- [ ] **Step 2: Run adapter tests and verify RED**

Run:

```bash
cargo test -p orca-tools bash_wait_observes_cancel_callback -- --nocapture
cargo test -p orca-tools external_tool_wait_observes_cancel_callback -- --nocapture
cargo test -p orca-runtime cancelled_user_input_returns_cancelled_result -- --nocapture
cargo test -p orca-runtime cancelled_pre_tool_hook_returns_cancelled_result -- --nocapture
```

Expected: assertions report `failed` because cancellation is currently collapsed into runtime failure.

- [ ] **Step 3: Return typed observed cancellation**

Use `ToolResult::cancelled` after shell/external process trees have been killed and joined. Map the exact MCP transport cancellation error to cancelled only when the request cancel predicate is true. Map a cancelled pre-tool hook and a user-cancelled input request to cancelled. Preserve completed tool results when only their post-tool hook is cancelled.

- [ ] **Step 4: Run adapter tests and verify GREEN**

Run:

```bash
cargo test -p orca-tools cancel -- --nocapture
cargo test -p orca-mcp cancel -- --nocapture
cargo test -p orca-runtime cancelled_ -- --nocapture
```

Expected: all selected tests pass; process/MCP cancellation remains prompt and reconnectable.

- [ ] **Step 5: Commit adapter cancellation**

```bash
git add crates/orca-tools/src/bash.rs crates/orca-tools/src/external.rs crates/orca-tools/src/registry.rs crates/orca-runtime/src/lifecycle.rs crates/orca-runtime/src/runtime_user_input.rs
git commit -m "fix(tools): preserve observed cancellation terminals"
```

### Task 6: Canonical Runtime Sibling Closure

**Files:**
- Modify: `crates/orca-runtime/src/session.rs`
- Modify: `crates/orca-runtime/src/tool_turn.rs`
- Modify: `crates/orca-runtime/src/runtime_readonly_tool_turn.rs`
- Modify: `crates/orca-runtime/src/tool_execution.rs`
- Modify: `crates/orca-runtime/src/lifecycle.rs`
- Modify: `crates/orca-runtime/src/extension.rs`
- Modify: `crates/orca-runtime/src/goals.rs`
- Modify: `crates/orca-runtime/src/child_agent_loop_runner.rs`
- Test: `tests/runtime_lifecycle_contract.rs`
- Test: `tests/agent_loop_contract.rs`

- [ ] **Step 1: Write failing early-return closure tests**

Use one provider response containing three calls. Cover cancellation before the first call and cancellation/approval failure after the first call. Assert:

```rust
assert_eq!(assistant_call_ids, terminal_result_ids);
assert_eq!(terminal_result_ids.len(), 3);
assert_eq!(terminal_statuses, ["cancelled", "cancelled", "cancelled"]);
assert_eq!(events_for_each_call_id, 1);
```

Add an indeterminate status mapping test requiring `RunStatus::Failed`; cancelled requires `RunStatus::Cancelled` and `RuntimeTaskStatus::Cancelled`. Goal accounting must exclude cancelled/unstarted calls and count indeterminate started calls as an attempted tool.

- [ ] **Step 2: Run runtime tests and verify RED**

Run:

```bash
cargo test -p orca-runtime tool_turn_closes_unstarted_siblings_on_cancel -- --nocapture
cargo test -p orca-runtime tool_terminal_status_maps_to_runtime_lifecycle -- --nocapture
cargo test --test agent_loop_contract cancelled_tool_turn_has_complete_boundaries -- --nocapture
```

Expected: missing result/event assertions fail for the unstarted sibling calls.

- [ ] **Step 3: Add one runtime closure path**

Make `record_tool_result_for_agent` append a terminal-aware `Message::Tool` and persist that same message. Add a helper that receives pending `ToolRequest`s, emits one requested/completed event pair when needed, records `cancelled_before_start` results, and advances the sampling cursor. Invoke it before every early return from `run_tool_turns`. Do not mark completed results cancelled merely because a cancellation request races with completion.

- [ ] **Step 4: Map terminal status exhaustively**

Map `Cancelled` to cancelled run/task/extension outcomes. Map `Indeterminate` to failed run/task state and an indeterminate extension outcome with `handler_executed: true`. Keep `Denied` as approval-required and preserve old failure semantics.

- [ ] **Step 5: Run runtime tests and verify GREEN**

Run:

```bash
cargo test -p orca-runtime tool_turn -- --nocapture
cargo test -p orca-runtime tool_terminal -- --nocapture
cargo test --test runtime_lifecycle_contract -- --test-threads=1
cargo test --test agent_loop_contract -- --test-threads=1
```

Expected: all selected tests pass with one terminal per accepted call.

- [ ] **Step 6: Commit runtime closure**

```bash
git add crates/orca-runtime/src tests/runtime_lifecycle_contract.rs tests/agent_loop_contract.rs
git commit -m "fix(runtime): close every tool invocation"
```

### Task 7: TUI And Server Surface Parity

**Files:**
- Modify: `crates/orca-tui/src/agent_runner.rs`
- Modify: `crates/orca-tui/src/agent_tool_execution.rs`
- Modify: `crates/orca-tui/src/runtime_event_projection.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-runtime/src/protocol/wire.rs`
- Test: `tests/session_server_contract.rs`

- [ ] **Step 1: Write failing TUI and server parity tests**

Require a cancelled TUI bash row to retain `status == "cancelled"`, an indeterminate repaired history item to display a state-inspection warning, and server `tool.call.completed` plus `thread/items/list` projections to expose the same terminal status. Add a TUI multi-call cancellation test proving all sibling rows terminate without a permanent spinner.

- [ ] **Step 2: Run surface tests and verify RED**

Run:

```bash
cargo test -p orca-tui tool_terminal -- --nocapture
cargo test --test session_server_contract tool_terminal -- --test-threads=1
```

Expected: TUI or server assertions fail because current paths collapse or omit terminal metadata.

- [ ] **Step 3: Reuse canonical runtime semantics in projections**

Use terminal-aware conversation writes in the TUI loop, close unstarted siblings before every TUI early return, and preserve status strings through `TuiEvent::ToolCompleted`. Render `cancelled` as interrupted and `indeterminate` as state unknown; never render either as success. Keep existing server method/event names and add only optional fields.

- [ ] **Step 4: Run TUI/server tests and verify GREEN**

Run:

```bash
cargo test -p orca-tui tool_terminal -- --nocapture
cargo test -p orca-tui tui_streaming_bash_observes_turn_cancel -- --nocapture
cargo test --test session_server_contract tool_terminal -- --test-threads=1
```

Expected: all selected tests pass and no receiving/running tool row survives turn completion.

- [ ] **Step 5: Commit surface parity**

```bash
git add crates/orca-tui/src crates/orca-runtime/src/protocol/wire.rs tests/session_server_contract.rs
git commit -m "feat(tui): show truthful tool interruption state"
```

### Task 8: Real DeepSeek Replay And Full Verification

**Files:**
- Modify: `scripts/release/real-api-e2e.mjs`
- Modify: `scripts/release/test-real-api-e2e.mjs`

- [ ] **Step 1: Write the failing release-harness contract**

Require the real API harness to create a temporary JSONL session with one assistant tool call and no result, resume it, and assert that the next DeepSeek request succeeds without re-executing the missing tool. Require output evidence containing the repaired call ID and `indeterminate` marker.

- [ ] **Step 2: Run the harness contract and verify RED**

Run:

```bash
node scripts/release/test-real-api-e2e.mjs
```

Expected: failure because the scenario and evidence checks do not exist.

- [ ] **Step 3: Implement and run the real API scenario**

Run:

```bash
node scripts/release/test-real-api-e2e.mjs
node scripts/release/real-api-e2e.mjs
```

Expected: both pass; the real request accepts the repaired boundary and does not invoke the legacy missing call.

- [ ] **Step 4: Run the full local gate**

Run:

```bash
cargo fmt
cargo fmt -- --check
cargo test --workspace --all-targets --offline -- --test-threads=1
cargo clippy --workspace --all-targets --offline -- -D warnings
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
git diff --check
```

Expected: every command exits zero.

- [ ] **Step 5: Commit verification harness**

```bash
git add scripts/release/real-api-e2e.mjs scripts/release/test-real-api-e2e.mjs
git commit -m "test(release): verify indeterminate tool replay"
```

### Task 9: Document And Release `v0.2.17`

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `README.md`
- Modify: `npm/orca/package.json`
- Modify: `site/index.html`
- Modify: `site/src/shared.ts`
- Modify: `site/src/changelog/Changelog.tsx`
- Modify: `docs/production-roadmap.md`
- Create: `docs/releases/v0.2.17.md`

- [ ] **Step 1: Update architecture and public version metadata**

Document the user-visible behavior: interrupted tools stop showing generic failure, incomplete legacy turns resume with an explicit state-unknown result, and multi-call turns cannot leave permanent running rows. Mark only P0.1 complete; keep P0.2+ pending and name the old drop behavior that was removed.

- [ ] **Step 2: Run release staging checks**

Run:

```bash
node scripts/release/stage-npm.mjs --version 0.2.17
node scripts/release/test-stage-npm.mjs
cargo fmt -- --check
git diff --check
```

Expected: staged npm package and repository metadata agree on `0.2.17`.

- [ ] **Step 3: Commit the release slice**

```bash
git add Cargo.toml Cargo.lock README.md npm/orca/package.json site docs/production-roadmap.md docs/releases/v0.2.17.md
git commit -m "release: prepare v0.2.17"
```

- [ ] **Step 4: Push, tag, and verify public artifacts**

Run:

```bash
git push origin main
git tag v0.2.17
git push origin v0.2.17
gh run list --repo echoVic/blade-deepseek --limit 10
node scripts/release/verify-published.mjs --version 0.2.17 --repo echoVic/blade-deepseek --package @blade-ai/orca --bin orca
```

Expected: the tag workflow succeeds; GitHub Release, npm package, and `npm exec` smoke are publicly verified.

## Completion Audit

- [ ] Every assistant tool call has exactly one matching tool result in model-bound context.
- [ ] Observed adapter cancellation serializes as `cancelled`, not `failed`.
- [ ] Legacy missing results serialize in memory as `indeterminate` with a compatibility marker.
- [ ] No automatic retry path treats `IndeterminateAfterStart` as safe.
- [ ] TUI, headless, and server expose the same terminal status.
- [ ] Runtime, TUI, process, MCP, and provider work is joined or synchronously completed; no new detached worker exists.
- [ ] Old drop-incomplete-call tests and behavior are removed.
- [ ] Existing JSONL and wire shapes remain readable/accepted.
- [ ] Focused tests, full workspace tests, clippy, real DeepSeek smoke, site checks, and release tests pass.
- [ ] The implementation is committed in semantic slices, pushed, tagged, and publicly verified as `v0.2.17`.
