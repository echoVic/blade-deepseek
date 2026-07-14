# Streaming Mention File Search Implementation Plan

> **For agentic workers:** Execute this plan in order. Keep every stage compiling and passing its
> focused tests before starting the next stage. Do not remove the synchronous fallback until the
> asynchronous TUI path is verified.

**Goal:** Replace synchronous `@` file candidate indexing with a session-scoped streaming search
that remains responsive and cancellable with up to 1,000,000 eligible workspace paths.

**Architecture:** Add a reusable `orca-file-search` crate with a parallel `ignore` walker, a
long-lived `nucleo::Nucleo` matcher, bounded latest-wins commands, coalesced snapshot publication,
owned worker shutdown, and a single warm-idle catalog. Keep mention parsing and workspace-safe file
expansion in `orca-runtime`; add a TUI manager that owns token identity, generation checks, popup
projection, and the lightweight dirty event.

**Tech Stack:** Rust 2024, full Nucleo, nucleo-matcher-compatible smart path matching,
`ignore::WalkBuilder`, bounded channels and shared latest-value slots, ratatui, deterministic
concurrency tests, and a separate million-path performance gate.

---

## Non-Negotiable Contracts

1. No filesystem traversal, Git subprocess, full-catalog clone, or fuzzy scoring runs on the TUI
   input thread.
2. Candidate discovery has no file-count hard cap; it is best-effort exhaustive and bounded by the
   accepted eligibility, error, cancellation, memory, and performance contracts.
3. Each TUI owns at most one catalog and one active mention-search generation.
4. The catalog supports 1,000,000 eligible paths with completed incremental RSS at or below
   512 MiB and construction peak at or below 768 MiB.
5. Walker and matcher compute concurrency never exceeds four workers each.
6. Query, wakeup, snapshot, and TUI dirty-event backlog remain bounded.
7. Cancelled workers are owned and joined; detached search threads are forbidden.
8. Snapshot application validates generation, active token identity/query, and popup pending query.
9. Manual selection is anchored by candidate path across streaming updates.
10. Empty `@` and trailing `/` use asynchronous browse mode; other non-empty queries use global
    fuzzy matching.

## File Map

- `Cargo.toml`, `Cargo.lock`: add the full Nucleo and bounded-channel dependencies and register the
  new crate through the existing `crates/*` workspace membership.
- `crates/orca-file-search/Cargo.toml`: new reusable search crate.
- `crates/orca-file-search/src/lib.rs`: minimal public API exports.
- `crates/orca-file-search/src/types.rs`: generation, query mode, match, phase, progress, and
  snapshot types.
- `crates/orca-file-search/src/session.rs`: one catalog/session lifecycle, latest-wins query slot,
  latest snapshot slot, cancellation, refresh, and owned worker handles.
- `crates/orca-file-search/src/worker.rs`: Nucleo matcher coordinator and coalesced publication.
- `crates/orca-file-search/src/discovery.rs`: parallel workspace traversal and candidate injection.
- `crates/orca-file-search/src/eligibility.rs`: ignore, hidden, VCS metadata, and workspace-safe
  symlink policy.
- `crates/orca-file-search/src/browse.rs`: asynchronous direct-child browse mode.
- `crates/orca-file-search/src/freshness.rs`: canonical root and Git-index fingerprinting.
- `crates/orca-file-search/src/testing.rs`: deterministic fake path source, fake clock, barriers,
  and test reporter helpers behind `cfg(test)` or a test-utils feature.
- `crates/orca-file-search/tests/`: integration coverage for discovery, lifecycle, cancellation,
  ranking, and snapshots.
- `crates/orca-file-search/examples/million_path_bench.rs`: explicit performance and memory gate.
- `crates/orca-runtime/src/mentions.rs`: retain parsing, selection application, content expansion,
  line ranges, and workspace safety; remove synchronous indexing and scoring.
