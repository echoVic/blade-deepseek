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
- [ ] Rebase latest `main` and rerun affected tests.
- [ ] Run the workspace gate and verify no test-owned process remains.

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

Before integration, run the repository's required workspace, site, release
script, and real DeepSeek gates from a main-rebased branch.
