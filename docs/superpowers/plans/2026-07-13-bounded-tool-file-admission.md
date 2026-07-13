# Bounded Tool File Admission Implementation Plan

**Goal:** Complete P0.1d-a by bounding tool-facing file reads, exact edits, and
TUI file-change previews at admission.

**Architecture:** Add one `orca_tools::file_admission` boundary for regular-file
open, UTF-8 streaming, range retention, whole-file ceilings, and cancellation.
Edit/write emit typed non-serialized file-change previews; TUI consumes them
without rereading the workspace.

## Constraints

- Preserve public tool arguments/results and server/JSONL/persistence shapes.
- Keep policy limits in callers; keep low-level file mechanics shared.
- Reject oversized whole-file transforms before mutation.
- Do not add a second diff fact source or retain TUI filesystem snapshots.
- Do not change release metadata or publish from this branch.

## Tasks

- [x] Audit read, edit, write, and TUI diff ownership.
- [x] Compare Codex committed deltas and Package 3 range/preflight policies.
- [x] Define limits, compatibility, migration, and deletion gates.
- [ ] Add RED bounded stream, growth-race, UTF-8, and cancellation tests.
- [ ] Add RED read-file range and huge-line tests.
- [ ] Add RED oversized edit and committed-preview tests.
- [ ] Implement shared text-file admission.
- [ ] Stream read-file raw and numbered range modes.
- [ ] Bound exact edit and edit/write diff previews.
- [ ] Delete TUI filesystem snapshots and consume typed previews.
- [ ] Run focused tests, static deletion scans, Clippy, formatting, and residual
  resource checks.
- [ ] Rebase latest main and run a detached non-`/tmp` workspace gate.
- [ ] Commit implementation and verification evidence separately.

## RED Verification

```bash
cargo test -p orca-tools file_admission::tests -- --test-threads=1
cargo test -p orca-tools read_file::tests -- --test-threads=1
cargo test -p orca-tools edit::tests -- --test-threads=1
cargo test -p orca-tui diff::tests -- --test-threads=1
```

## Final Verification

```bash
cargo test -p orca-tools --lib --locked --offline -- --test-threads=1
cargo test -p orca-tui --lib --locked --offline -- --test-threads=1
cargo test --test tool_contract --locked --offline -- --test-threads=1
cargo test --test runtime_lifecycle_contract --locked --offline -- --test-threads=1
cargo test --workspace --all-targets --locked --offline -- --test-threads=1
cargo clippy --workspace --all-targets --locked --offline
cargo fmt --all -- --check
git diff --check
rg -n 'read_to_string' crates/orca-tools/src/read_file.rs \
  crates/orca-tools/src/edit.rs
rg -n 'std::fs|read_to_string|File::open' crates/orca-tui/src/diff.rs
```
