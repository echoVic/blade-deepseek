# TUI Single-Surface Interaction Rail Spec

## Status

This is an independently reviewable architecture slice rebased onto `origin/main` at `445baf596`. It removes a TUI-only legacy interaction execution rail. It does not claim that the runtime-wide P1.3 durable interaction broker is fully complete.

## Problem And Evidence

The production TUI ordinary-turn runner is already `TuiSurfaceActions::run_turn`, which attaches a typed runtime surface, admits a typed operation, drains typed events, and sends typed interaction responses back through the runtime surface. The same crate still contains a second test/compatibility rail:

- `TuiInteractionBroker` stores process-local waiters in `Arc<Mutex<HashMap<...>>>` and wakes them on interrupt or shutdown.
- `runtime_interaction_adapter.rs` implements four TUI handlers that register local waiters, mirror records into `RuntimePendingInteractionStore`, emit TUI prompts, and wait for local responses.
- `app.rs` exposes `run_hosted_operation`, `OrdinaryTurnRunner::Legacy`, and legacy test runtimes that exercise that rail.
- `operation_controller.rs` owns both legacy operation/generation state and typed surface state, so the same TUI controller has two interaction ownership models.

This is an architecture defect, not a local handler bug: production behavior and test behavior can use different owners and different recovery semantics. A process-local waiter cannot be recovered after process loss, while typed surface interaction state is runtime-owned and persisted.

## User And Reliability Value

After this slice, all TUI ordinary turns and all TUI interaction behavior tests use the typed runtime surface. A foreground approval, permission request, user question, or MCP elicitation cannot accidentally be implemented or verified through a non-durable local waiter. Cancellation, terminal publication, and operation ownership have one TUI control path, which makes future recovery fixes observable to users instead of only to a legacy test harness.

## Scope

1. Remove the TUI-local `TuiInteractionBroker` and `TuiTurnControl` implementation and their tests.
2. Remove the four legacy runtime interaction adapters and their tests.
3. Remove `run_hosted_operation`, `OrdinaryTurnRunner::Legacy`, legacy runtime helpers, and direct tests that invoke them.
4. Make the TUI operation controller a typed-surface controller only. Keep its supervised presentation-task lifecycle, typed operation activation, cancellation, background handoff, interaction response routing, and shutdown/join behavior.
5. Migrate the two workflow-notification tests that intentionally used the legacy runner to the normal typed test runtime.
6. Delete the legacy event projection and observer support that was reachable only from the removed hosted-operation path (`runtime_event_projection.rs` and the observer implementation in `hosted_runtime.rs`). Keep the typed `TuiHostedOperationOutcome` used by the remaining hosted path.
7. Update the reviewed TUI entrypoint manifest and production roadmap to describe the single typed route.

## Non-Goals And Compatibility

- No CLI argument, TUI user workflow, server/JSONL wire schema, or persisted surface format changes.
- No changes to DeepSeek provider streaming, retry, context compression, or tool execution semantics.
- Do not remove `RuntimePendingInteractionStore` in this slice. It remains a runtime-host compatibility dependency for legacy Goal continuation preflight; its removal requires a separate runtime-wide migration with server/CLI verification.
- Do not remove typed surface recovery or presentation supervision.

## Semantics

### Normal turn

`run_hosted_ordinary_turn` starts and admits one typed surface operation. Runtime interaction requests are persisted by the runtime surface, projected to the TUI, and responses are committed through typed surface commands. The operation terminal event is emitted once and the TUI presentation observer is joined before shutdown.

### Cancel and shutdown

`TuiSurfaceTaskControl` is the only TUI operation-control owner. Interrupt and shutdown request cancellation through the typed surface, cancel supervised presentation tasks, wait for their joins, and preserve terminal/recovery state. There is no local interaction waiter to reset or wake.

### Deny, timeout, disconnect, restart

Existing typed surface semantics remain authoritative: deny/timeout/EOF responses are typed interaction responses; disconnect and restart are represented by persisted surface snapshots and recovery actions. This slice removes only an alternate TUI implementation and therefore must not alter these outcomes.

## Ownership And Type Boundaries

