# Runtime-Owned Typed Surface Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Do not skip a RED run, a focused GREEN gate, a deletion review, or a commit.

**Goal:** Make the runtime the only owner of operation identity, generation lifecycle, interactions, semantic state, replay, terminal publication, Goal/workflow/task facts, and session policy; migrate TUI, ACP, and the released JSONL server onto that typed surface; then publish and publicly verify the next Orca patch release.

**Architecture:** `RuntimeHost` remains the execution authority and gains an in-process `RuntimeSurfaceHostHandle` plus per-thread `RuntimeSurfaceHandle`. A thread actor owns commands and scoped publisher permits; `RuntimeCommitCoordinator` durably commits one complete `SurfaceCommitBatch` before a passive resident `SurfaceHub` applies and publishes it. Materialization, replay, and live delivery share the same pure reducer. TUI is an in-process client, ACP is a bounded 0.10.4 transport adapter, and JSONL is the final v0.2.50 compatibility adapter. None of the clients may own active-operation, interaction, terminal, settings, Goal, task, workflow, or replay truth.

**Tech Stack:** Rust 2024 workspace, Tokio and crossbeam channels, SQLite/JSONL thread stores, serde/serde_json, agent-client-protocol `=0.10.4`, Node.js contract and release harnesses, ratatui/crossterm PTY tests, DeepSeek, GitHub Actions, GitHub Releases, npm, and Astro site checks.

---

## Normative Inputs And Execution Rules

The following two documents and their machine manifest are the normative input. Implement exact closed types, fields, transitions, command matrices, adapter dispositions, constants, and test-vector membership from them; this plan chooses module boundaries and execution order but does not redefine the contract:

- `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-contract-design.md`
- `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md`
- `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`

Before Task 1, confirm that the Phase 0A bundle is committed and has explicit written review naming its commit and both artifact SHA-256 values. Do not hardcode those hashes in production or tests; the manifest validator recomputes and compares the reviewed artifact metadata. Any later edit to either contract artifact returns execution to Phase 0A and blocks all production work.

Keep these rules throughout:

- Use one `SurfaceCommitBatch` as the preflight, persistence, cursor, reduction, and delivery unit. Never publish a partial batch.
- Keep the coordinator and resident hub actor-readable while operation work moves through `spawn_blocking`; never move resident surface ownership into a generation worker.
- Allocate surface cursors only from the commit coordinator. `EventFactory` sequence values may contain holes and are compatibility metadata, not surface cursors.
- A fresh attach returns `SnapshotAtCursor` followed only by batches after that cursor. A cursor attach returns replay after that cursor and no snapshot.
- `SurfaceHub` is passive resident projection and subscription state, not another actor or command owner.
- TUI never uses ACP internally. ACP and JSONL never become alternate runtime ownership layers.
- Preserve released JSONL v0.2.50 request ids, events, fields, errors, and ordering. Preserve ACP 0.10.4; an SDK upgrade is a different release slice.
- Do not start Phase 4B until the Phase 4A public schemas and fixtures receive separate written approval.
- End each implementation task with a clean focused gate and its named commit. Run the broader phase gate before starting the next phase.

## Phase 0B: Validators And Executable Contract

### Task 1: Freeze The Manifest Validator And Baseline Inventories

**Files:**

- Create: `scripts/validate-runtime-surface-contract.mjs`
- Create: `scripts/test-validate-runtime-surface-contract.mjs`
- Create: `crates/orca-runtime/tests/runtime_surface_manifest.rs`
- Create: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Read only: the three normative files above

- [ ] **Step 1: Write the validator self-tests and inventory tests**

Cover malformed JSON, artifact hash mismatch, duplicate ids, wrong row widths, missing command targets/acks, non-closed transition rows, invalid test generators, and mismatch between the manifest and current runtime facts, `UserAction` variants, or mutation-capable TUI entrypoints. The TUI test must classify all 21 current actions and separately recognize the required future `ResumeOperation` and exact `CancelOperation` additions without pretending they already exist.

- [ ] **Step 2: Run RED and confirm the validator is absent**

Run:

```bash
node scripts/test-validate-runtime-surface-contract.mjs
cargo test --locked --offline -p orca-runtime --test runtime_surface_manifest -- --test-threads=1
cargo test --locked --offline -p orca-tui --lib surface_boundary_tests -- --test-threads=1
```

Expected: the Node command fails because the validator does not exist; Rust fails because the manifest inventory harness and TUI test module do not exist.

- [ ] **Step 3: Implement the minimal structural gate**

Use JSON parsing plus canonical SHA-256 calculation. Validate every invariant named in `phase_0a_manifest_invariants`, every row against its declared columns, uniqueness and closed references, the parent commit/blob/hash backlink, the private-contract hash, and exact current source/action/entrypoint inventories. Emit a stable success line only after all checks pass. Keep this structural: it does not generate Rust or mutate the frozen artifacts.

- [ ] **Step 4: Run GREEN and the frozen-bundle sanity check**

Run the three commands from Step 2, then:

```bash
git diff --check
git status --short
```

Expected: all validators pass; status contains only the intended validator/test files plus already reviewed artifacts.

- [ ] **Step 5: Commit**

```bash
git add scripts/validate-runtime-surface-contract.mjs scripts/test-validate-runtime-surface-contract.mjs crates/orca-runtime/tests/runtime_surface_manifest.rs crates/orca-tui/src/surface_boundary_tests.rs crates/orca-tui/src/lib.rs
git commit -m "test(runtime): freeze typed surface inventories"
```

### Task 2: Prove The Repo-Local ACP RPC Facade Is Feasible

**Files:**

- Create: `crates/orca-runtime/src/acp/rpc_facade.rs`
- Create: `crates/orca-runtime/tests/acp_rpc_facade.rs`
- Modify: `crates/orca-runtime/src/acp/mod.rs`

- [ ] **Step 1: Write transport-only RED tests**

Test same-session prompt-before-cancel read order even when handlers are reverse-scheduled, bounded inbound/outbound lanes, short writes, `write_all` failure, flush failure, a writer that remains pending, oversized inbound frames, direction validation, correlated write acknowledgements, EOF sealing, and joined shutdown. This task must not route a production ACP session to the new surface yet.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test acp_rpc_facade -- --test-threads=1
```

Expected: compilation fails because `acp::rpc_facade` and its ordered/acknowledged transport API are missing.

- [ ] **Step 3: Implement the minimal feasibility facade**

Add bounded read and write lanes, monotonic read sequence assignment, per-session sequencing, physical `write_all` plus `flush` acknowledgement, explicit oversize/direction/protocol errors, and shutdown that joins reader, scheduler, and writer. Keep it package-private and unused by `OrcaAcpAgent` until Phase 4B.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test acp_rpc_facade -- --test-threads=1
cargo test --locked --offline -p orca-runtime acp:: --lib -- --test-threads=1
```

Expected: all facade and existing ACP unit tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/acp/rpc_facade.rs crates/orca-runtime/tests/acp_rpc_facade.rs crates/orca-runtime/src/acp/mod.rs
git commit -m "test(acp): prove ordered acknowledged rpc facade"
```

## Phase 1: Resident Operation And Interaction Core

### Task 3: Add The Exact Private Surface Type Modules

**Files:**

- Create: `crates/orca-runtime/src/runtime_surface/mod.rs`
- Create: `crates/orca-runtime/src/runtime_surface/identity.rs`
- Create: `crates/orca-runtime/src/runtime_surface/operation.rs`
- Create: `crates/orca-runtime/src/runtime_surface/interaction.rs`
- Create: `crates/orca-runtime/src/runtime_surface/projection.rs`
- Create: `crates/orca-runtime/src/runtime_surface/commands.rs`
- Create: `crates/orca-runtime/tests/runtime_surface_types.rs`
- Modify: `crates/orca-runtime/src/lib.rs`

- [ ] **Step 1: Write exhaustive type and serialization RED tests**

Instantiate every primitive wrapper, fence, scope, capability, operation/generation state, terminalization cause, repair token, interaction state, patch, snapshot/read/attach/wait result, all 22 thread commands, all 24 host commands, and their result algebras. Add exhaustive matches driven by the manifest, canonical round trips where serialization is allowed, and constructor rejection at every byte/count bound.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_types -- --test-threads=1
```

Expected: compilation fails because `runtime_surface` and the closed values are missing.

- [ ] **Step 3: Implement only the closed values and constructors**

Transcribe exact fields and variants from the private contract into the six modules. Keep authority-bearing constructors crate-private, expose the reviewed `unstable_surface` facade, use checked newtypes for bounded text/bytes/ids and non-empty collections, and prohibit open `serde_json::Value`, wildcard enum fallbacks, raw writable handles, or client callbacks.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_types -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
```

Expected: type exhaustiveness, bounds, serialization, and manifest closure pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/runtime_surface crates/orca-runtime/tests/runtime_surface_types.rs crates/orca-runtime/src/lib.rs
git commit -m "feat(runtime): add closed typed surface domain"
```

