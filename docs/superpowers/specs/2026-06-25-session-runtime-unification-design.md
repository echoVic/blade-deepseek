# Session Runtime Unification Design

Date: 2026-06-25

## Goal

Move Orca's interactive session ownership out of the TUI bridge and into `orca-runtime`, so TUI and headless execution can converge on one runtime-owned session model before the protocol and tool-system refactors.

## Scope

This is P0 release one for the Codex-inspired architecture work. It intentionally focuses on the runtime session boundary, not the full Codex app-server model.

In scope:

- Create a runtime-owned interactive session type.
- Centralize conversation initialization, resume/fork replay, history writer setup, project instructions, memory loading, MCP registry initialization, hooks, cost tracking, and workflow task registry.
- Refactor `orca-tui` to wrap the runtime session instead of owning those fields directly.
- Keep current TUI event types and user actions stable.
- Keep current `orca exec`, JSONL, workflow, subagent, and goal behavior compatible.
- Document the new boundary and the next P1 protocol step.

Out of scope:

- Replacing all TUI turn-loop code with a Codex-style async `Thread` in this release.
- Changing public JSONL event schema.
- Renaming tools or changing tool exposure.
- Moving TUI-specific approval prompts into runtime.
- Adding app-server transport.

## Reference

Local Codex reference:

- `/Users/qingyun/Documents/GitHub/codex/codex-rs/core/src/session/session.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/core/src/codex_thread.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/protocol/src/protocol.rs`

Codex keeps long-lived session state in core runtime and lets TUI/client layers send commands and consume events. Orca should move in that direction incrementally: first make the session state runtime-owned, then introduce a protocol layer, then collapse duplicate turn execution paths.

## Current Problem

`crates/orca-tui/src/bridge.rs` owns a `TuiConversationSession` with runtime concerns:

- `Conversation`
- `SessionWriter`
- session id
- project instructions
- cost tracker
- MCP registry
- hooks
- memory block
- task registry

`crates/orca-runtime/src/controller.rs` separately builds the same ingredients for headless execution. This duplication makes every feature touch two execution surfaces and increases the chance that TUI, `exec`, workflows, subagents, and goals drift.

## Proposed Architecture

Add `orca_runtime::session::InteractiveSession`.

Responsibilities:

- Initialize from `RunConfig`, an initial prompt title, and an optional preloaded transcript.
- Own the conversation and persisted history writer.
- Expose session id, usage totals, workflow activity status, backtracking, pinned context, skill context, goal context, model updates, and completion.
- Provide history-safe append helpers.
- Provide `compact()` so the compaction and summary-state persistence path is no longer TUI-owned.

`orca-tui` keeps a small compatibility wrapper named `TuiConversationSession` for now. The wrapper delegates to `InteractiveSession` and exists only to preserve local TUI call sites during the migration.

This creates a real runtime boundary without forcing all event mapping into one release.

## Data Flow

Startup:

1. CLI/TUI builds `RunConfig`.
2. TUI calls `TuiConversationSession::new_with_preloaded`.
3. The wrapper calls `InteractiveSession::new_with_preloaded`.
4. Runtime loads instructions, memory, hooks, MCP registry, task registry, conversation, and history writer.

Turn execution:

1. TUI passes the runtime session into the existing TUI turn runner.
2. The runner mutates the runtime-owned conversation through wrapper accessors.
3. History writes still happen through runtime-owned append/complete methods.
4. Compaction uses `InteractiveSession::compact`.

Future P1a:

1. Runtime emits protocol events.
2. TUI maps protocol events into display state.
3. The TUI-specific turn runner is removed.

## Testing

Add tests before implementation:

- TUI session wrapper exposes the same behavior while delegating ownership to runtime.
- Runtime interactive session reuses conversation across submits.
- Runtime interactive session preserves resume/fork history behavior.
- Runtime compaction persists summary state through the same writer path.

Keep existing integration tests:

- `cargo test --workspace --all-targets`
- `node scripts/release/test-stage-npm.mjs`
- site checks if release docs/site metadata change.

## Documentation

Update:

- `docs/production-roadmap.md`
- `docs/releases/v0.1.31.md`
- this design file
- implementation plan file

## Release Gate

After implementation:

1. Run full Rust tests.
2. Run npm staging test.
3. Run `git diff --check`.
4. Bump version to `0.1.31`.
5. Commit and push.
6. Tag `v0.1.31` and push tag.
7. Wait for GitHub Actions release workflow.
8. Verify GitHub Release, npm registry, and `npm exec` with `scripts/release/verify-published.mjs`.
