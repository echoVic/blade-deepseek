# Orca Audit Remediation v0.3.2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix every defect, performance cliff, contract gap, architectural debt item, and release deliverable in the 2026-08-03 Orca audit, then publish and independently verify v0.3.2 on GitHub and npm.

**Architecture:** Preserve Orca's single-owner runtime and typed surface while moving blocking work outside Tokio actors, making cancellation operation-owned, fencing TUI events by session attachment, and replacing syntax snapshots with behavioral boundaries. Extract ThreadActor one state machine at a time, invert provider/tool dependencies through provider-neutral schemas, and release only after the complete evidence matrix is green.

**Tech Stack:** Rust 2024, Tokio, crossbeam/std channels, rusqlite, reqwest, ratatui, Node.js contract validators, GitHub Actions, npm multi-platform packages.

---

## File Map

Runtime correctness:

- crates/orca-runtime/src/goal_actor.rs: bounded goal actor replies.
- crates/orca-runtime/src/runtime_host.rs: async completions, blocking store dispatch, operation cancellation, actor delegation.
- crates/orca-runtime/src/runtime_surface/identity.rs and reducer.rs: appendable streaming text.
- crates/orca-runtime/src/tasks.rs and runtime_tool_call.rs: operation-owned cancellation.

Tool and provider boundaries:

- crates/orca-tools/src/registry.rs and web_search.rs: cooperative tool cancellation.
- crates/orca-tools/src/schema.rs: provider-neutral tool definitions and normalization.
- crates/orca-provider/src/deepseek_http.rs and tool_schema.rs: generic DeepSeek lowering.
- crates/orca-runtime/src/tool_invocation.rs: runtime-owned tool policy.

TUI correctness and performance:

- crates/orca-tui/src/types.rs: ToolCall index, usage sequencing, attachment fence, consistency assertions.
- crates/orca-tui/src/app.rs: session transaction, rename, cancellation, MCP startup.
- crates/orca-tui/src/surface_projection.rs: attachment-scoped projection.
- crates/orca-tui/src/session_picker_actions.rs and ui.rs: action filtering and rendering.
- crates/orca-tui/src/transcript_search.rs and transcript_view.rs: bounded search and reflow.

Contracts and architecture:

- scripts/validate-runtime-surface-contract.mjs and its tests: parser, digest, inventory, export gates.
- crates/orca-runtime/src/runtime_surface/mod.rs and its eleven sibling modules: explicit exports and imports.
- crates/orca-runtime/src/server/connection_supervisor.rs, direct_interaction_adapter.rs, opaque_permission_router.rs, surface_adapter.rs, four processor modules, and crates/orca-runtime/src/acp/agent.rs and supervisor.rs: curated surface facade consumers.
- crates/orca-runtime/src/runtime_actor: capability, goal, background, and commit controllers.
- tests/dependency_architecture_contract.rs: structured dependency gates.

Evidence and release:

- docs/reports/2026-08-03-orca-audit-remediation-evidence.md: finding-to-proof ledger.
- architecture, lifecycle, release, README, release-note, and website documentation.
- Cargo.toml, Cargo.lock, and npm/orca/package.json: v0.3.2 version sync.

---

### Task 1: Establish The Audit Evidence Ledger

**Files:**
- Create: docs/reports/2026-08-03-orca-audit-remediation-evidence.md
- Reference: docs/reports/2026-08-03-architecture-performance-design-review.md
- Reference: docs/superpowers/specs/2026-08-03-orca-audit-remediation-v032-design.md

- [x] **Step 1: Create one row per requirement**

Use this exact table shape:

~~~markdown
| ID | Finding | Baseline evidence | Regression or gate | Fix commit | Final evidence |
|---|---|---|---|---|---|
| C1 | Goal actor synchronous reply can block a Tokio actor | reply_rx.recv() in goal_actor.rs | goal_actor_request_times_out_with_typed_error | Not yet run | Not yet run |
~~~

Use IDs C1-C5 for concurrency/contracts, S1-S6 for session lifecycle, A1-A9 for architecture, and P1-P6 for performance/state. Include every numbered or bulleted audit finding exactly once.

- [x] **Step 2: Verify row uniqueness**

Run:

~~~sh
rg -n '^\| (C|S|A|P)[0-9]+' docs/reports/2026-08-03-orca-audit-remediation-evidence.md
~~~

Expected: every planned identifier appears exactly once and no row is missing.

- [x] **Step 3: Commit**

~~~sh
git add docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "docs(audit): track remediation evidence"
~~~

### Task 2: Bound Goal Actor Replies

**Files:**
- Modify: crates/orca-runtime/src/goal_actor.rs
- Test: crates/orca-runtime/src/goal_actor.rs
- Modify: docs/reports/2026-08-03-orca-audit-remediation-evidence.md

- [x] **Step 1: Write a timeout regression**

Add a test-only delayed command and bounded constructor. Assert:

~~~rust
let error = handle.latest_active().unwrap_err();
assert!(matches!(
    error,
    GoalActorError::Timeout { timeout }
        if timeout == Duration::from_millis(20)
));
~~~

After the delayed command settles, issue another request and prove the actor remains healthy.

- [x] **Step 2: Run RED**

~~~sh
cargo test -p orca-runtime goal_actor_request_times_out_with_typed_error --lib -- --nocapture
~~~

Expected: FAIL because Timeout and the bounded test constructor do not exist.

- [x] **Step 3: Implement the bounded API**

Add:

~~~rust
const GOAL_ACTOR_REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

pub enum GoalActorError {
    Closed,
    Timeout { timeout: Duration },
    Store(String),
    Invalid(String),
    OwnerActive { path: String, message: String },
}

pub struct GoalRuntimeHandle {
    sender: SyncSender<GoalActorCommand>,
    request_timeout: Duration,
}
~~~

Use recv_timeout. Map Timeout to GoalActorError::Timeout and Disconnected to Closed. Do not retry timed-out mutations.