### Task 4: Implement The Pure Reducer And Complete-Batch Preflight

**Files:**

- Create: `crates/orca-runtime/src/runtime_surface/reducer.rs`
- Create: `crates/orca-runtime/tests/runtime_surface_reducer.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/mod.rs`

- [ ] **Step 1: Write reducer RED tests**

Generate all manifest transition vectors. Cover exact scope, revision, fence, event-id and cursor continuity, complete-batch atomicity, duplicate rematerialization as `AlreadyApplied`, live duplicate rejection, illegal terminal escape, workflow `Stopping`, Task/Workflow/Subagent absorbing terminals, Goal continuation idempotency, and unchanged state on any patch failure.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_reducer -- --test-threads=1
```

Expected: compilation fails because `SurfaceReducer` and `preflight_batch` are missing.

- [ ] **Step 3: Implement the pure reducer**

Implement `preflight_batch(snapshot, batch, mode)` against a clone, apply every patch only after the full batch validates, and return the exact reducer result/error algebra. Use the same function for live application, rematerialization, and replay; do not directly assign snapshots or parse raw event payloads.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_reducer -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test runtime_surface_types -- --test-threads=1
```

Expected: all manifest-generated transitions and atomicity cases pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/runtime_surface/reducer.rs crates/orca-runtime/tests/runtime_surface_reducer.rs crates/orca-runtime/src/runtime_surface/mod.rs
git commit -m "feat(runtime): add atomic surface reducer"
```

### Task 5: Add The Commit Coordinator, Ledger, And Recovery

**Files:**

- Create: `crates/orca-runtime/src/runtime_surface/commit.rs`
- Create: `crates/orca-runtime/src/runtime_surface/store.rs`
- Create: `crates/orca-runtime/tests/runtime_surface_commit.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/mod.rs`
- Modify: `crates/orca-runtime/src/thread_store/types.rs`
- Modify: `crates/orca-runtime/src/thread_store/writer.rs`
- Modify: `crates/orca-runtime/src/thread_store/local.rs`
- Modify: `crates/orca-runtime/src/thread_store/projection.rs`

- [ ] **Step 1: Write persistence and crash-window RED tests**

Cover durable-before-publish ordering, partial append, cursor reuse prohibition, cross-store finalize-intent settlement, immutable shutdown plans, existing-vs-requested winning terminal causes, same-incarnation projection reset, cold-owner takeover, every replayability recovery row including matching terminal-unavailable interaction precedence over generic RuntimeRestart, `FinalizingDegraded` retry split, policy/thread owner lease fail-closed behavior, and wall-clock rollback having no authority effect.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_commit -- --test-threads=1
```

Expected: compilation fails because the coordinator, surface ledger, injected monotonic clock, and recovery APIs are missing.

- [ ] **Step 3: Implement the minimal durable coordinator**

Add per-thread sequence allocation, canonical batch digest, preflight, append/checkpoint, settlement ledger, durable finalization/shutdown records, owner epoch plus OS-lock witnesses, and materialization through `SurfaceReducer`. A failed durable append publishes nothing; post-append projection failure returns the contract's repair token without writing a second semantic terminal.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_commit -- --test-threads=1
cargo test --locked --offline -p orca-runtime thread_store --lib -- --test-threads=1
```

Expected: all crash windows, lease rules, recovery rows, and store tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/runtime_surface/commit.rs crates/orca-runtime/src/runtime_surface/store.rs crates/orca-runtime/src/runtime_surface/mod.rs crates/orca-runtime/tests/runtime_surface_commit.rs crates/orca-runtime/src/thread_store/types.rs crates/orca-runtime/src/thread_store/writer.rs crates/orca-runtime/src/thread_store/local.rs crates/orca-runtime/src/thread_store/projection.rs
git commit -m "feat(runtime): persist surface commit batches"
```

### Task 6: Add The Passive Surface Hub, Attach, Replay, And Backpressure

**Files:**

- Create: `crates/orca-runtime/src/runtime_surface/hub.rs`
- Create: `crates/orca-runtime/tests/runtime_surface_attach.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/mod.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/commit.rs`

- [ ] **Step 1: Write attach/replay RED tests**

Cover atomic fresh attach, `SnapshotAtCursor` plus strictly later live batches, cursor attach with no snapshot, retained half-open replay, attach concurrent with commit, crash after append before publish, gap detection, stale/future/wrong-thread cursor errors, bounded subscriber overflow, baseline size failure, detach, and a slow client that cannot block the actor or other clients.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_attach -- --test-threads=1
```

Expected: compilation fails because `SurfaceHub`, `attach_fresh`, `attach_after`, and bounded subscriptions are missing.

- [ ] **Step 3: Implement the passive hub**

Store the resident Arc snapshot, retained complete-batch suffix, attachment registry, and bounded sender state behind one synchronization boundary. The thread actor/coordinator remains the only writer; hub methods only attach, read, apply committed batches, signal gaps, and detach.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_attach -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test runtime_surface_commit -- --test-threads=1
```

Expected: ordering, replay, gap, and non-blocking tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/runtime_surface/hub.rs crates/orca-runtime/src/runtime_surface/commit.rs crates/orca-runtime/src/runtime_surface/mod.rs crates/orca-runtime/tests/runtime_surface_attach.rs
git commit -m "feat(runtime): add atomic surface attach and replay"
```

### Task 7: Move Operation Reservation, Generation, Control, And Finalization Into Runtime

**Files:**

- Create: `crates/orca-runtime/tests/runtime_surface_operation.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/runtime_lifecycle.rs`
- Modify: `crates/orca-runtime/src/runtime_turn_start.rs`
- Modify: `crates/orca-runtime/src/runtime_turn_loop.rs`
- Modify: `crates/orca-runtime/src/background_turn.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/commit.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/hub.rs`

- [ ] **Step 1: Write operation lifecycle RED tests**

Cover globally unique ids, reserve-before-canonical-input, lease expiry, `AdmitReserved`, every legal operation/generation edge, pre-admission cancel, startup/cancel races, exact-fence steer/pause/resume/background, foreground handoff ordering, duplicate controls, cancellation waking interactions without inline join, all terminal mappings, one finalizer, one terminal cursor, waiter repair outcomes, Goal composite generation identity, and host/thread shutdown barriers.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_operation -- --test-threads=1
```

Expected: tests compile against the new types but fail because `RuntimeHost` still starts operations through the legacy one-stage `ThreadCommand::StartTurn` path and does not emit committed surface batches.

- [ ] **Step 3: Implement the resident control core**

Extend the existing thread mailbox with the exact contract commands, reserve ids before fallible input work, bind generation admission to the operation fence, route all control through the actor, and drive finalization only from actor completion/recovery. Split actor-resident surface state from the `ThreadActorState` payload moved into blocking execution so the mailbox can commit and answer attaches while a generation runs. Keep legacy handles as temporary internal bridges for not-yet-migrated clients, but prevent them from owning identity, broker responses, terminal decisions, or waiters.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_operation -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test runtime_host -- --test-threads=1
```

Expected: operation and existing runtime-host suites pass with one committed lifecycle rail.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/src/runtime_lifecycle.rs crates/orca-runtime/src/runtime_turn_start.rs crates/orca-runtime/src/runtime_turn_loop.rs crates/orca-runtime/src/background_turn.rs crates/orca-runtime/src/runtime_surface/commit.rs crates/orca-runtime/src/runtime_surface/hub.rs crates/orca-runtime/tests/runtime_surface_operation.rs
git commit -m "feat(runtime): own operation lifecycle on typed surface"
```

### Task 8: Move All Five Interaction Kinds Into The Runtime Broker

**Files:**

- Create: `crates/orca-runtime/src/runtime_surface/broker.rs`
- Create: `crates/orca-runtime/tests/runtime_surface_interaction.rs`
- Modify: `crates/orca-runtime/src/runtime_pending_interaction.rs`
- Modify: `crates/orca-runtime/src/runtime_approval.rs`
- Modify: `crates/orca-runtime/src/runtime_permission.rs`
- Modify: `crates/orca-runtime/src/runtime_user_input.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/mod.rs`

- [ ] **Step 1: Write interaction RED tests**

