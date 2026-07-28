# JSONL Stateless Submit Runtime Surface Specification

**Status:** Implemented and verified on the feature branch. The runtime
ownership replacement, public API compatibility repair, clean-EOF lifecycle,
and real DeepSeek stateless path are complete. The branch remains unintegrated
and unpublished.

**Base:** `origin/main@d6f98b0ac9eafc9228594096db5390f0d3f860e9`

**Related contract:** `2026-07-21-runtime-owned-typed-surface-private-contract.md`

## Problem And Classification

Unbound JSONL `submit` and `turn/start` requests still bypass the runtime-owned
typed surface. `server/processors/submit.rs` routes them to `server::run_submit`,
which constructs a history-disabled `RunConfig` and calls
`controller::run_to_writer_with_options` directly. Thread-bound JSONL requests,
TUI requests, and ACP requests instead reserve and admit operations through the
runtime host.

This is an architecture defect, not a processor-local defect:

- the server owns a second agent loop and a second shutdown path;
- the runtime host cannot identify, cancel, await, or reclaim stateless work;
- stateless work has no typed operation, generation fence, interaction route, or
  terminal fact;
- EOF and transport failures cannot close the work through the connection's
  runtime shutdown barrier;
- tests can prove released wire bytes, but cannot prove runtime ownership.

The July 21 private contract already assigns this request class to
`CreateThread(EphemeralNonCataloguedOneShot) + ReserveOperation`. This slice
implements that existing contract; it does not introduce a new released
protocol.

## User Value

The direct TUI value is architectural reliability shared by every interactive
surface: there is one agent loop whose cancellation, terminal state, tool work,
and interactions are observable and reclaimable. Removing the JSONL-only loop
also prevents future TUI fixes from diverging in server mode and makes the
runtime lifecycle tests authoritative for all frontends.

For JSONL users, an interrupted or closed connection no longer leaves work that
the server cannot name or join. Existing request and event shapes remain
unchanged.

## Scope

This slice includes:

1. an explicit history-disabled runtime-thread start mode for an ephemeral,
   noncatalogued, one-shot typed surface;
2. a process-local owner authority and atomic in-memory commit ledger;
3. live-revision surface commits using `CommitClass::Ephemeral`;
4. stateless JSONL mention expansion, operation reservation, admission,
   interaction routing, streaming projection, cancellation, and cleanup through
   the existing runtime host;
5. automatic one-shot actor termination after its first operation terminal;
6. transport-scope cleanup that requests the runtime close barrier on projection
   failure;
7. removal of `server::run_submit` and its direct controller call;
8. direct JSONL protocol projection of committed assistant and plan facts;
9. behavior tests for wire compatibility, no persistence, and resource cleanup.

## Non-Goals

- No released JSONL field, method, event, or ordering contract is renamed.
- No persisted session or surface format changes.
- No restart recovery is claimed for ephemeral threads.
- No new detached worker or adapter-owned operation registry is introduced.
- No implementation from the preserved `durable-interaction-broker-p13`
  worktree is transplanted.
- Goal approval crash tests and workflow-result continuation ownership remain
  separate later slices.

## Ownership Model

| Resource | Sole owner | Release rule |
| --- | --- | --- |
| Ephemeral thread identity | `RuntimeSurfaceHost` start request | Removed when the actor exits |
| Thread actor and generation | `RuntimeSurfaceHost` / `ThreadActor` | Cancelled, joined, terminalized, then actor task joined |
| Process-local owner authority | `RuntimeCommitCoordinator` | Dropped with resident surface state |
| Ephemeral surface facts | in-memory surface commit ledger | Dropped with resident surface state; never written to disk |
| Surface subscription and JSONL projection worker | `JsonlTransportTurn` | Joined on terminal, projection failure, prune, or connection shutdown |
| Transport-loss close request | ephemeral projection guard | Calls the runtime thread close barrier; never cancels tools directly |
| JSONL connection | `JsonlConnectionSupervisor` | Closes ingress, retires routes, settles services, then shuts down the host |

The adapter may retain a runtime thread handle while the request is live. It may
request `RuntimeThreadHandle::shutdown`, but it never owns a cancel token,
generation handle, tool task, or terminal fact.

## Type And Module Boundaries

### Start policy

`RuntimeThreadStartRequest` gains an explicit ephemeral one-shot policy. Plain
`HistoryMode::Disabled` remains surface-less for internal callers. This prevents
unrelated history-disabled helpers, tool turns, and tests from silently gaining
a public surface.