- [x] **Step 4: Run GREEN**

~~~sh
cargo test -p orca-runtime goal_actor_request_times_out --lib -- --nocapture
cargo test -p orca-runtime goal_actor --lib -- --test-threads=1
~~~

Expected: all matching tests pass.

- [x] **Step 5: Update C1 and commit**

~~~sh
git add crates/orca-runtime/src/goal_actor.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "fix(runtime): bound goal actor replies"
~~~

### Task 3: Keep Goal Store Waits Off Tokio Actors

**Files:**
- Modify: crates/orca-runtime/src/runtime_host.rs
- Test: crates/orca-runtime/tests/runtime_host.rs
- Modify: docs/reports/2026-08-03-orca-audit-remediation-evidence.md

- [ ] **Step 1: Write actor responsiveness regression**

Use a test goal handle whose next request blocks on a barrier. Submit the goal command, then issue a read-only thread snapshot and require it to settle within 100 ms.

~~~rust
let snapshot = tokio::time::timeout(
    Duration::from_millis(100),
    thread.read_snapshot(),
).await;
assert!(snapshot.is_ok());
~~~

- [ ] **Step 2: Run RED**

~~~sh
cargo test -p orca-runtime --test runtime_host goal_store_wait_does_not_block_thread_actor -- --nocapture
~~~

Expected: FAIL by timeout while ThreadActor calls the synchronous handle inline.

- [ ] **Step 3: Add typed goal completion events**

Add a bounded Tokio completion channel to ThreadActor::run. Define explicit completion variants for set/edit/clear, pause/resume, preview/commit, and finish/verify. Clone GoalRuntimeHandle, call it in tokio::task::spawn_blocking, and send the typed completion. The command branch must return to select without awaiting the blocking join.

Each completion carries its existing surface request or operation fence. Ignore it after terminalization or fence replacement. Preserve the actor's serialized GoalRuntimeHandle and do not create a second SQLite owner.

- [ ] **Step 4: Run GREEN**

~~~sh
cargo test -p orca-runtime --test runtime_host goal_store_wait_does_not_block_thread_actor -- --nocapture
cargo test -p orca-runtime runtime_surface_goal --lib -- --test-threads=1
~~~

- [ ] **Step 5: Update C1 and commit**

~~~sh
git add crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/tests/runtime_host.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "fix(runtime): isolate goal store waits"
~~~

### Task 4: Move Host-Supervisor Storage Off The Async Loop

**Files:**
- Modify: crates/orca-runtime/src/runtime_host.rs
- Test: crates/orca-runtime/tests/runtime_host.rs
- Test: tests/thread_store_contract.rs

- [x] **Step 1: Write supervisor responsiveness test**

Inject a ThreadStore whose list_threads blocks on a barrier. While blocked, start an ephemeral thread and require startup within 100 ms.

- [x] **Step 2: Run RED**

~~~sh
cargo test -p orca-runtime --test runtime_host session_listing_does_not_block_host_supervisor -- --nocapture
~~~

Expected: FAIL by timeout.

- [x] **Step 3: Add a typed blocking store dispatcher**

Create one helper that accepts a static operation name, a Send closure, and the command's reply sender. It runs the closure in spawn_blocking, maps JoinError into that HostCommand's declared error, and sends exactly one result from a separately spawned async task.

Route replacement-scope metadata, list, search, read, list-turns, list-items, and update-metadata through it.

- [x] **Step 4: Prove bounded picker materialization**

Create 2,000 metadata-only sessions in tests/thread_store_contract.rs. Request the picker page with its finite limit. Assert the page length does not exceed the limit and a test read counter proves transcript message bodies outside the page were not parsed.

- [x] **Step 5: Run GREEN**

~~~sh
cargo test -p orca-runtime --test runtime_host session_listing_does_not_block_host_supervisor -- --nocapture
cargo test --test thread_store_contract bounded_session_page_does_not_materialize_all_transcripts -- --nocapture
~~~

- [x] **Step 6: Update C2 and commit**

~~~sh
git add crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/tests/runtime_host.rs tests/thread_store_contract.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "fix(runtime): isolate host session storage"
~~~

### Task 5: Make Web Search Cooperatively Cancellable

**Files:**
- Modify: crates/orca-tools/src/registry.rs
- Modify: crates/orca-tools/src/web_search.rs
- Modify: crates/orca-runtime/src/runtime_tool_call.rs
- Test: crates/orca-tools/src/web_search.rs

- [ ] **Step 1: Write withheld-response cancellation test**

Start a local TCP server that accepts but never responds. Execute web search with a CancelToken, cancel after accept, and assert it settles within 250 ms as ToolStatus::Cancelled.

- [ ] **Step 2: Run RED**

~~~sh
cargo test -p orca-tools web_search_cancellation_preempts_http_timeout -- --nocapture
~~~

Expected: FAIL because web search is blocking and has no cooperative token.

- [ ] **Step 3: Add the execution context**

Add:

~~~rust
pub struct ToolExecutionContext<'a> {
    pub cwd: &'a Path,
    pub cancel: &'a CancelToken,
    pub task_registry: Option<&'a TaskRegistry>,
}
~~~

Declare ExecutionMode::AsyncCooperative for web_search while preserving the synchronous path for ordinary file tools.

- [ ] **Step 4: Convert web search**

Use reqwest::Client with the existing 25-second fallback timeout. Race send and response body reads against cancel.cancelled() with tokio::select!. Return the canonical cancelled ToolResult, not an HTTP error.

- [ ] **Step 5: Run GREEN and commit**

~~~sh
cargo test -p orca-tools web_search -- --nocapture
cargo test -p orca-runtime runtime_tool_call --lib -- --nocapture
git add crates/orca-tools/src/registry.rs crates/orca-tools/src/web_search.rs crates/orca-runtime/src/runtime_tool_call.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "fix(tools): cancel web search cooperatively"
~~~

### Task 6: Cancel Foreground Subagent Task Trees

