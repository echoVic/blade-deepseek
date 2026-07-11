# DeepSeek Stream Cancellation Design

## Summary

Orca must cancel the underlying DeepSeek HTTP operation when a TUI user
interrupts a turn or manual compaction. Returning control to the TUI while a
detached blocking request or response-reader thread remains alive is not a
complete cancellation contract.

This slice replaces the blocking streaming transport with async `reqwest`
request and body reads. The existing synchronous provider API remains as a
temporary compatibility facade: it runs the async provider future on one
joinable worker thread and forwards owned provider steps to the caller. No
request or response-reader thread may be abandoned after cancellation.

## User Outcome

- Ctrl+C, Esc, and Ctrl+G leave `Compacting context...` promptly even when the
  server has accepted the request but has not sent response headers.
- Interrupting a turn while an SSE response body is stalled also closes the
  network connection promptly.
- Retry and `Retry-After` waits stop promptly after cancellation.
- A cancelled operation does not accumulate hidden provider threads that can
  keep sockets and process resources alive.
- Existing DeepSeek reasoning, content, tool-call progress, strict-schema
  fallback, empty-response retry, and usage behavior remains unchanged.

## Root Cause

The current transport has two caller-only cancellation boundaries:

1. `send_streaming_request` moves `reqwest::blocking::RequestBuilder::send`
   into a detached thread and abandons its result receiver on cancellation.
2. `IdleReadTimeoutReader` moves each blocking body read into another detached
   thread and abandons that response on cancellation or idle timeout.

`STREAMING_CLIENT` also has no total timeout. A peer that never sends headers
or another body byte can therefore retain one of those detached threads
indefinitely. Polling an atomic cancel token only releases the waiting caller;
it cannot interrupt the blocking socket operation owned by the helper thread.

## Architecture

### Async Streaming HTTP Boundary

Each async provider operation owns an async `reqwest::Client` configured with
Hickory DNS. Request establishment, retry backoff, and response body reads race
the Orca cancel token through `tokio::select!` or timeout-wrapped futures.
Dropping the losing request/body future releases the response and closes its
connection instead of leaving a blocking owner behind. Keeping the client
operation-scoped also avoids sharing a Hyper connection pool across the
compatibility facade's short-lived Tokio runtimes.

The existing non-streaming helper stays blocking in this release. It has a
finite request timeout and is not used by the TUI streaming or compaction path.

### Async SSE Boundary

`orca-provider::streaming` gains an async response parser that consumes
`reqwest::Response::chunk()`. It keeps an incremental byte buffer, extracts
complete UTF-8 lines, and feeds the same stream accumulator used by the current
reader-based parser. The idle timeout applies to each awaited body chunk and
cancellation wins immediately.

The blocking `IdleReadTimeoutReader` is removed once the async DeepSeek path
has equivalent parser and timeout coverage.

### Synchronous Compatibility Facade

The runtime and TUI are still synchronous today. `orca_provider::call_streaming`
therefore launches one named, joinable worker that owns a current-thread Tokio
runtime and calls `call_streaming_async`. Provider steps cross a zero-capacity
handoff as owned values. The worker waits for an acknowledgement sent only
after the caller-thread callback returns, preserving callback backpressure and
preventing prefetched deltas from crossing a callback-triggered cancellation.
Dropping the acknowledgement during callback unwind cancels the worker, and the
facade always joins it before returning or unwinding.

This facade is an explicit migration boundary. The next Runtime Operation Host
slice will call the async provider API directly and then remove the worker.

## Cancellation Contract

For request establishment, retry sleep, and response-body reads:

1. Cancellation returns the existing provider cancellation error.
2. The in-flight async future is dropped.
3. The compatibility worker reaches a terminal result and is joined.
4. The test peer observes EOF or connection reset within a bounded deadline.

The current resettable `CancelToken` remains wire-compatible for `v0.2.16`, but
the transport never resets it. Replacing it with one-shot operation-scoped
tokens belongs to the next Runtime Operation Host release.

## Data Flow

1. Runtime/TUI calls the synchronous provider facade with conversation,
   provider config, cancel token, and step callback.
2. The facade starts a joinable async worker.
3. The worker establishes the DeepSeek request through the async retry helper.
4. The async SSE parser emits owned reasoning, message, and tool progress
   steps over the channel.
5. The caller forwards those steps through the existing runtime/TUI event path.
6. Completion or cancellation sends one terminal worker result.
7. The facade joins the worker before returning the `ProviderResponse`.

## Error Handling

- HTTP status and strict-schema rejection strings remain compatible with the
  existing fallback classifier.
- Malformed SSE JSON and EOF without a finish reason or `[DONE]` are explicit
  integrity errors. One integrity retry is allowed only before any callback
  step has been emitted; visible partial output is never replayed.
- A transport-complete known-tool call with invalid argument JSON remains a
  tool request, then fails schema validation before approval, hooks, task
  creation, or execution so the model can correct it without ending the turn.
- Connect, request, body-read, idle-timeout, cancellation, and worker failures
  return explicit text rather than panicking.
- A worker channel closing without a terminal result becomes a provider error.
- UTF-8 split across chunks remains buffered until a full line is available.
  Invalid complete UTF-8 lines are explicit integrity errors and receive the
  same pre-visible-output retry policy as malformed SSE JSON.

## Alternatives Considered

### Add A Finite Timeout To The Blocking Client

This bounds the leak but still leaves hidden work alive after the TUI reports
cancellation. It also forces one total timeout to cover both headers and long
valid generations. Rejected as the final fix.

### Keep Detached Threads And Document Caller-Only Cancellation

This makes the UI responsive but does not satisfy resource ownership or
transport cancellation. Rejected.

### Make The Entire Runtime Async In This Release

This is the desired long-term direction but would combine provider transport,
turn orchestration, TUI clientization, and cancellation-scope redesign in one
release. Rejected for `v0.2.16`; the joined facade keeps this slice testable and
reversible while exposing the async API needed by the next release.

## Non-Goals

- Do not redesign the turn loop or introduce `RuntimeOperation` here.
- Do not change app-server or JSONL event shapes.
- Do not introduce typed provider errors in this release.
- Do not change DeepSeek retry counts, strict-tool fallback, empty-response
  retries, context policy, or model routing.
- Do not release plugin, skill, or model-catalog changes with this slice.

## Acceptance Criteria

1. Cancelling before response headers closes the accepted TCP connection and
   returns within 500 ms in the focused test.
2. Cancelling during a stalled SSE body closes the TCP connection and returns
   within 500 ms in the focused test.
3. Cancelling during `Retry-After` backoff returns within 250 ms and does not
   issue another request.
4. Cancelling from a synchronous callback closes the peer, joins the worker,
   and does not deliver a second delta prefetched while the callback ran or
   assembled later from the same SSE frame.
5. The synchronous provider facade joins its async worker on success, error,
   callback unwind, and cancellation.
6. Stream integrity retries have an independent budget and never replay visible
   partial output.
7. Existing provider, TUI compaction, and server interruption tests pass.
8. Provider and TUI Clippy, full workspace tests, formatting, and diff checks
   pass before commit.
9. Site/release staging and the real DeepSeek compaction smoke pass before the
   `v0.2.16` tag is pushed.
10. GitHub Release, npm package, `npm exec`, and Pages are verified remotely.

## Release Scope

Target release: `v0.2.16`.

The release note should describe one user-visible contract: TUI compaction is
visible and interruptible through hooks, request establishment, streaming
summary generation, persistence, and completion, without leaving hidden
provider work behind.
