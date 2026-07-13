# Goal Resource Incident Analysis

Date: 2026-07-12

Affected report: Orca v0.2.18 running a long Goal workload in
`/Users/qingyun/Documents/GitHub/blade-agent-sdk`.

## Executive Summary

The machine freeze was caused by orphaned Node test workers, not by an Orca
transcript heap leak. A sandboxed Vitest/Tinypool parent repeatedly failed to
signal its workers with `kill EPERM`; 10 workers remained alive and reached
40.51 GiB of resident pages in aggregate. v0.2.19 fixed that direct macOS
Seatbelt signal-policy defect. A planned v0.2.22 resource-governance release is
intended to add broader process ownership, bounded output, shutdown, and
credential protections so unrelated terminal paths do not leave children
running or retain arbitrarily large captured output. That broader hardening
remains draft scope until it is integrated, verified, and released.

## Evidence

The incident session is:

```text
~/.orca/sessions/2026/07/11/
session-2026-07-11T15-33-39-b55d2465-5bf6-4fb9-91d6-2954aff6f5eb.jsonl
```

Observed evidence:

- The transcript is 1,052,281 bytes. Its task state is 438,258 bytes across
  191 records. Disk size is not a direct heap measurement, but these inputs
  cannot by themselves account for a 40 GiB resident set.
- The transcript records repeated Vitest/Tinypool teardown failures:
  `Error: kill EPERM`, `ChildProcess.kill`, and `ProcessWorker.terminate`.
- `/Library/Logs/DiagnosticReports/JetsamEvent-2026-07-12-090731.ips`
  reports a 16 KiB page size and identifies `node` as the largest process
  family.
- The 10 same-batch workers, PIDs 31420, 31423, 31424, 31425, 31427, 31428,
  31430, 31432, 31433, and 31435, total 2,654,651 resident pages. That is
  40.5068 GiB, or 3.9046-4.1610 GiB per worker.
- Orca processes in the same Jetsam record use 27.19 MiB and 30.94 MiB.
- The screenshot shows Goal pausing after an unknown-tool parsing failure. That
  explains the visible terminal state, but it does not explain the memory
  footprint and is not the resource root cause.

## Root Cause

The v0.2.18 workspace-write and read-only Seatbelt profiles allowed sandboxed
commands to run tests but did not allow the process manager to signal its own
descendants. When Vitest/Tinypool tried to stop a worker, macOS returned
`EPERM`. The parent failed while the workers survived and became launchd-owned.
Repeated test runs accumulated memory outside the Orca process.

This is a process-lifecycle and sandbox-policy incident. It is not evidence of
a Rust heap leak, a Goal-record leak, or unbounded transcript retention inside
Orca.

## Remediation Timeline

- **v0.2.19**: allowed a sandboxed process to signal only itself and its own
  descendants. Broader signal targets remain denied. This is the direct
  incident fix.
- **v0.2.22 (planned)**: the resource-governance draft is intended to add
  defense in depth across other lifecycle paths: process-tree termination and
  wait on external-tool stdin failure; 1 MiB per-stream capture limits; MCP and
  workflow reaping; owned and identity-checked async subagent workers; bounded
  two-phase server shutdown; ordinary shell-tree cleanup; and anonymous bounded
  stdin handoff for worker API keys. None of this draft scope is attributed to
  the published v0.2.21 release.

## Security Review

The focused source review identified plausible resource-exhaustion and
credential-exposure paths, and the v0.2.22 candidate changes are intended to
address them. The formal Codex Security Deep Scan was not completed: its
preflight requires six usable discovery workers per round, while the review
session exposed only three worker slots. No Deep Scan findings or report are
claimed from that blocked run.

Open hardening items:

- Windows descendant-tree termination is not yet equivalent to Unix process-
  group cleanup.
- The stdio MCP response channel is still unbounded.
- WorkflowHost lacks a total workflow deadline and global event-count ceiling.
- The managed network proxy lacks connection-count and per-connection I/O
  deadlines.

## Operational Guidance

Upgrade macOS installations to at least v0.2.19 for the direct incident fix. Do
not rely on the planned v0.2.22 resource-lifecycle hardening until its candidate
changes are integrated, pass the release gate, and are published. If a
pre-v0.2.19 session exhibits `kill EPERM`, inspect the process tree for orphaned
test workers and terminate only descendants attributable to that workload. Do
not infer an Orca heap leak from total terminal memory without separating the
Orca process from its child processes.
