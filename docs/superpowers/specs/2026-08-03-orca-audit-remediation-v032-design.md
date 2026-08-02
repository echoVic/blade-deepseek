# Orca Audit Remediation v0.3.2 Design

Date: 2026-08-03
Status: approved for specification
Target release: v0.3.2
Scope: remediate every confirmed defect, performance cliff, missing contract gate,
test gap, and planned architectural debt item from the 2026-08-03 comprehensive
Orca audit before publishing the next patch release

## Objective

Bring Orca's runtime, TUI, provider/tool layering, and release controls to a
state where the audit findings are fixed by behavior and enforced by durable
tests or CI gates. The work ships as ordered, independently verifiable commits
on one release branch. No intermediate public release is produced: v0.3.2 is
published only after the complete scope passes the release evidence matrix.

The local Codex, Claude Code, and Grok Build repositories may inform bounded
actor calls, cancellation, process ownership, and session fencing. They are
references rather than source templates. Orca's final design remains native to
Rust, Tokio, its typed runtime surface, and the DeepSeek API.

## Current Baseline

The audit originally described an unsubmitted session-lifecycle branch. That
state has since changed: commit `c8d069c57` is `main`, `origin/main`, and the
published v0.3.1 tag. GitHub and npm both expose v0.3.1. The remediation is
therefore a forward patch release, not a rewrite of an unpublished branch.

The following findings remain directly reproducible at v0.3.1:

- `GoalRuntimeHandle::request` waits on an unbounded synchronous receive.
- the host supervisor executes JSONL session-store operations in its async loop;
- runtime-surface streaming rebuilds accumulated text for every delta;
- the session-lifecycle fork, rename, and picker paths retain the reviewed
  projection and action-availability gaps;
- the runtime-surface contract validator fails on the current manifest review
  metadata, its own test suite fails while scanning current Rust syntax, and no
  required workflow runs it;
- the TUI still depends directly on `orca-provider` despite no source use;
- source-text assertions, broad surface exports, `ThreadActor` concentration,
  provider/tool dependency inversion, and duplicated MCP ownership remain.

Other audit claims are treated as hypotheses until their regression test has
demonstrated the current behavior. A finding that cannot be reproduced must
still end with an explicit evidence record explaining why no production change
was required.

## Delivery Strategy

The branch is organized as coherent slices in dependency order:

1. concurrency and cancellation correctness;
2. streaming and TUI performance correctness;
3. session lifecycle and event-generation correctness;
4. executable contract and CI recovery;
5. architectural test migration;
6. runtime-surface and `ThreadActor` boundary extraction;
7. provider/tool and MCP ownership correction;
8. documentation, versioning, release, and public verification.

Each slice begins with a failing behavioral or boundary test, makes one scoped
change, and returns the relevant focused suite to green before the next slice.
The complete branch remains bisectable. Architectural moves must not be bundled
with unrelated behavior changes.

## Runtime Concurrency

### Goal actor calls

The goal actor remains the serialized owner of goal state and SQLite access,
but its callers no longer block a Tokio runtime worker.

- Goal requests use an async reply channel at runtime call sites.
- The goal actor's synchronous SQLite work runs on a dedicated blocking worker
  boundary. Its single-owner command ordering is preserved.
- Every request has a named, bounded response deadline. Timeout returns a typed
  `GoalActorError::Timeout` and never aliases actor closure.
- A timed-out caller does not cancel or roll back a command that the serialized
  actor may already have committed. Commands that can be retried expose or reuse
  stable request identities so a timeout cannot create duplicate mutations.
- Shutdown and actor failure settle outstanding callers instead of leaving them
  parked indefinitely.

The timeout is a responsiveness bound, not a substitute for moving blocking
work off the async executor. SQLite's five-second busy timeout remains a store
policy and must not occupy the thread actor or host-supervisor loop.

### Host-supervisor store calls

All `SessionStore` filesystem and JSONL operations in `run_host_supervisor` use
one typed `spawn_blocking` adapter. This includes replacement-scope metadata,
list, search, read, list-turns, list-items, and update-metadata operations.

The adapter:

- owns all values moved to the blocking closure;
- maps join failures into the error type declared by that `HostCommand` reply;
- sends exactly one reply for success, storage failure, or join failure; and
- leaves the supervisor free to dispatch other host commands.

Session listing retains its public pagination contract. Storage discovery may
scan metadata internally when required for global ordering, but the picker and
JSONL API must request a finite page and must not materialize message bodies for
sessions outside that page. A fixture with thousands of session files proves
bounded reply size and host responsiveness while listing is in flight.

## Cancellation Ownership

Esc maps to one runtime-owned foreground-operation cancellation command.
Cancellation is defined over the admitted operation, not over whichever token
the TUI can currently reach.

The runtime cancellation path:

