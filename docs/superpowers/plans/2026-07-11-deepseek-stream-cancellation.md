# DeepSeek Stream Cancellation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `v0.2.16` with TUI compaction and turn cancellation that closes in-flight DeepSeek header/body connections and leaves no detached provider reader threads.

**Architecture:** Add an async `reqwest` streaming request and SSE body path, preserve the current synchronous provider API through one joinable worker facade, and keep all existing runtime/TUI wire behavior stable. Prove cancellation at the TCP peer, not only by measuring caller return time.

**Tech Stack:** Rust 2024, async reqwest, Tokio current-thread runtime, Cargo tests, local TCP test servers, Orca release scripts.

---

## File Map

- `Cargo.toml`: enable reqwest streaming support and any async stream dependency required by the response API.
- `crates/orca-provider/src/http_client.rs`: own async request establishment, retry, cancellation, and backoff.
- `crates/orca-provider/src/streaming.rs`: own incremental async SSE body parsing and idle timeout.
- `crates/orca-provider/src/deepseek_http.rs`: own async DeepSeek request folding and the joined synchronous compatibility facade.
- `crates/orca-provider/src/lib.rs`: expose the async provider entry point while preserving the synchronous API.
- `crates/orca-provider/src/context.rs`: retain the summary cancellation regression through the compatibility facade.
- `crates/orca-tui/src/app.rs`, `crates/orca-tui/src/status_key_actions.rs`: retain manual compaction fresh-token and shortcut coverage.
- `tests/session_server_contract.rs`: retain isolated server command homes and cancellation contracts.
- `docs/production-roadmap.md`, `docs/releases/v0.2.16.md`: record the transport ownership guarantee and verification evidence.

### Task 1: Prove Header Cancellation Leaks The Connection

**Files:**
- Modify: `crates/orca-provider/src/http_client.rs`

- [x] **Step 1: Add a TCP peer-close regression test**

Add `cancelled_streaming_request_closes_in_flight_connection`. The server must
read the complete request, signal the canceller, then wait at most 400 ms for
EOF or connection reset. The test must assert both the existing prompt caller
deadline and the server-observed close.

- [x] **Step 2: Run the test red**

Run:

```bash
CARGO_TARGET_DIR=/tmp/blade-v0216-async-red \
  cargo test -p orca-provider \
  cancelled_streaming_request_closes_in_flight_connection -- --nocapture
```

Expected: FAIL because `send_streaming_request` abandons the receiver while
the blocking request thread still owns the TCP connection.

- [x] **Step 3: Preserve the red output in the implementation record**

Record the failure reason and elapsed peer-close deadline in the release
verification notes before replacing the transport.

### Task 2: Add Async Streaming Request Establishment

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/orca-provider/src/http_client.rs`

- [x] **Step 1: Enable the reqwest stream feature**

Change the workspace reqwest features to include `stream` while retaining
`blocking`, `json`, and `rustls-tls` for the non-streaming compatibility path.

- [x] **Step 2: Introduce the async streaming client and response function**

Keep `execute_with_retry` unchanged. Replace the streaming blocking client with
an async `reqwest::Client`, and expose:

```rust
pub async fn execute_streaming_with_retry(
    build_request: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
    cancel: &CancelToken,
) -> Result<reqwest::Response, String>
```

Each request future must race a cancel waiter. Retry delays must use async
sleep and the same cancel waiter. Preserve status handling, `Retry-After`, retry
count, backoff, jitter, and strict-schema error body text.

- [x] **Step 3: Run request and retry cancellation tests green**

Run:

```bash
CARGO_TARGET_DIR=/tmp/blade-v0216-async \
  cargo test -p orca-provider http_client::tests -- --nocapture
```

Expected: header connection closes, retry cancellation remains under 250 ms,
and all existing backoff/status tests pass.

### Task 3: Prove Body Cancellation Leaks The Reader

**Files:**
- Modify: `crates/orca-provider/src/deepseek_http.rs`

- [x] **Step 1: Add a stalled-SSE peer-close regression test**

Add `cancelled_streaming_body_closes_in_flight_connection`. The server sends
valid SSE headers and one message delta, then stalls. The callback cancels the
token after receiving that delta. The server must observe EOF/reset within 400
ms and the provider call must return within 500 ms.

- [x] **Step 2: Run the test red against the blocking reader**

Run:

```bash
CARGO_TARGET_DIR=/tmp/blade-v0216-async-red \
  cargo test -p orca-provider \
  cancelled_streaming_body_closes_in_flight_connection -- --nocapture
```

Expected: FAIL because `IdleReadTimeoutReader` returns cancellation to the
parser but its helper thread still owns the blocked `reqwest::blocking::Response`.

### Task 4: Add Incremental Async SSE Parsing

**Files:**
- Modify: `crates/orca-provider/src/streaming.rs`

- [x] **Step 1: Extract one reusable stream accumulator**

Move finish reason, reasoning, content, tool-call accumulation, usage, and
tool-progress tracking into a private accumulator with `push_line` and
`finish`. Keep `parse_sse_stream` using that accumulator so existing parser
tests continue to cover identical semantics.

- [x] **Step 2: Add the async response parser**

Expose a crate-private async parser accepting `reqwest::Response`, cancel
token, idle timeout, and delta callback. Buffer arbitrary response chunks until
complete newline-delimited SSE records exist. Race each body chunk against
cancellation and `tokio::time::timeout`.

- [x] **Step 3: Remove the blocking timeout reader**

Delete `IdleReadTimeoutReader` and replace its cancellation/timeout tests with
async response tests or the provider-level stalled-body contract. No production
streaming path may spawn a body-reader thread.

- [x] **Step 4: Run parser tests**

Run:

```bash
CARGO_TARGET_DIR=/tmp/blade-v0216-async \
  cargo test -p orca-provider streaming::tests -- --nocapture