Preparation allocates one UUIDv7 before session construction. The same value is
used by `RuntimeThread`, `SurfaceThreadId`, the owner authority, and the initial
snapshot. A history-disabled `InteractiveSession` still has no session id and no
history writer.

### Owner authority

`ExclusiveOwnerLease` supports two opaque backends:

- durable filesystem lock plus epoch file for recorded threads;
- process-local thread-bound authority for ephemeral threads.

The process-local backend has owner epoch 1, never creates a path, and remains
authoritative only for the lifetime of the coordinator-owned lease. It cannot be
reopened or recovered.

### Commit receipt and ledger

`SurfaceCommitLedger` returns a typed `SurfaceBatchReceipt`:

- `Recorded(DurableBatchReceipt)` contains a durable revision;
- `Ephemeral(EphemeralBatchReceipt)` contains a live revision.

The in-memory ledger accepts only `CommitClass::Ephemeral`, enforces one thread
and incarnation, validates exact cursor continuity, and makes append/probe/
checkpoint idempotent in memory. It does not fabricate a durable revision or a
durability acknowledgement.

`RuntimeSurfaceCommitLedger` is the resident runtime enum over the recorded and
ephemeral ledger implementations. Recovery is still available only through the
recorded ledger. The recorded coordinator is recovered first and then mapped to
the resident enum; an ephemeral coordinator is created fresh.

### Snapshot and batches

The initial snapshot receives `ThreadPersistence` explicitly:

- recorded: `CursorSourceRevision::Recorded { durable_revision: 1 }`;
- ephemeral: `CursorSourceRevision::Ephemeral { live_revision: 1 }`.

The runtime event-batch constructor derives its `CommitClass` and next source
revision from the snapshot cursor. Every event envelope in a batch uses the same
class. Recorded recovery helpers remain recorded-only.

### Server adapter

`JsonlSurfaceAdapter::prepare_stateless_turn` performs this sequence:

1. normalize the config to JSONL, disabled history, no picker, and no desktop
   notification;
2. create one ephemeral noncatalogued runtime surface;
3. expand mentions using that request's cwd, workspace roots, and MCP registry;
4. attach as JSONL, apply typed permission settings, reserve one user operation,
   and claim the subscription;
5. admit it through the runtime actor and project committed surface events with
   the released request id;
6. retain the projection worker and ephemeral runtime close guard until it
   settles.

The projector emits assistant item lifecycle events from typed completed response
items and emits `turn_plan_updated` from the committed `SurfacePlanSnapshot`.
It does not reparse proposed-plan markup to derive item identities. A narrow
wire compatibility normalization maps a single stored reasoning `content` value
back to the released JSONL `summary` shape.

No `thread_started` event is projected and the ephemeral id is never inserted in
the session catalog.

## Lifecycle Semantics

### Normal completion

The operation reaches exactly one typed terminal. The projection worker emits
the existing final JSONL event and exits. The one-shot actor observes the first
terminal, joins background work, seals subscriptions, and exits. The host reaps
the actor task and the adapter removes its closed binding.

### Validation or reservation rejection

The server writes the existing correlated error. Because no executable operation
owns the thread, the adapter immediately requests the runtime close barrier and
removes the binding. There is no catalog record and no orphan actor.

### Not admitted

If a reservation exists but admission becomes terminally not admitted, that
typed terminal is the one-shot completion. The same normal close path runs; no
second request may reuse the ephemeral thread.

### Explicit cancellation

Correlated JSONL control resolves the exact typed operation. Cancellation is
committed before the generation cancel token is triggered. The actor awaits the
generation and commits one terminal before closing.

### Interaction allow, deny, or timeout

The existing JSONL opaque/direct interaction routes remain authoritative.
Allow, deny, and unavailable settlement resume or terminalize the same typed
operation. A transport route is never replaced by a controller-local prompt.
An unresolved route at connection shutdown is retired before host shutdown.

### Provider retry

DeepSeek retry remains inside the runtime generation and retains the same
operation and generation ownership. It does not create a second ephemeral
thread. A final provider failure commits one typed terminal with a bounded,
redacted diagnostic, projects one `error` frame before `turn_completed`, and
closes the one-shot thread. Errors that trigger compaction retry do not commit
a terminal diagnostic before the retry decision.

### Workflow lifecycle

