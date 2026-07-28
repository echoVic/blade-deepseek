# JSONL Stateless Submit Runtime Surface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Route unbound JSONL submit/turn requests through one runtime-owned ephemeral typed surface and delete the server's second controller loop.

**Architecture:** Add a process-local owner and in-memory surface ledger behind the existing generic commit coordinator, start ephemeral one-shot actors only through an explicit runtime start policy, and make the JSONL adapter retain only projection and close-guard state. The runtime actor owns admission, cancellation, terminalization, background joins, and automatic close.

**Tech Stack:** Rust 2024, Tokio, serde, existing runtime surface reducer/commit coordinator, JSONL server contract tests, cargo-semver-checks.

---

## Task 1: Type The Ephemeral Commit Boundary

**Status: Completed.** Ephemeral receipts, the in-memory ledger, cursor/digest
validation, and recorded/ephemeral coordinator delegation are covered by the
runtime surface commit contract suite.

**User value:** Prevents the server path from pretending volatile state is durable, so lifecycle diagnostics and future TUI/server convergence report truthful state.

**Architecture value:** Gives recorded and ephemeral surfaces one coordinator API without conflating durable and live revisions.

**Files:**

- Modify: `crates/orca-runtime/src/runtime_surface/store.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/commit.rs`
- Test: `crates/orca-runtime/tests/runtime_surface_commit.rs`

- [ ] Add RED tests that an in-memory ledger accepts an exact ephemeral batch,
      returns an ephemeral live-revision receipt, probes idempotently, and rejects
      recorded classes, wrong cursors, wrong incarnations, and conflicting ids.
- [ ] Add `EphemeralBatchReceipt` and closed `SurfaceBatchReceipt` variants.
- [ ] Change `SurfaceCommitLedger` and `CommitProbe` to use the typed receipt.
- [ ] Update the JSONL ledger and existing fake ledgers to wrap recorded receipts.
- [ ] Implement `InMemorySurfaceCommitLedger` with exact cursor and digest checks.
- [ ] Add private `RuntimeSurfaceCommitLedger` delegation.
- [ ] Add `RuntimeCommitCoordinator::map_ledger` so recorded recovery remains
      specialized and the resident coordinator can use the enum.
- [ ] Run:

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_commit
cargo test --locked --offline -p orca-runtime runtime_surface::commit::tests
```

## Task 2: Add Process-Local Owner Authority

**Status: Completed.** The ephemeral owner is process-local, thread-bound, and
does not create filesystem authority artifacts.

**User value:** Ephemeral work can be cancelled and fenced without writing hidden
lock or epoch files into the user's session storage.

**Architecture value:** Keeps owner checks mandatory for every coordinator while
making authority backend explicit.

**Files:**

- Modify: `crates/orca-runtime/src/runtime_surface/store.rs`
- Test: `crates/orca-runtime/tests/runtime_surface_commit.rs`

- [ ] Add RED tests for a thread-bound process-local lease: epoch 1, matching
      thread authorized, other thread rejected, no filesystem artifact.
- [ ] Replace direct lease file fields with an opaque durable/ephemeral backend.
- [ ] Add the process-local thread constructor and backend-specific authority
      checks/drop behavior.
- [ ] Re-run Task 1 focused tests.

## Task 3: Start An Explicit Ephemeral Runtime Surface

**Status: Completed.** One-shot creation is an explicit runtime start policy;
history-disabled internal callers remain surface-less.

**User value:** Gives stateless requests the same typed operation, cancellation,
interaction, and terminal semantics as TUI and thread-bound server work.

**Architecture value:** Preserves plain history-disabled internal threads while
making surface ownership an explicit start-time choice.

**Files:**

- Modify: `crates/orca-runtime/src/thread.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Test: `crates/orca-runtime/tests/runtime_surface_host.rs`

- [ ] Add RED external behavior tests for explicit one-shot persistence, absent
      session id, ephemeral cursor/live revision, usable JSONL attachment, and no
      catalog entry.
- [ ] Add an explicit one-shot policy to `RuntimeThreadStartRequest`.
- [ ] Allocate one UUIDv7 during prepare and pass it into `RuntimeThread` so the
      actor, surface, and owner share one identity.
- [ ] Prepare either a recorded filesystem owner or ephemeral process-local
      owner; do not infer ephemeral surfaces from `HistoryMode::Disabled`.