```

Expected: content, reasoning, tool calls, split chunks, usage, cancellation,
and idle timeout tests pass.

### Task 5: Add Async DeepSeek Provider And Joined Sync Facade

**Files:**
- Modify: `crates/orca-provider/src/deepseek_http.rs`
- Modify: `crates/orca-provider/src/lib.rs`

- [x] **Step 1: Convert DeepSeek streaming folding to async**

Make request establishment and SSE parsing await the new helpers. Keep request
construction, strict-schema fallback, empty-response retry, tool-call parsing,
replay state, and usage folding unchanged.

- [x] **Step 2: Expose `call_streaming_async`**

Add an async provider entry point that handles mock, fixture, and DeepSeek
providers. Mock delays must use cancellation-aware async sleep.

- [x] **Step 3: Preserve `call_streaming` through a joined facade**

Clone the provider inputs into one named worker thread, run a current-thread
Tokio runtime there, forward owned `ProviderStep` values over a channel, invoke
the existing callback on the caller thread, and join the worker before return.
Channel closure or worker panic must become a provider error response.

- [x] **Step 4: Run DeepSeek and context tests green**

Run:

```bash
CARGO_TARGET_DIR=/tmp/blade-v0216-async \
  cargo test -p orca-provider deepseek_http::tests -- --nocapture
CARGO_TARGET_DIR=/tmp/blade-v0216-async \
  cargo test -p orca-provider context::tests -- --nocapture
```

Expected: header/body peer-close tests pass together with strict fallback,
empty response, reasoning replay, and summary cancellation tests.

### Task 6: Validate The Complete v0.2.16 Slice

**Files:**
- Verify: all modified Rust and contract files

- [x] **Step 1: Run focused crate tests**

```bash
CARGO_TARGET_DIR=/tmp/blade-v0216-final cargo test -p orca-provider -- --test-threads=1
CARGO_TARGET_DIR=/tmp/blade-v0216-final cargo test -p orca-tui -- --test-threads=1
CARGO_TARGET_DIR=/tmp/blade-v0216-final cargo test --test session_server_contract -- --test-threads=1
```

- [x] **Step 2: Run Clippy and the full workspace suite**

```bash
CARGO_TARGET_DIR=/tmp/blade-v0216-final cargo clippy --workspace --all-targets
CARGO_TARGET_DIR=/tmp/blade-v0216-final cargo test --workspace --all-targets -- --test-threads=1
```

Status on 2026-07-11: workspace Clippy and the complete workspace suite pass;
the largest crate suites contain 153 provider, 652 runtime, and 359 TUI tests.
An additional strict `-D warnings` run remains blocked by pre-existing Rust
1.95 lint findings that reproduce at the committed `b126657eb` baseline, so
the release uses the repository's established non-denying Clippy gate.

- [x] **Step 3: Run formatting and repository checks**

```bash
cargo fmt --all -- --check
git diff --check
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
```

- [x] **Step 4: Run the real DeepSeek release gate**

```bash
node scripts/release/real-api-e2e.mjs --max-budget 0.02
```

Expected: provider summary, CLI, malformed-history resume, and server smoke
complete with their sentinels and successful terminal status.

### Task 7: Review, Commit, And Publish

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/releases/v0.2.16.md`

- [x] **Step 1: Update release evidence and architecture wording**

State that cancellation now closes in-flight request/body connections and
joins the compatibility worker. Do not claim the Runtime Operation Host or
one-shot token migration is complete.

- [x] **Step 2: Obtain independent review**

Review provider cancellation, callback ordering, strict fallback, worker join,
and the full current diff. Fix all correctness findings and rerun affected
focused tests.

- [ ] **Step 3: Commit independent follow-up slices**

```bash
git add Cargo.toml Cargo.lock crates/orca-provider docs/superpowers
git commit -m "fix(provider): cancel streaming transport"
git add crates/orca-tui/src/app.rs crates/orca-tui/src/status_key_actions.rs
git commit -m "fix(tui): harden compaction cancellation"
git add tests/session_server_contract.rs
git commit -m "test(server): clean isolated command homes"
```

- [ ] **Step 4: Rebase/push/tag only after all gates remain green**

Push `main`, create annotated `v0.2.16`, monitor Release and Pages Actions, and
run:

```bash
node scripts/release/verify-published.mjs \
  --version 0.2.16 \
  --repo echoVic/blade-deepseek \
  --package @blade-ai/orca \
  --bin orca
```

Expected: GitHub Release, npm package, `npm exec`, and Pages are confirmed from
remote state.
