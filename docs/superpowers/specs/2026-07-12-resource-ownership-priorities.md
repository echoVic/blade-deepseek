# Resource Ownership Refactor Priorities

Date: 2026-07-12

## Objective

Make every external byte stream, child process, background task, and protocol
connection have one explicit owner for admission, cancellation, terminal state,
cleanup, and retained output. Large refactors are acceptable, but each migration
must delete its old unbounded path and remain independently releasable.

## Reference Findings

Codex provides the strongest low-level patterns:

- bounded submission and transport channels (`mcp-server/src/lib.rs` uses 128;
  `exec-server/src/local_process.rs` uses 256);
- process groups, `kill_on_drop`, explicit terminate handles, and bounded thread
  shutdown;
- 1 MiB head/tail retained unified-exec output with omission accounting
  (`core/src/unified_exec/{mod.rs,head_tail_buffer.rs}`);
- 8 KiB output deltas and bounded MCP lines (8 MiB stdout, 1 MiB stderr in
  `rmcp-client/src/executor_process_transport.rs`);
- one-shot cancellation tokens tied to the operation that owns cleanup.

Package 3 provides useful task-product patterns:

- `src/utils/ShellCommand.ts` sends shell output directly to a task file and
  observes parent `exit`, not descendant-held stdio `close`;
- `src/utils/task/diskOutput.ts` serves bounded tails/ranges instead of loading
  the full file and applies a disk cap;
- task/agent cleanup paths kill owned process trees;
- a file-size watchdog exists because moving bytes out of memory does not
  remove the need for a quota (the motivating boundary was a 768 GB incident).

Orca should adopt these ownership properties, not copy either implementation.

## Completed Foundation

### P0.1 Process output ingress

`orca-tools::process` now drains stdout/stderr concurrently into bounded
head/tail retention, supports cancellation/timeouts, kills the process group,
and waits. Hooks, verification, external tools, and other migrated callers no
longer collect unbounded `Command::output` buffers.

### P0.1b WorkflowHost lifecycle

The workflow Node host now has bounded bidirectional frames, aggregate event
limits, synchronous Node output backpressure, bounded multi-consumer agent
admission, one run token, callback-only production events, bounded stderr, and
RAII process/file cleanup. Terminal output cancels unawaited agents, and parent
exit cannot leave pipe-holding descendants behind.

### P0.1c Bounded one-shot command adapters

Git status, worktree Git, grep, and ripgrep thread search now apply memory
admission while bytes arrive. A shared bounded-line collector owns drain,
deadline, process-group retirement, wait, and reader joins. Grep retains only
the requested page; thread search parses bounded JSON frames online; Git paths
retain bounded diagnostics. The named production `.output()` paths are deleted.

### Current next slice: P0.1d-a tool-facing file admission

Start with user-triggered file reads and transforms: `read_file`, edit, and TUI
diff. They have the clearest direct memory-risk boundary and can establish the
shared metadata preflight plus bounded range reader before repository-controlled
runtime inputs adopt stricter typed limits in P0.1d-b.

## Ranked Work

### 1. P0.1c Bounded one-shot command adapters (completed)

**Risk:** High. **Effort:** Medium. **Depends on:** P0.1.

The completed slice removed post-exit collection from:

- `crates/orca-tools/src/grep.rs`;
- `crates/orca-tools/src/git.rs`;
- `crates/orca-runtime/src/thread_store/local.rs`;
- `crates/orca-runtime/src/worktree.rs`.

The shared adapter drains bounded stdout lines and stderr concurrently, retires
the process group, waits, joins readers, and reports omission. Grep pagination
counts the full stream while retaining one bounded page; `head_limit=0` cannot
restore unlimited retention.

**Deletion gate:** no production `.output()` remains in these modules, and a
large newline-free logical stream does not cause proportional retained memory
in regression tests.

### 2. P0.1d-a Tool-facing bounded file admission

**Risk:** High. **Effort:** Medium. **Depends on:** shared bounded readers.

User-triggered tool/UI paths apply output limits only after loading an entire
file:

- `crates/orca-tools/src/read_file.rs` calls `read_to_string` before line-range
  selection and output truncation;