- `crates/orca-tui/src/mention_search_manager.rs`: token ownership, warm-idle controller, dirty-event
  notifier, stale-result guards, and snapshot-to-popup projection.
- `crates/orca-tui/src/mention_menu_actions.rs`: keyboard behavior and selection anchoring over
  accepted snapshots.
- `crates/orca-tui/src/composer_input_actions.rs`: publish token changes without doing search work.
- `crates/orca-tui/src/types.rs`: add the dirty event and richer mention popup state.
- `crates/orca-tui/src/app.rs`: own the manager, intercept dirty events, and consume the latest
  snapshot before generic runtime projection.
- `crates/orca-tui/src/ui.rs`: render loading, progress, incomplete state, matched-character
  highlighting, and stable selection.

## Stage 1: Add The Search Crate Foundation

### Task 1: Add Dependencies And Public Types

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `crates/orca-file-search/Cargo.toml`
- Create: `crates/orca-file-search/src/lib.rs`
- Create: `crates/orca-file-search/src/types.rs`

- [ ] Add a workspace `nucleo` dependency compatible with the selected matcher behavior and a
  bounded channel dependency if the standard library primitives cannot provide non-blocking
  latest-wins semantics cleanly.
- [ ] Define strongly typed `SessionGeneration`, `SearchMode`, `MatchKind`, `SearchMatch`,
  `SearchPhase`, `SearchProgress`, and `SearchSnapshot` values.
- [ ] Keep result limit fixed at 12 and define Unicode character indices explicitly in the type
  documentation.
- [ ] Add equality-focused tests for complete snapshot objects rather than field-by-field tests.

Run:

```bash
cargo test -p orca-file-search types -- --nocapture
cargo check -p orca-file-search
```

### Task 2: Implement Bounded Session Primitives

**Files:**
- Create: `crates/orca-file-search/src/session.rs`
- Create: `crates/orca-file-search/src/worker.rs`
- Create: `crates/orca-file-search/src/testing.rs`

- [ ] Write failing tests proving rapid query updates retain only the latest query.
- [ ] Write failing tests proving multiple worker updates produce at most one pending dirty
  notification and that consuming it returns the newest snapshot.
- [ ] Implement one shared latest-query slot, one shared latest-snapshot slot, and coalesced wakeup
  and dirty flags.
- [ ] Start owned matcher/coordinator workers and retain every join handle in session state.
- [ ] Implement query append detection for Nucleo `reparse(..., append=true)` and full reparse for
  deletion or arbitrary replacement.
- [ ] Suppress publication when top-12 matches and visible status metadata are unchanged.

Run:

```bash
cargo test -p orca-file-search latest -- --nocapture
cargo test -p orca-file-search coalesc -- --nocapture
```

## Stage 2: Add Discovery, Browse, And Lifecycle

### Task 3: Implement Eligibility And Parallel Discovery

**Files:**
- Create: `crates/orca-file-search/src/eligibility.rs`
- Create: `crates/orca-file-search/src/discovery.rs`
- Test: `crates/orca-file-search/tests/discovery.rs`

- [ ] Write a real temporary-tree test containing tracked-style files, untracked-style files,
  hidden entries, ignored entries, VCS metadata directories, internal symlinks, external symlinks,
  and a symlinked directory.
- [ ] Configure `ignore::WalkBuilder` to include hidden entries, respect repository ignore rules,
  exclude VCS metadata, and not recursively follow symlinked directories.
- [ ] Canonicalize symlink targets and emit the link path only when the target remains inside the
  canonical root.
- [ ] Inject eligible paths as they are discovered and report monotonic scanned counts.
- [ ] Check cancellation at least every 10 ms or 256 processed entries.

Run:

```bash
cargo test -p orca-file-search discovery -- --nocapture --test-threads=1
cargo test -p orca-file-search symlink -- --nocapture --test-threads=1
```

### Task 4: Add Browse And Freshness Modes

