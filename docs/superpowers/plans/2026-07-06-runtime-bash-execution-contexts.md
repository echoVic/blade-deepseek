# Runtime Bash Execution Contexts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Continue the Codex/package-3-inspired tool boundary split by grouping `runtime_bash.rs` sandbox and one-shot shell execution inputs behind focused context structs.

**Architecture:** Keep `RuntimeBashInvocationContext` as the public runtime-normal-tool-to-bash boundary. Add internal `RuntimeBashSandboxContext` and `RuntimeBashOnceContext` structs so sandbox execution and shell-session spawning no longer depend on long argument lists or `clippy::too_many_arguments` escapes. Preserve bash behavior, permission retry flow, network proxy handling, sandbox diagnostics, output truncation, cancellation, and task-registry lifecycle behavior.

**Tech Stack:** Rust workspace, `orca-runtime`, existing runtime lifecycle and bash tests, architecture ownership tests in `crates/orca-runtime/src/lib.rs`.

## Global Constraints

- Do not change public CLI flags, JSONL event names, tool result shapes, approval prompts, or npm package layout.
- Follow existing architecture tests that assert focused runtime module ownership.
- Use TDD: add the failing ownership test before changing `runtime_bash.rs`.
- Each feature slice gets its own commit before release metadata changes.

---

### Task 1: Lock Runtime Bash Internal Execution Contexts With A Failing Test

**Files:**
- Modify: `crates/orca-runtime/src/lib.rs`
- Verify: `crates/orca-runtime/src/runtime_bash.rs`

**Interfaces:**
- Consumes: existing `runtime_bash.rs` module and `bash_runtime_runner_uses_grouped_invocation_context` ownership test.
- Produces: a new ownership test that requires `RuntimeBashSandboxContext`, `RuntimeBashOnceContext`, context-accepting helper signatures, and no `too_many_arguments` escape hatches inside `runtime_bash.rs`.

- [ ] **Step 1: Write the failing ownership test**

Add a test in `crates/orca-runtime/src/lib.rs`:

```rust
#[test]
fn runtime_bash_internal_execution_uses_grouped_contexts() {
    let runtime_bash_source = include_str!("runtime_bash.rs");

    for marker in [
        "struct RuntimeBashSandboxContext",
        "struct RuntimeBashOnceContext",
        "fn execute_bash_with_sandbox(context: RuntimeBashSandboxContext",
        "fn execute_bash_once(context: RuntimeBashOnceContext",
    ] {
        assert!(
            runtime_bash_source.contains(marker),
            "runtime_bash must own grouped internal bash execution detail {marker}"
        );
    }

    assert!(
        !runtime_bash_source.contains("#[allow(clippy::too_many_arguments)]"),
        "runtime_bash internal bash execution must not need too_many_arguments escape hatches"
    );

    for field_name in [
        "command:",
        "cwd:",
        "additional_roots:",
        "sandbox:",
        "shell_timeout_secs:",
        "task_registry:",
        "cancel:",
    ] {
        assert!(
            runtime_bash_source.contains(field_name),
            "RuntimeBashSandboxContext must carry sandbox field {field_name}"
        );
    }

    for field_name in [
        "additional_readable_directories:",
        "additional_working_directories:",
        "denied_working_directories:",
        "allowed_unix_socket_roots:",
        "env:",
        "sandbox:",
    ] {
        assert!(
            runtime_bash_source.contains(field_name),
            "RuntimeBashOnceContext must carry shell-spawn field {field_name}"
        );
    }
}
```

- [ ] **Step 2: Run the test and confirm RED**

Run:

```bash
cargo test -p orca-runtime runtime_bash_internal_execution_uses_grouped_contexts -- --nocapture
```

Expected: FAIL because `RuntimeBashSandboxContext` and `RuntimeBashOnceContext` do not exist yet.

### Task 2: Add The Internal Context Structs And Rewrite Call Sites

**Files:**
- Modify: `crates/orca-runtime/src/runtime_bash.rs`

**Interfaces:**
- Consumes: `RuntimeBashInvocationContext`, `TaskRegistry`, `TurnPermissionOverlay`, `CommandExecSandbox`, `ShellSessionCommand`.
- Produces:
  - `struct RuntimeBashSandboxContext<'a>`
  - `struct RuntimeBashOnceContext<'a>`
  - `fn execute_bash_with_sandbox(context: RuntimeBashSandboxContext<'_>) -> BashExecutionResult`
  - `fn execute_bash_once(context: RuntimeBashOnceContext<'_>) -> BashShellOutput`

- [ ] **Step 1: Add `RuntimeBashSandboxContext`**