Cover ToolApproval, PermissionRequest, UserInput, MCP elicitation, and BackgroundApproval request/route/response/expiry/cancel flows; monotonic interaction deadlines, exact issuing-clock loss witnesses, persisted-disposition source matching, the durable interaction-close/waiter barrier followed by generation return and child join before the operation stop/finalization barrier, and crash recovery between those barriers; durable request-before-publication; route grants and epochs; secret-bearing answer boundaries; stale and duplicate responses; late-response tombstones; client detach/reroute; cross-thread ids; cancellation wakeup; restart recovery; and no client receiving a writable broker.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_interaction -- --test-threads=1
```

Expected: tests fail because current handlers use `RuntimePendingInteractionStore` and client-installed callbacks rather than actor-owned interaction commands and committed receipts.

- [ ] **Step 3: Implement the durable broker**

Make the actor create and settle every interaction through the coordinator. Waiters receive only bounded typed answers after a matching route grant is consumed. Adapt existing TUI/server handlers temporarily as presentation/transport bridges; they may encode requests and forward typed responses but cannot remove pending entries or decide cancellation/expiry.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_interaction -- --test-threads=1
cargo test --locked --offline -p orca-runtime runtime_pending_interaction --lib -- --test-threads=1
```

Expected: all five kinds, recovery, and stale-response cases pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/runtime_surface/broker.rs crates/orca-runtime/src/runtime_surface/mod.rs crates/orca-runtime/src/runtime_pending_interaction.rs crates/orca-runtime/src/runtime_approval.rs crates/orca-runtime/src/runtime_permission.rs crates/orca-runtime/src/runtime_user_input.rs crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/tests/runtime_surface_interaction.rs
git commit -m "feat(runtime): own durable interaction routing"
```

## Phase 2: Complete Semantic Surface And Host Facade

### Task 9: Route Item, Assistant, Tool, Plan, Usage, And Context Facts Through Typed Ingress

**Files:**

- Create: `crates/orca-runtime/src/runtime_surface/ingress.rs`
- Create: `crates/orca-runtime/tests/runtime_surface_semantic.rs`
- Modify: `crates/orca-runtime/src/runtime_event_projector.rs`
- Modify: `crates/orca-runtime/src/tool_item_projection.rs`
- Modify: `crates/orca-runtime/src/runtime_tool_call.rs`
- Modify: `crates/orca-runtime/src/runtime_turn_iteration.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/mod.rs`

- [ ] **Step 1: Write semantic-ingress RED tests**

Cover user/assistant/reasoning items, streaming fragments, tool requested/output/completed, file changes and diffs, proposed plans and plan updates, usage, context windows, errors, exact scope permits, canonical byte limits, event/source inventory coverage, and rejection of a publisher using another operation/generation/tool fence.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_semantic -- --test-threads=1
```

Expected: tests fail because current projection still constructs raw `EventEnvelope` payloads and does not commit typed patches through scoped permits.

- [ ] **Step 3: Implement minimal typed ingress**

Issue scoped publisher permits from the actor, convert authoritative runtime facts directly into closed patches, and commit them through the coordinator. Keep legacy event emission only as a downstream compatibility projection; it cannot feed the surface reducer.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_semantic -- --test-threads=1
cargo test --locked --offline -p orca-runtime runtime_event_projector --lib -- --test-threads=1
```

Expected: semantic facts and legacy projections agree without raw-payload reconstruction.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/runtime_surface/ingress.rs crates/orca-runtime/src/runtime_surface/mod.rs crates/orca-runtime/src/runtime_event_projector.rs crates/orca-runtime/src/tool_item_projection.rs crates/orca-runtime/src/runtime_tool_call.rs crates/orca-runtime/src/runtime_turn_iteration.rs crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/tests/runtime_surface_semantic.rs
git commit -m "feat(runtime): commit typed semantic facts"
```

### Task 10: Add Task, Workflow, Subagent, Goal, Settings, Catalog, And Session Facts

**Files:**

- Create: `crates/orca-runtime/tests/runtime_surface_domain.rs`
- Modify: `crates/orca-runtime/src/tasks.rs`
- Modify: `crates/orca-runtime/src/workflow/state.rs`
- Modify: `crates/orca-runtime/src/workflow_execution.rs`
- Modify: `crates/orca-runtime/src/subagent.rs`
- Modify: `crates/orca-runtime/src/subagent_execution.rs`
- Modify: `crates/orca-runtime/src/goal_actor.rs`
- Modify: `crates/orca-runtime/src/goal_store.rs`
- Modify: `crates/orca-runtime/src/goal_tracker.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/ingress.rs`

- [ ] **Step 1: Write domain RED tests**

Cover all closed Task, workflow run/phase/agent attempt, Subagent, Goal, settings, catalog, pinned-context, session-health, and lifecycle transitions. Include workflow `Stopping -> Stopped`, standalone and generation-child workflow identity, idempotent result follow-up, Goal intent/store receipt atomicity, mandatory predecessor continuation, duplicate predecessor `AlreadyApplied`, all continuation stop mappings, settings revision capture at admission, catalog revision fences, and unknown legacy continuation preservation.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_domain -- --test-threads=1
```

Expected: tests fail because these stores still expose direct mutation or emit open DTO/event state outside the surface coordinator.

- [ ] **Step 3: Implement the complete private semantic surface**

Route each owner through typed patches under its required revision/fence. Inject one process-registry Goal handle; couple Goal post-commit changes to the Goal-store receipt. Give workflows and subagents the exact revisioned identities from the contract. Make settings, folder trust, memory, catalog, pinned context, and session health host/thread facts rather than adapter caches.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_domain -- --test-threads=1
cargo test --locked --offline -p orca-runtime goal_ --lib -- --test-threads=1
cargo test --locked --offline -p orca-runtime workflow --lib -- --test-threads=1
```

Expected: all closed state machines and current Goal/workflow tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/tasks.rs crates/orca-runtime/src/workflow/state.rs crates/orca-runtime/src/workflow_execution.rs crates/orca-runtime/src/subagent.rs crates/orca-runtime/src/subagent_execution.rs crates/orca-runtime/src/goal_actor.rs crates/orca-runtime/src/goal_store.rs crates/orca-runtime/src/goal_tracker.rs crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/src/runtime_surface/ingress.rs crates/orca-runtime/tests/runtime_surface_domain.rs
git commit -m "feat(runtime): complete typed semantic surface"
```

### Task 11: Expose The Closed Host And Thread Facades

**Files:**

- Create: `crates/orca-runtime/src/runtime_surface/host.rs`
- Create: `crates/orca-runtime/tests/runtime_surface_host.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/mod.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/history.rs`
- Modify: `crates/orca-runtime/src/memory.rs`

- [ ] **Step 1: Write facade RED tests**

Exercise every host and thread command/output from the manifest: create/open/load/fork/close, attach/detach/read/page/wait, catalog and MCP catalog queries, settings, credentials, memory, trust, operation controls, interactions, task/workflow/Goal actions, repair, and bounded shutdown. Test capability rejection, stale fences/revisions, conditional acknowledgement sets, deferred results, immutable shutdown plans, retained shutdown output, and host restart.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_host -- --test-threads=1
```

Expected: compilation fails because `RuntimeSurfaceHostHandle` and `RuntimeSurfaceHandle` do not exist.

- [ ] **Step 3: Implement the minimal facade**

Expose only the exact closed command methods and typed read/attach/wait results. Route lifecycle and mutations to the existing host/thread actors, catalog/history/memory/trust to their runtime-owned stores, and all semantic mutation through the coordinator. Do not expose `RuntimeHostHandle`, `RuntimeThreadHandle`, `OperationHandle`, stores, brokers, registries, or callbacks through the facade.

- [ ] **Step 4: Run GREEN and the Phase 2 gate**

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_host -- --test-threads=1
cargo test --locked --offline -p orca-runtime --all-targets -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
```

Expected: the complete runtime crate and manifest gate pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/runtime_surface/host.rs crates/orca-runtime/src/runtime_surface/mod.rs crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/src/history.rs crates/orca-runtime/src/memory.rs crates/orca-runtime/tests/runtime_surface_host.rs
git commit -m "feat(runtime): expose closed surface facades"
```

## Phase 3: TUI Vertical Cutover

### Task 12: Add The TUI Surface Client And Pure Presentation Projection

**Files:**

- Create: `crates/orca-tui/src/surface_client.rs`
- Create: `crates/orca-tui/src/surface_projection.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/runtime_event_actions.rs`
- Modify: `crates/orca-tui/src/transcript_view.rs`
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Write presentation RED tests**

Test snapshot hydration, ordered batch reduction, all manifest TUI mappings, interaction dialogs, Goal/task/workflow/subagent panels, settings pending/committed/rejected state, recovery-required controls, gaps forcing reload, terminal cursors, and rendering parity for existing message/tool/plan/approval/background states.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-tui --lib surface_projection -- --test-threads=1
```

Expected: compilation fails because the TUI surface client and typed presentation reducer are missing.

- [ ] **Step 3: Implement the pure client projection**

Hold only `RuntimeSurfaceHostHandle`, an optional `RuntimeSurfaceHandle`, attachment cursor, and presentation state. Apply `SnapshotAtCursor` and complete batches without accessing raw `EventEnvelope.payload`; turn gaps into explicit detach/reload state. Preserve ratatui view models and keyboard behavior.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-tui --lib surface_projection -- --test-threads=1
cargo test --locked --offline -p orca-tui transcript --lib -- --test-threads=1
```

