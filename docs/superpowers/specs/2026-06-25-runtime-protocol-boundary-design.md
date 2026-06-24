# Runtime Protocol Boundary Design

Date: 2026-06-25

## Goal

Introduce a typed runtime protocol boundary for Orca server mode so clients and
runtime code exchange named submissions and events through one contract instead
of ad hoc JSON mapping in `server.rs`.

## Scope

This is the P1 release after the runtime-owned session boundary in v0.1.31. It
is intentionally a compatibility slice: keep the current server wire format
stable while moving request decoding and event encoding into
`orca_runtime::protocol`.

In scope:

- Add typed `Submission`, `ClientOp`, `ServerEvent`, and event envelope types.
- Decode current `{"id": ..., "op": "submit", "prompt": ...}` requests into a
  runtime-owned submission type.
- Encode server events through typed variants while preserving the existing flat
  JSON shape: `{"id": ..., "event": "...", ...}`.
- Move runtime JSONL event mapping out of the server loop into protocol helpers.
- Keep unsupported op and invalid request error behavior compatible.

Out of scope:

- Replacing the headless controller loop with a long-lived thread actor.
- Changing public server wire event names.
- Adding approval-response, cancellation, or backtrack commands.
- Moving TUI events onto this protocol in the same release.

## Reference

Local Codex reference:

- `/Users/qingyun/Documents/GitHub/codex/codex-rs/protocol/src/protocol.rs`
- `/Users/qingyun/Documents/GitHub/codex/codex-rs/core/src/codex_thread.rs`

Codex separates client submissions from runtime events with typed `Submission`,
`Op`, `Event`, and `EventMsg` structures. Orca does not need the full app-server
surface yet, but it should adopt the same direction: server mode should parse
commands into typed operations and serialize runtime events through typed event
variants.

## Proposed Architecture

Add `crates/orca-runtime/src/protocol.rs`.

Responsibilities:

- Decode client submissions.
- Represent supported client operations.
- Represent server-facing events.
- Map internal JSONL runtime events into server-facing event variants.
- Serialize server events in the existing legacy-compatible flat JSON format.

`crates/orca-runtime/src/server.rs` becomes the I/O loop:

1. Read JSONL from stdin.
2. Decode into `protocol::Submission`.
3. Dispatch supported `ClientOp` variants.
4. Stream controller JSONL through `protocol::map_runtime_event_line`.
5. Write events with `protocol::write_server_event`.

## Compatibility

The public server mode contract remains compatible in this release:

- Request: `{"id":1,"op":"submit","prompt":"hello"}`
- Response: `{"id":1,"event":"message_delta","text":"..."}`

Internally, these are now typed. Future releases can add new `ClientOp` variants
without spreading string matching across the server loop.

## Testing

Add tests for:

- Submit request decoding.
- Unsupported op errors preserving the request id.
- Typed server event serialization preserving the old flat wire shape.
- Runtime event mapping to typed server events.
- Existing server-mode contract test.

## Release Gate

1. Run focused runtime tests.
2. Run full workspace tests.
3. Run site build and SEO checks.
4. Run npm staging.
5. Release v0.1.32 and verify GitHub Release, npm registry, and `npm exec`.
