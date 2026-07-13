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
- [x] Add RED bounded-line and collector-failure process tests.
- [x] Add RED grep pagination and zero-limit tests.
- [x] Implement the generic bounded stdout-line collector.
- [x] Migrate grep to online requested-page collection.
- [x] Migrate Git status and worktree Git commands.
- [x] Migrate ripgrep thread search to bounded online JSON parsing.
- [x] Add static deletion and high-volume regression coverage.
- [x] Run focused tests, workspace gate, Clippy, formatting, and residual
  process scan.
- [x] Commit implementation and verification evidence separately.

## Implemented Outcome

- One shared process owner now combines deadlines, process-group retirement,
  stoppable nonblocking readers, wait, reader joins, and bounded line frames.
- Collector errors are remembered while stdout continues draining, so a failed
  parser cannot deadlock a child on a full pipe.
- Grep counts the complete stream while retaining only the requested page;
  `head_limit=0` normalizes to 250 and explicit limits clamp at 1,000.
- Git status uses a 120-second deadline and bounded head/tail output.
- Worktree Git retains at most 64 KiB per stream and preserves cleanup behavior.
- Thread search parses at most 1 MiB per ripgrep JSON frame, skips truncated
  frames, retains at most 4,096 process hits, and caps snippets at 8 KiB.
- The named production modules no longer use `Command::output`.

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

## Verification Evidence

- RED: middle-page grep returned the wrong retained sample; `head_limit=0`
  returned all 300 fixture rows.
- Focused process tests: 8/8 after rebasing onto escaped-lifecycle cleanup.
- Focused grep tests: 7/7; Git status: 1/1; worktree Git: 1/1; thread-search
  collector: 2/2.
- `thread_store_contract`: 20/20; worktree isolation: subagent 1/1 and workflow
  1/1.
- The full workspace test command above exited 0 in a detached non-`/tmp`
  worktree. Notable suites:
  `session_server_contract` 106/106, `orca-runtime` 705/705, `orca-tools`
  151/151, and `orca-tui` 404/404.
- The first sandboxed gate attempt failed only because the outer Codex tool
  sandbox denied the test's `/bin/ps` spawn with `EPERM`; the same committed
  tree passed when the gate ran outside that outer sandbox.
- Workspace Clippy: exit 0 with existing warnings.
- Formatting, branch diff check, static deletion scan, and residual-process
  scan: pass.