Expected: presentation and existing rendering tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tui/src/surface_client.rs crates/orca-tui/src/surface_projection.rs crates/orca-tui/src/lib.rs crates/orca-tui/src/types.rs crates/orca-tui/src/runtime_event_actions.rs crates/orca-tui/src/transcript_view.rs crates/orca-tui/src/ui.rs
git commit -m "feat(tui): add typed surface projection"
```

### Task 13: Cut Every TUI Mutation And Session Workflow To The Facade

**Files:**

- Create: `crates/orca-tui/src/surface_actions.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/agent_runtime.rs`
- Modify: `crates/orca-tui/src/action_dispatcher.rs`
- Modify: `crates/orca-tui/src/running_actions.rs`
- Modify: `crates/orca-tui/src/approval_actions.rs`
- Modify: `crates/orca-tui/src/background_tasks.rs`
- Modify: `crates/orca-tui/src/workflow_panel_actions.rs`
- Modify: `crates/orca-tui/src/slash_command_actions.rs`
- Modify: `crates/orca-tui/src/slash_menu_actions.rs`
- Modify: `crates/orca-tui/src/session_picker_actions.rs`
- Modify: `crates/orca-tui/src/setup_actions.rs`
- Modify: `crates/orca-tui/src/mention_search_manager.rs`
- Modify: `crates/orca-tui/src/submitted_turn.rs`

- [ ] **Step 1: Write action-routing RED tests**

For every manifest action and entrypoint, prove exactly one closed host/thread command and typed receipt. Cover startup/latest/load/fork, submit, cancel, pause/resume/steer/background, exact recovery `ResumeOperation` and `CancelOperation`, interactions, compact/backtrack, task stop/foreground, workflow/result, Goal, model/reasoning/mode/roots/settings, scoped memory, folder trust, pinned context, catalog/mention queries, thread close, and app shutdown. Add race tests for cancel during startup/background and no fire-and-forget error loss.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-tui --lib surface_actions -- --test-threads=1
```

Expected: tests fail because current TUI paths directly use `RuntimeHostHandle`, `RuntimeThreadHandle`, controllers, history, Goal/task/MCP registries, memory, trust, and mutable config.

- [ ] **Step 3: Implement the complete action cutover**

Add the two exact recovery variants and replace `Remember(String)` with scoped `Remember { scope, note }`. Route every mutation/query through `surface_actions`; wait for required receipts before changing effective presentation. Keep mention/fuzzy search local only against immutable snapshot data, otherwise use the closed catalog queries.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-tui --lib surface_actions -- --test-threads=1
cargo test --locked --offline -p orca-tui --lib -- --test-threads=1
```

Expected: all TUI unit tests pass with no direct authority path needed for behavior.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-tui/src/surface_actions.rs crates/orca-tui/src/types.rs crates/orca-tui/src/app.rs crates/orca-tui/src/agent_runtime.rs crates/orca-tui/src/action_dispatcher.rs crates/orca-tui/src/running_actions.rs crates/orca-tui/src/approval_actions.rs crates/orca-tui/src/background_tasks.rs crates/orca-tui/src/workflow_panel_actions.rs crates/orca-tui/src/slash_command_actions.rs crates/orca-tui/src/slash_menu_actions.rs crates/orca-tui/src/session_picker_actions.rs crates/orca-tui/src/setup_actions.rs crates/orca-tui/src/mention_search_manager.rs crates/orca-tui/src/submitted_turn.rs
git commit -m "feat(tui): route all control through runtime surface"
```

### Task 14: Delete TUI Ownership And Prove PTY Parity

**Files:**

- Delete: `crates/orca-tui/src/operation_controller.rs`
- Delete: `crates/orca-tui/src/interaction_broker.rs`
- Delete: `crates/orca-tui/src/runtime_interaction_adapter.rs`
- Delete: `crates/orca-tui/src/runtime_interaction_adapter_tests.rs`
- Delete: `crates/orca-tui/src/runtime_event_projection.rs`
- Delete: `crates/orca-tui/src/background_approval.rs`
- Delete: `crates/orca-tui/src/workflow_notifications.rs`
- Modify: `crates/orca-tui/src/hosted_runtime.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Create: `tests/tui_pty_contract.rs`

- [ ] **Step 1: Strengthen deletion and PTY RED tests**

The import guard must scan every production TUI module and reject `OperationHandle`, `RuntimeThreadHandle`, `RuntimeHostHandle`, `GoalRuntimeHandle`, `TaskRegistry`, `McpRegistry`, `RuntimePendingInteractionStore`, generation handlers, writable stores/brokers/config, direct history/memory/folder-trust access, and `EventEnvelope.payload`. PTY tests cover submit/stream/cancel, approval recovery, background handoff, Goal recovery controls, session reload, and terminal restoration.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-tui --lib surface_boundary_tests -- --test-threads=1
cargo test --locked --offline --test tui_pty_contract -- --test-threads=1
```

Expected: boundary tests report legacy imports/files and PTY parity still traverses legacy controllers.

- [ ] **Step 3: Delete the legacy ownership paths**

Remove module declarations, controller-based test support, terminal buffers, raw observers/reducers, pending interaction assembly, approval allowlists, workflow continuation queues, and direct store/registry access. Retain only presentation/rendering, keys, clipboard, terminal lifecycle, and facade-backed client state.

- [ ] **Step 4: Run GREEN and the Phase 3 gate**

```bash
cargo test --locked --offline -p orca-tui --lib surface_boundary_tests -- --test-threads=1
cargo test --locked --offline --test tui_pty_contract -- --test-threads=1
cargo test --workspace --all-targets --locked --offline -- --test-threads=1
```

Expected: import guard, PTY parity, and workspace tests pass.

- [ ] **Step 5: Commit**

```bash
git add -A crates/orca-tui/src/operation_controller.rs crates/orca-tui/src/interaction_broker.rs crates/orca-tui/src/runtime_interaction_adapter.rs crates/orca-tui/src/runtime_interaction_adapter_tests.rs crates/orca-tui/src/runtime_event_projection.rs crates/orca-tui/src/background_approval.rs crates/orca-tui/src/workflow_notifications.rs crates/orca-tui/src/hosted_runtime.rs crates/orca-tui/src/lib.rs crates/orca-tui/src/surface_boundary_tests.rs tests/tui_pty_contract.rs
git commit -m "refactor(tui): delete runtime ownership mirrors"
```

## Phase 4A: Public ACP Schema Gate

### Task 15: Freeze ACP Public Documentation, Schemas, And Canonical Fixtures

**Files:**

- Create: `docs/protocol/acp-runtime-surface-v1.md`
- Create: `docs/protocol/schemas/acp-runtime-surface-v1.schema.json`
- Create: `docs/protocol/fixtures/acp-runtime-surface-v1/standard-only.jsonl`
- Create: `docs/protocol/fixtures/acp-runtime-surface-v1/orca-surface-v1.jsonl`
- Create: `docs/protocol/fixtures/acp-runtime-surface-v1/failures.jsonl`
- Create: `scripts/validate-acp-runtime-surface.mjs`
- Create: `scripts/test-validate-acp-runtime-surface.mjs`

- [ ] **Step 1: Write schema-validator RED tests**

Cover every method direction, initialize metadata, new/load/update/prompt metadata, reverse requests/responses, extension envelopes, terminal results, transport errors, capability calls/results, size limits, forbidden unknown fields, fixture correlation, and complete StandardOnly/OrcaSurfaceV1 disposition membership.

- [ ] **Step 2: Run RED**

```bash
node scripts/test-validate-acp-runtime-surface.mjs
```

Expected: failure because the protocol document, schema, fixtures, and validator do not exist.

- [ ] **Step 3: Author and validate the public contract**

Derive the public wire shape from the private ACP projection contract without serializing private Rust candidates automatically. Freeze baseline-before-load-response, live-after-response, prompt binding states, interaction extension behavior, exact standard fallbacks, terminal mapping, gaps, transport retirement, capability ambiguity, and canonical size limits.

- [ ] **Step 4: Run GREEN**

```bash
node scripts/test-validate-acp-runtime-surface.mjs
node scripts/validate-acp-runtime-surface.mjs
git diff --check
```

Expected: schemas and all canonical fixtures validate.

- [ ] **Step 5: Commit the Phase 4A artifact**

```bash
git add docs/protocol/acp-runtime-surface-v1.md docs/protocol/schemas/acp-runtime-surface-v1.schema.json docs/protocol/fixtures/acp-runtime-surface-v1 scripts/validate-acp-runtime-surface.mjs scripts/test-validate-acp-runtime-surface.mjs
git commit -m "docs(acp): freeze runtime surface wire schema"
```