**Files:**
- Create: `crates/orca-file-search/src/browse.rs`
- Create: `crates/orca-file-search/src/freshness.rs`
- Test: `crates/orca-file-search/tests/browse.rs`

- [ ] Write failing tests for empty-root browse and trailing-slash child browse.
- [ ] Apply the same eligibility and safety policy used by the full walk.
- [ ] Sort browse results with directories first and smart-case lexical order.
- [ ] Resolve Git index paths correctly for normal repositories and worktrees.
- [ ] Represent completed walks as filesystem snapshots; do not add a watcher.

Run:

```bash
cargo test -p orca-file-search browse -- --nocapture
cargo test -p orca-file-search freshness -- --nocapture
```

### Task 5: Complete Cancellation And Warm-Idle Lifecycle

**Files:**
- Modify: `crates/orca-file-search/src/session.rs`
- Test: `crates/orca-file-search/tests/lifecycle.rs`

- [ ] Use an injected clock to prove the catalog is retained before 30 seconds and destroyed at
  the deadline without sleeping.
- [ ] Prove cancellation stops publication within 50 ms and joins all workers within 500 ms on a
  supported local temporary filesystem.
- [ ] Prove a replacement root cannot start until the previous workers reach the cancellation
  terminal state.
- [ ] Prove stale tracked state resets and rebuilds the same catalog rather than allocating a second
  million-path catalog.
- [ ] Allow only watchdog timeouts around deterministic synchronization points.

Run:

```bash
cargo test -p orca-file-search lifecycle -- --nocapture --test-threads=1
cargo test -p orca-file-search cancel -- --nocapture --test-threads=1
```

### Task 6: Add The Million-Path Gate

**Files:**
- Create: `crates/orca-file-search/examples/million_path_bench.rs`
- Optional Create: `scripts/bench-mention-search.sh`

- [ ] Generate 1,000,000 deterministic synthetic relative paths without creating one million real
  filesystem entries.
- [ ] Measure completed incremental RSS, construction peak RSS, first progress, appended-query p95,
  arbitrary-reparse p95, and publication cadence.
- [ ] Fail with a non-zero exit when the accepted 512/768 MiB or 100/50/150 ms thresholds are
  exceeded.
- [ ] Report platform, CPU count, path length distribution, and release/debug build mode.

Run:

```bash
cargo run -p orca-file-search --release --example million_path_bench -- \
  --paths 1000000 --assert-slo
```

## Stage 3: Add TUI Projection Without Cutover

### Task 7: Add Token Identity And The Search Manager

**Files:**
- Modify: `crates/orca-runtime/src/mentions.rs`
- Create: `crates/orca-tui/src/mention_search_manager.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [ ] Expose a pure mention-token parser that returns byte range, query text, quoted state, and a
  stable token identity derived from the active range/generation.
- [ ] Keep file expansion and canonical safety behavior unchanged.
- [ ] Add a TUI manager that owns exactly one search session, advances generation when token/root
  ownership changes, and drives warm-idle transitions.
- [ ] Connect the crate notifier to the existing TUI sender with only
  `MentionSearchDirty { generation }`.

Run:

```bash
cargo test -p orca-runtime mention_token -- --nocapture
cargo test -p orca-tui mention_search_manager -- --nocapture --test-threads=1
```

### Task 8: Add Popup State And Three Stale Guards

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/mention_menu_actions.rs`

- [ ] Replace the string-only candidate state with match, phase, progress, pending query, selected
  path anchor, and manual-navigation state.
- [ ] Intercept dirty events in the app loop, take the latest snapshot, and validate session
  generation before projection.
- [ ] Validate active token identity/query and popup pending query independently.
- [ ] Preserve selected path after manual navigation and clamp only when it disappears.
- [ ] Add deterministic tests for late old-generation, old-token, and old-pending-query snapshots.

Run:

