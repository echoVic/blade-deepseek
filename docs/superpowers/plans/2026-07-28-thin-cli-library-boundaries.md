# Thin CLI Library Boundaries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `src/cli.rs` a sub-1,000-line Clap parsing and forwarding adapter by moving command behavior to capability-owning library crates without changing CLI behavior.

**Architecture:** Private Clap structs convert into owned, Clap-free request types. `orca-core` owns effective config resolution, `orca-runtime` owns non-visual command and workflow use cases, and `orca-tui` owns interactive startup and update prompt rendering. Structural tests enforce ownership in addition to command-level parity tests.

**Tech Stack:** Rust 2024, Clap, serde/serde_json, crossterm, Cargo workspace tests, process-level CLI contract tests.

---

## File Map

### Files created

- `tests/cli_architecture_contract.rs`: rejects business implementation in the root CLI and re-export shims in `main.rs`.
- `crates/orca-runtime/src/command/mod.rs`: exports focused non-visual CLI use cases.
- `crates/orca-runtime/src/command/config.rs`: builds effective `RunConfig` values from typed overrides.
- `crates/orca-runtime/src/command/exec.rs`: owns `orca exec`, stdin prompt composition, validation, and controller launch.
- `crates/orca-runtime/src/command/history.rs`: owns history command operations and textual output.
- `crates/orca-runtime/src/command/trust.rs`: owns folder-trust command operations and textual output.
- `crates/orca-runtime/src/command/launch.rs`: owns server, ACP, and async-subagent-worker launch preparation.
- `crates/orca-runtime/src/workflow/command.rs`: owns workflow command, worker process, persistence, and control behavior.
- `crates/orca-tui/src/cli.rs`: owns default interactive launch plus update prompt terminal presentation.

### Files modified

- `crates/orca-core/src/config/file.rs`: adds effective environment/file/CLI override resolution.
- `crates/orca-runtime/src/lib.rs`: exports `command`.
- `crates/orca-runtime/src/update_check.rs`: expands to non-visual update preflight/install behavior.
- `crates/orca-runtime/src/workflow/mod.rs`: exports `command`.
- `crates/orca-tui/src/lib.rs`: exports the interactive CLI facade.
- `src/cli.rs`: retains only Clap definitions, conversions, and dispatch.
- `src/main.rs`: retains only `mod cli` and process exit.
- `Cargo.toml`: removes root implementation dependencies and moves test-only dependencies under dev dependencies.

### Files removed

The root re-export shims become unused and are deleted: `src/acp.rs`,
`src/approval/mod.rs`, `src/config/mod.rs`, `src/event/mod.rs`, `src/mcp/mod.rs`,
`src/mentions.rs`, `src/model.rs`, `src/provider/mod.rs`, `src/runtime/mod.rs`,
`src/sandbox/mod.rs`, `src/server.rs`, `src/tools/mod.rs`, `src/tui/mod.rs`, and
`src/verification/mod.rs`.

---

### Task 1: Add the architectural failure gate

**Files:**
- Create: `tests/cli_architecture_contract.rs`

- [ ] **Step 1: Write the failing architecture test**

Create this source-level contract:

```rust
use std::fs;

#[test]
fn root_cli_is_only_argument_parsing_conversion_and_forwarding() {
    let cli = fs::read_to_string("src/cli.rs").expect("read root CLI");
    assert!(cli.lines().count() < 1_000, "root CLI must stay below 1,000 lines");
    assert!(cli.contains("Cli::parse()"));
    for forbidden in [
        "WorkflowRunner", "WorkflowStateStore", "ProcessCommand",
        "terminal::enable_raw_mode", "fs::write", "RunConfig {",
        "check_latest_for_prompt", "#[cfg(test)]",
    ] {
        assert!(!cli.contains(forbidden), "root CLI still owns {forbidden}");
    }
    for facade in [
        "orca_runtime::command::exec", "orca_runtime::command::history",
        "orca_runtime::command::trust", "orca_runtime::workflow::command",
        "orca_runtime::command::launch", "orca_tui::cli",
    ] {
        assert!(cli.contains(facade), "root CLI does not forward through {facade}");
    }
}

#[test]
fn binary_entrypoint_has_no_library_reexport_shims() {
    let main = fs::read_to_string("src/main.rs").expect("read main");
    assert_eq!(main.matches("mod ").count(), 1);
    assert!(main.contains("mod cli;"));
    assert!(!main.contains("mod runtime;"));
    assert!(!main.contains("mod config;"));
}

#[test]
fn workflow_and_update_behavior_live_in_library_crates() {
    let workflow = fs::read_to_string("crates/orca-runtime/src/workflow/command.rs")
        .expect("workflow command library module");
    assert!(workflow.contains("pub enum WorkflowCommandRequest"));
    assert!(workflow.contains("pub fn run("));
    assert!(workflow.contains("spawn_workflow_worker"));
    let update = fs::read_to_string("crates/orca-runtime/src/update_check.rs")
        .expect("update library module");
    assert!(update.contains("pub enum UpdateAction"));
    assert!(update.contains("pub fn run_update"));
}
```