- `crates/orca-tools/src/edit.rs` and `crates/orca-tui/src/diff.rs` materialize
  complete before/after contents, with the diff path retaining both copies.

Port Package 3's bounded range/tail reader semantics into a shared Orca file
reader. `read_file` should stream only the requested line page under both a byte
and line ceiling. Whole-file transforms such as edit/diff must preflight file
metadata, reject unsupported sizes before allocation, and re-check bytes read
to handle file growth races.

**Deletion gate:** tool/UI paths do not truncate after a full-file read; a
sparse or multi-gigabyte file cannot cause proportional RSS.

### 3. P0.1d-b Runtime-owned bounded file inputs

**Risk:** High. **Effort:** Medium/High. **Depends on:** P0.1d-a shared readers.

Repository-controlled instructions, skills, memory, workflow scripts/state,
and persisted JSONL currently lack one typed admission policy. Apply smaller
domain-specific ceilings before parsing, distinguish `too_large` from malformed
content, and never parse silently truncated syntax. Persisted logs need bounded
frame iteration rather than whole-file materialization.

**Deletion gate:** every config, instruction, skill, workflow, memory, and
persisted-state loader declares its byte/frame ceiling and returns a typed
oversize result before proportional allocation.

### 4. P0.2 MCP transport session owners

**Risk:** Critical. **Effort:** High. **Depends on:** shared bounded-frame helper
from P0.1b.

`crates/orca-mcp/src/transport.rs` has two independent ownership failures. The
stdio path has an unbounded response channel, unbounded `read_line`, direct-child
kill without process-group ownership, a detached reader, discarded stderr, and
kill paths that do not consistently wait. The SSE path spawns one detached
thread per request, returns on cancellation without aborting that request, and
loads the complete response with `Response::text`.

Create a common `McpTransportSession` contract, first implemented by
`McpStdioSession` and `McpHttpSession`, with:

- process-group child guard and idempotent terminate/wait;
- bounded 8 MiB JSON-RPC frames in both directions;
- bounded response channel and notification admission;
- 1 MiB retained stderr diagnostics;
- operation cancellation plus startup/tool deadlines;
- reader join and reconnect only after the prior process is reaped;
- abortable HTTP request ownership and bounded SSE/JSON response bodies.

**Deletion gate:** no unbounded line/channel, direct `Child::kill` return, or
detached reader remains in stdio; cancellation leaves no SSE request thread
running; no MCP response body is collected without a byte ceiling.

### 5. P0.3 Server transport framing and backpressure

**Risk:** High. **Effort:** Medium/High. **Depends on:** bounded-frame helper.

`crates/orca-runtime/src/server.rs` reads stdin with unbounded `read_line`, while
runtime workers serialize directly through one `Arc<Mutex<Write>>`. A hostile
client can allocate one huge request; a slow client can block unrelated runtime
workers while they hold serialized event payloads.

Split transport from request processing:

- bounded JSONL request frames with an explicit maximum;
- one bounded outbound queue and one writer task;
- per-event/delta byte limits and queue-overload semantics;
- connection cancellation that triggers bounded active-turn/process shutdown;
- processors depend on a `ServerEventSink`, not a concrete shared writer.

**Deletion gate:** no production server `read_line` into a reusable `String`, no
direct shared-writer mutex in worker threads, and overload/EOF tests prove
bounded shutdown.

### 6. P0.4 HTTP and provider response budgets

**Risk:** High. **Effort:** Medium. **Depends on:** none after frame utilities.

`crates/orca-provider/src/http_client.rs` reads error bodies with
`Response::text`, and `streaming.rs` permits an unbounded partial SSE line plus
unbounded reasoning/content/tool-argument accumulation.
`crates/orca-tools/src/web_search.rs` also loads the complete Exa response.
MCP HTTP transport is migrated in P0.2, but should consume the same bounded-body
primitive. Cancellation is now correct in the provider path, but memory
admission is not.

Add separate budgets for error-body retention, SSE frame size, accumulated
reasoning/content, tool count, per-tool arguments, and tool HTTP results. Fail
oversized frames before allocation growth, preserve a bounded diagnostic, and
distinguish protocol-limit failures from retryable transport failures.