**Files:**
- Modify: crates/orca-runtime/src/runtime_host.rs
- Modify: crates/orca-runtime/src/tasks.rs
- Modify: crates/orca-tui/src/app.rs
- Test: tests/subagent_contract.rs

- [ ] **Step 1: Write foreground cancellation regression**

Launch an async subagent helper that records its PID and waits. Send UserAction::Cancel. Assert within two seconds that its task is stopped, its process tree is absent, and one terminal event exists. Launch an independently detached background task and prove it remains active.

- [ ] **Step 2: Run RED**

~~~sh
cargo test --test subagent_contract foreground_cancel_stops_async_subagent_tree_only -- --nocapture
~~~

Expected: FAIL because foreground cancellation only cancels generation.

- [ ] **Step 3: Track operation task ownership**

Associate admitted SurfaceOperationId values with root TaskIds. Add TaskRegistry::request_stop_tree, traversing canonical parent IDs and invoking existing process-tree stop for active descendants.

- [ ] **Step 4: Unify runtime cancellation**

InterruptOperation cancels generation, settles approval/permission/input/elicitation waits, requests stop for operation-owned roots, and commits one cancelled terminal state. Repeated commands return already-terminal state.

- [ ] **Step 5: Run GREEN and commit**

~~~sh
cargo test --test subagent_contract foreground_cancel_stops_async_subagent_tree_only -- --nocapture
cargo test -p orca-runtime cancel_operation --lib -- --test-threads=1
git add crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/src/tasks.rs crates/orca-tui/src/app.rs tests/subagent_contract.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "fix(runtime): cancel foreground task trees"
~~~

### Task 7: Append Streaming Text In Linear Work

**Files:**
- Modify: crates/orca-runtime/src/runtime_surface/identity.rs
- Modify: crates/orca-runtime/src/runtime_surface/reducer.rs
- Test: crates/orca-runtime/tests/runtime_surface_reducer.rs

- [x] **Step 1: Write offset and work-bound tests**

Feed mixed Unicode deltas and 1,000 ten-byte ASCII deltas. Add a cfg(test) appended-byte counter and assert exactly 10,000 bytes of append work.

- [x] **Step 2: Run RED**

~~~sh
cargo test -p orca-runtime --test runtime_surface_reducer assistant_delta_append_is_linear -- --nocapture
~~~

Expected: FAIL because append semantics and work counter do not exist.

- [x] **Step 3: Add append semantics**

Add crate-private DisplayText::push_str using String::push_str. Preserve byte-offset validation, then replace the format-based accumulated-string rebuild with push_str.

- [x] **Step 4: Prove replay equivalence**

Replay the Unicode stream through live and persisted reducers. Compare snapshots exactly and retain duplicate/out-of-order rejection tests.

- [x] **Step 5: Run GREEN and commit**

~~~sh
cargo test -p orca-runtime --test runtime_surface_reducer assistant_delta -- --nocapture
git add crates/orca-runtime/src/runtime_surface/identity.rs crates/orca-runtime/src/runtime_surface/reducer.rs crates/orca-runtime/tests/runtime_surface_reducer.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "perf(runtime): append assistant deltas"
~~~

### Task 8: Index TUI Tool Calls

**Files:**
- Modify: crates/orca-tui/src/types.rs
- Test: crates/orca-tui/src/types.rs

- [ ] **Step 1: Write mutation equivalence tests**

Exercise push, replace, clear, truncate, retain, and history reload. After each operation compare tool_call_message_index with a test-only linear scan and call assert_tool_call_index_consistent.

- [ ] **Step 2: Run RED**

~~~sh
cargo test -p orca-tui tool_call_index_matches_canonical_scan_after_mutations --lib
~~~

Expected: FAIL because index APIs do not exist.

- [ ] **Step 3: Add the index**

Add tool_call_indices: HashMap<String, usize> to AppState. Preserve current duplicate behavior by indexing the first surviving message. Insert unique append IDs incrementally; rebuild once after removals or reorder.

- [ ] **Step 4: Replace hot scans**

Use the index in push_message duplicate detection and ToolCallProgress lookup. Debug/test builds assert consistency after structural transcript mutations.

- [ ] **Step 5: Run GREEN and commit**

~~~sh
cargo test -p orca-tui tool_call_index --lib
cargo test -p orca-tui tool_call_progress --lib
git add crates/orca-tui/src/types.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "perf(tui): index projected tool calls"
~~~


### Task 9: Apply Usage By Sequence, Not Maximum

**Files:**
- Modify: crates/orca-tui/src/types.rs
- Modify: crates/orca-tui/src/surface_projection.rs
- Test: crates/orca-tui/src/types.rs

- [ ] **Step 1: Write compaction and stale-order tests**

Apply revision 10 with 50,000 current-context tokens, revision 11 with 8,000, then revision 9 with 60,000. Assert current context remains 8,000 and lifetime totals equal revision 11.

- [ ] **Step 2: Run RED**

~~~sh
cargo test -p orca-tui usage_update_allows_compaction_drop_and_rejects_stale_revision --lib
~~~

Expected: FAIL because UsageUpdated has no revision and merges with max.

- [ ] **Step 3: Add a sequenced event**

Use:

~~~rust
TuiEvent::UsageUpdated {
    revision: u64,
    usage: UsageTotals,
}
~~~

Add usage_revision to AppState. Apply only newer revisions, assigning the authoritative UsageTotals. Derive revision from the runtime-surface cursor.

- [ ] **Step 4: Run GREEN and commit**

~~~sh
cargo test -p orca-tui usage_update --lib
cargo test -p orca-tui compaction --lib
git add crates/orca-tui/src/types.rs crates/orca-tui/src/surface_projection.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "fix(tui): sequence usage projection"
~~~

### Task 10: Bound Transcript Search And Reflow Work

**Files:**
- Modify: crates/orca-tui/src/transcript_search.rs
- Modify: crates/orca-tui/src/transcript_view.rs
- Modify: crates/orca-tui/src/ui.rs
- Test: same files