Stateless JSONL retains the released behavior of waiting for workflows launched
by the foreground turn. The workflow start, completion, failure, cancellation,
and result are committed through a typed workflow lifecycle ingress before the
parent operation terminal. The discarded compatibility writer is not an
internal event source, and the adapter never parses an `EventEnvelope` payload
to reconstruct workflow state.

The workflow ingress uses the active generation fence and returns a typed task
and workflow revision receipt. The actor validates that fence and commits the
existing `TaskPatch` and `WorkflowPatch` facts. Workflow facts remain
thread-scoped, as required by the reducer, and carry
`SurfaceWorkflowFence.parent` so an adapter can project only facts owned by its
operation. Completing an agent-tool workflow never finalizes the parent
operation; the normal generation terminalizer remains the only owner of that
terminal.

Workflow waiting observes the generation cancellation token. Cancellation asks
the workflow task to stop, joins the worker, commits one typed cancelled or
failed workflow terminal, and only then allows generation shutdown to finish.
Until a true background handoff with a `SurfaceBackgroundFence` exists, a typed
surface turn may not silently launch a `wait_for_background_workflows=false`
workflow.

### Projection write failure

The projection worker returns the original I/O error and its ephemeral close
guard requests the runtime thread close barrier. The runtime cancels and joins
the generation and background work. The worker itself is joined by adapter
pruning or connection shutdown.

### EOF or server shutdown

Ingress closes first. Routes and compatibility services settle next. The sole
runtime host shutdown then cancels, awaits, terminalizes, and joins every actor,
including in-flight ephemeral one-shots. No stateless controller loop exists
outside that barrier.

For a clean stdin EOF, each already-admitted ephemeral one-shot keeps an
explicit completion-on-clean-EOF transport policy. The server waits for those
one-shots to publish their typed terminals; recorded thread turns retain the
normal connection-close cancellation policy and do not extend that wait. There
is no guessed wall-clock grace period: provider, tool, turn-budget, and runtime
operation bounds remain the owners of execution deadlines.

The wait rechecks permission and direct-interaction routes. If an ephemeral
turn begins waiting for client input after EOF, the server stops waiting and
enters the normal host shutdown barrier, which cancels, terminalizes, joins, and
reclaims the operation. The legacy wire terminal may be `failed` when the
unreachable interaction itself resolves as a foreground failure. This preserves
released pipe/`execFile` submit behavior without allowing an unreachable
interaction waiter or persistent thread turn to keep the sole-connection server
alive.

### Process crash and restart

Ephemeral state is intentionally lost. No history, surface WAL, catalog record,
owner epoch, or recovery receipt exists, so restart cannot expose or replay the
operation. This is not reported as recovered work.

## External Compatibility

The following remain byte-compatible after dynamic identity normalization:

- `submit` and unbound `turn/start` request shapes;
- `turn_started`, item stream, legacy deltas, and `turn_completed` event shapes;
- correlated error shape;
- absence of `thread_started` for stateless requests.

Provider failure diagnostics retain their existing event ordering and useful
text, but keyed and standalone secret values are redacted before projection.
This is an intentional security tightening; clients must not depend on raw
provider credentials appearing in error payloads.

The only intentional scheduling change is that the server read loop is no longer
blocked inside a second controller run. Multiple request ids may make progress
under the existing ordered writer, as thread-bound JSONL turns already do.

The additive runtime start and receipt types are part of the unstable typed
surface. The stable `orca_runtime::surface` export remains unchanged unless a
test requires exposing `ThreadPersistence` for snapshot inspection; any such
addition is additive.

### Runtime workflow-draft facade

`cargo-semver-checks 0.49.0` against `origin/main@d6f98b0ac` found that this
slice replaced the public `RuntimeWorkflowDraftRequest.session_id` field with
`task_registry`. Because downstream callers can construct this public struct
with a literal, the replacement is a major Rust API break. It is unintended.

The released public request shape remains exactly `workflows_enabled`, `cwd`,
`session_id`, and `max_concurrent_agents`. Its existing workspace-local draft
storage behavior remains compatible. Runtime-internal dispatch uses a
crate-private entry point that supplies the already-owned `TaskRegistry`, so
ephemeral stateless execution still receives process-local workflow artifact
storage. Both entry points delegate to one draft creation implementation; they
do not create a second workflow state machine or fact source.

## Migration And Temporary State