- cancels provider generation;
- cancels pending approval, permission, input, and elicitation waits;
- requests stop for the foreground operation's task subtree, including async
  subagents and descendant processes;
- passes a cooperative cancellation token into in-process tools; and
- records terminal cancelled state exactly once.

Task-tree cancellation is scoped by operation ownership. Background work that
was launched independently and is intentionally detached is not stopped by a
foreground Esc. The TUI reports detached work explicitly when it remains.

`web_search` moves from `reqwest::blocking` to the existing async HTTP/runtime
tool path and races response completion against its cancellation token. Its
network timeout remains a fallback bound, while Esc settles the tool promptly.
Tests use a local server that deliberately withholds a response; no public
network service is required.

Cancellation is idempotent and safe during all operation phases. Repeated Esc,
late tool completion, and child exit after cancellation cannot create a second
terminal event or apply stale file changes.

## Streaming And TUI Performance

### Appendable runtime-surface text

`DisplayText` gains crate-private append semantics. `AssistantPatch::Delta`
continues to validate its offset against the current UTF-8 byte length, then
appends with `String::push_str`. The public event and persisted protocol remain
unchanged.

Both the authoritative runtime reducer and TUI surface projection use the same
append behavior. Tests cover Unicode offsets, duplicate/out-of-order rejection,
replay equivalence, and a deterministic allocation/work counter showing that N
deltas perform O(total text) copied work rather than O(N squared) work.

### Tool-call lookup

`AppState` owns a `HashMap<tool_call_id, message_index>` for live ToolCall
projection. Every message mutation path that inserts, removes, truncates,
retains, reloads, or replaces transcript state updates or rebuilds the index.
Debug/test invariants compare it with a linear scan. Duplicate IDs retain the
existing canonical winner rule.

### Usage sequencing

Usage is latest-authoritative state ordered by the runtime-surface cursor or a
typed usage revision. The TUI no longer merges usage fields with `.max()`.
Older events are ignored; a later compaction event may lower current-context
tokens while lifetime input, output, cache, and cost counters continue to obey
their own cumulative semantics.

### Search and reflow

Transcript search recomputes only when its query or searchable-content
generation changes. Spinner frames and unrelated projection updates do not
rescan the transcript. Streaming changes invalidate only affected searchable
entries before merging the result set.

Width, theme, syntax-theme, and force-expand changes schedule a reflow rather
than rebuilding every message in one frame. Reflow consumes a bounded time or
entry budget per frame, prioritizes the visible viewport, and keeps an anchor
to prevent scroll jumps. Until an off-screen entry is rebuilt, the previous
cache remains usable. Deterministic tests prove bounded per-frame visits and
eventual convergence with a full rebuild.

## Session Lifecycle Correctness

### Typed session attachment

The TUI controller owns a monotonically increasing `SessionAttachmentId`.
Every event that can mutate session-specific transcript, workflow, goal,
metadata, usage, or operation state carries the attachment ID assigned when its
runtime thread is attached. `AppState` accepts only the active attachment.

This is an in-process presentation fence. Durable runtime-surface cursors remain
the ordering authority within a session.

### Session switch transaction

New, resume, and fork share one switch procedure:

1. validate that switching is allowed;
2. start and validate the replacement runtime;
3. allocate and install its attachment ID;
4. reset transcript and all session-scoped transient projections;
5. replay the replacement history snapshot;
6. publish identity and runtime-ready state for the new attachment; and
7. shut down the previous runtime after the replacement is authoritative.

If startup fails, the original runtime and projection remain authoritative. If
old-runtime shutdown fails after the swap, the new session remains active and
the old runtime is explicitly reaped; stale events from it are fenced out.

A picker fork therefore cannot leave the source transcript visible while the
identity points at the child session. Tests inject source-session workflow and
usage events after a switch and prove that they are discarded.

### Rename transaction

Current-session rename is a runtime-owned metadata operation:

- read the current surface metadata revision;
- submit `SessionMetadataPatch::SetTitle` with an exact revision precondition;
- persist the same title through the runtime host's session-store boundary;
- project success only after both authoritative updates complete; and
- update the active identity, terminal title, and matching picker row from the
  committed result.

Failure never reports the new title as committed. A partial failure either
completes by retrying the missing idempotent side or restores the previous
surface title with a revision-checked compensating mutation before returning a
typed error. `announce_runtime_ready` must read the committed metadata and
cannot roll the title back.

### Picker availability and coverage

Archive and Delete are absent from the action list for the active session. The
user never enters a destructive confirmation flow that is guaranteed to fail.
The captured session ID remains the mutation target through filtering and async
refreshes.

Rendering and behavior tests cover Browsing, Actions, Renaming,
ConfirmArchive, and ConfirmDelete at compact and normal terminal sizes. Durable
history tests use an isolated Orca home to cover fork, rename, archive, delete,
source preservation, and current-session rejection.