- [ ] **Step 6: STOP for independent written review**

Record the commit plus SHA-256 of the protocol document, schema, and three fixture files. Do not modify `acp/agent.rs`, switch sessions, or begin Task 16 until an independent reviewer explicitly approves those exact artifacts. Any edit after review repeats this task and review.

## Phase 4B: ACP Vertical Cutover

### Task 16: Add ACP Projection, Prompt Binding, And Connection Supervision

**Files:**

- Create: `crates/orca-runtime/src/acp/projection.rs`
- Create: `crates/orca-runtime/src/acp/prompt_binding.rs`
- Create: `crates/orca-runtime/src/acp/supervisor.rs`
- Create: `crates/orca-runtime/tests/acp_projection.rs`
- Create: `crates/orca-runtime/tests/acp_prompt_binding.rs`
- Create: `crates/orca-runtime/tests/acp_shutdown.rs`
- Modify: `crates/orca-runtime/src/acp/rpc_facade.rs`
- Modify: `crates/orca-runtime/src/acp/mod.rs`

- [ ] **Step 1: Write ACP state-machine RED tests**

Cover the frozen projection and terminal matrices; Decoded/Reserved/Bound/TerminalGated/ResponseWriting/Completed/TransportRetired transitions; reverse-scheduled prompt/cancel; exact operation cancellation after backgrounding; terminal cursor flush before response; gaps and oversized baselines; slow/failing writer; stdio vs embedded EOF; multi-session detach; monotonic request-id tombstone expiry and issuing-clock loss; and joined shutdown.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test acp_projection -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test acp_prompt_binding -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test acp_shutdown -- --test-threads=1
```

Expected: compilation fails because projection, prompt binding, and supervisor modules are missing.

- [ ] **Step 3: Implement the transport-owned state only**

Map typed snapshots/batches to the reviewed wire schema, sequence prompt/cancel by read order, bind prompt correlation to the runtime-reserved operation, gate responses on terminal cursor flush, retire/tombstone failed transports exactly once, and join all connection work. Keep runtime lifecycle and interactions behind surface commands.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test acp_projection -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test acp_prompt_binding -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test acp_shutdown -- --test-threads=1
node scripts/validate-acp-runtime-surface.mjs
```

Expected: all ACP transport state machines match the reviewed fixtures.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/acp/projection.rs crates/orca-runtime/src/acp/prompt_binding.rs crates/orca-runtime/src/acp/supervisor.rs crates/orca-runtime/src/acp/rpc_facade.rs crates/orca-runtime/src/acp/mod.rs crates/orca-runtime/tests/acp_projection.rs crates/orca-runtime/tests/acp_prompt_binding.rs crates/orca-runtime/tests/acp_shutdown.rs
git commit -m "feat(acp): add typed surface transport state"
```

### Task 17: Cut ACP Sessions, Content, Interactions, Capabilities, And Terminals To The Surface

**Files:**

- Create: `crates/orca-runtime/src/acp/interactions.rs`
- Create: `crates/orca-runtime/src/acp/capability_calls.rs`
- Create: `crates/orca-runtime/tests/acp_interactions.rs`
- Create: `crates/orca-runtime/tests/acp_capability_calls.rs`
- Modify: `crates/orca-runtime/src/acp/agent.rs`
- Modify: `crates/orca-runtime/src/acp/mod.rs`
- Modify: `crates/orca-runtime/tests/acp_agent.rs`

- [ ] **Step 1: Write end-to-end ACP RED tests**

Cover create/load with MCP servers and additional directories, complete fresh baseline before load response, live after response, content-block ordering and explicit unsupported rejection before reservation, reserve/admit prompt, exact cancel, all five interaction kinds and extensionless fallback, route/grant rotation, late responses, all seven client capability calls, 4 MiB result construction limits, delivery ambiguity, remote terminal kill-then-release, and exact terminal/error mapping.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test acp_agent -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test acp_interactions -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test acp_capability_calls -- --test-threads=1
```

Expected: tests fail because `OrcaAcpAgent` still owns raw runtime handles, flattens/skips content, mirrors active operations, and lacks the runtime interaction/capability routes.

- [ ] **Step 3: Implement the complete ACP cutover**

Change sessions to `RuntimeSurfaceHostHandle`/`RuntimeSurfaceHandle`, route new/load/settings/catalog/prompt/cancel through closed commands, attach to typed batches, proxy client interactions/capabilities with exact grants, and respond only from runtime terminal facts after the physical barrier. Preserve only transport correlation in ACP state.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test acp_agent -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test acp_interactions -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test acp_capability_calls -- --test-threads=1
cargo test --locked --offline -p orca-runtime acp:: --lib -- --test-threads=1
```

Expected: ACP behavior and all frozen mappings pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/acp/interactions.rs crates/orca-runtime/src/acp/capability_calls.rs crates/orca-runtime/src/acp/agent.rs crates/orca-runtime/src/acp/mod.rs crates/orca-runtime/tests/acp_agent.rs crates/orca-runtime/tests/acp_interactions.rs crates/orca-runtime/tests/acp_capability_calls.rs
git commit -m "feat(acp): cut sessions over to runtime surface"
```

### Task 18: Delete ACP Mirrors And Enforce The Import Boundary

**Files:**

- Delete: `crates/orca-runtime/src/acp/event_map.rs`
- Create: `crates/orca-runtime/tests/acp_import_boundary.rs`
- Modify: `crates/orca-runtime/src/acp/agent.rs`
- Modify: `crates/orca-runtime/src/acp/mod.rs`

- [ ] **Step 1: Write the ACP boundary RED test**

Scan all production `acp/**` modules and reject `RuntimeHostHandle`, `RuntimeThreadHandle`, `OperationHandle`, raw history/session stores, `EventEnvelope` payload access, writable brokers/registries, generation handlers, local outcome/stop-reason inference, `current_op`, `cancel_requested`, and unbounded channels.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test acp_import_boundary -- --test-threads=1
```

Expected: the test reports current raw event mapping and any remaining legacy imports/state.

- [ ] **Step 3: Delete the mirrors**

Remove raw event-map authority, transcript preload semantics, local terminal inference, active-operation mirrors, and the unbounded notification lane. Keep only the closed surface facades, reviewed ACP DTOs, pure projection, RPC facade, and supervisor.

- [ ] **Step 4: Run GREEN and the Phase 4 gate**

```bash
cargo test --locked --offline -p orca-runtime --test acp_import_boundary -- --test-threads=1
cargo test --locked --offline -p orca-runtime --all-targets -- --test-threads=1
node scripts/validate-acp-runtime-surface.mjs
```

Expected: ACP boundary, runtime tests, and public schema gate pass.

- [ ] **Step 5: Commit**

```bash
git add -A crates/orca-runtime/src/acp/event_map.rs crates/orca-runtime/src/acp/agent.rs crates/orca-runtime/src/acp/mod.rs crates/orca-runtime/tests/acp_import_boundary.rs
git commit -m "refactor(acp): delete runtime ownership mirrors"
```

## Phase 5: JSONL Compatibility Convergence

### Task 19: Move JSONL Host, Thread, Read, And Turn Routing To The Surface

**Files:**

- Create: `crates/orca-runtime/src/server/surface_adapter.rs`
- Create: `crates/orca-runtime/tests/jsonl_surface_routing.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Modify: `crates/orca-runtime/src/server_runtime.rs`
- Modify: `crates/orca-runtime/src/server/router.rs`
- Modify: `tests/server_runtime_contract.rs`
- Modify: `tests/session_server_contract.rs`

- [ ] **Step 1: Write routing RED tests**

Cover released thread list/search/read/item pages/metadata/start/resume/fork/resolve-running, settings-before-Requested, one operation with multiple generations, exact turn ids, controls, unsupported `thread/close`, `LegacyAcceptedDropped`, pagination/filter semantics, coherent reads, attach gaps, and every manifest JSONL request routing row.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test jsonl_surface_routing -- --test-threads=1
cargo test --locked --offline --test server_runtime_contract -- --test-threads=1
```

Expected: tests fail because `ServerThreadRuntime` owns a live handle map and server request handlers read/write `SessionStore` directly.

- [ ] **Step 3: Implement the compatibility adapter**

Decode the released wire into closed host/thread commands, keep wire ids and errors unchanged, project typed reads/results back to v0.2.50 shapes, and attach turn streams to typed batches. Do not add `thread/close`; retain the released unsupported-method response.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test jsonl_surface_routing -- --test-threads=1
cargo test --locked --offline --test server_runtime_contract -- --test-threads=1
cargo test --locked --offline --test session_server_contract -- --test-threads=1
```

