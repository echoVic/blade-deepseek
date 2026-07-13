# Bounded Tool File Admission Design

Date: 2026-07-13

## User Value

A TUI read, edit, or diff preview must not freeze or exhaust Orca because a
workspace path names a huge regular file, FIFO, device, growing file, or one
newline-free record. Normal source-file behavior stays unchanged; unsupported
whole-file edits fail before proportional allocation and explain the limit.

## Current Structural Problems

- `orca-tools/src/read_file.rs` calls `read_to_string`, builds a second ranged
  string, and only then applies the output budget.
- `orca-tools/src/edit.rs` reads and clones the complete file before there is a
  file-size admission decision.
- `orca-tui/src/diff.rs` rereads the target before and after every edit/write,
  retains both copies, and computes an unbounded diff. The UI, not the mutation
  owner, decides what change occurred.
- The three paths do not reject non-regular files, do not re-check bytes after
  metadata preflight, and long reads do not observe turn cancellation.

These are boundary defects, not isolated truncation bugs. Output truncation
after a complete read is not memory admission, and a filesystem reread is not
an authoritative committed mutation record.

## Reference Findings

Package 3's `src/utils/readFileInRange.ts` separates regular-file preflight from
streaming line selection. Its streaming path counts or discards lines outside
the requested range, caps selected output, and explicitly prevents a huge
unterminated line from growing the partial buffer. `FileEditTool` stats before a
whole-file transform, and its file-state caches have aggregate byte limits.

Codex's exec-server opens only regular files, uses nonblocking handles, caps
each random-access block, and limits open read handles. Codex's
`TurnDiffTracker` consumes typed committed patch deltas instead of rereading the
workspace in the TUI; diff computation also has a deadline.

Orca should combine those properties with its simpler synchronous tool model:
one bounded text admission module, caller-owned policies, and a typed internal
file-change preview produced by the mutation owner.

## Target Ownership

`orca_tools::file_admission` owns low-level text-file admission:

- open and descriptor metadata validation for regular files;
- nonblocking device-safe open where supported;
- metadata size preflight for whole-file transforms;
- a byte ceiling rechecked while reading to catch file growth races;
- incremental UTF-8 validation across chunk boundaries;
- bounded raw and numbered-line retention;
- cancellation checks between chunks;
- typed `not_regular`, `too_large`, `invalid_utf8`, `cancelled`, and I/O errors.

`read_file` owns range semantics and public tool-result formatting. `edit` and
`write_file` own the exact mutation and emit an internal typed
`FileChangePreview` on `ToolResult`. The preview is skipped by serde, so CLI,
server/JSONL, transcript, and provider tool-result wire shapes do not change.
The TUI consumes that preview and performs no workspace file reads.

## Resource Policy

- Read chunk: 8 KiB.
- Read output retention: the existing normalized tool output byte budget.
- Maximum whole file accepted by exact edit: 16 MiB.
- Maximum before/after input per rendered diff: 4 MiB.
- Maximum retained unified diff preview: 256 KiB.
- Diff computation deadline: 100 ms.

The edit ceiling is intentionally lower than Package 3's 1 GiB guard. Exact
replacement plus UTF-8 text, updated content, and diff state can require several
copies; a 1 GiB admission limit is not a useful memory safety boundary for Orca.

## Read Semantics

- Reads without `offset` or `limit` preserve raw text and existing head/tail
  micro-compaction, but bytes are admitted while streaming.
- Ranged reads preserve one-based numbered output and `offset`-past-end
  diagnostics.
- Lines outside the selected range are counted or discarded without retention.
- A selected newline-free line can consume only the output budget.
- An explicit finite limit may stop after the requested page; total line count
  is required only when EOF proves the requested offset is past the file.
- Empty-file, CRLF, missing-final-newline, invalid UTF-8, and existing result
  kinds remain compatible.

## Mutation And Diff Semantics

- Edit preflights and bounded-reads the file before exact-match counting.
- A file that exceeds 16 MiB fails without writing or allocating its contents.
- Bytes read beyond the preflight size or ceiling fail closed as a growth race.
- Edit/write compute a bounded preview from the exact before/after values used
  by the mutation; later external changes cannot rewrite the displayed diff.
- Files above the diff-input limit may still be written when otherwise valid,
  but the typed preview records that detailed rendering was omitted.
- Diff timeout or output truncation produces a bounded preview, never an
  unbounded UI allocation.

## Compatibility

- Tool names, arguments, success text, failure status, server/JSONL protocol,
  and persisted `ToolResult` shape remain unchanged.
- Normal UTF-8 files at or below the edit ceiling preserve current read, edit,
  write, and TUI diff behavior.
- Deliberate safety changes: non-regular files are rejected; exact edits above
  16 MiB are rejected; long read scans become cancellable.
- No read-before-edit policy is added in this slice. Package 3's stale-read
  cache is useful but belongs to a separate consistency design, not this memory
  admission boundary.

## Migration

1. Add RED low-level admission, read-range, oversized edit, and committed-diff
   tests.
2. Introduce `file_admission` and typed non-serialized file-change previews.
3. Stream `read_file` through the shared boundary.
4. Preflight edit and emit bounded edit/write previews.
5. Delete TUI before/after filesystem snapshots and render typed previews.
6. Run focused tool/TUI/runtime contracts, then the complete workspace gate.

There is no long-lived compatibility adapter: once previews are emitted by the
tool layer, the TUI filesystem snapshot path is deleted in the same slice.

## Acceptance Criteria

1. Full and ranged reads retain memory proportional to the output budget, not
   file size or physical line length.
2. A growing file cannot cross a whole-file ceiling after metadata preflight.
3. Invalid UTF-8 across any streamed chunk fails explicitly.
4. Read cancellation returns before scanning the rest of a large file.
5. An oversized edit fails before mutation and leaves the file unchanged.
6. TUI diff renders the committed edit/write preview even if the file changes
   again before event projection.
7. Diff input, output, and computation time are bounded.
8. Existing tool, TUI, runtime, server, and persistence contracts remain green.

## Deletion Gate

This slice is incomplete while production `read_file` or `edit` calls
`read_to_string`; while `orca-tui/src/diff.rs` reads the filesystem; while a
whole-file transform has no preflight plus read-time ceiling; or while a file
preview can allocate without input, output, and compute limits.