## Executable Runtime-Surface Contract

The contract validator becomes a normal repository gate instead of a manual
artifact.

- Repair the Rust scanner so current cfg/test syntax is parsed without an
  unterminated-body failure. The scanner's fixtures include the construct that
  failed at v0.3.1.
- Synchronize current action rows and `closed_inventory`. Rust boundary tests
  read both sections and compare their exact sets.
- Replace commit-message SHA trailers as the continuing trust mechanism with a
  checked-in canonical contract digest file generated from the private
  contract, manifest, and implementation plan. CI validates the digest from the
  checkout, so legitimate manifest evolution does not require rewriting Git
  history or making the latest manifest commit carry hidden metadata.
- The validator and its unit tests run in pull-request/main CI and the release
  workflow before workspace tests or packaging.
- Contract updates are atomic: private contract, manifest, plan, digest, Rust
  inventory test, and validator fixtures change together.

The contract asserts public ownership and allowed dependency paths. It must not
freeze incidental function spelling or implementation layout.

## Architectural Test Migration

The v0.3.1 tree contains 282 `include_str!` call sites in the inspected Rust
test surfaces: 254 in `orca-runtime/src/lib.rs` and 28 in TUI/tests. This is the
recorded migration baseline, not an assumption that every call site is invalid.
Every `include_str!` assertion that inspects Rust source is classified as one of:

- behavioral invariant: replace with a unit/integration/differential test;
- dependency or import boundary: replace with `cargo metadata`, compile-fail,
  visibility, or AST inventory evidence;
- generated/protocol fixture: retain only when exact bytes are the public
  contract and document why;
- obsolete duplication: delete after equivalent coverage is identified.

The migration is complete when no Rust source-text assertion remains merely to
enforce which private function contains a string. Exact fixture inclusion for
JSONL, JavaScript host code, or other public byte contracts is not removed
mechanically.

This gate precedes large file moves so refactoring failures indicate behavior or
boundary regressions rather than stale text snapshots.

## Runtime-Surface Boundaries

`runtime_surface/mod.rs` replaces broad sibling globs with explicit
module-local imports and explicit exports. `orca-runtime::surface` remains the
curated public facade. `unstable_surface` becomes an internal migration module
with an allowlist that shrinks to zero.

Server consumers migrate by capability:

- read-only snapshot and subscription types;
- typed command/request handles;
- interaction response types;
- operation and task control; and
- ACP/JSONL adapters.

No server module may import the full internal surface namespace. CI checks the
allowlist and dependency graph. Once all consumers use the facade,
`unstable_surface` is removed.

## ThreadActor Decomposition

`ThreadActor` remains the single-threaded owner of live thread state. The work
does not introduce mutex-based shared mutation. Instead, four existing state
machine boundaries become owned components that are driven by the actor loop:

1. `RuntimeCapabilityController`: ACP file/terminal calls, permission/input
   waits, capability availability, and terminal lifecycle;
2. `GoalOperationController`: goal commands, continuation state, goal turn
   context, and goal terminalization;
3. `BackgroundOperationController`: background task/workflow admission,
   completion, stop, and the fields currently named `pending_*` for background
   work;
4. `SurfaceCommitController`: commit preparation, settlement, retry,
   terminalization, and commit-waiter ownership.

Each component owns its pending state and exposes a small command/event API.
The actor loop selects commands and task completions, delegates to one
component, then applies returned effects to canonical thread/session state.
Components do not retain `&mut ThreadActor`, call TUI code, or write storage
outside their declared runtime boundary.

Extraction proceeds one component at a time. Before and after each move,
differential runtime-surface tests replay the same command/event trace and
compare snapshots, emitted commits, terminal outcomes, and durable records.
The target is to reduce the main `ThreadActor` implementation to orchestration
and cross-component invariants, approximately eight thousand production lines,
without setting a line count as a correctness condition.

## Provider And Tool Layering

The dependency direction becomes:

```text
orca-core <- orca-tools <- orca-runtime -> orca-provider
     ^                              |
     +------------------------------+
```

`orca-provider` no longer depends on `orca-tools` and no longer recognizes
concrete Orca tool names in HTTP transport code.

- Generic provider wire types and DeepSeek request/response adaptation stay in
  `orca-provider`.
- Canonical tool definitions, schemas, and argument normalization live in
  `orca-tools` using provider-neutral JSON-schema/value types.
- `orca-runtime` selects the active tool policy, asks `orca-tools` for canonical
  schemas/normalized arguments, and passes provider-neutral definitions to the
  provider adapter.
- DeepSeek-specific limitations are represented as provider capabilities or a
  generic schema-lowering pass. They do not branch on `web_search`,
  `update_plan`, or any other concrete tool name.