**Deletion gate:** no production `Response::text` remains on an externally
sized body; oversized body, newline-free SSE, and tool-argument flood tests stay
within configured memory and return typed terminal errors.

### 7. P0.5 Runtime event admission and projection

**Risk:** High. **Effort:** Medium/High. **Depends on:** provider delta policy.

Resource bounds can still be lost after transport parsing:

- `workflow/runner.rs::SharedEventBuffer` stores every child JSONL event in an
  `Arc<Mutex<Vec<u8>>>`, then clones the complete buffer just to project tool
  evidence;
- `orca-tui/src/agent_runner.rs::TuiRuntimeEventWriter` accepts an unbounded
  partial JSONL frame;
- the production TUI event/action lanes and provider stream in
  `orca-tui/src/{app.rs,agent_runner.rs}` use unbounded `std::mpsc` queues while
  the renderer consumes only bounded batches.

Replace full child-event retention with an online bounded evidence projector.
Give TUI control, terminal, and lossy delta traffic separate admission rules:
control/terminal events must remain deliverable, while adjacent text deltas can
be coalesced or rejected at an explicit byte/count ceiling. Cap the JSONL frame
before deserialization and expose queue saturation in diagnostics.

**Deletion gate:** no workflow child clones a complete event transcript, no TUI
JSONL partial frame grows without a ceiling, and stress tests with a stalled
renderer keep queue memory bounded without losing terminal/control events.

### 8. P1 Shared operation host crate

**Risk:** Architectural. **Effort:** High. **Depends on:** P0.1c, P0.1d, and
P0.2-P0.5 proving the required primitives.

Create a low-level `orca-process` crate rather than letting `orca-runtime`,
`orca-tools`, and `orca-mcp` grow incompatible child guards. It should own:

- process-group spawn/terminate/wait;
- bounded byte, line, and head/tail collectors;
- cancellation/deadline polling;
- idempotent terminal outcome (`completed`, `failed`, `cancelled`, `timed_out`,
  `indeterminate`);
- reader/writer join obligations and residual-process tests.

Keep tool policy, MCP protocol, workflow state, and UI projection above this
crate. The abstraction is ready only when it deletes duplicate lifecycle code,
not merely wraps it.

### 9. P1 Task supervisor and cancellation tree

**Risk:** Architectural. **Effort:** High. **Depends on:** shared operation host.

Extend `TaskRegistry` from a status store into a supervisor with parent/child
relationships, one-shot cancellation propagation, bounded shutdown reports,
and explicit ownership of shell, MCP, workflow, and subagent operations. Parent
agent exit must terminate its owned background operations, matching Package 3's
agent cleanup registry; global shutdown should resemble Codex's bounded thread
shutdown instead of joining indefinitely.

### 10. P2 Quota-managed output spool

**Risk:** Medium. **Effort:** Medium. **Depends on:** shared operation host.

For workflows that need full diagnostics, add optional direct-to-file output
with byte-range/tail reads, per-operation and global quotas, eviction, and a
disk-growth watchdog. Retained memory remains the default. Do not add an
unbounded spool: Package 3's historical 768 GB output incident is the boundary
condition to design against.

### 11. P2 Pre-main process hardening

**Risk:** Security hardening. **Effort:** Low/Medium. **Depends on:** stable
packaging/startup boundary.

Adapt Codex's dedicated pre-main hardening boundary: disable core dumps, deny
same-user debugger attachment where supported, and remove `LD_*`/`DYLD_*`
injection variables before secrets and configuration are loaded. Keep this
separate from sandbox policy and make unsupported-platform behavior explicit.

## Execution Order

1. Complete tool-facing bounded file admission, then apply typed limits to
   runtime-owned file inputs.
2. Rebuild both MCP transports, then server transport, around bounded frames and
   explicit owners.
3. Bound HTTP/provider frames and aggregate response state, then close the
   workflow/TUI event-admission gap.
4. Extract only the proven common lifecycle into `orca-process`.
5. Build the task cancellation tree on top, then add optional disk spooling and
   pre-main hardening.

Do not bundle these slices into one release. Each slice requires RED resource
tests, focused contracts, the workspace gate, Clippy, formatting, static
deletion scans, and a residual-process audit.