Add near `BashShellOutput`:

```rust
struct RuntimeBashSandboxContext<'a> {
    command: &'a str,
    cwd: &'a Path,
    additional_roots: &'a [PathBuf],
    sandbox: &'a crate::server::CommandExecSandbox,
    shell_timeout_secs: u64,
    task_registry: &'a TaskRegistry,
    cancel: Option<&'a CancelToken>,
}
```

- [ ] **Step 2: Add `RuntimeBashOnceContext`**

Add near `RuntimeBashSandboxContext`:

```rust
struct RuntimeBashOnceContext<'a> {
    command: &'a str,
    cwd: &'a Path,
    additional_readable_directories: Vec<PathBuf>,
    additional_working_directories: Vec<PathBuf>,
    denied_working_directories: Vec<PathBuf>,
    allowed_unix_socket_roots: Vec<PathBuf>,
    env: BTreeMap<String, Option<String>>,
    sandbox: ShellSandboxMode,
    shell_timeout_secs: u64,
    task_registry: &'a TaskRegistry,
    cancel: Option<&'a CancelToken>,
}
```

- [ ] **Step 3: Rewrite `execute_bash_with_sandbox`**

Change the helper signature to:

```rust
fn execute_bash_with_sandbox(context: RuntimeBashSandboxContext<'_>) -> BashExecutionResult
```

Destructure the context at the top of the function and keep the existing network proxy, sandbox root merge, command execution, and block-report collection logic unchanged.

- [ ] **Step 4: Rewrite `execute_bash_once`**

Change the helper signature to:

```rust
fn execute_bash_once(context: RuntimeBashOnceContext<'_>) -> BashShellOutput
```

Destructure the context at the top of the function and keep the existing `RuntimeShellSessionManager::spawn`, stdin close, wait-or-cancel, and output wrapping logic unchanged.

- [ ] **Step 5: Update all helper call sites**

Replace each call to `execute_bash_with_sandbox(...)` and `execute_bash_once(...)` with the corresponding context struct literal. Do not change call ordering or retry behavior.

### Task 3: Prove Behavior Is Preserved

**Files:**
- Test: `crates/orca-runtime/src/runtime_bash.rs`
- Test: `crates/orca-runtime/src/lib.rs`
- Test: `tests/runtime_lifecycle_contract.rs`
- Test: `tests/session_server_contract.rs`

- [ ] **Step 1: Run the new ownership test and confirm GREEN**

Run:

```bash
cargo test -p orca-runtime runtime_bash_internal_execution_uses_grouped_contexts -- --nocapture
```

Expected: PASS.

- [ ] **Step 2: Run existing bash ownership coverage**

Run:

```bash
cargo test -p orca-runtime bash_runtime_runner_uses_grouped_invocation_context -- --nocapture
```

Expected: PASS.

- [ ] **Step 3: Run focused bash/runtime tests**

Run:

```bash
cargo test -p orca-runtime bash -- --nocapture
cargo test --test runtime_lifecycle_contract normal_tool -- --nocapture
cargo test --test session_server_contract command_exec -- --nocapture
```

Expected: PASS for model-visible bash execution, normal tool runtime policy, and command/exec compatibility coverage.

- [ ] **Step 4: Run focused runtime all-targets tests**

Run:

```bash
cargo test -p orca-runtime --all-targets -- --test-threads=1
```

Expected: PASS.

- [ ] **Step 5: Run formatting and lint checks**

Run:

```bash
cargo fmt -- --check
git diff --check
cargo clippy -p orca-runtime --all-targets
```

Expected: all exit 0. Existing warnings are acceptable only if clippy exits 0.

### Task 4: Feature Commit

**Files:**
- Stage: `crates/orca-runtime/src/lib.rs`
- Stage: `crates/orca-runtime/src/runtime_bash.rs`
- Stage: `docs/superpowers/plans/2026-07-06-runtime-bash-execution-contexts.md`

- [ ] **Step 1: Review diff**

Run:

```bash
git diff --stat
git diff -- crates/orca-runtime/src/lib.rs crates/orca-runtime/src/runtime_bash.rs
```

Expected: diff only adds the ownership test and groups runtime bash internal execution inputs behind context structs.

- [ ] **Step 2: Commit feature**

Run:

```bash
git add crates/orca-runtime/src/lib.rs crates/orca-runtime/src/runtime_bash.rs docs/superpowers/plans/2026-07-06-runtime-bash-execution-contexts.md
git commit -m "refactor(runtime): group bash execution contexts"
```

Expected: one feature commit with no release version changes.
