# Session Index Boundary Repair Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make session indexing ignore non-regular filesystem entries and make stateless lifecycle tests assert business persistence rather than index infrastructure.

**Architecture:** Keep discovery owned by `thread_store::local`, using `DirEntry::file_type()` as the admission boundary for every recursive scan. `thread_store::writer` owns no-follow/nonblocking history-file opening and validates the opened handle is regular, closing the discovery-to-open race. Keep the SQLite index and runtime-host lifecycle unchanged beyond receiving safe paths; update server tests to distinguish transcript/catalog state from an index database created by listing.

**Tech Stack:** Rust, `std::fs::DirEntry`, SQLite-backed `orca-runtime` session index, Rust unit/integration tests, TUI session-picker behavior tests.

---

### Task 1: Trace and specify the discovery boundary

**Files:**
- Create: `docs/superpowers/specs/2026-08-08-session-index-boundary-repair.md`
- Create: `docs/superpowers/plans/2026-08-08-session-index-boundary-repair.md`

- [x] **Step 1: Record reproducible failures and ownership.** Capture the FIFO timeout in `runtime_host::tests::session_listing_does_not_block_host_supervisor` and the `sessions-index.sqlite3` false positive in the two stateless server tests.
- [x] **Step 2: Define compatibility and acceptance.** Preserve all public protocols and transcript formats; require regular-file-only discovery, worker settlement, and business-level stateless assertions.

### Task 2: Add the regular-file discovery regression test

**Files:**
- Modify: `crates/orca-runtime/src/thread_store/local.rs`
- Test: `crates/orca-runtime/src/thread_store/local.rs` unit tests

- [x] **Step 1: Write the failing behavior test.** Add a Unix-only test that creates a FIFO and a symlink named with `.jsonl`, creates one valid regular `.jsonl`, invokes discovery without opening any candidate, and asserts only the regular transcript is admitted.
- [x] **Step 2: Run the minimal test and verify RED.** Run `cargo test -p orca-runtime thread_store::local::tests::session_discovery_ignores_non_regular_history_entries -- --exact --nocapture`. Baseline failed because discovery returned the FIFO, symlink, and regular file.

### Task 3: Implement the filesystem admission boundary

**Files:**
- Modify: `crates/orca-runtime/src/thread_store/local.rs`
- Modify: `crates/orca-runtime/src/thread_store/session_index.rs`

- [x] **Step 1: Use entry file types for recursive discovery.** Recurse only when `DirEntry::file_type().is_dir()` and invoke the callback only when `file_type().is_file()` and the existing history suffix predicate matches. Do not use `Path::is_dir()` or `fs::metadata()` for admission, so symlinks and FIFOs cannot be followed or opened.
- [x] **Step 2: Apply the same boundary to recent leaf traversal.** Update `sorted_subdirs` and `collect_leaf_files` to use `DirEntry::file_type()`, preserving current newest-first ordering and `.jsonl`/`.jsonl.zst` support. Validate indexed rows with `symlink_metadata` and evict paths replaced by special files.
- [x] **Step 3: Run the discovery tests and verify GREEN.** Re-run the discovery test and the indexed-path eviction RED/GREEN cycle; format the Rust sources before the focused gate.
- [x] **Step 4: Close the discovery-to-open race.** Add a direct FIFO reader RED/GREEN test, then route parser, compression, and decompression reads through one no-follow/nonblocking regular-file opener.

### Task 4: Restore the stateless lifecycle invariant

**Files:**
- Modify: `crates/orca-runtime/src/server.rs`
- Test: `crates/orca-runtime/src/server.rs` existing stateless lifecycle tests

- [x] **Step 1: Write the contract assertion.** Change the helper to assert no recorded catalog rows and no regular transcript files (`.jsonl`/`.jsonl.zst`) below `sessions` or `archive`; allow the index database and SQLite sidecars as infrastructure. Keep the assertion before and after shutdown so it proves submit does not materialize a session.
- [x] **Step 2: Run both stateless tests.** Run the two exact server tests and verify the discovery fix removes the SQLite false positive without weakening catalog/transcript assertions.

### Task 5: Verify TUI and shared lifecycle behavior

**Files:**
- Test: `crates/orca-runtime/src/runtime_host.rs`
- Test: existing `orca-tui` session picker tests and PTY contract harness

- [x] **Step 1: Run the host-supervisor regression test.** `runtime_host::tests::session_listing_does_not_block_host_supervisor` settles while the FIFO has no writer, joins both request workers, and shuts down the host cleanly.
- [x] **Step 2: Run focused shared gates.** `thread_store` (49), `server::tests` (89), runtime-host integration (66), TUI session-picker (9), PTY contract (4), and lifecycle contract (54) pass offline and serially.
- [x] **Step 3: Run the required full gates.** `cargo test --workspace --all-targets --locked --offline -- --test-threads=1`, all-target offline clippy, formatter, and diff check pass. The relevant offline lifecycle and PTY contracts pass; no real API call is needed for this filesystem-only slice.

### Task 6: Review, document, commit, and rebase

**Files:**
- Modify: relevant roadmap/release-note documentation after inspecting current conventions

- [x] **Step 1: Perform local code-quality review.** Found and fixed the discovery-to-open FIFO replacement race by making parser, compression, and decompression reads share a no-follow/nonblocking regular-file opener. No remaining Critical/Important findings.
- [x] **Step 2: Update roadmap/release notes.** Updated the production roadmap with the regular-file discovery/opening boundary and stateless verification contract. This is an unreleased branch, so no release note is created.
- [x] **Step 3: Commit one semantic slice.** Staged only the spec, plan, implementation, tests, and documentation; committed `fix(runtime): bound session index discovery to regular files`.
- [x] **Step 4: Rebase and reverify.** Fetched `origin/main` at `445baf596`; the rebase was a clean no-op because this slice was created from that tip. Re-ran affected focused tests and static checks after the final plan amend. The branch remains unmerged and unpushed.