- `RuntimeSurface` owns operation, interaction, response, cancellation, and terminal facts.
- `TuiSurfaceTaskControl` owns only presentation-side activation, cancellation requests, delivery watermarks, and joinable presentation tasks.
- `TuiSurfaceActions` and `surface_client` own typed client attachment, admission, event draining, and typed response submission.
- No TUI module owns a local interaction map or generation waiter after this slice.
- Runtime commands/events and interaction capabilities remain typed enums/structs.

## Acceptance Criteria

1. `rg` finds no `TuiInteractionBroker`, `TuiTurnControl`, `runtime_interaction_adapter`, `run_hosted_operation`, or `OrdinaryTurnRunner::Legacy` in compiled TUI code or tests.
2. The TUI crate compiles with the typed surface control as its only operation-control path.
3. Focused TUI behavior tests pass, including cancellation, background handoff, presentation-task cancellation/join, typed interaction response routing, and workflow-notification submission.
4. Runtime surface interaction and lifecycle suites pass unchanged, demonstrating that approval, permission, user input, MCP elicitation, cancel, and cold recovery behavior remain runtime-owned.
5. The workspace formatter, diff check, clippy, and required workspace tests pass, or any failure is reproduced on clean `origin/main` and recorded as an unrelated baseline gate.
6. The reviewed manifest and roadmap no longer advertise the removed legacy entrypoints and explicitly identify the remaining runtime-host pending-store migration as a later gate.
7. A fresh rebase onto the latest `origin/main` is followed by rerunning the affected focused tests and runtime lifecycle/full gate before the slice is committed.

## Migration, Rollback, And Deletion Gates

The migration order is: boundary RED test, delete local broker/adapters, simplify controller and call sites, migrate tests, update manifest/docs, focused verification, runtime/full verification, review, rebase, commit. Rollback is a branch revert or commit revert; no persisted data migration is needed. The old TUI rail is deleted in this commit. The separate `RuntimePendingInteractionStore` deletion gate is: runtime-host Goal continuation no longer reads process-local pending records, equivalent typed surface facts cover its preflight, and server/CLI lifecycle tests plus real API recovery pass.

## Verification Commands

```bash
cargo test -p orca-tui
cargo test -p orca-runtime --test runtime_surface_interaction
cargo test -p orca-runtime --test runtime_lifecycle_contract
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

The real TUI interaction recovery harness must be rerun before integration because the slice changes the TUI execution boundary, even though it does not change the DeepSeek provider contract.

## Verification Record

- `git fetch origin main` followed by `git rebase origin/main` completed without conflicts; the branch and `origin/main` now share `445baf596` as their tip.
- `cargo test -p orca-tui --lib -- --test-threads=1`: 1021 passed.
- `cargo test -p orca-runtime --test runtime_surface_interaction -- --test-threads=1`: 34 passed.
- `cargo test --test runtime_lifecycle_contract -- --test-threads=1`: 54 passed.
- `cargo test --workspace --all-targets -- --test-threads=1` was not clean on the rebased main: the upstream `runtime_host::tests::session_listing_does_not_block_host_supervisor` fails to settle after FIFO release, and two upstream stateless server tests observe `sessions-index.sqlite3` under `ORCA_HOME`. The isolated FIFO reproduction timed out after 45 seconds; the runtime source is byte-for-byte identical to `origin/main` for this slice (`git diff --quiet origin/main -- crates/orca-runtime`).
- The workspace gate with those three upstream tests explicitly filtered passed across all other targets. `cargo clippy --workspace --all-targets --all-features -- -D warnings` remains blocked by 17 pre-existing `orca-core` lints on the rebased base, including `collapsible_if`, `new_without_default`, and `derivable_impls`; this slice changes no `orca-core` code.
- `cargo-semver-checks 0.49.0` against `origin/main@445baf596` passed 223/223 checks for each of `orca-core`, `orca-runtime`, and `orca-tui`; no semver update is required.
- `cargo fmt --all -- --check`, `git diff --check`, `node scripts/validate-runtime-surface-contract.mjs`, `node scripts/test-validate-runtime-surface-contract.mjs`, and `node scripts/release/test-real-api-tui-approval-recovery.mjs` passed.
- A fresh DeepSeek TUI recovery run emitted `ORCA_TUI_APPROVAL_RECOVERY_REAL_OK ORCA_TUI_RECOVERY_1786113835382_75096` after approval, latest-session recovery, and stream cancellation. The harness also tests ANSI cursor-addressed popup text and OSC title sequences, matching terminal output encountered during validation.