- [ ] Generalize the initial snapshot and event-batch builder over recorded and
      ephemeral cursor source revisions.
- [ ] Recover the recorded coordinator, map it to the resident ledger enum, and
      bootstrap an ephemeral coordinator without recovery/persistence hooks.
- [ ] Extract the common hub binding and resident state construction so both
      paths use one dispatcher and capability boundary.
- [ ] Change resident helper signatures to the unified coordinator type.
- [ ] Run:

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_host
cargo test --locked --offline -p orca-runtime --test runtime_surface_interaction
```

## Task 4: Auto-Close And Reap One-Shot Actors

**Status: Completed.** The runtime actor observes its typed terminal, seals its
surface, and the host reaps the exact one-shot actor.

**User value:** A completed, rejected, or cancelled stateless request cannot stay
alive and consume runtime resources.

**Architecture value:** Places terminal observation and actor reclamation in the
runtime host instead of an adapter-local registry.

**Files:**

- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Test: `crates/orca-runtime/tests/runtime_surface_host.rs`

- [ ] Add RED tests for success and NotAdmitted terminal causing subscription
      seal, closed command channel, joined actor, and no reusable one-shot.
- [ ] Add one-shot terminal detection to the actor loop only after the exact
      terminal commit is visible and no semantic commit retry is pending.
- [ ] Close ingress to the actor, settle/join background work, seal the hub, and
      exit.
- [ ] Add an actor-exit channel to the host supervisor and join/remove the exact
      actor entry immediately; host shutdown still drains all remaining actors.
- [ ] Run the runtime host focused tests.

## Task 5: Route Stateless JSONL Through The Surface

**Status: Completed.** Production unbound JSONL requests use the typed one-shot
surface. Projection writes protocol item and plan facts directly from committed
surface facts; no compatibility parser reconstructs proposed-plan identities.

**User value:** Existing stateless JSONL clients keep their stream while gaining
reliable cancellation, interactions, and cleanup.

**Architecture value:** Removes the last production server-owned agent loop.

**Files:**

- Modify: `crates/orca-runtime/src/server/surface_adapter.rs`
- Modify: `crates/orca-runtime/src/server/processors/submit.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Test: `crates/orca-runtime/src/server.rs`
- Test: `tests/jsonl_surface_differential.rs`
- Test: `tests/session_server_contract.rs`

- [ ] Add RED behavior tests showing stateless submit creates an ephemeral
      surface operation, emits no `thread_started`, writes no session/catalog
      entry, and preserves the normalized v0.2.50 fixture.
- [ ] Add `prepare_stateless_turn` to normalize config, expand mentions, start
      the one-shot, apply typed permissions, reserve, and claim the subscription.
- [ ] Give the projection worker an ephemeral runtime close guard. On terminal or
      projection failure it requests the host close barrier; it never touches a
      generation cancel token directly.
- [ ] Add a typed workflow lifecycle ingress carrying start, terminal, result,
      task revision, workflow revision, and the active generation fence. Do not
      reconstruct workflow facts from JSON `EventEnvelope` payloads.
- [ ] Make foreground workflow waiting cancellation-aware: request stop, join the
      worker, commit its typed terminal, then allow the parent generation to
      terminalize. Reject unmodeled typed `wait=false` before side effects.
- [ ] Project only thread-scoped workflow facts whose typed parent fence belongs
      to the JSONL operation; preserve workflow lifecycle and item ordering before
      `turn_completed`.
- [ ] Retain and join the worker through the existing transport-turn collection;
      prune the adapter binding only after the runtime handle is unavailable.
- [ ] Route unbound submit operations through the new adapter method.
- [ ] Delete `run_submit`, `ServerRequestWriter` usage that exists only for it,
      and the production `controller::run_to_writer_with_options` call.
- [ ] Run:

```bash
cargo test --locked --offline -p orca-runtime --lib server::tests:: -- --test-threads=1
cargo test --locked --offline --test jsonl_surface_differential
cargo test --locked --offline --test session_server_contract
```

## Task 6: Prove Failure And Shutdown Cleanup

**Status: Completed.** EOF, output failure, provider failure, workflow
cancellation, and host shutdown are covered by behavioral cleanup tests. A
clean EOF has a bounded completion harvest only while no interaction route is
awaiting client input.

