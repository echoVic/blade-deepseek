# Managed Network Proxy Supervisor Design

Date: 2026-07-15

## User Value

When a TUI or server command runs under a network permission profile, stopping
the command, cancelling the turn, or exiting Orca must also stop every proxy
connection created for that command. A command that opens too many concurrent
connections must fail fast with a bounded diagnostic instead of creating an
unbounded number of threads. Normal allowed and blocked HTTP behavior remains
unchanged.

## Current Structural Problems And Evidence

`crates/orca-runtime/src/network_proxy.rs` has a listener thread that creates a
detached `std::thread::spawn` for every accepted connection. The
`RuntimeNetworkProxy` handle retains only the listener thread. Dropping it stops
and joins the listener but cannot cancel or join any active connection worker.
A CONNECT tunnel can therefore outlive the TUI tool, server command, or proxy
handle that created it.

The same boundary has three admission failures:

- there is no concurrent connection ceiling;
- request and header lines use `BufRead::read_line`, so one connection can grow
  a `String` without a byte limit;
- network block reports use unbounded `mpsc::channel` queues and are normally
  drained only after the command reaches a retry decision.

These are ownership and boundary defects, not isolated socket bugs. Adding a
timeout to one copy call would not give the proxy owner control over all child
work, and limiting only the final command output would not bound proxy memory.

## Reference Findings

Codex gives its managed proxy an explicit `NetworkProxyHandle`. Shutdown aborts
the listener tasks and awaits them, and environment-specific proxy tasks are
drained by the same handle. Its complete async proxy is much broader than Orca
needs, but the ownership property is the relevant reference: listener and
connection work must remain under one cancellable task owner.

Orca should keep its current lightweight HTTP policy proxy and synchronous
public construction API. Internally, a dedicated Tokio supervisor thread can
own async socket tasks, bounded admission, DNS, and shutdown without spreading
async requirements into TUI, runtime bash, or server command call sites.

## Target Ownership And Module Boundary

`RuntimeNetworkProxy` remains the only public lifecycle handle. It owns:

- the loopback listener address;
- one shutdown sender;
- one supervisor thread join handle;
- shared active-connection diagnostics.

The supervisor thread owns one Tokio runtime, one listener, one Hickory DNS
resolver, one connection semaphore, and one `JoinSet` containing every accepted
connection. No connection task is spawned outside that `JoinSet`.

Each connection task owns its client socket, any upstream socket, and all copy
futures. Dropping or aborting the task closes those sockets. Supervisor shutdown
stops admission, aborts every connection task, awaits every terminal, and only
then lets the supervisor thread exit. `RuntimeNetworkProxy::drop` signals that
shutdown and joins the supervisor thread.

## Admission And Resource Policy

- Maximum concurrent connections: 32.
- Request line: 8 KiB.
- Individual header line: 16 KiB.
- Aggregate headers: 64 KiB and at most 100 fields.
- Block report queue: 8 records, sent with non-blocking `try_send`.
- DNS lookup deadline: 5 seconds.
- Upstream connect deadline: 5 seconds per candidate address.
- Socket read or write idle deadline: 10 seconds per I/O operation.

An over-capacity connection receives `503 Service Unavailable` with
`x-proxy-error: connection-limit`. Oversized request framing receives
`431 Request Header Fields Too Large`. These responses are bounded and are
written directly by the supervisor without creating another task.

## External Compatibility

The following remain unchanged:

- CLI flags and permission profile TOML;
- TUI and server command flows;
- `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, and `NO_PROXY` behavior;
- allowed, allowlist-blocked, denylist-blocked, and local-target policy results;
- `RuntimeNetworkBlockReport` content consumed by permission retry logic;
- server/JSONL, task persistence, and provider protocols.

The deliberate reliability additions are overload and framing-limit HTTP
responses. Proxy feature expansion such as SOCKS, MITM, or broader request-body
support is outside this slice.

## Migration And Temporary State

1. Add RED behavior tests for connection admission, active-worker shutdown,
   bounded framing, and bounded block reports.
2. Replace the listener plus detached worker threads with the supervisor and
   `JoinSet` implementation in one module.
3. Move all three block-report call sites to the shared bounded channel helper.
4. Delete the synchronous connection-worker and unbounded line-reading path.
5. Run focused runtime/TUI/server tests, the complete workspace gate, Clippy,
   formatting, static deletion scans, and residual-connection tests.

The feature branch may contain failing RED tests before implementation. There
is no committed state with old and new proxy implementations running in
parallel, and no compatibility adapter remains after the replacement.

## Acceptance Criteria

1. Active proxy connections never exceed 32; excess connections fail with the
   typed overload response without spawning work.
2. Dropping the proxy while a client is stalled returns promptly, closes the
   client socket, and leaves zero active connection tasks.
3. Dropping the proxy while a CONNECT tunnel is active closes both client and
   upstream sides and awaits the tunnel task.
4. Request/header memory is bounded before parsing, including a newline-free
   oversized request.
5. Block reports have fixed queue capacity and a reporting worker never blocks
   on a full consumer queue.
6. Existing policy, local-network, TUI bash, runtime bash, and server command
   behavior tests remain green.
7. CLI/TUI/server external contracts and persisted shapes do not change.

## Deletion Gate

This slice is incomplete while production `network_proxy.rs` spawns detached
connection threads, reads request framing with unbounded `read_line`, or drops
the proxy without awaiting its connection tasks. It is also incomplete while
runtime bash, TUI bash, or server command execution creates an unbounded
network-block report channel.

## Candidate Implementation Evidence

The feature branch replaces the old connection path rather than wrapping it:

- one named supervisor thread owns a Tokio listener, Hickory resolver,
  semaphore, shutdown signal, and connection `JoinSet`;
- `RuntimeNetworkProxy::drop` signals shutdown and joins the supervisor after
  every connection task has been aborted and awaited;
- request/header parsing enforces the declared line, aggregate, and field-count
  ceilings before string parsing;
- runtime bash, TUI bash, and server command execution all use
  `runtime_network_block_channel`, whose sender uses non-blocking `try_send`;
- the production module contains no per-connection `std::thread::spawn` and no
  `BufRead::read_line` request path.

Focused candidate verification:

```bash
cargo test -p orca-runtime network_proxy::tests --locked --offline -- --test-threads=1
cargo test -p orca-runtime runtime_bash::tests --locked --offline -- --test-threads=1
cargo test -p orca-tui agent_tool_execution::tests --locked --offline -- --test-threads=1
cargo test --test session_server_contract network --locked --offline -- --test-threads=1
cargo check --workspace --all-targets --locked --offline
cargo clippy -p orca-runtime -p orca-tui --all-targets --locked --offline
```

The complete workspace, release-script, site, and real-provider gates remain
required after the candidate rebases onto the latest `main`.