- [ ] **Step 1: Write unchanged-frame search test**

Open search on 5,000 messages, render 100 unchanged frames, and assert the scan counter increases once. Append one streaming change and assert only that entry is rescanned.

- [ ] **Step 2: Run RED**

~~~sh
cargo test -p orca-tui transcript_search_ignores_unchanged_render_frames --lib
~~~

Expected: FAIL because ui.rs initiates refresh each frame.

- [ ] **Step 3: Add per-entry search generations**

Track query_revision and indexed message revisions. Query edits rebuild once; transcript changes update matches only for changed revisions. Rendering only consumes prepared matches.

- [ ] **Step 4: Write reflow budget test**

Change width for 5,000 cached messages. With a test budget of 32, assert each prepare visits at most 32 off-screen dirty entries plus visible entries, preserves viewport anchor, and repeated calls converge with an unlimited rebuild.

- [ ] **Step 5: Run RED**

~~~sh
cargo test -p orca-tui transcript_reflow_is_budgeted_and_converges --lib
~~~

Expected: FAIL because width/theme invalidation rebuilds all entries in one call.

- [ ] **Step 6: Implement viewport-first scheduled reflow**

Add a ReflowSchedule carrying generation, pending indices, and viewport anchor. Queue visible entries first. Consume a five-millisecond production budget or deterministic test entry budget. Keep old lines until replaced and adjust scroll from the anchor.

- [ ] **Step 7: Run GREEN and commit**

~~~sh
cargo test -p orca-tui transcript_search --lib
cargo test -p orca-tui transcript_reflow --lib
git add crates/orca-tui/src/transcript_search.rs crates/orca-tui/src/transcript_view.rs crates/orca-tui/src/ui.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "perf(tui): bound search and reflow work"
~~~

### Task 11: Fence TUI Events By Session Attachment

**Files:**
- Modify: crates/orca-tui/src/types.rs
- Modify: crates/orca-tui/src/app.rs
- Modify: crates/orca-tui/src/surface_projection.rs
- Test: crates/orca-tui/src/app.rs

- [ ] **Step 1: Write stale-event injection tests**

Attach A, queue WorkflowTasksUpdated and UsageUpdated for A, switch to B, then deliver A events. Assert B transcript, workflow, usage, goal, identity, and operation state are unchanged.

- [ ] **Step 2: Run RED**

~~~sh
cargo test -p orca-tui stale_attachment_events_do_not_mutate_switched_session --lib -- --test-threads=1
~~~

Expected: FAIL because events have no attachment identity.

- [ ] **Step 3: Introduce the event envelope**

Add:

~~~rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SessionAttachmentId(u64);

pub(crate) struct AttachedTuiEvent {
    pub(crate) attachment: Option<SessionAttachmentId>,
    pub(crate) event: TuiEvent,
}
~~~

Global UI events use None. Runtime/session events use Some(id). The controller allocates monotonically; AppState rejects a non-active attachment.

- [ ] **Step 4: Route every session producer**

Envelope transcript, workflow, goal, usage, interaction, operation, metadata, and runtime-ready producers. Leave only truly session-independent input/UI events unattached.

- [ ] **Step 5: Run GREEN and commit**

~~~sh
cargo test -p orca-tui attachment --lib -- --test-threads=1
cargo test -p orca-tui --lib --no-run
git add crates/orca-tui/src/types.rs crates/orca-tui/src/app.rs crates/orca-tui/src/surface_projection.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "fix(tui): fence events by session attachment"
~~~

### Task 12: Make Session Switching Replace Projection Atomically

**Files:**
- Modify: crates/orca-tui/src/app.rs
- Modify: crates/orca-tui/src/types.rs
- Test: crates/orca-tui/src/app.rs
- Test: tests/history_contract.rs

- [ ] **Step 1: Reproduce picker fork leakage**

Create source and child histories with distinguishable messages, execute ForkSavedSession, reduce emitted events, and assert no source-only message remains after child identity is active.

- [ ] **Step 2: Run RED**

~~~sh
cargo test -p orca-tui picker_fork_replaces_source_transcript --lib -- --test-threads=1
~~~

Expected: FAIL because picker fork emits SessionForked without history replacement.

- [ ] **Step 3: Extract one switch transaction**

Create install_hosted_session. Validate the new snapshot, allocate/install attachment, emit SessionProjectionReset, replay history, publish identity and ready under that attachment, then reap the prior handle. New, resume, current fork, and picker fork all use it.

- [ ] **Step 4: Cover failure behavior**

Failed replacement startup leaves source authoritative. Failed old shutdown keeps the replacement authoritative, fences old events, and hands old cleanup to a reaper.

- [ ] **Step 5: Add durable history coverage**

Prove child ID differs, copied history and title are durable, and source remains unchanged and loadable.

- [ ] **Step 6: Run GREEN and commit**

~~~sh
cargo test -p orca-tui hosted_session_switch --lib -- --test-threads=1
cargo test --test history_contract session_fork -- --test-threads=1
git add crates/orca-tui/src/app.rs crates/orca-tui/src/types.rs tests/history_contract.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "fix(tui): replace projection on session switch"
~~~

### Task 13: Make Rename And Picker Actions Match The Contract

**Files:**
- Modify: crates/orca-tui/src/app.rs
- Modify: crates/orca-tui/src/surface_actions.rs
- Modify: crates/orca-tui/src/session_picker_actions.rs
- Modify: crates/orca-tui/src/ui.rs
- Test: crates/orca-tui/src/app.rs
- Test: tests/history_contract.rs

- [ ] **Step 1: Write rename persistence/projection tests**

Rename a live session, invoke announce_runtime_ready, and assert surface snapshot, disk, AppState, and terminal title retain the new value. Inject disk failure and assert visible title remains old.

- [ ] **Step 2: Run RED**

