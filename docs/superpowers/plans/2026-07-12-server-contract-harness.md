# Server Contract Harness Implementation Plan

**Goal:** Replace unbounded server JSONL test reads with one deadline-aware,
diagnostic, process-owning test client.

**Architecture:** A test-only client owns the server child, stdin, nonblocking
stdout/stderr readers, bounded event and capture buffers, recent transcript,
deadlines, and cleanup. Tests express request/event expectations; impossible
terminal events fail immediately.

## Constraints

- Preserve production server behavior and all public JSONL shapes.
- Start from current `main`, and rebase again before implementation and full
  verification if the release branch advances.
- Use real process behavior for timeout and cleanup tests.
- Do not skip the failing network test merely because a host cannot bind the
  local proxy.
- Delete the old unbounded event-reader path after migration.
- Commit this slice independently from runtime and release metadata changes.

## Tasks

- [x] Add failing tests for deadline diagnostics, impossible terminal events,
  and Drop cleanup.
- [x] Add `tests/support/server_test_client.rs` with bounded stdout ingestion,
  typed expectations, and idempotent cleanup.
- [x] Migrate the two server bash network-permission tests.
- [x] Migrate remaining shared JSONL event waits and delete unbounded helpers.
- [x] Run formatting and focused harness/server contracts.
- [x] Review event ordering, reader failure, and descendant cleanup edge cases.
- [x] Commit as one reviewable lifecycle/validation slice.
- [x] Rebase the slice from the v0.2.19 base onto current `main` at v0.2.20 and
  rerun affected tests.
- [x] Rerun the workspace gate after rebase and verify no test-owned process
  remains.

## Verification

```bash
CARGO_TARGET_DIR=/private/tmp/blade-deepseek-server-contract-harness-target \
  cargo test --test session_server_contract server_test_client -- --nocapture

CARGO_TARGET_DIR=/private/tmp/blade-deepseek-server-contract-harness-target \
  cargo test --test session_server_contract \
  server_mode_bash_inherits_thread_active_permission_profile_network_policy \
  -- --nocapture

CARGO_TARGET_DIR=/private/tmp/blade-deepseek-server-contract-harness-target \
  cargo test --test session_server_contract -- --test-threads=1

cargo fmt --all -- --check
git diff --check
```

Pre-rebase verification observed on 2026-07-12 from
`/private/tmp/blade-deepseek-v020-bounded-server-harness`:

- harness lifecycle tests: 21 passed;
- previously hanging network-policy contract: passed in under one second;
- complete `session_server_contract`: 122 passed in 36.01 seconds;
- `cargo test --workspace --all-targets --locked --offline --
  --test-threads=1`: passed;
- `cargo clippy --workspace --all-targets --locked --offline`: exit 0 with
  existing warnings;
- `cargo fmt --all -- --check` and `git diff --check`: passed;
- post-gate process inspection found no Cargo, contract-test, or test-owned Orca
  process associated with this worktree.

The workspace gate initially exposed a location-dependent macOS Seatbelt test
fixture: a worktree below `/private/tmp` inherited the profile's intentional
`/tmp` write allowance, so a nominal outside-workspace path was not denied.
Commit `947e3df6b` moves deny-sensitive fixtures outside the temporary allow
roots and keeps explicit `/tmp` allowance tests unchanged.

Post-rebase verification against `main@c6ec1ad16`:

- harness lifecycle tests: 21 passed;
- previously hanging network-policy contract: passed in 0.74 seconds;
- complete `session_server_contract`: 122 passed in 36.50 seconds;
- `cargo test --workspace --all-targets --locked --offline --
  --test-threads=1`: passed, including the v0.2.20 TUI tests;
- `cargo clippy --workspace --all-targets --locked --offline`: exit 0 with
  existing warnings;
- `cargo fmt --all -- --check` and `git diff --check`: passed;
- remote `main` still resolved to `c6ec1ad16`, and post-gate process inspection
  found no test-owned process associated with this worktree.

Before integration, run the repository's required workspace, site, release
script, and real DeepSeek gates from a main-rebased branch.
