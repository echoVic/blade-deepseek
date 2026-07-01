# Server Router Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move server operation dispatch out of `crates/orca-runtime/src/server.rs` into a focused router module without changing the server wire protocol or runtime behavior.

**Architecture:** Keep `server.rs` as the stdio entry point that owns line decoding, active process draining, and lifecycle cleanup. Add `crates/orca-runtime/src/server/router.rs` to own the `ClientOp` match and route each decoded submission to the existing operation handlers. This creates the first Codex-style request-processor boundary while preserving all existing handler functions.

**Tech Stack:** Rust 2024, `orca-runtime`, existing server JSONL tests, architecture tests in `crates/orca-runtime/src/lib.rs`.

## Global Constraints

- Use TDD: write the failing architecture test before production code.
- Preserve the legacy flat JSON wire format and all existing `ClientOp` behavior.
- Do not rename existing server events in this slice.
- Keep the change limited to a router boundary; per-operation processor extraction follows in later patch releases.
- Each completed feature gets its own commit.

---

### Task 1: Server Router Boundary

**Files:**
- Modify: `crates/orca-runtime/src/server.rs`
- Create: `crates/orca-runtime/src/server/router.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/plans/2026-07-02-server-router-boundary.md`

**Interfaces:**
- Consumes: `ServerConfig`, `ServerState`, `Submission`, `ClientOp`, existing `run_*` server handler functions, `lock_error`, `protocol::write_server_event`, `run_turn_control`.
- Produces: `router::dispatch_submission(config: &ServerConfig, state: &mut ServerState, submission: Submission, writer: Arc<Mutex<W>>) -> io::Result<()>`.

- [x] **Step 1: Write the failing architecture test**

Add `server_operation_dispatch_is_owned_by_router_module` in `crates/orca-runtime/src/lib.rs`. The test must assert:

```rust
let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
let server_source = std::fs::read_to_string(manifest_dir.join("src/server.rs"))
    .expect("server source");
let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
    .expect("server router source");

assert!(server_source.contains("mod router;"));
assert!(server_source.contains("router::dispatch_submission("));
assert!(!server_source.contains("match &submission.op"));
assert!(router_source.contains("pub(super) fn dispatch_submission"));
assert!(router_source.contains("match &submission.op"));
assert!(router_source.contains("ClientOp::Submit"));
assert!(router_source.contains("ClientOp::CommandExecTerminate"));
```

- [x] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-runtime server_operation_dispatch_is_owned_by_router_module -- --nocapture
```

Expected: FAIL because `src/server/router.rs` does not exist yet or `server.rs` still owns the op match.

- [x] **Step 3: Add the router module**

Create `crates/orca-runtime/src/server/router.rs` with:

```rust
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

use super::*;

pub(super) fn dispatch_submission<W: Write + Send + 'static>(
    config: &ServerConfig,
    state: &mut ServerState,
    submission: Submission,
    writer: Arc<Mutex<W>>,
) -> io::Result<()> {
    // Existing ClientOp match moves here unchanged.
}
```

In `crates/orca-runtime/src/server.rs`, add `mod router;` and replace the inline `match &submission.op` block in `handle_line` with:

```rust
router::dispatch_submission(config, state, submission, writer)?;
```

- [x] **Step 4: Run focused tests**

Run:

```bash
cargo fmt -- --check
cargo test -p orca-runtime server_operation_dispatch_is_owned_by_router_module -- --nocapture
cargo test --test server_runtime_contract -- --nocapture
```

Expected: all commands exit 0.

- [x] **Step 5: Update roadmap**

Update `docs/production-roadmap.md` current baseline and runtime/protocol notes to mention the new server router boundary and that per-operation processors remain follow-up work.

- [ ] **Step 6: Run release-gate verification**

Run:

```bash
cargo fmt -- --check
cargo test --workspace --all-targets
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
git diff --check
```

- [ ] **Step 7: Commit**

Run:

```bash
git add crates/orca-runtime/src/server.rs crates/orca-runtime/src/server/router.rs crates/orca-runtime/src/lib.rs docs/production-roadmap.md docs/superpowers/plans/2026-07-02-server-router-boundary.md
git commit -m "refactor(server): route operations through server router"
```