~~~sh
cargo test -p orca-tui hosted_rename_updates_surface_and_survives_runtime_ready --lib -- --test-threads=1
~~~

Expected: FAIL because rename currently updates only saved storage.

- [ ] **Step 3: Implement revision-checked rename**

Read metadata revision, submit SessionMetadataPatch::SetTitle with Exact precondition, then persist through RuntimeSurfaceHostHandle. On disk failure, use a revision-checked compensating patch restoring the old title. Emit success only after both authorities agree.

- [ ] **Step 4: Filter destructive picker actions**

Make picker actions a pure function of selected and active IDs. Archive/Delete are absent when IDs match, so confirmation cannot be entered.

- [ ] **Step 5: Add phase/status rendering tests**

Render Browsing, Actions, Renaming, ConfirmArchive, ConfirmDelete, and complete/unknown status at 80x24 and 40x12 with TestBackend. Assert expected text and no overflow.

- [ ] **Step 6: Add durable archive/delete coverage**

In isolated Orca home, archive and delete non-active sessions, reload the store, and assert persistence plus captured-ID targeting.

- [ ] **Step 7: Run GREEN and commit**

~~~sh
cargo test -p orca-tui rename --lib -- --test-threads=1
cargo test -p orca-tui session_picker --lib -- --test-threads=1
cargo test -p orca-tui status --lib
cargo test --test history_contract session_archive_delete -- --test-threads=1
git add crates/orca-tui/src/app.rs crates/orca-tui/src/surface_actions.rs crates/orca-tui/src/session_picker_actions.rs crates/orca-tui/src/ui.rs tests/history_contract.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "fix(tui): honor session lifecycle contract"
~~~

### Task 14: Repair And Enforce The Runtime-Surface Contract

**Files:**
- Modify: scripts/validate-runtime-surface-contract.mjs
- Modify: scripts/test-validate-runtime-surface-contract.mjs
- Modify: docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json
- Create: docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json
- Modify: crates/orca-tui/src/surface_boundary_tests.rs
- Create: .github/workflows/runtime-contract.yml
- Modify: .github/workflows/release.yml
- Modify: .github/workflows/windows-ci.yml

- [ ] **Step 1: Add scanner and inventory regressions**

Add the current cfg/test syntax that triggers the unterminated-body failure. Add a fixture with 30 current actions and 23 closed inventory rows and assert validation lists the seven missing actions.

- [ ] **Step 2: Run RED**

~~~sh
node --test scripts/test-validate-runtime-surface-contract.mjs
~~~

Expected: FAIL with the scanner exception.

- [ ] **Step 3: Repair scanner**

Handle attributes, raw strings, nested macro tokens, comments, lifetimes/char literals, and cfg-gated test modules. Each fixture asserts its exact discovered function set.

- [ ] **Step 4: Replace commit-trailer trust**

Create a digest JSON with algorithm sha256 and hashes for the private contract, manifest, and implementation plan. The validator recomputes current checkout files. Remove the requirement that the latest manifest commit carry private/manifest/plan sha trailers.

- [ ] **Step 5: Synchronize inventory**

Add the exact seven current session actions to closed_inventory. Rust boundary tests deserialize current and closed sets and compare exactly.

- [ ] **Step 6: Run GREEN**

~~~sh
node --test scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
cargo test -p orca-tui runtime_surface_contract --lib
~~~

- [ ] **Step 7: Wire workflows**

runtime-contract.yml runs on relevant pull requests/main pushes and executes Node tests, validator, Rust inventory test, and dependency gates. Run the same portable Node gates before tests in release.yml and both Windows jobs.

- [ ] **Step 8: Commit**

~~~sh
git add scripts/validate-runtime-surface-contract.mjs scripts/test-validate-runtime-surface-contract.mjs docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json crates/orca-tui/src/surface_boundary_tests.rs .github/workflows/runtime-contract.yml .github/workflows/release.yml .github/workflows/windows-ci.yml docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "ci: enforce runtime surface contract"
~~~

### Task 15: Replace Rust Source-Text Assertions

**Files:**
- Modify: crates/orca-runtime/src/lib.rs
- Modify: crates/orca-tui/src/surface_boundary_tests.rs
- Modify: tests/cli_architecture_contract.rs
- Create: tests/dependency_architecture_contract.rs
- Modify: behavior-specific runtime test modules

- [ ] **Step 1: Inventory all 282 sites**

Record containing test and classify each as behavior, dependency/import, exact byte fixture, or obsolete. Baseline must show 254 runtime lib sites and 28 TUI/tests sites.

- [ ] **Step 2: Add structured boundary tests**

Use cargo metadata JSON for crate edges and target ownership. Use compile visibility tests for private APIs. Do not replace one source string scanner with another source string scanner.

- [ ] **Step 3: Replace behavioral assertions**

For each source assertion, write or identify a direct behavioral/differential test, run it green against current behavior, then remove the string assertion.

- [ ] **Step 4: Retain only exact byte fixtures**

Keep include_str only where fixture bytes are consumed as a public contract, such as JSONL or workflow JavaScript. Add a comment naming that contract. No retained test asserts private Rust spelling or file placement.

- [ ] **Step 5: Verify**

~~~sh
rg -n 'include_str!' crates/orca-runtime/src/lib.rs crates/orca-tui/src tests
cargo test --test dependency_architecture_contract -- --nocapture
cargo test -p orca-runtime --lib --no-run
cargo test -p orca-tui --lib --no-run
~~~

Expected: zero Rust source-layout assertions; retained fixture inclusions are documented.

- [ ] **Step 6: Commit**

~~~sh
git add crates/orca-runtime/src/lib.rs crates/orca-tui/src/surface_boundary_tests.rs tests/cli_architecture_contract.rs tests/dependency_architecture_contract.rs crates/orca-runtime/src docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "test(architecture): replace source text assertions"
~~~

### Task 16: Make Runtime-Surface Exports Explicit

