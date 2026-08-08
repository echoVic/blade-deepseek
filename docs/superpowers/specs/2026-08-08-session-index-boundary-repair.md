# Session Index Boundary Repair

## User value

The TUI session picker must remain responsive when its session directories contain
editor FIFOs, sockets, devices, or symlinks with session-looking names. A picker
request must settle and release its runtime worker without requiring the user to
open or write to an unrelated filesystem entry.

## Evidence and root cause

The latest `origin/main` commit `445baf596` introduced the SQLite session index.
Its discovery paths use filename suffixes as the final admission test. A FIFO
named `blocked-session.jsonl` therefore enters recent seeding/backfill and is
later opened as a transcript. The existing runtime-host regression test hangs
after one FIFO release because another indexing path still owns a blocked reader.

The same index is created by listing even when no recorded session exists. The
stateless server tests currently equate any file below `ORCA_HOME` with recorded
session persistence, so a valid empty `sessions-index.sqlite3` infrastructure
file produces a false failure.

This is a boundary/lifecycle defect, not a provider or cancellation defect:
filesystem discovery admits objects outside the transcript contract, and the
test invariant does not distinguish the index's infrastructure ownership from
the thread store's persisted session ownership.

## Target ownership and boundaries

- `thread_store::local` owns filesystem discovery. It admits only regular files
  whose names end in `.jsonl` or `.jsonl.zst`; directory traversal uses entry
  file types and does not follow symlinks.
- `thread_store::session_index` owns index creation and backfill. It receives
  only admitted transcript paths and must never open a special file during
  listing or backfill.
- `thread_store::writer` owns history-file opening. It opens no-follow,
  nonblocking handles and verifies their metadata is regular before parsing, so
  a path replacement after discovery cannot reintroduce a blocking FIFO read.
- The runtime host owns the listing task and remains responsible for joining it
  during shutdown. The fix removes a blocking input rather than adding a
  detached worker, resettable cancellation token, or timeout-only workaround.
- Stateless server tests assert no transcript/catalog records are materialized;
  an empty index database created as listing infrastructure is allowed.

## Scope

1. Harden all session-file discovery paths used by seeding, backfill, search,
   selector resolution, and recent traversal to reject non-regular files and
   symlinks.
2. Add behavior tests for FIFO and symlink boundaries, including the existing
   host-supervisor listing lifecycle test and a direct history-reader FIFO test.
3. Replace the stateless filesystem assertion with a business-level invariant:
   no session transcript files and no recorded thread catalog rows, while
   allowing the index database and its SQLite sidecars.
4. Update the roadmap/release documentation to record the repaired session
   discovery boundary and the removed verification false positive.

## Non-goals and compatibility

- No public API, CLI argument, JSONL event, server protocol, SQLite schema, or
  persisted transcript format changes.
- No migration or deletion of existing regular transcripts.
- No attempt to make malformed regular transcripts valid; they remain skipped
  by the existing summary parser.
- No merge, push, release, or deletion of existing worktrees/backup refs in
  this slice.

## Migration and rollback

The migration is source-local and immediate: existing regular `.jsonl` and
`.jsonl.zst` files continue to be indexed, while special files are ignored on
the next scan. Reverting this slice is a single commit revert; no data migration
is needed. The old suffix-only admission path is deleted in the same commit.

## Acceptance criteria

- A FIFO or symlink named `*.jsonl` is ignored without opening its target, and
  direct history reading rejects a FIFO without waiting for a writer.
- `session_listing_does_not_block_host_supervisor` completes without a FIFO
  release and the host shuts down cleanly.
- Both stateless server lifecycle tests pass and still prove zero recorded
  catalog rows and zero transcript files.
- Focused thread-store/runtime-host/server/TUI tests, workspace full gate,
  formatter, diff check, and clippy pass with only documented pre-existing
  warnings.
- The feature branch is committed and rebased onto the latest `origin/main`
  before final re-verification; it remains unmerged and unpushed.