- System-prompt assembly that describes available tools is runtime-owned and
  consumes the same canonical registry used for request schemas.

Dependency tests run `cargo metadata` and fail if `orca-provider` reaches
`orca-tools`. Contract tests send representative built-in, MCP, external, goal,
and child-agent schemas through DeepSeek lowering and prove semantic parity.

## TUI Dependency And MCP Ownership

The unused `orca-provider` dependency is removed from `orca-tui`.

Runtime startup becomes the only owner that constructs the root
`McpRegistry`. The TUI supplies configuration and consumes a typed runtime
handle plus projected connection state; it does not create a registry and pass
it through presentation call chains. Child-agent and workflow registries remain
runtime-owned derivatives with explicit scope.

Tests prove one initialization per hosted root session, consistent startup
warnings, session-switch replacement, and shutdown of the previous registry's
connections.

## State Consistency

The runtime-surface reducer remains the canonical presentation state. TUI-only
fields are limited to interaction state such as selection, modal phase, and
render caches. Any shadow field retained for performance has:

- a named derivation from a canonical snapshot;
- one update function;
- a debug/test consistency assertion; and
- reset behavior tied to `SessionAttachmentId`.

This requirement covers usage, task/workflow summaries, current identity,
tool-call lookup, goal projection, and operation state. The branch may extract
these assertions incrementally with their owning slice, but release requires
the full consistency suite.

## Documentation

The branch updates:

- this design and its implementation plan;
- an audit-remediation evidence report mapping every finding to code and tests;
- ADR-0005 or a successor ADR for behavioral boundary enforcement and
  `ThreadActor` components;
- runtime-surface private-contract documentation and manifest;
- session lifecycle and cancellation documentation;
- architecture/dependency diagrams for provider, tools, runtime, and MCP;
- `docs/release-process.md` with the new contract/CI gates;
- English and Chinese README text for user-visible cancellation or session
  behavior changes;
- `docs/releases/v0.3.2.md`; and
- website release/changelog data.

Documentation distinguishes measured facts from design targets and records any
known limitation that remains intentionally outside the audit scope.

## Verification Matrix

Every audit item must appear in the evidence report with one of these proofs:

- a regression test observed failing before its fix and passing afterward;
- a deterministic performance/work-bound test or benchmark;
- a compile/dependency/import boundary test;
- a durable-store integration test;
- an explicit non-reproduction record with the commands and current evidence;
  or
- public release verification for distribution requirements.

The minimum local release candidate gates are:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
node --test scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-windows-platform-boundaries.mjs
cargo nextest run -p orca-tui --lib --locked --profile ci-serial
cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast
node scripts/release/test-verify-version-sync.mjs
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
npm --prefix site run build
npm --prefix site run check:seo
git diff --check
```

Focused stress and regression tests additionally prove:

- host command dispatch while goal SQLite and session listing are blocked;
- cancellation latency for web search and an async subagent process tree;
- linear streaming accumulation work;
- indexed ToolCall equivalence after transcript mutations;
- usage reduction after compaction with stale-event rejection;
- bounded reflow/search work and eventual convergence;
- fork/resume transcript replacement and stale attachment fencing;
- rename revision conflicts and partial-failure recovery;
- all picker phases and durable archive/delete behavior;
- explicit surface exports and zero `unstable_surface` consumers;
- component-level ThreadActor trace equivalence;
- absence of the provider-to-tools dependency; and
- single runtime ownership of root MCP initialization.

GitHub Actions must pass the existing native Windows x64 full suite and ARM64
behavior/build gates in addition to Linux/macOS jobs. A macOS-only run or cross
compile is not sufficient release evidence.

## Release Procedure

After all code and documentation gates pass on the release candidate:

1. update `Cargo.toml`, `Cargo.lock`, and `npm/orca/package.json` to `0.3.2`;
2. update site release metadata and bilingual changelog summaries;
3. create and verify `docs/releases/v0.3.2.md`;
4. rerun the complete local release candidate matrix;
5. push the reviewed branch, integrate it to `main`, and verify required CI;
6. create and push tag `v0.3.2` without force;
7. monitor the release workflow until all six native archives, checksum files,
   the GitHub Release, six platform npm packages, and root npm package exist;
8. run `scripts/release/verify-published.mjs` for `0.3.2`; and
9. independently query GitHub and npm, install the public package in a clean
   temporary directory, and run `orca --version`.

Tag creation alone is not completion. A failed or partial publish is repaired
and reverified before the goal is marked complete.

## Out Of Scope

The work does not redesign the proven runtime turn pipeline, replace Orca's
single-owner actor model with shared locks, change the persisted streaming
protocol, add a second model backend, or mechanically copy another agent's
implementation. Those would expand the product beyond the audit remediation.