**Files:**
- Modify: crates/orca-runtime/src/runtime_surface/mod.rs
- Modify: crates/orca-runtime/src/runtime_surface/commands.rs
- Modify: crates/orca-runtime/src/runtime_surface/commit.rs
- Modify: crates/orca-runtime/src/runtime_surface/host.rs
- Modify: crates/orca-runtime/src/runtime_surface/hub.rs
- Modify: crates/orca-runtime/src/runtime_surface/identity.rs
- Modify: crates/orca-runtime/src/runtime_surface/ingress.rs
- Modify: crates/orca-runtime/src/runtime_surface/interaction.rs
- Modify: crates/orca-runtime/src/runtime_surface/operation.rs
- Modify: crates/orca-runtime/src/runtime_surface/projection.rs
- Modify: crates/orca-runtime/src/runtime_surface/reducer.rs
- Modify: crates/orca-runtime/src/runtime_surface/store.rs
- Test: crates/orca-runtime/tests/runtime_surface_types.rs

- [ ] **Step 1: Add export gate**

Extend the contract validator to compare public exports with the curated manifest. Add a fixture proving pub use commands::* fails.

- [ ] **Step 2: Run RED**

~~~sh
node scripts/validate-runtime-surface-contract.mjs
~~~

Expected: FAIL on wildcard exports.

- [ ] **Step 3: Replace globs**

List exports explicitly in mod.rs. Replace production use super::* imports with exact sibling imports. Test modules may use super::* only when the module itself is the intended test namespace.

- [ ] **Step 4: Run GREEN and commit**

~~~sh
node scripts/validate-runtime-surface-contract.mjs
cargo test -p orca-runtime --test runtime_surface_types
cargo check -p orca-runtime --locked
git add crates/orca-runtime/src/runtime_surface scripts/validate-runtime-surface-contract.mjs scripts/test-validate-runtime-surface-contract.mjs docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json
git commit -m "refactor(runtime): make surface exports explicit"
~~~


### Task 17: Remove unstable_surface Consumers

**Files:**
- Modify: crates/orca-runtime/src/lib.rs
- Modify: crates/orca-runtime/src/server/connection_supervisor.rs
- Modify: crates/orca-runtime/src/server/direct_interaction_adapter.rs
- Modify: crates/orca-runtime/src/server/opaque_permission_router.rs
- Modify: crates/orca-runtime/src/server/processors/mcp_elicitation.rs
- Modify: crates/orca-runtime/src/server/processors/permission.rs
- Modify: crates/orca-runtime/src/server/processors/turn.rs
- Modify: crates/orca-runtime/src/server/processors/user_input.rs
- Modify: crates/orca-runtime/src/server/surface_adapter.rs
- Modify: crates/orca-runtime/src/acp/agent.rs
- Modify: crates/orca-runtime/src/acp/supervisor.rs
- Modify: crates/orca-tui/src/surface_projection.rs

- [ ] **Step 1: Add zero-consumer gate**

The contract validator rejects every production import of unstable_surface. Tests must import the curated surface facade.

- [ ] **Step 2: Run RED**

~~~sh
node scripts/validate-runtime-surface-contract.mjs
~~~

Expected: FAIL listing current production consumers.

- [ ] **Step 3: Migrate by capability**

Expose only named read, subscription, interaction, operation/task control, ACP, and JSONL types needed by a consumer. Migrate read-only clients first, interaction adapters second, mutation/control last. Never replace the import with another glob.

- [ ] **Step 4: Remove the module**

Delete pub mod unstable_surface after production imports reach zero.

- [ ] **Step 5: Run GREEN and commit**

~~~sh
node scripts/validate-runtime-surface-contract.mjs
cargo test -p orca-runtime --tests runtime_surface -- --test-threads=1
cargo test -p orca-tui surface_projection --lib
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/server crates/orca-runtime/src/acp crates/orca-tui/src/surface_projection.rs scripts/validate-runtime-surface-contract.mjs docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "refactor(runtime): close unstable surface access"
~~~

### Task 18: Extract Capability And Goal Controllers

**Files:**
- Create: crates/orca-runtime/src/runtime_actor/mod.rs
- Create: crates/orca-runtime/src/runtime_actor/capability.rs
- Create: crates/orca-runtime/src/runtime_actor/goal.rs
- Modify: crates/orca-runtime/src/runtime_host.rs
- Modify: crates/orca-runtime/src/lib.rs
- Test: crates/orca-runtime/tests/runtime_host.rs

- [ ] **Step 1: Add differential traces**

Create deterministic traces for ACP file/terminal capabilities, interaction settlement, goal set/run, pause/resume, verification, and terminalization. Capture snapshots, commits, terminals, and durable records as typed/in-memory values.

- [ ] **Step 2: Define component effects**

Use explicit effects rather than borrowing ThreadActor:

~~~rust
pub(crate) enum RuntimeActorEffect {
    Commit(SurfaceCommitBatch),
    ReplyCapability(CapabilityReply),
    SpawnBlockingGoal(GoalBlockingRequest),
    Wake,
}

pub(crate) struct RuntimeCapabilityController {
    pending: HashMap<SurfaceCapabilityCallId, PendingCapabilityCall>,
    terminals: HashMap<SurfaceTerminalId, RuntimeTerminal>,
}

pub(crate) struct GoalOperationController {
    runtime: Option<GoalRuntimeHandle>,
    turn: Option<GoalTurnContext>,
    pending: HashMap<SurfaceRequestId, PendingGoalOperation>,
}
~~~

- [ ] **Step 3: Move capability/terminal ownership**

Move the capability/terminal methods and fields. ThreadActor supplies canonical inputs and applies effects. The component does not retain &mut ThreadActor, call TUI, or write outside declared runtime boundaries.

- [ ] **Step 4: Verify capability parity**

~~~sh
cargo test -p orca-runtime --test runtime_host capability_controller_trace_equivalence -- --nocapture
~~~

- [ ] **Step 5: Move goal ownership**

