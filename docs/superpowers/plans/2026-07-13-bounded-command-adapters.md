# Bounded Command Adapters Implementation Plan

**Goal:** Finish P0.1c by making one-shot Git, ripgrep search, and grep paging
bounded at process ingress while preserving lifecycle ownership.

**Architecture:** Extend `orca_tools::process` with a bounded line-reader
collector that drains stdout concurrently with bounded stderr. Migrate each
adapter to a small policy-specific collector rather than returning full
process buffers.

## Constraints

- Preserve public tool, server, thread-store, and worktree result shapes.
- Preserve ripgrep-not-installed fallback behavior.
- Do not change release metadata or publish from this branch.
- Do not introduce another process owner outside `orca_tools::process`.
- Delete all named `.output()` paths in this slice.

## Tasks

- [x] Re-audit P0.1c after the resource-lifecycle branch.
- [x] Compare Codex process ownership and Package 3 range/tail policies.
- [x] Define limits, compatibility, failures, and deletion gates.
- [ ] Add RED bounded-line and collector-failure process tests.
- [ ] Add RED grep pagination and zero-limit tests.
- [ ] Implement the generic bounded stdout-line collector.
- [ ] Migrate grep to online requested-page collection.
- [ ] Migrate Git status and worktree Git commands.
- [ ] Migrate ripgrep thread search to bounded online JSON parsing.
- [ ] Add static deletion and high-volume regression coverage.
- [ ] Run focused tests, workspace gate, Clippy, formatting, and residual
  process scan.
- [ ] Commit implementation and verification evidence separately.

## RED Verification

```bash
cargo test -p orca-tools process::tests::bounded_line_reader -- --nocapture
cargo test -p orca-tools grep::tests -- --nocapture
cargo test --test thread_store_contract -- --test-threads=1
```

## Final Verification

```bash
cargo test -p orca-tools --lib --locked --offline -- --test-threads=1
cargo test -p orca-runtime --lib --locked --offline -- --test-threads=1
cargo test --test thread_store_contract --locked --offline -- --test-threads=1
cargo test --test subagent_contract --locked --offline -- --test-threads=1
cargo test --test workflow_runtime_contract --locked --offline -- --test-threads=1
cargo test --workspace --all-targets --locked --offline -- --test-threads=1
cargo clippy --workspace --all-targets --locked --offline
cargo fmt --all -- --check
git diff --check
rg -n '\.output\(\)' crates/orca-tools/src/git.rs \
  crates/orca-runtime/src/worktree.rs \
  crates/orca-runtime/src/thread_store/local.rs
```

After process stress tests, verify no command containing this worktree path
remains alive.