- [ ] **Step 2: Run the test and verify the intended RED state**

Run: `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test cli_architecture_contract -- --test-threads=1`

Expected: FAIL because `src/cli.rs` has 3,093 lines and owns the forbidden workflow, process, terminal, config, and update code.

- [ ] **Step 3: Commit the red architecture contract**

```bash
git add tests/cli_architecture_contract.rs
git commit -m "test(cli): define thin binary boundary" -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Move effective configuration resolution into `orca-core`

**Files:**
- Modify: `crates/orca-core/src/config/file.rs`

- [ ] **Step 1: Add failing tests beside `ConfigOverrides`**

Add serial environment tests for the new API:

```rust
#[test]
fn effective_config_applies_file_environment_and_cli_in_order() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("config.toml"), "model = 'file-model'\n").unwrap();
    let _guard = test_env_lock();
    unsafe { std::env::set_var("ORCA_MODEL", "env-model") };
    let config = load_effective_config(
        temp.path(),
        ConfigOverrides { model: Some("cli-model".into()), ..Default::default() },
    ).unwrap();
    unsafe { std::env::remove_var("ORCA_MODEL") };
    assert_eq!(config.model, "cli-model");
}

#[test]
fn effective_config_rejects_invalid_environment_mode() {
    let temp = tempfile::tempdir().unwrap();
    let _guard = test_env_lock();
    unsafe { std::env::set_var("ORCA_MODE", "reckless") };
    let error = load_effective_config(temp.path(), ConfigOverrides::default()).unwrap_err();
    unsafe { std::env::remove_var("ORCA_MODE") };
    assert!(error.contains("unsupported mode 'reckless'"));
}
```

- [ ] **Step 2: Verify RED**

Run: `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-core config::file::tests::effective_config -- --test-threads=1`

Expected: compilation failure for missing `load_effective_config`.

- [ ] **Step 3: Implement effective config resolution**

Add:

```rust
pub fn load_effective_config(cwd: &Path, cli: ConfigOverrides) -> Result<FileConfig, String> {
    let file = load_layered_config(cwd);
    let env = environment_overrides()?;
    Ok(apply_override_layers(file, env, cli))
}

pub fn environment_overrides() -> Result<ConfigOverrides, String> {
    Ok(ConfigOverrides {
        model: std::env::var("ORCA_MODEL").ok()
            .or_else(|| std::env::var("DEEPSEEK_MODEL").ok()),
        mode: std::env::var("ORCA_MODE").ok()
            .map(|value| parse_approval_mode(&value)).transpose()?,
        api_key: std::env::var("ORCA_API_KEY").ok()
            .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok()),
        base_url: std::env::var("ORCA_BASE_URL").ok()
            .or_else(|| std::env::var("DEEPSEEK_BASE_URL").ok()),
        reasoning_effort: std::env::var("ORCA_REASONING_EFFORT").ok()
            .or_else(|| std::env::var("DEEPSEEK_REASONING_EFFORT").ok())
            .map(|value| parse_reasoning_effort(&value)).transpose()?,
    })
}
```

Use the exact existing error strings for invalid approval mode and reasoning effort.

- [ ] **Step 4: Run core configuration tests**

Run: `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-core config::file::tests -- --test-threads=1`

Expected: all config file tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-core/src/config/file.rs
git commit -m "refactor(core): own effective config resolution" -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Establish runtime command configuration foundations

**Files:**
- Create: `crates/orca-runtime/src/command/mod.rs`
- Create: `crates/orca-runtime/src/command/config.rs`
- Modify: `crates/orca-runtime/src/lib.rs`

- [ ] **Step 1: Add failing request-to-config tests**

Define typed, Clap-free override inputs for exec, interactive, workflow worker,
protocol, and subagent worker launches. Test that each constructor preserves
provider/model/cwd/history/output/update settings and uses
`orca_core::config::file::load_effective_config` for file/environment/CLI
precedence.

- [ ] **Step 2: Verify RED**

Run: `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime command::config::tests -- --test-threads=1`

Expected: compilation failure because `command::config` does not exist.

- [ ] **Step 3: Implement focused config constructors**

Create request structs with owned fields and constructor functions that parse
`ModelSelection`, load default external tools directly from `orca_tools`,
and return the exact existing `RunConfig` shapes. No constructor performs
command output, process spawning, terminal access, or controller launch.

- [ ] **Step 4: Run focused tests and workspace check**

```bash
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime command::config::tests -- --test-threads=1
cargo check -p orca-runtime
```

Expected: both pass.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/command crates/orca-runtime/src/lib.rs
git commit -m "refactor(runtime): add command config boundary" -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Move update behavior to runtime and terminal presentation to TUI

**Files:**
- Modify: `crates/orca-runtime/src/update_check.rs`
- Create: `crates/orca-tui/src/cli.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `src/cli.rs`