Move goal methods, runtime handle, turn context, and pending completion state while retaining Task 3's blocking boundary.

- [ ] **Step 6: Verify and commit**

~~~sh
cargo test -p orca-runtime --test runtime_host goal_controller_trace_equivalence -- --nocapture
cargo check -p orca-runtime --locked
git add crates/orca-runtime/src/runtime_actor crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/src/lib.rs crates/orca-runtime/tests/runtime_host.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "refactor(runtime): extract capability and goal controllers"
~~~

### Task 19: Extract Background And Commit Controllers

**Files:**
- Create: crates/orca-runtime/src/runtime_actor/background.rs
- Create: crates/orca-runtime/src/runtime_actor/commit.rs
- Modify: crates/orca-runtime/src/runtime_actor/mod.rs
- Modify: crates/orca-runtime/src/runtime_host.rs
- Test: crates/orca-runtime/tests/runtime_host.rs
- Test: crates/orca-runtime/tests/runtime_surface_commit.rs

- [ ] **Step 1: Add traces**

Trace background admission/completion/stop, workflow updates, commit prepare/settle/retry, injected failures, cancellation, and terminalization. Capture typed outputs and durable records.

- [ ] **Step 2: Define owned shapes**

~~~rust
pub(crate) struct BackgroundOperationController {
    pending: HashMap<TaskId, PendingBackgroundOperation>,
    capacity: usize,
}

pub(crate) struct SurfaceCommitController {
    pending: BTreeMap<SurfaceCommitId, PendingSurfaceCommit>,
    terminalization: HashMap<SurfaceOperationId, PendingTerminalization>,
}
~~~

- [ ] **Step 3: Move background ownership**

Move background methods and associated pending fields. TaskRegistry remains canonical; the component owns actor-side pending state and effects.

- [ ] **Step 4: Move commit ownership**

Move commit preparation, waiter settlement, retry, terminalization, and pending fields. Storage writes stay behind the surface commit boundary.

- [ ] **Step 5: Verify parity**

~~~sh
cargo test -p orca-runtime --test runtime_host background_controller_trace_equivalence -- --nocapture
cargo test -p orca-runtime --test runtime_surface_commit commit_controller_trace_equivalence -- --nocapture
~~~

- [ ] **Step 6: Measure and commit**

Record production method/field/line counts in the evidence ledger without making the approximate 8k-line target a correctness gate.

~~~sh
git add crates/orca-runtime/src/runtime_actor crates/orca-runtime/src/runtime_host.rs crates/orca-runtime/tests/runtime_host.rs crates/orca-runtime/tests/runtime_surface_commit.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "refactor(runtime): extract background and commit controllers"
~~~

### Task 20: Correct Provider And Tool Dependency Direction

**Files:**
- Create: crates/orca-tools/src/schema.rs
- Modify: crates/orca-tools/src/lib.rs
- Modify: crates/orca-tools/src/registry.rs
- Modify: crates/orca-provider/src/deepseek_http.rs
- Modify: crates/orca-provider/src/tool_schema.rs
- Modify: crates/orca-provider/Cargo.toml
- Modify: crates/orca-runtime/src/tool_invocation.rs
- Test: tests/dependency_architecture_contract.rs
- Test: tests/provider_contract.rs

- [ ] **Step 1: Add dependency and parity tests**

Assert no orca-provider to orca-tools edge. Build representative root, goal, child, MCP, and external tool definitions; assert DeepSeek JSON preserves names, descriptions, required properties, and strictness.

- [ ] **Step 2: Run RED**

~~~sh
cargo test --test dependency_architecture_contract provider_does_not_depend_on_tools -- --nocapture
~~~

Expected: FAIL on current dependency.

- [ ] **Step 3: Add canonical definitions in tools**

Define:

~~~rust
pub struct CanonicalToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

pub fn canonical_tool_definitions(
    policy: &ToolPolicy,
    registry: &ToolRegistry,
) -> Vec<CanonicalToolDefinition>;

pub fn normalize_tool_arguments(
    name: &ToolName,
    value: Value,
) -> Result<Value, ToolSchemaError>;
~~~

Move built-in schemas and tool-name normalization into these APIs. System prompts consume the same definitions.

- [ ] **Step 4: Make DeepSeek lowering generic**

Provider accepts provider-neutral definitions. Lower unsupported JSON-schema keywords generically, never branching on web_search, update_plan, or other concrete names.

- [ ] **Step 5: Remove dependency**

Delete orca-tools from crates/orca-provider/Cargo.toml. Runtime obtains canonical definitions and passes them to provider.

- [ ] **Step 6: Run GREEN and commit**

~~~sh
cargo test --test dependency_architecture_contract provider_does_not_depend_on_tools -- --nocapture
cargo test --test provider_contract tool_schema -- --nocapture
cargo test -p orca-provider --locked
cargo test -p orca-runtime tool_invocation --lib
git add crates/orca-tools crates/orca-provider crates/orca-runtime/src/tool_invocation.rs tests/dependency_architecture_contract.rs tests/provider_contract.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "refactor: move tool contracts above provider transport"
~~~

### Task 21: Give Runtime Sole Root MCP Ownership

**Files:**
- Modify: crates/orca-tui/Cargo.toml
- Modify: crates/orca-tui/src/app.rs
- Modify: crates/orca-tui/src/mention_search_manager.rs
- Modify: crates/orca-runtime/src/runtime_host.rs
- Test: crates/orca-tui/src/app.rs
- Test: tests/dependency_architecture_contract.rs

- [ ] **Step 1: Add dependency and ownership tests**

Assert TUI has no provider dependency. Instrument MCP construction and assert one root construction per hosted session, replacement on switch, and old connection shutdown.

- [ ] **Step 2: Run RED**

~~~sh
cargo test --test dependency_architecture_contract tui_does_not_depend_on_provider -- --nocapture
cargo test -p orca-tui runtime_owns_root_mcp_registry --lib -- --test-threads=1
~~~

Expected: current dependency and TUI construction fail.