**User value:** Closing a terminal or losing output cannot leave an invisible
model/tool task running after the JSONL client is gone.

**Architecture value:** Makes the connection supervisor's close barrier the
single behavioral proof instead of relying on source-shape assertions.

**Files:**

- Modify: `crates/orca-runtime/src/server.rs`
- Modify: `crates/orca-runtime/src/server/surface_adapter.rs`
- Modify: `crates/orca-runtime/tests/jsonl_surface_routing.rs`
- Modify: `tests/session_server_contract.rs`

- [x] Add RED tests with controlled executors/writers for provider failure,
      write failure, EOF during generation, workflow cancellation, and explicit
      host shutdown.
- [x] Assert one terminal or shutdown disposition, one generation join, zero
      live ephemeral binding, zero transport worker, and empty session catalog.
- [x] Replace any new source-string assertion with behavior/state evidence. Keep
      unrelated legacy shape tests unchanged in this slice.
- [x] Run all JSONL and surface focused tests.

## Task 7: Gates, Review, Commit, And Rebase

**Status: Implemented and verified on the feature branch.** The final focused
suites, raw single-threaded workspace gate, non-strict Clippy, package-level
Rust API checks, and isolated real DeepSeek stateless smoke are green. Strict
Clippy remains a recorded `origin/main` baseline blocker rather than a claimed
pass. The branch is intentionally not integrated or published.

**User value:** Delivers one independently reviewable reliability improvement,
not a transitional dual-owner state.

**Architecture value:** Verifies shared runtime, protocol, persistence, resource,
and public API boundaries together.

**Files:**

- Modify after review only: files in Tasks 1-6

- [x] Run formatting and diff checks.
- [x] Run all focused tests from the specification.
- [x] Run full workspace tests. The final raw single-threaded workspace command
      passed without exclusions, including `orca-runtime` 1051/1051 and
      `orca-tui` 457/457.
- [ ] Run strict Clippy. Current `orca-core` baseline has pre-existing
      `-D warnings` failures; the final strict run exited 101 first at
      `crates/orca-core/src/config/file.rs:276` (`collapsible_if`) and reported
      17 existing `orca-core` errors. The non-strict workspace command exited 0.
- [x] Run the Rust API gate against current `origin/main@d6f98b0ac`.
      `cargo-semver-checks 0.49.0` passed 223/223 for each of `orca-core`,
      `orca-runtime`, and `orca-tui`, with no semver update required. The
      unreleased wrapper script lives only in the preserved P1.3 worktree and
      resolves that worktree as its current repository, so this slice used the
      tool's package-level `--baseline-rev origin/main` checks directly.
- [x] Run real DeepSeek stateless JSONL smoke. The first live run exposed the
      fixed two-second clean-EOF harvest as a lifecycle defect: streaming
      succeeded, then host shutdown cancelled the one-shot before its message
      terminal. After replacing the grace period with an explicit ephemeral
      completion policy, the final isolated run produced 88 request events,
      one successful terminal, the exact sentinel, zero `thread_started`, no
      workspace artifacts, no unexpected home artifacts, and empty stderr.
- [x] Complete code-quality review and resolve every Critical and Important
      finding. Review and live verification found Critical 0. The Important
      workflow join, public request-shape compatibility, clean-EOF lifecycle,
      and implicit policy-selection defects are resolved with behavioral or API
      evidence.
- [x] Confirm `git diff --cached` contains only this semantic slice.
- [x] Commit once with a semantic message.
- [x] Fetch `origin/main`. It remained at the recorded base, so no rebase or
      redundant post-rebase gate was required. If it advances later, rebase, rerun affected focused tests,
      full workspace tests, and the API gate.
- [x] Do not merge, push, release, or delete either worktree in this task.

```bash
cargo fmt --all -- --check
git diff --check
cargo test --locked --offline --workspace -- --test-threads=1
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo-semver-checks check-release --manifest-path Cargo.toml -p orca-runtime --baseline-rev origin/main --release-type patch --color never
```

## Final Deletion Gate

This slice is complete only when production contains exactly one stateless agent
execution owner. `run_submit` and its direct controller invocation are deleted,
the new ephemeral path passes all acceptance tests, and no committed state keeps
both routes available behind a compatibility branch.