- [ ] **Step 1: Add failing runtime update tests**

Move the current action/command tests from `src/cli.rs` and target these public types:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateAction {
    NpmGlobalLatest,
    StandaloneInstaller { install_dir: Option<PathBuf> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateCommand {
    pub program: &'static str,
    pub args: Vec<String>,
    pub display: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateRunOutcome { Updated, Failed(Option<i32>), StartFailed(String) }
```

Assert npm wrapper detection, standalone install-dir preservation, download-before-execute construction, development-run suppression, and exit mapping.

- [ ] **Step 2: Verify RED**

Run: `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime update_check::tests -- --test-threads=1`

Expected: compilation failure for the new update action/execution API.

- [ ] **Step 3: Implement the non-visual update service**

Move action/command detection, installer construction, development-build detection, and command execution into `update_check.rs`. Expose:

```rust
pub fn current_update_action() -> UpdateAction;
pub fn update_preflight(enabled: bool, current: &str) -> UpdatePreflight;
pub fn run_update(action: &UpdateAction) -> UpdateRunOutcome;
```

Keep check/dismiss cache behavior and file format unchanged.

- [ ] **Step 4: Add failing TUI update presentation tests**

Move navigation/rendering assertions into `crates/orca-tui/src/cli.rs`. Use injected writer/key-source helpers to assert the exact three choices, command display, wraparound, quit, and skip without touching the terminal.

- [ ] **Step 5: Implement the TUI adapter**

Define `InteractiveLaunchRequest` and `pub fn run(request: InteractiveLaunchRequest) -> i32`. It receives a prepared config request, performs update preflight, restores raw mode through RAII, maps choices to dismiss/update/continue/quit, and calls `run_tui`.

- [ ] **Step 6: Replace root update/default launch code with conversion and one call**

Remove crossterm, filesystem, process, and update implementation imports from `src/cli.rs`.

- [ ] **Step 7: Run parity tests**

```bash
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime update_check::tests -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-tui cli::tests -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test tui_pty_contract -- --test-threads=1
```

Expected: all selected tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/orca-runtime/src/update_check.rs crates/orca-tui/src/cli.rs crates/orca-tui/src/lib.rs src/cli.rs
git commit -m "refactor(update): move launch flow behind libraries" -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Move workflow CLI behavior into `orca-runtime`

**Files:**
- Create: `crates/orca-runtime/src/workflow/command.rs`
- Modify: `crates/orca-runtime/src/workflow/mod.rs`
- Modify: `src/cli.rs`

- [ ] **Step 1: Write failing library tests**

Move root credential/launch-record tests into the new module and target:

```rust
pub enum WorkflowCommandRequest {
    Run(WorkflowRunRequest),
    List(WorkflowListRequest),
    Show { task_id: String },
    Source { name: String },
    Stop { task_id: String },
    Pause { task_id: String },
    Resume { run_id: String },
    Clone { run_id: String },
    Restart { run_id: String, phase: Option<String> },
    Worker(WorkflowWorkerRequest),
}
```

Preserve the 64 KiB bound, no-key-in-argv/launch-record properties, symlink/oversize rejection, atomic legacy migration, and effective-key handoff.

- [ ] **Step 2: Verify RED**

Run: `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime workflow::command::tests -- --test-threads=1`

Expected: compilation failure because the module does not exist.

- [ ] **Step 3: Move workflow implementation with crate-native imports**

Move the complete root workflow command/worker/persistence region into `workflow/command.rs`. Expose:

```rust
pub fn run(request: WorkflowCommandRequest) -> i32;
pub fn run_with_io(
    request: WorkflowCommandRequest,
    stdin: impl Read,
    stdout: impl Write,
    stderr: impl Write,
) -> i32;
```

Keep response field names, ordering, async return, state paths, and launch-record shape compatible.

- [ ] **Step 4: Replace root workflow implementation with conversion/forwarding**

Keep only Clap workflow types and `From` implementations. The dispatch arm calls `orca_runtime::workflow::command::run(args.into())`.

- [ ] **Step 5: Run workflow contracts serially**

```bash
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime workflow::command::tests -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test workflow_runtime_contract -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test workflow_cli_contract -- --test-threads=1
```

Expected: all selected tests pass, including all 10 CLI contracts.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/workflow/command.rs crates/orca-runtime/src/workflow/mod.rs src/cli.rs
git commit -m "refactor(workflow): own CLI lifecycle in runtime" -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 6: Move exec, history, and trust to runtime command modules

**Files:**
- Create: `crates/orca-runtime/src/command/exec.rs`
- Create: `crates/orca-runtime/src/command/history.rs`
- Create: `crates/orca-runtime/src/command/trust.rs`
- Modify: `src/cli.rs`

- [ ] **Step 1: Add failing exec stdin tests**

Define `ExecCommandRequest` and:

```rust
pub fn resolve_prompt(
    prompt_args: Vec<String>,
    stdin_is_terminal: bool,
    mut stdin: impl Read,
) -> Result<String, String>;
```

Test explicit args, `-`, omitted prompt, empty input, and appended `<stdin>` context with exact current errors.

- [ ] **Step 2: Verify RED**

Run: `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime command::exec::tests -- --test-threads=1`

Expected: compilation failure because `command` does not exist.

- [ ] **Step 3: Implement exec launch using the shared config boundary**

`command/exec.rs` uses the Task 3 config constructor and preserves flag
exclusivity, history fallback, stdin handling, config precedence, and
controller launch with injected I/O for tests.

- [ ] **Step 4: Add failing history writer tests and implement history facade**

Define `HistoryCommandRequest` with all existing variants; test list/show through an in-memory writer and move all history operations/message formatting without output changes.

- [ ] **Step 5: Add failing trust writer tests and implement trust facade**

Define:

```rust
pub enum TrustAction { Show, Add, Remove }
pub struct TrustCommandRequest { pub cwd: Option<PathBuf>, pub action: TrustAction }
```

Test unknown/show, add, and remove against an isolated trust store with exact messages.

- [ ] **Step 6: Replace root implementations with conversions and forwarding**

Remove root config, stdin, exec, history, trust, message rendering, and history helpers.

- [ ] **Step 7: Run command parity tests**

```bash
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime command:: -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test exec_jsonl -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test history_contract -- --test-threads=1
```

Expected: all selected tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/orca-runtime/src/command src/cli.rs
git commit -m "refactor(runtime): own core CLI use cases" -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 7: Move protocol and hidden-worker launch preparation to runtime

**Files:**
- Create: `crates/orca-runtime/src/command/launch.rs`
- Modify: `crates/orca-runtime/src/command/mod.rs`
- Modify: `crates/orca-runtime/src/acp/mod.rs`
- Modify: `src/cli.rs`

- [ ] **Step 1: Add failing request validation tests**

Define:

```rust
pub enum RuntimeLaunchRequest {
    Server(ProtocolLaunchRequest),
    Acp(ProtocolLaunchRequest),
    SubagentWorker(SubagentWorkerLaunchRequest),
}
```

Assert server/ACP reject simultaneous subcommand/prompt with exact diagnostics and the subagent worker delegates to `subagent_async_worker::run_async_subagent_worker` with runtime-owned config.

- [ ] **Step 2: Verify RED**

Run: `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime command::launch::tests -- --test-threads=1`

Expected: compilation failure because the facade does not exist.

- [ ] **Step 3: Implement runtime launch facade**

Move server, ACP, subagent worker, and worker-config construction into `command/launch.rs`. Move the `RuntimeHost::start` ACP wrapper into the runtime ACP module.

- [ ] **Step 4: Replace root launch setup with request conversion/forwarding**

Top-level mode dispatch and the hidden worker arm construct typed launch requests only.

- [ ] **Step 5: Run contracts**

```bash
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime command::launch::tests -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test session_server_contract -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test subagent_contract -- --test-threads=1
```

Expected: all selected tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/command crates/orca-runtime/src/acp/mod.rs src/cli.rs
git commit -m "refactor(runtime): own protocol and worker launch" -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 8: Finish the thin adapter and remove root shims

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `Cargo.toml`
- Delete: all root shim files from the File Map
- Modify: `tests/cli_architecture_contract.rs`

- [ ] **Step 1: Tighten the architecture contract**

Require fewer than 1,000 lines, exactly one `mod` in `main.rs`, all facade references, no business tests/forbidden calls, absence of each shim, and root normal dependencies limited to `orca-core`, `orca-runtime`, `orca-tui`, and `clap`.

- [ ] **Step 2: Run the contract and record remaining RED assertions**

Run: `CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test cli_architecture_contract -- --test-threads=1`

Expected: FAIL listing remaining implementation/shim/dependency residue.

- [ ] **Step 3: Delete shims and simplify `main.rs`**

```rust
mod cli;

fn main() {
    std::process::exit(cli::run());
}
```

- [ ] **Step 4: Remove unused root dependencies**

Keep only parsing/forwarding dependencies; retain integration-only libraries under dev dependencies. Run `cargo check --workspace --all-targets` after manifest reduction.

- [ ] **Step 5: Make `src/cli.rs` purely declarative**

Non-derive functions are limited to `From` conversions and one dispatcher. No I/O, config loading, state access, process management, terminal control, or runtime construction remains.

- [ ] **Step 6: Run gates**

```bash
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test cli_architecture_contract -- --test-threads=1
cargo check --workspace --all-targets
```

Expected: both pass.

- [ ] **Step 7: Commit**

```bash
git add -A Cargo.toml src tests/cli_architecture_contract.rs
git commit -m "refactor(cli): keep only parsing and forwarding" -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 9: Final parity verification and completion audit

**Files:**
- Modify only if verification exposes a regression in an owned boundary.

- [ ] **Step 1: Format and reject whitespace drift**

```bash
cargo fmt --all -- --check
git diff --check origin/main...HEAD
```

Expected: both exit 0. If formatting is needed, run `cargo fmt --all`, inspect, and commit with the required trailer.

- [ ] **Step 2: Run focused library tests serially**

```bash
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-core config::file::tests -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime command:: -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime workflow::command::tests -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-runtime update_check::tests -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test -p orca-tui cli::tests -- --test-threads=1
```

Expected: every focused test passes.

- [ ] **Step 3: Run command-level parity contracts serially**

```bash
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test cli_architecture_contract -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test exec_jsonl -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test history_contract -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test workflow_cli_contract -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test session_server_contract -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test subagent_contract -- --test-threads=1
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --test tui_pty_contract -- --test-threads=1
```

Expected: every contract passes.

- [ ] **Step 4: Run workspace compilation and broad comparison**

```bash
cargo check --workspace --all-targets
cargo build --workspace
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 cargo test --workspace
```

Expected: check/build pass. Broad-test failures must be compared with the identical clean baseline; rerun any baseline failure individually and serially before classifying it as pre-existing.

- [ ] **Step 5: Perform the prompt-to-artifact audit**

```bash
git worktree list --porcelain
git status --short --branch
wc -l src/cli.rs
sed -n '1,220p' src/cli.rs
sed -n '1,80p' src/main.rs
rg -n "WorkflowRunner|WorkflowStateStore|ProcessCommand|enable_raw_mode|RunConfig \{|check_latest_for_prompt|#\[cfg\(test\)\]" src/cli.rs
rg -n "pub enum WorkflowCommandRequest|pub fn run\(" crates/orca-runtime/src/workflow/command.rs
rg -n "pub enum UpdateAction|pub fn run_update" crates/orca-runtime/src/update_check.rs
git diff --stat origin/main...HEAD
git log --format=fuller --oneline origin/main..HEAD
```

Map evidence to: isolated worktree, thin CLI, workflow runtime ownership, update runtime/TUI ownership, removal of all other business logic, shim/dependency cleanup, architecture guard, behavior parity, credential safety, persisted formats, compilation, and tests. Missing or ambiguous evidence is incomplete.

- [ ] **Step 6: Verify trailers and clean state**

```bash
git log --format='%H%n%B%n---' origin/main..HEAD
git status --short --branch
```

Expected: every implementation commit ends with exactly one `Co-authored-by: TRAE CLI <noreply@bytedance.com>` trailer and the worktree is clean.
