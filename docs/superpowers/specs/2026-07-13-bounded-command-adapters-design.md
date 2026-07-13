# Bounded Command Adapters Design

Date: 2026-07-13

## Problem

The resource-lifecycle work bounds most child-process output, but four
production adapters still either use `Command::output` or retain a bounded
head/tail stream before applying line pagination:

- `orca-tools/src/git.rs` collects all `git status --short` output;
- `orca-runtime/src/worktree.rs` collects all stdout and stderr from worktree
  Git commands;
- `orca-runtime/src/thread_store/local.rs` collects all ripgrep JSON output
  before parsing search hits;
- `orca-tools/src/grep.rs` bounds process output, but pagination runs after
  head/tail retention, so a large result set can lose the requested middle
  page and cannot report a trustworthy next offset.

Output truncation after process exit is not an admission boundary. A command
can allocate proportional to its complete stdout or stderr before the tool,
search, or worktree layer applies its result limit.

## References

Codex uses process groups, explicit deadlines, concurrent stdout/stderr
draining, bounded retained diagnostics, and bounded channels between process
readers and consumers. Package 3 sends long-running shell output to a task
file, reads only bounded ranges/tails, kills owned process trees, and treats an
unlimited output mode as a disk and memory incident boundary.

Orca should preserve the useful property shared by both systems: the process
owner controls admission while bytes are arriving, not after they have already
been accumulated.

## Ownership

`orca_tools::process` remains the low-level owner. A new bounded stdout-line
adapter owns:

- the child process group and null stdin;
- a fixed-size stdout frame buffer;
- a caller-supplied collector running on the stdout reader thread;
- bounded head/tail stderr retention;
- timeout, termination, wait, and reader joins;
- observed and omitted byte accounting.

The collector receives at most one bounded line at a time. If a physical line
exceeds the frame limit, the adapter retains only its prefix, drains the rest,
and marks that line as truncated. Collector errors are remembered while the
reader continues draining so a child cannot deadlock on a full pipe before the
owner reaps it.

## Resource Policy

- Generic retained stdout/stderr: existing 1 MiB per stream default.
- Bounded stdout line frame: caller-selected, never above 1 MiB in migrated
  adapters.
- Git status retained output: the tool output budget, with a minimum 8 KiB.
- Worktree Git stdout/stderr: 64 KiB per stream.
- Worktree/Git tool deadline: 120 seconds.
- Ripgrep JSON frame: 1 MiB.
- Thread-search snippet: 8 KiB head/tail.
- Thread-search process hits: 4,096 maximum before later sorting/pagination.
- Grep requested page: at most 1,000 lines and a bounded aggregate byte budget.

The thread-search cap is an explicit safety ceiling, not a claim that a search
has no later matches. The public protocol remains a page of results; this slice
does not add a new wire field for omitted global matches.

## Compatibility

- Existing success/failure result shapes, exit codes, no-match behavior, and
  worktree preservation rules remain unchanged.
- Grep offsets are evaluated against the complete drained line count rather
  than a retained head/tail sample.
- `head_limit=0` no longer creates an unlimited allocation path. It normalizes
  to the existing default page size of 250.
- Explicit grep limits above 1,000 are clamped to 1,000.
- Oversized individual grep lines are returned as bounded previews with an
  omission notice.
- Oversized ripgrep JSON records are skipped because a truncated JSON object
  cannot be parsed safely.

## Failure Semantics

- Spawn, reader, parser-collector, wait, and timeout failures are explicit.
- A timeout terminates the process group, waits for the child, then joins both
  readers before returning.
- A nonzero Git/worktree status returns bounded stdout/stderr diagnostics.
- Ripgrep exit code 1 remains a successful no-match result; missing ripgrep
  still falls back to the in-process search path.
- Omitted output is reflected in tool truncation metadata where a public tool
  result supports it.

## Acceptance Criteria

1. A multi-megabyte newline-free stdout line never allocates more than the
   configured line frame plus caller state.
2. Collector failure cannot leave the child blocked on stdout or skip wait.
3. Grep pagination retains only the requested page while counting the complete
   drained stream.
4. Git status and worktree Git commands have bounded stdout/stderr, deadlines,
   process-group termination, and wait.
5. Thread search parses bounded JSON frames online and retains at most 4,096
   bounded snippets.
6. Existing grep, thread-store, subagent, and workflow-worktree contracts pass.

## Deletion Gate

This slice is incomplete while production `.output()` remains in
`orca-tools/src/git.rs`, `orca-runtime/src/worktree.rs`, or
`orca-runtime/src/thread_store/local.rs`; while grep pagination materializes all
lines from retained output; or while `head_limit=0` restores unlimited
retention.

