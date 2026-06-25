# Tool System Convergence Design

## Goal

Unify Orca tool invocation records, approval classification, validation, and
result shaping across built-in, MCP, and external tools before adding shell
sessions, plugin tools, or richer TUI protocol adapters.

This is the P2 release after the runtime-owned session boundary in v0.1.31 and
the typed server protocol boundary in v0.1.32.

## Current Problem

The tool registry already exposes common `ToolSpec` metadata, but execution is
still split across several paths:

- `orca-tools` resolves built-in, MCP, and external tools.
- `orca-runtime::controller` performs validation, approval request construction,
  pre-tool hooks, hook-modified request validation, special workflow/subagent
  dispatch, and normal tool execution.
- `orca-tui::bridge` has a parallel approval classification helper.

This leaves too much policy near UI/controller code. It also makes later work
harder: long-running shell sessions, plugin-provided tools, and protocol-driven
TUI approvals need one stable runtime record of what was requested, what action
kind was enforced, what request actually executed, and what result came back.

## Scope

P2 introduces a narrow runtime tool execution boundary. It does not rewrite the
tool registry or change public tool names.

### In Scope

- Add a runtime-owned tool invocation module.
- Produce a `ToolInvocation` record for every model-requested tool.
- Resolve canonical action kind through the existing registry/MCP/external
  metadata.
- Preserve the subagent max-depth guard as a runtime policy decision.
- Centralize validation before approval and after pre-tool hook request
  mutation.
- Centralize approval request construction so controller and TUI use the same
  description/id/action.
- Keep workflow and subagent special execution paths, but route their request
  preparation through the same invocation boundary.
- Preserve existing JSONL event names, TUI behavior, approval modes, and public
  tool schemas.

### Out Of Scope

- New shell session or PTY tools.
- New plugin manifest format.
- Full TUI protocol adapter.
- Removing the existing `bash` tool.
- Changing MCP transport behavior.
- Changing approval rule syntax.

## Architecture

Add `orca_runtime::tool_invocation`.

The module is responsible for deriving a stable execution record from a
`ToolRequest`:

```rust
pub struct ToolInvocation {
    pub requested: ToolRequest,
    pub effective: ToolRequest,
    pub action: Option<ActionKind>,
}
```

`requested` is the model-supplied request. `effective` starts as a clone and may
be replaced after a pre-tool hook returns `modified_target`. `action` is the
canonical runtime-enforced action kind. It is `None` when the request is blocked
before approval, such as subagent max depth.

The module also exposes helpers:

- `prepare_tool_invocation(...) -> ToolInvocation`
- `validate_tool_invocation(...) -> Result<(), ToolExecutionFailure>`
- `apply_pre_tool_outcome(...) -> ToolInvocation`
- `approval_request_for_invocation(...) -> Option<ApprovalRequest>`
- `ToolExecutionFailure::into_result(...) -> ToolResult`

The controller keeps ownership of event ordering and actual side-effectful
execution. P2 intentionally does not hide workflow/subagent branching because
those paths still need controller state: instructions, memory, hooks, cost
tracking, cancellation, and background workflow registries.

## Data Flow

1. Model emits a `ToolRequest`.
2. Runtime calls `prepare_tool_invocation`.
3. Runtime validates the requested invocation.
4. Runtime emits `tool.call.requested` using the original request.
5. Runtime builds approval request from the invocation action.
6. Runtime resolves approval through existing policy/interactive flow.
7. Runtime runs `pre_tool_use` hook.
8. Runtime applies hook-modified target to produce a new effective invocation.
9. Runtime validates the effective invocation.
10. Runtime executes workflow, subagent, or normal registry execution.
11. Runtime emits `tool.call.completed` with the final result.

## Compatibility

P2 must preserve:

- tool names and aliases
- JSONL event names and payload shape
- server-mode flat event mapping
- TUI approval prompts and allowlist behavior
- existing TOML external tool config
- existing MCP tool names
- existing test fixture behavior

## Real API Verification

After local tests pass, run real DeepSeek smoke checks using the local
`DEEPSEEK_API_KEY` environment variable or `~/.orca/auth.json`:

```bash
cargo run -p orca-provider --example summary_render_realapi
```

```bash
./target/debug/orca exec --output-format jsonl --no-history --mode suggest --max-budget 0.02 \
  "Reply with exactly: ORCA_REAL_E2E_OK"
```

```bash
printf '%s\n' '{"id":101,"op":"submit","prompt":"Reply with exactly: ORCA_SERVER_REAL_OK"}' | \
  ./target/debug/orca --mode server
```

The final release verification must still use:

```bash
node scripts/release/verify-published.mjs --version 0.1.33 --repo echoVic/blade-deepseek --package @blade-ai/orca --bin orca
```

## Release

Release target: v0.1.33.

Update:

- `Cargo.toml`
- `Cargo.lock`
- `README.md`
- `docs/production-roadmap.md`
- `docs/releases/v0.1.33.md`
- site version and changelog files

## Acceptance Criteria

- Built-in, MCP, and external tool approval classification flows through one
  runtime invocation helper.
- Hook-modified requests are revalidated through the same helper.
- Subagent max depth remains denied before approval/execution.
- Existing contract tests pass.
- A real API CLI smoke test succeeds.
- A real API server-mode smoke test succeeds.
- GitHub Release and npm package are published and verified before P3 starts.