- [ ] **Step 3: Remove unused provider dependency**

Delete orca-provider from TUI Cargo.toml and verify zero TUI imports.

- [ ] **Step 4: Move initialization**

Remove TUI initialize_registry and the registry parameter passed through presentation call chains. RuntimeThreadStartRequest constructs from RunConfig.mcp_servers. RuntimeThreadHandle exposes typed access and projected connection/errors state.

- [ ] **Step 5: Run GREEN and commit**

~~~sh
cargo test --test dependency_architecture_contract tui_does_not_depend_on_provider -- --nocapture
cargo test -p orca-tui runtime_owns_root_mcp_registry --lib -- --test-threads=1
cargo check -p orca-tui --locked
git add crates/orca-tui crates/orca-runtime/src/runtime_host.rs tests/dependency_architecture_contract.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "refactor(runtime): own root MCP registry"
~~~

### Task 22: Enforce Projection Consistency

**Files:**
- Modify: crates/orca-tui/src/types.rs
- Modify: crates/orca-tui/src/surface_projection.rs
- Test: crates/orca-tui/src/types.rs
- Test: crates/orca-runtime/tests/jsonl_surface_differential.rs

- [ ] **Step 1: Add consistency assertion**

Create AppState::assert_surface_projection_consistent covering usage/context, tasks/workflows, identity, tool index, goal, and operation state. Run after every projected batch in test/debug builds.

- [ ] **Step 2: Run RED and record mismatches**

~~~sh
cargo test -p orca-tui surface_projection_consistency --lib -- --nocapture
~~~

Expected: mismatches identify retained shadow-state divergence. Record each before fixing.

- [ ] **Step 3: Centralize derivation**

Derive from the reducer snapshot at read time or update in one named projection function. Reset every session shadow with SessionProjectionReset under the active attachment.

- [ ] **Step 4: Run GREEN and commit**

~~~sh
cargo test -p orca-tui surface_projection_consistency --lib -- --nocapture
cargo test -p orca-runtime --test jsonl_surface_differential -- --test-threads=1
git add crates/orca-tui/src/types.rs crates/orca-tui/src/surface_projection.rs crates/orca-runtime/tests/jsonl_surface_differential.rs docs/reports/2026-08-03-orca-audit-remediation-evidence.md
git commit -m "test(tui): enforce projection consistency"
~~~

### Task 23: Document, Verify, And Publish v0.3.2

**Files:**
- Modify: docs/architecture/adr/0005-runtime-host-operation-control-plane.md
- Modify: docs/superpowers/specs/2026-08-02-tui-session-lifecycle-commands-design.md
- Modify: docs/release-process.md
- Modify: README.md and README.zh-CN.md
- Create: docs/releases/v0.3.2.md
- Modify: site/src/shared.ts and site/src/changelog/Changelog.tsx
- Modify: Cargo.toml, Cargo.lock, npm/orca/package.json
- Finalize: docs/reports/2026-08-03-orca-audit-remediation-evidence.md

- [ ] **Step 1: Update documentation**

Document blocking boundaries, operation-owned cancellation, four actor controllers, explicit facade, provider-neutral tool contracts, runtime MCP ownership, session attachments, and rename/switch transactions. Add contract gates to release process.

- [ ] **Step 2: Finalize evidence ledger**

Replace every `Not yet run` cell with a commit SHA and fresh output summary. Focused latency, durability, dependency, or public proofs cannot be replaced by a broad workspace test.

- [ ] **Step 3: Run pre-version matrix**

~~~sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
node --test scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-windows-platform-boundaries.mjs
cargo nextest run -p orca-tui --lib --locked --profile ci-serial
cargo nextest run --workspace --all-targets --locked --profile ci --no-fail-fast
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
npm --prefix site run build
npm --prefix site run check:seo
git diff --check
~~~

Expected: all exit 0 before versioning.

- [ ] **Step 4: Prepare v0.3.2**

Set Cargo workspace/package, Cargo.lock, and root npm package to 0.3.2. Write release notes with Changes, Compatibility, Verification, and Upgrade. Add v0.3.2 to site releases and both language summary maps.

- [ ] **Step 5: Verify version assets and commit**

~~~sh
cargo check --workspace --locked
node scripts/release/test-verify-version-sync.mjs
node scripts/release/test-stage-npm.mjs
npm --prefix site run build
npm --prefix site run check:seo
git add Cargo.toml Cargo.lock npm/orca/package.json docs README.md README.zh-CN.md site
git commit -m "release: prepare Orca v0.3.2"
~~~

- [ ] **Step 6: Rerun complete release matrix**

Repeat Step 3 after the version commit. Record date, exit, and test counts; commit the final evidence update.

- [ ] **Step 7: Push and integrate**

~~~sh
git push -u origin codex/orca-audit-remediation-v032
gh pr create --title "fix: remediate Orca runtime and architecture audit" --body-file docs/reports/2026-08-03-orca-audit-remediation-evidence.md
~~~

Wait for required Linux and Windows checks, fix failures on the branch, and merge only when green.

- [ ] **Step 8: Tag and monitor**

After local main equals origin/main and contains release commit:

~~~sh
git tag v0.3.2
git push origin v0.3.2
gh run list --repo echoVic/orca-agent --workflow release.yml --limit 5
~~~

Never force or recreate an existing tag. Wait for successful publication.

- [ ] **Step 9: Verify public artifacts**

~~~sh
node scripts/release/verify-published.mjs \
  --version 0.3.2 \
  --repo echoVic/orca-agent \
  --package @blade-ai/orca \
  --bin orca
~~~

Also query gh release view and npm for root plus six platform packages. Install @blade-ai/orca@0.3.2 in mktemp -d and require orca --version to report 0.3.2.

- [ ] **Step 10: Close the evidence report**

Add release URL, workflow run ID, npm timestamps, clean-install output, and final commit/tag SHAs. Completion requires every row and distribution requirement to have direct evidence.