Expected: routing and existing server compatibility tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/server.rs crates/orca-runtime/src/server_runtime.rs crates/orca-runtime/src/server/router.rs crates/orca-runtime/src/server/surface_adapter.rs crates/orca-runtime/tests/jsonl_surface_routing.rs tests/server_runtime_contract.rs tests/session_server_contract.rs
git commit -m "feat(server): route jsonl sessions through runtime surface"
```

### Task 20: Converge JSONL Turn Control, Interactions, Permissions, And Shutdown

**Files:**

- Create: `crates/orca-runtime/src/server/opaque_permission_router.rs`
- Create: `crates/orca-runtime/src/server/direct_interaction_adapter.rs`
- Create: `crates/orca-runtime/src/server/connection_supervisor.rs`
- Create: `crates/orca-runtime/tests/jsonl_surface_interactions.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Modify: `crates/orca-runtime/src/server/permission_manager.rs`
- Modify: `crates/orca-runtime/src/server/user_input_manager.rs`
- Modify: `crates/orca-runtime/src/server/mcp_elicitation_manager.rs`
- Modify: `crates/orca-runtime/src/server/command_exec_manager.rs`
- Modify: `crates/orca-runtime/src/server/shell_manager.rs`
- Modify: `crates/orca-runtime/src/server/fuzzy_file_search_manager.rs`
- Modify: `crates/orca-runtime/src/server/mention_search_manager.rs`
- Modify: `tests/session_server_contract.rs`

- [ ] **Step 1: Write supervisor and responder RED tests**

Cover `ControlJsonlTurn`, opaque request-id registration/publication/response/tombstone transitions, shared `permission/respond` routing between thread and command/exec permissions, direct user-input/MCP responders, duplicate/late/wrong-kind responses, legacy opaque MCP content, and folder-trust policy epochs. Drive both ledgers through one connection admission lock and live counter; assert `JSONL_LIVE_REQUEST_LIMIT = 1_024` and `JSONL_REPAIR_AUTHORITY_LIMIT = 1_024`. Before any request frame is written, registration must expire tombstones, check the combined live limit, and reserve a collision-free opaque id, the next connection-scoped retirement sequence, and one non-cloneable repair-authority permit. Exercise opaque-id, retirement-sequence, live-limit, and repair-capacity exhaustion without writing a frame.

For each admission rejection, require the owner-specific fail-closed settlement: thread permission, direct user input, and direct MCP elicitation produce only `Applied`, `DeferredToRuntime`, or `RecoveryRetained`; command/exec permission produces the exact `FailedBeforeExecution` fence receipt and starts no side effect. Assert tombstone expiry before every lookup and insertion, the one shared retirement allocator, and the closed ordering `ThreadPermission = 0`, `CommandExecPermission = 1`, `DirectUserInput = 2`, `DirectMcpElicitation = 3`.

Cover EOF with active turns/interactions/processes, writer failure, and every supervisor close row. A `Routed` transport retirement must carry the exact `JsonlOwnerSettlement`; `DeferredToRuntime` must consume the admission permit, create the durable repair record, and transfer it before the route can become `Tombstoned(TransportRetired)`. Entry to close freezes exactly the remaining `CommittedPending` repair plan, retires only permission/direct response routes, drains repairs under one absolute `JSONL_COMMITTED_REPAIR_DRAIN_DEADLINE_MS = 5_000` deadline, then detaches surfaces and settles command-exec, shell, file-search, and mention-search under one absolute `JSONL_SUPERVISOR_JOIN_DEADLINE_MS = 5_000` deadline. Require repair settlements `Completed`, `RetainedPending`, or `FailedRetained`; service settlements `Joined`, `AbortedAfterDeadline`, or `CleanupUnconfirmed`; and final results `Clean`, `CleanupDegraded`, `ShutdownFailed`, or `IoFailed`. Timeout or repair failure must transfer the durable record to the runtime recovery owner with its transfer receipt, and every committed tombstone must retain the original committed receipt rather than becoming `TransportRetired`. Assert `ShutdownHost` is called exactly once in every close path.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline -p orca-runtime --test jsonl_surface_interactions -- --test-threads=1
```

Expected: tests fail because `ServerActiveTurnRegistry` and three pending managers still own thread operation/interaction waiters; the ledgers lack shared bounded admission, retirement, and durable-repair transfer authority; non-thread permission routing is not isolated behind one host owner; and shutdown does not yet produce the closed repair/service evidence or single `ShutdownHost` call.

- [ ] **Step 3: Implement the closed adapters**

Route thread interactions and controls to runtime commands; retain only opaque wire-id correlations and tombstones. Implement one connection-scoped admission authority shared by the permission and direct ledgers: expire tombstones first, enforce both 1,024-entry budgets, and reserve the opaque id, retirement sequence, and non-cloneable repair-authority permit atomically before frame publication. On admission rejection or transport retirement, settle the exact owner through `JsonlOwnerSettlement`; a deferred interaction must create and transfer its durable repair record before tombstoning, and a command/exec route must fail its exact pre-execution fence. Never leave a grant or executable fence live. Use the shared checked retirement allocator and exact owner rank for deterministic tombstone cleanup and close planning.

Move command/exec permission routing behind the host-owned opaque router with the same policy epoch. Add one connection supervisor that seals input; freezes the exact `CommittedPending` repair plan; retires only permission/direct response routes; drains or transfers repairs under the single 5,000 ms repair deadline; detaches adapter surfaces; and settles the four fixed services under the single absolute `JSONL_SUPERVISOR_JOIN_DEADLINE_MS = 5_000` deadline. Replace the blocking `stop_all()`/`JoinHandle::join()` paths in `fuzzy_file_search_manager.rs` and `mention_search_manager.rs` with bounded settlement under that shared absolute deadline, producing exactly `Joined`, `AbortedAfterDeadline`, or `CleanupUnconfirmed`. Preserve original committed receipts, carry complete repair/service evidence into the exact final result, retain the host shutdown handle, and invoke `ShutdownHost` exactly once after cleanup evidence is fixed.

- [ ] **Step 4: Run GREEN**

```bash
cargo test --locked --offline -p orca-runtime --test jsonl_surface_interactions -- --test-threads=1
cargo test --locked --offline --test session_server_contract -- --test-threads=1
```

Expected: interaction, permission, bounded admission, deterministic retirement, committed-repair transfer, EOF/I/O close, service settlement, one-shot host shutdown, process, and existing server tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/server/opaque_permission_router.rs crates/orca-runtime/src/server/direct_interaction_adapter.rs crates/orca-runtime/src/server/connection_supervisor.rs crates/orca-runtime/src/server/permission_manager.rs crates/orca-runtime/src/server/user_input_manager.rs crates/orca-runtime/src/server/mcp_elicitation_manager.rs crates/orca-runtime/src/server/command_exec_manager.rs crates/orca-runtime/src/server/shell_manager.rs crates/orca-runtime/src/server/fuzzy_file_search_manager.rs crates/orca-runtime/src/server/mention_search_manager.rs crates/orca-runtime/src/server.rs crates/orca-runtime/tests/jsonl_surface_interactions.rs tests/session_server_contract.rs
git commit -m "feat(server): converge jsonl control and interactions"
```

### Task 21: Add Differential Fixtures And Delete JSONL Ownership

**Files:**