1. Add receipt, in-memory ledger, owner backend, and focused unit tests.
2. Add explicit ephemeral host start and runtime-host behavior tests.
3. Add stateless adapter routing and cleanup tests.
4. Switch the production processor to the new path.
5. Delete `run_submit` and the direct controller call in the same commit.
6. Run compatibility, lifecycle, workspace, and Rust API gates.

During steps 1 through 3, production still uses `run_submit`; that is a local
uncommitted migration state only. No commit may retain both paths as accepted
production owners.

## Rollback

The slice is one semantic commit. Reverting it restores the previous synchronous
stateless controller path without changing persisted data, because the new
ephemeral surface creates no durable artifacts. The preserved old branch and
worktree are not part of rollback.

## Acceptance Criteria

- Production contains no `server::run_submit` or stateless direct controller
  invocation.
- An unbound submit creates one ephemeral, noncatalogued runtime thread and one
  nonreplayable typed operation; its canonical logical turn id drives control.
- Success, rejection, provider failure, interrupt, projection failure, EOF, and
  host shutdown leave zero available ephemeral actors and zero unjoined
  projection or workflow workers.
- Ephemeral execution writes no session, archive, task-session, workflow,
  automatic-memory, owner-lock, or surface-ledger artifact under `ORCA_HOME` or
  the workspace. User-authorized tool side effects remain permitted.
- Workflow start, result, completion, failure, and cancellation are committed as
  typed task/workflow facts before the parent operation terminal and preserve
  the released JSONL lifecycle events.
- The JSONL projector associates thread-scoped workflow facts by their typed
  parent fence and cannot cross-project concurrent operations.
- EOF closes ingress, emits no post-trigger frames, cancels and joins active
  work, and returns within the supervisor deadline.
- The v0.2.50 stateless fixture remains byte-compatible after deterministic
  identity normalization, including event order and trailing newlines.
- Focused runtime, surface, JSONL interaction, workflow, and shutdown tests pass;
  the raw single-threaded workspace gate, non-strict Clippy, package-level Rust
  API checks, and real DeepSeek smoke pass against the final `origin/main`
  merge base. Strict Clippy's existing `orca-core` warning baseline is recorded
  separately and is not represented as a pass.
- `RuntimeWorkflowDraftRequest` remains source-compatible with `origin/main`,
  while internal stateless workflow drafts still use the runtime-owned
  `TaskRegistry` artifact policy.
- A stateless submit that takes longer than two seconds after clean stdin EOF
  still completes successfully, while recorded turns and interaction-blocked
  one-shots retain the unified cancellation-and-join shutdown path.
- The slice is one semantic commit and is not merged, pushed, released, or used
  to delete either preserved worktree in this task.

1. A history-disabled explicit one-shot starts with
   `ThreadPersistence::EphemeralNonCataloguedOneShot` and an ephemeral cursor.
2. Its commits use monotonically increasing live revisions and typed ephemeral
   receipts; no durable receipt is fabricated.
3. A stateless mock submit produces the released normalized JSONL fixture with no
   `thread_started` event.
4. Session listing and the isolated `ORCA_HOME` contain no record for the
   ephemeral id.
5. Success, provider failure, NotAdmitted, projection write failure, EOF, and
   host shutdown leave zero live ephemeral actor and zero unjoined projection
   worker.
6. The connection supervisor remains the sole shutdown rail.
7. `server::run_submit` and the production call to
   `controller::run_to_writer_with_options` are absent.
8. Focused tests, full workspace tests, non-strict Clippy, formatting, diff
   checks, and the Rust API gate pass; the strict warning-baseline failure is
   preserved with its exact diagnostic.

## Verification Commands

```bash
cargo test --locked --offline -p orca-runtime --test runtime_surface_commit
cargo test --locked --offline -p orca-runtime --test runtime_surface_host
cargo test --locked --offline -p orca-runtime --lib server::tests:: -- --test-threads=1
cargo test --locked --offline --test jsonl_surface_differential
cargo test --locked --offline --test session_server_contract
cargo test --locked --offline -p orca-runtime --test jsonl_surface_routing
cargo test --locked --offline --workspace -- --test-threads=1
cargo clippy --locked --offline --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo-semver-checks check-release --manifest-path Cargo.toml -p orca-core --baseline-rev origin/main --release-type patch --color never
cargo-semver-checks check-release --manifest-path Cargo.toml -p orca-runtime --baseline-rev origin/main --release-type patch --color never
cargo-semver-checks check-release --manifest-path Cargo.toml -p orca-tui --baseline-rev origin/main --release-type patch --color never
```