```bash
cargo test -p orca-tui stale_mention -- --nocapture --test-threads=1
cargo test -p orca-tui mention_selection -- --nocapture
```

### Task 9: Render Streaming States And Highlights

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] Render `Searching files…`, `Scanning… N paths`, `No matches`, `Refreshing…`, and
  `Search incomplete` without hiding accepted partial rows.
- [ ] Highlight Nucleo Unicode character indices without treating them as UTF-8 byte offsets.
- [ ] Retain the 12-row viewport and scrolling behavior.
- [ ] Add render tests for every phase, Unicode highlights, anchored selection, and narrow terminal
  widths.

Run:

```bash
cargo test -p orca-tui mention_popup -- --nocapture
```

## Stage 4: Cut Over And Remove The Synchronous Index

### Task 10: Route Composer And Tab Through Snapshots

**Files:**
- Modify: `crates/orca-tui/src/composer_input_actions.rs`
- Modify: `crates/orca-tui/src/mention_menu_actions.rs`
- Modify: `crates/orca-tui/src/input_event_actions.rs`

- [ ] Publish token changes to the manager after composer edits, paste, history recall, and Vim
  edits.
- [ ] Use browse mode for empty and trailing-slash queries and fuzzy mode otherwise.
- [ ] Make Tab use current accepted candidates and their common prefix; it must not invoke
  filesystem or full-catalog search.
- [ ] Preserve Enter selection, quoting, spaces, directory continuation, and Esc dismissal.

Run:

```bash
cargo test -p orca-tui mention_menu -- --nocapture
cargo test -p orca-tui composer -- --nocapture
```

### Task 11: Delete The Old Cache And Migrate Tests

**Files:**
- Modify: `crates/orca-runtime/src/mentions.rs`
- Modify: `crates/orca-runtime/Cargo.toml`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

- [ ] Delete `CachedIndex`, global cache state, five-second synchronous refresh, `git_ls_files`,
  fallback walk, and synchronous fuzzy scoring.
- [ ] Remove runtime `ignore` and `nucleo-matcher` dependencies if no other runtime module uses them.
- [ ] Move search/ranking tests into `orca-file-search`; keep parsing, expansion, quoting, range, and
  workspace-safety tests in `orca-runtime`.
- [ ] Confirm no `list_mention_candidates` or synchronous completion call remains on the TUI input
  path.

Run:

```bash
rg -n "list_mention_candidates|mention_index_paths|git_ls_files|CachedIndex" \
  crates/orca-runtime crates/orca-tui
cargo test -p orca-runtime mentions -- --nocapture
```

### Task 12: Final Validation

- [ ] Run focused tests serially where timing or shared machine load matters.
- [ ] Run the million-path release benchmark on an otherwise idle machine.
- [ ] Build the real TUI and exercise empty browse, trailing-slash browse, rapid query edits,
  Unicode matching, manual selection during streaming, Esc dismissal, warm reuse, and cwd/session
  shutdown in a PTY.
- [ ] Confirm scanning does not delay model/runtime event projection in a concurrent TUI test.
- [ ] Run the complete workspace gate without fixing unrelated warning debt.

Run:

```bash
cargo fmt --check
git diff --check
cargo check --workspace
cargo test -p orca-file-search -- --test-threads=1
cargo test -p orca-runtime mentions -- --nocapture
cargo test -p orca-tui mention -- --test-threads=1
cargo test --workspace -- --test-threads=1
cargo run -p orca-file-search --release --example million_path_bench -- \
  --paths 1000000 --assert-slo
```

## Completion Evidence

The implementation is complete only when the final handoff reports:

- candidate count and average path length used by the million-path benchmark;
- completed and peak incremental RSS;
- first-progress, appended-query, and arbitrary-reparse p95 latency;
- maximum observed snapshot publication rate;
- cancellation publication-stop and worker-join latency;
- focused and workspace test results;
- PTY behaviors exercised;
- files changed and any unrelated pre-existing worktree changes.