- Create: `tests/fixtures/jsonl-v0.2.50/requests.jsonl`
- Create: `tests/fixtures/jsonl-v0.2.50/expected-events.jsonl`
- Create: `tests/jsonl_surface_differential.rs`
- Create: `crates/orca-runtime/tests/jsonl_import_boundary.rs`
- Delete: `crates/orca-runtime/src/server/active_turn_registry.rs`
- Delete: `crates/orca-runtime/src/server_runtime.rs`
- Delete: `crates/orca-runtime/src/server/permission_manager.rs`
- Delete: `crates/orca-runtime/src/server/user_input_manager.rs`
- Delete: `crates/orca-runtime/src/server/mcp_elicitation_manager.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Modify: `tests/exec_jsonl.rs`
- Modify: `tests/session_server_contract.rs`

- [ ] **Step 1: Write differential and import-boundary RED tests**

Freeze only actually released v0.2.50 flows from the manifest: request/response/error fields, event names, item lifecycle, settings ordering, multi-loop-turn behavior, controls, permissions/interactions, pagination, gaps, and EOF. Add deterministic internal fixtures for the 1,024-live-request boundary, owner-specific admission rejection, tombstone expiry and rank ordering, committed repair completion/retention/failure, all three service settlement states, all four close results, and EOF/write/flush close races; these fixtures validate typed internal evidence without adding fields to the released JSONL wire. The import guard rejects active-turn authority, direct lifecycle SessionStore/history access, thread-bound pending waiter ownership, raw semantic payload projection, local terminal inference, and any second JSONL shutdown or repair authority.

- [ ] **Step 2: Run RED**

```bash
cargo test --locked --offline --test jsonl_surface_differential -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test jsonl_import_boundary -- --test-threads=1
```

Expected: the fixture harness is absent and the boundary test reports `ServerActiveTurnRegistry`, direct stores, and pending managers.

- [ ] **Step 3: Complete compatibility projection and deletions**

Encode typed batches/results into the frozen output fixtures, including exact gap behavior and `AgentLoopTurnStarted`, while keeping close evidence and repair authority private. Prove that every admission failure has settled its interaction or exact command/exec fence before any rejected frame could be emitted; every transport-retired tombstone carries its exact owner settlement; committed receipts survive all transport retirement races; and cleanup health changes only the typed close result, never v0.2.50 wire output. Delete active-turn and thread-bound waiter ownership; leave command/exec services only behind their host-owned router, leave one final wire encoder as the raw JSON boundary, and leave the runtime recovery owner as the only post-connection owner of transferred durable repair records.

- [ ] **Step 4: Run GREEN and the Phase 5 gate**

```bash
cargo test --locked --offline --test jsonl_surface_differential -- --test-threads=1
cargo test --locked --offline -p orca-runtime --test jsonl_import_boundary -- --test-threads=1
cargo test --locked --offline --test exec_jsonl -- --test-threads=1
cargo test --locked --offline --test session_server_contract -- --test-threads=1
cargo test --workspace --all-targets --locked --offline -- --test-threads=1
```

Expected: the v0.2.50 differential corpus remains byte-stable; bounded-admission, retirement-order, repair-transfer, and close-result fixtures pass; the deletion guard finds no JSONL lifecycle, waiter, shutdown, or repair authority; and JSONL integration plus workspace suites pass.

- [ ] **Step 5: Commit**

```bash
git add -A crates/orca-runtime/src/server/active_turn_registry.rs crates/orca-runtime/src/server/permission_manager.rs crates/orca-runtime/src/server/user_input_manager.rs crates/orca-runtime/src/server/mcp_elicitation_manager.rs crates/orca-runtime/src/server.rs crates/orca-runtime/src/server_runtime.rs crates/orca-runtime/tests/jsonl_import_boundary.rs tests/fixtures/jsonl-v0.2.50/requests.jsonl tests/fixtures/jsonl-v0.2.50/expected-events.jsonl tests/jsonl_surface_differential.rs tests/exec_jsonl.rs tests/session_server_contract.rs
git commit -m "refactor(server): delete jsonl runtime ownership"
```

## Phase 6: Documentation, Real DeepSeek, And Release

### Task 22: Document The Shipped Ownership And Recovery Contracts

**Files:**

- Modify: `README.md`
- Modify: `docs/architecture/adr/0005-runtime-host-operation-control-plane.md`
- Modify: `docs/production-roadmap.md`
- Modify: `docs/harness-contract.md`
- Modify: `docs/goal-mode.md`
- Create: `docs/acp.md`
- Create: `docs/server-mode.md`
- Create: `docs/troubleshooting.md`
- Modify: `docs/release-process.md`
- Modify: `scripts/test-validate-runtime-surface-contract.mjs`

- [ ] **Step 1: Write documentation contract RED checks**

Extend the manifest validator or add assertions to `scripts/test-validate-runtime-surface-contract.mjs` for the public ownership statement, fresh/cursor attach semantics, interaction recovery, Goal continuation, TUI recovery actions, ACP extension/load/terminal barriers, JSONL compatibility limits, shutdown/repair guidance, and the full release gate.

- [ ] **Step 2: Run RED**

```bash
node scripts/test-validate-runtime-surface-contract.mjs
```

Expected: documentation assertions fail because the shipped docs still describe legacy handles/adapters or omit the new recovery and release contracts.

- [ ] **Step 3: Update the documentation**

Describe user-visible behavior and operational recovery, not private implementation type dumps. State that TUI, ACP, and JSONL are clients of one runtime-owned surface; document supported ACP standard/extension behavior and JSONL v0.2.50 preservation; update release steps so local green is not release completion.

- [ ] **Step 4: Run GREEN**

```bash
node scripts/test-validate-runtime-surface-contract.mjs
npm --prefix site run check:seo
git diff --check
```

Expected: documentation contract, SEO checks, and whitespace checks pass.

- [ ] **Step 5: Commit**

```bash
git add README.md docs/architecture/adr/0005-runtime-host-operation-control-plane.md docs/production-roadmap.md docs/harness-contract.md docs/goal-mode.md docs/acp.md docs/server-mode.md docs/troubleshooting.md docs/release-process.md scripts/test-validate-runtime-surface-contract.mjs
git commit -m "docs: describe runtime-owned typed surface"
```

### Task 23: Add Deterministic And Credentialed Real-API Harnesses

**Files:**

- Create: `scripts/release/real-api-tui-approval-recovery.mjs`
- Create: `scripts/release/test-real-api-tui-approval-recovery.mjs`
- Create: `scripts/release/real-api-acp-surface.mjs`
- Create: `scripts/release/test-real-api-acp-surface.mjs`
- Create: `scripts/release/real-api-server-approval-recovery.mjs`
- Create: `scripts/release/test-real-api-server-approval-recovery.mjs`
- Modify: `scripts/release/real-api-e2e.mjs`
- Modify: `scripts/release/test-real-api-e2e.mjs`

- [ ] **Step 1: Write fake-peer RED self-tests**

Each self-test must prove its harness fails on a missing event, wrong ordering, stale response acceptance, terminal before flush, unreaped child, and timeout; succeeds only after cleanup; accepts an absolute `--bin`; and prints one stable success sentinel. Cover TUI PTY approval/cancel/reattach, ACP new/load/prompt/cancel/baseline ordering, and server approval/EOF recovery.

- [ ] **Step 2: Run RED**

```bash
node scripts/release/test-real-api-tui-approval-recovery.mjs
node scripts/release/test-real-api-acp-surface.mjs
node scripts/release/test-real-api-server-approval-recovery.mjs
```

Expected: all three commands fail because the focused harnesses do not exist.

- [ ] **Step 3: Implement the harnesses**

Use deterministic fake peers in self-tests and isolated temporary `ORCA_HOME` directories. The real scripts use DeepSeek credentials from the existing supported environment/auth path, enforce bounded timeouts/budgets, validate exact semantic milestones, kill all child process groups, and never treat a fake self-test as credentialed evidence.

- [ ] **Step 4: Run deterministic GREEN**

```bash
node scripts/release/test-real-api-e2e.mjs
node scripts/release/test-real-api-tui-approval-recovery.mjs
node scripts/release/test-real-api-acp-surface.mjs
node scripts/release/test-real-api-server-approval-recovery.mjs
```

Expected: all fake deterministic oracles pass without network credentials.

- [ ] **Step 5: Build and run the credentialed pre-tag gate**

```bash
cargo build --release --locked --bin orca
ORCA_BIN="$PWD/target/release/orca"
test -x "$ORCA_BIN"
node scripts/release/real-api-e2e.mjs --orca-bin "$ORCA_BIN" --skip-build --max-budget 0.02 --timeout-ms 300000
node scripts/release/real-api-tui-approval-recovery.mjs --bin "$ORCA_BIN"
node scripts/release/real-api-acp-surface.mjs --bin "$ORCA_BIN"
node scripts/release/real-api-server-approval-recovery.mjs --bin "$ORCA_BIN"
```

Expected: with valid DeepSeek credentials, provider/Goal/CLI/history/JSONL baselines plus focused TUI, ACP, and server recovery all print their success sentinels. Any missing credential, unexpected event, surviving process, or timeout is failure.

- [ ] **Step 6: Commit**

```bash
git add scripts/release/real-api-e2e.mjs scripts/release/test-real-api-e2e.mjs scripts/release/real-api-tui-approval-recovery.mjs scripts/release/test-real-api-tui-approval-recovery.mjs scripts/release/real-api-acp-surface.mjs scripts/release/test-real-api-acp-surface.mjs scripts/release/real-api-server-approval-recovery.mjs scripts/release/test-real-api-server-approval-recovery.mjs
git commit -m "test(release): add typed surface real api gates"
```

### Task 24: Harden Version Sync, Immutable npm Publication, And Public Verification

**Files:**

- Create: `scripts/release/verify-version-sync.mjs`
- Create: `scripts/release/verify-public-real-api.mjs`
- Modify: `scripts/release/test-stage-npm.mjs`
- Modify: `scripts/release/stage-npm.mjs`
- Modify: `scripts/release/test-verify-published.mjs`
- Modify: `scripts/release/verify-published.mjs`
- Modify: `.github/workflows/release.yml`
- Modify: `docs/release-process.md`

- [ ] **Step 1: Write release-hardening RED tests**

Add fixtures for version drift, absent/revoked npm auth, skipped dependencies, missing assets, wrong tag target, checksum failure, registry integrity mismatch, wrong optional-dependency aliases, package/archive binary mismatch, and install failure. Test that `verify-public-real-api.mjs` rejects a missing DeepSeek key and always installs the exact public version into a clean temporary directory.

- [ ] **Step 2: Run RED**

```bash
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
node scripts/release/verify-version-sync.mjs
```

Expected: tests fail on the new cases and `verify-version-sync.mjs` is absent.

- [ ] **Step 3: Implement release hardening**

Make the workflow run locked all-target workspace tests, require `npm-auth` before GitHub Release publication, publish the exact five staged/smoked `.tgz` files native-first/main-last, upload those same files, and add an `always()` final verify job that first requires `release`, `npm`, and `npm-release-assets` results to be `success`. Extend public verification to prove tag/main SHA, four archives plus checksums, five npm tarballs and versions, registry integrity and complete trees, alias keys, binary hash identity, installability, and binary version. Keep credentialed public real-API verification outside the no-secret Actions verifier.

- [ ] **Step 4: Run GREEN**

```bash
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
node scripts/release/verify-version-sync.mjs
node --test tests/pages_workflow_contract.test.mjs
```

Expected: all release helper and workflow contract tests pass.

- [ ] **Step 5: Commit**

```bash
git add scripts/release/verify-version-sync.mjs scripts/release/verify-public-real-api.mjs scripts/release/test-stage-npm.mjs scripts/release/stage-npm.mjs scripts/release/test-verify-published.mjs scripts/release/verify-published.mjs .github/workflows/release.yml docs/release-process.md
git commit -m "build(release): harden typed surface publication"
```

### Task 25: Prepare The Patch Version And Run The Complete Local Gate

**Files:**

- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `npm/orca/package.json`
- Modify: `README.md`
- Create: `docs/releases/v0.2.51.md`
- Modify: `docs/production-roadmap.md`
- Modify: `site/src/shared.ts`
- Modify: `site/src/changelog/Changelog.tsx`
- Modify: `site/index.html`
- Modify: `site/public/sitemap.xml`

- [ ] **Step 1: Rebase and choose the actual version**

Fetch and rebase onto current `origin/main` while preserving all reviewed commits. Confirm `v0.2.51` is still unused before editing. If it has become unavailable, stop and revise this task's version/path/commands to the next unused patch before proceeding. Do not bump internal `0.1.0` crate versions.

- [ ] **Step 2: Run version-sync RED before editing all surfaces**

```bash
node scripts/release/verify-version-sync.mjs
```

Expected: failure naming every still-stale Cargo/npm/README/site/roadmap/release-note surface.

- [ ] **Step 3: Update version and release content**

Synchronize all listed files, both English and Chinese changelog maps, pinned install examples, `softwareVersion`, sitemap dates/URLs, and release notes. Keep claims limited to gates actually run.

- [ ] **Step 4: Run the deterministic release gate**

```bash
cargo fetch --locked
cargo fmt --all -- --check
cargo test --workspace --all-targets --locked --offline -- --test-threads=1
cargo clippy --workspace --all-targets --locked --offline
git diff --check
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
node scripts/release/test-real-api-e2e.mjs
node scripts/release/test-real-api-tui-approval-recovery.mjs
node scripts/release/test-real-api-acp-surface.mjs
node scripts/release/test-real-api-server-approval-recovery.mjs
node scripts/release/verify-version-sync.mjs
node scripts/validate-runtime-surface-contract.mjs
node scripts/validate-acp-runtime-surface.mjs
node --test tests/pages_workflow_contract.test.mjs
npm --prefix site run build
npm --prefix site run check:seo
```

Expected: every command succeeds on the rebased tree. Separate pre-existing warnings/generated cache churn from regressions; do not waive failures.

- [ ] **Step 5: Repeat the credentialed pre-tag gate**

Run the four real DeepSeek commands from Task 23 against the final release binary. Expected: all succeed with the final version and no surviving processes.

- [ ] **Step 6: Commit the release preparation**

```bash
git add Cargo.toml Cargo.lock npm/orca/package.json README.md docs/releases/v0.2.51.md docs/production-roadmap.md site/src/shared.ts site/src/changelog/Changelog.tsx site/index.html site/public/sitemap.xml
git commit -m "chore(release): prepare v0.2.51"
```

### Task 26: Publish And Verify GitHub, npm, Pages, And Public DeepSeek Behavior

**Files:** No source edits are expected. If verification finds a defect, stop publication claims, fix it in a new reviewed commit, rerun Task 25, and use a new patch version if any immutable artifact was already published.

- [ ] **Step 1: Obtain explicit publication authorization and push main**

Publication, tag creation, npm writes, and paid DeepSeek checks are external side effects. Do not modify `main` during implementation. Only after Task 26 has explicit publication authorization, record that authorization, confirm credentials, identify the primary `main` worktree, and run:

```bash
test "${ORCA_PUBLICATION_AUTHORIZED:-}" = "yes"
FEATURE_WORKTREE="$(git rev-parse --show-toplevel)"
FEATURE_REVIEWED_SHA="$(git rev-parse HEAD)"
test -z "$(git status --porcelain=v1 --untracked-files=all)"
PRIMARY_MAIN_WORKTREE="$(git worktree list --porcelain | awk '/^worktree / { worktree = substr($0, 10) } $0 == "branch refs/heads/main" { print worktree }')"
test -n "$PRIMARY_MAIN_WORKTREE"
test "$(printf '%s\n' "$PRIMARY_MAIN_WORKTREE" | wc -l | tr -d ' ')" = "1"
test "$PRIMARY_MAIN_WORKTREE" != "$FEATURE_WORKTREE"
test -z "$(git -C "$PRIMARY_MAIN_WORKTREE" status --porcelain=v1 --untracked-files=all)"
git -C "$PRIMARY_MAIN_WORKTREE" fetch origin
test "$(git -C "$PRIMARY_MAIN_WORKTREE" symbolic-ref --short HEAD)" = "main"
git -C "$PRIMARY_MAIN_WORKTREE" merge --ff-only "$FEATURE_REVIEWED_SHA"
git -C "$PRIMARY_MAIN_WORKTREE" push origin HEAD:main
git -C "$PRIMARY_MAIN_WORKTREE" fetch origin main
test "$(git -C "$PRIMARY_MAIN_WORKTREE" rev-parse HEAD)" = "$FEATURE_REVIEWED_SHA"
test "$(git -C "$PRIMARY_MAIN_WORKTREE" rev-parse origin/main)" = "$FEATURE_REVIEWED_SHA"
```

Expected: both worktrees are clean before publication, primary `main` fast-forwards without a merge commit to the fully reviewed feature SHA, and primary `HEAD`, the reviewed feature SHA, and fetched `origin/main` are exactly equal.

- [ ] **Step 2: Tag the exact main SHA and push the tag**

```bash
git tag -a v0.2.51 -m "Orca v0.2.51"
test "$(git rev-list -n1 v0.2.51)" = "$(git rev-parse origin/main)"
git push origin v0.2.51
```

Expected: the annotated tag targets the pushed main SHA and is an ancestor of `origin/main`.

- [ ] **Step 3: Wait for every Release and Pages job**

Use GitHub CLI/API to require every job in the tag Release workflow, including final `verify`, to report `success`, and require Pages build/deploy for the same release SHA. A skipped npm job, absent token, missing Pages run, or merely green partial job is failure.

- [ ] **Step 4: Verify public artifacts**

```bash
node scripts/release/verify-published.mjs --version 0.2.51 --sha "$(git rev-parse HEAD)"
```

Expected: published non-draft/non-prerelease GitHub Release at the exact SHA; four native archives, four checksums, five npm tarballs; matching registry integrity/trees and native binary hashes; correct aliases; clean install; and `orca 0.2.51`.

- [ ] **Step 5: Verify public site and install path**

Confirm the homepage and changelog show `v0.2.51`, their Release link targets that tag, public `install.sh` installs the tag, and the Pages deployment belongs to the release SHA.

- [ ] **Step 6: Run public-package real DeepSeek verification**

```bash
node scripts/release/verify-public-real-api.mjs --version 0.2.51 --require-deepseek-api-key
```

Expected: the script installs the exact public npm package in a clean directory, resolves its absolute binary, and passes the TUI, ACP, and server real-API harnesses. Missing credentials or any focused failure is release failure.

- [ ] **Step 7: Record release evidence**

Record the main/tag SHA, Release workflow URL and all job results, Pages workflow URL, GitHub Release URL, npm versions/integrities, artifact checksums, installed binary version, public site/install checks, and the four credentialed success sentinels. Only after this evidence exists is the runtime-owned typed surface release complete.
