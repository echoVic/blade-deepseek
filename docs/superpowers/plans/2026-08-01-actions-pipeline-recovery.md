# GitHub Actions Pipeline Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Restore the Release and Windows GitHub Actions workflows without weakening their compile, behavior, or publication gates.

**Architecture:** Give Linux a concrete `plain_bash_command` implementation so cfg resolution cannot recurse through the outer wrapper. Add a small cross-platform process helper that clears inheritance from the standalone CLI's existing standard handles on Windows before spawning a durable workflow worker; the worker's explicitly requested stdin/stdout pipes remain independently inherited. Keep the six-second workflow behavior contract and add a lower-level Windows EOF regression so both the mechanism and user-visible behavior are covered.

**Tech Stack:** Rust 2024, `std::process`, `windows-sys 0.61`, Cargo workspace tests, GitHub Actions, `gh` CLI

---

## File Map

- Modify `crates/orca-tools/src/sandbox/mod.rs`: provide a concrete Linux plain-shell constructor and Linux-only regression test.
- Modify `crates/orca-platform/src/process.rs`: expose the current-process standard-handle inheritance cleanup helper, with a Windows implementation and non-Windows no-op.
- Modify `crates/orca-platform/tests/process_contract.rs`: prove a background descendant cannot keep its launching parent's captured output pipe open.
- Modify `crates/orca-runtime/src/workflow/command.rs`: invoke the process helper immediately before spawning the standalone workflow worker.
- Modify `tests/workflow_cli_contract.rs`: assert the CLI launch returns before the deterministic six-second provider operation completes and retain live pause/stop behavior.
- Modify `docs/superpowers/specs/2026-08-01-actions-pipeline-recovery-design.md`: record the reviewed, race-free handle strategy.

### Task 1: Repair Linux sandbox dispatch

**Files:**
- Modify: `crates/orca-tools/src/sandbox/mod.rs:221-490`
- Test: `crates/orca-tools/src/sandbox/mod.rs` Linux-only `platform::linux_tests` module

- [ ] **Step 1: Reproduce the Linux cfg failure before editing production code**

Run:

```bash
cargo check -p orca-tools --all-targets --target x86_64-unknown-linux-gnu --locked
```

Expected: FAIL with `E0603` at `crates/orca-tools/src/sandbox/mod.rs:74` and an unconditional-recursion warning for `plain_bash_command`. Save the exact output for the completion audit.

- [ ] **Step 2: Add a Linux-only test that requires a concrete platform implementation**

Add beside the other non-macOS platform tests:

```rust
#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    use super::*;

    #[test]
    fn plain_commands_use_the_native_posix_shell() {
        let cwd = std::env::current_dir().expect("current directory");
        let command = plain_bash_command("printf orca", &cwd);

        assert_eq!(command.get_program(), "sh");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("printf orca")]
        );
        assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
    }
}
```

- [ ] **Step 3: Run the cross-target check and verify the test cannot compile against the broken dispatch**

Run:

```bash
cargo check -p orca-tools --all-targets --target x86_64-unknown-linux-gnu --locked
```

Expected: FAIL before test execution because Linux still has no concrete `platform::plain_bash_command`; the failure must remain on the same dispatch boundary, not move to an unrelated dependency.

- [ ] **Step 4: Add the minimal Linux implementation**

Add before the Windows-specific implementation in the non-macOS platform module:

```rust
#[cfg(target_os = "linux")]
pub fn plain_bash_command(command: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command).current_dir(cwd);
    cmd
}
```

Do not call the outer `crate::sandbox::plain_bash_command`; the outer function is the wrapper applying `prepare_non_interactive_command`.

- [ ] **Step 5: Verify Linux and host cfgs**

Run:

```bash
cargo check -p orca-tools --all-targets --target x86_64-unknown-linux-gnu --locked
cargo test -p orca-tools --lib sandbox --locked
```

Expected: both commands PASS. The first proves the Ubuntu cfg now resolves a concrete function; the second proves the current macOS sandbox tests remain green.

- [ ] **Step 6: Commit the Linux fix**

```bash
git add crates/orca-tools/src/sandbox/mod.rs
git diff --cached --check
git commit -m "fix(sandbox): define Linux plain shell dispatch"
```

### Task 2: Prevent inherited Windows capture handles

**Files:**
- Modify: `crates/orca-platform/src/process.rs:1-80` and Windows `platform` module
- Test: `crates/orca-platform/tests/process_contract.rs`

- [ ] **Step 1: Add a Windows subprocess regression for captured-output EOF**

Add these Windows-only tests to `process_contract.rs`. Reuse the file's existing `ChildWaitTimeout` and `windows_process_is_running` helpers.

```rust
#[cfg(windows)]
#[test]
fn windows_background_child_does_not_hold_parent_capture_open() {
    use std::time::{Duration, Instant};

    let temp = tempfile::tempdir().expect("tempdir");
    let child_pid = temp.path().join("background-child-pid");
    let started = Instant::now();
    let output = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "windows_background_capture_parent_helper",
            "--nocapture",
        ])
        .env("ORCA_BACKGROUND_CHILD_PID", &child_pid)
        .output()
        .expect("run background capture parent");
    let elapsed = started.elapsed();

    assert!(output.status.success(), "helper failed: {output:?}");
    assert!(
        elapsed < Duration::from_secs(2),
        "captured parent output stayed open for {elapsed:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("parent-exit"),
        "parent marker missing: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let pid = std::fs::read_to_string(&child_pid)
        .expect("background child pid")
        .trim()
        .parse::<u32>()
        .expect("valid pid");
    assert!(windows_process_is_running(pid));
    terminate_windows_process(pid).expect("terminate background fixture");
}

#[cfg(windows)]
#[test]
fn windows_background_capture_parent_helper() {
    let Some(child_pid) = std::env::var_os("ORCA_BACKGROUND_CHILD_PID") else {
        return;
    };

    orca_platform::process::clear_current_process_std_handle_inheritance()
        .expect("clear inherited std handles");
    let child = std::process::Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "windows_background_capture_child_helper",
            "--nocapture",
        ])
        .env("ORCA_BACKGROUND_CAPTURE_CHILD", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn background child");
    std::fs::write(child_pid, child.id().to_string()).expect("write child pid");
    println!("parent-exit");
}

#[cfg(windows)]
#[test]
fn windows_background_capture_child_helper() {
    if std::env::var_os("ORCA_BACKGROUND_CAPTURE_CHILD").is_some() {
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}
```

Add a cleanup helper using the already-enabled Windows APIs:

```rust
#[cfg(windows)]
fn terminate_windows_process(pid: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_TERMINATE, TerminateProcess,
    };

    let process = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if process.is_null() {
        return Ok(());
    }
    let terminated = unsafe { TerminateProcess(process, 1) };
    unsafe { CloseHandle(process) };
    if terminated == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
```

- [ ] **Step 2: Compile the Windows test and verify RED**

Run:

```bash
cargo check -p orca-platform --tests --target x86_64-pc-windows-msvc --locked
```

Expected: FAIL because `clear_current_process_std_handle_inheritance` does not exist.

- [ ] **Step 3: Add the cross-platform process helper**

Add the public wrapper to `orca-platform/src/process.rs`:

```rust
/// Prevents subsequently spawned background children from inheriting the
/// current process's existing standard handles. The handles remain open and
/// usable by this process.
pub fn clear_current_process_std_handle_inheritance() -> io::Result<()> {
    platform::clear_current_process_std_handle_inheritance()
}
```

Add the non-Windows no-op:

```rust
pub(super) fn clear_current_process_std_handle_inheritance() -> io::Result<()> {
    Ok(())
}
```

Add the Windows implementation inside the existing Windows `platform` module:

```rust
pub(super) fn clear_current_process_std_handle_inheritance() -> io::Result<()> {
    use windows_sys::Win32::Foundation::{
        GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
        SetHandleInformation,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { GetStdHandle(id) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            continue;
        }
        let mut flags = 0;
        if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if flags & HANDLE_FLAG_INHERIT != 0
            && unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0)
            } == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
```

- [ ] **Step 4: Verify the Windows process contract compiles**

Run:

```bash
cargo check -p orca-platform --tests --target x86_64-pc-windows-msvc --locked
cargo test -p orca-platform --locked
```

Expected: both PASS. The first compiles the Windows-only regression; the second executes the native host process contracts. Runtime proof of the new Windows test will come from the x64 and ARM64 Actions jobs.

- [ ] **Step 5: Commit the process boundary**

```bash
git add crates/orca-platform/src/process.rs crates/orca-platform/tests/process_contract.rs
git diff --cached --check
git commit -m "fix(windows): stop background capture handle inheritance"
```

### Task 3: Apply the boundary to workflow workers

**Files:**
- Modify: `crates/orca-runtime/src/workflow/command.rs:668-750`
- Test: `tests/workflow_cli_contract.rs:190-370`

- [ ] **Step 1: Strengthen the workflow launch contract before production changes**

In `workflow_run_returns_before_slow_workflow_completes`, measure the launch command and retain stdout/stderr in the assertion:

```rust
let started = Instant::now();
let run = Command::new(env!("CARGO_BIN_EXE_orca"))
    .current_dir(temp.path())
    .env("ORCA_HOME", &home)
    .args([
        "workflow",
        "run",
        "--provider",
        "mock",
        script.to_str().unwrap(),
    ])
    .output()
    .expect("run workflow");
let elapsed = started.elapsed();

assert_eq!(
    run.status.code(),
    Some(0),
    "workflow launch failed: stdout={} stderr={}",
    String::from_utf8_lossy(&run.stdout),
    String::from_utf8_lossy(&run.stderr)
);
assert!(
    elapsed < Duration::from_secs(5),
    "workflow run blocked for {elapsed:?}; it must return before the 6s model call completes"
);
```

Keep `wait_until_active` and the existing terminal-status assertion. Do not accept `completed` as an active state.

- [ ] **Step 2: Run the native contract as a baseline and compile it for Windows**

Run:

```bash
cargo test --test workflow_cli_contract workflow_run_returns_before_slow_workflow_completes --locked -- --exact --nocapture
cargo check --test workflow_cli_contract --target x86_64-pc-windows-msvc --locked
```

Expected: macOS PASS because Unix does not inherit unrelated descriptors across exec. Windows compilation PASS, while prior Actions evidence `30649340706` supplies the RED runtime result: about 6.86 seconds and first state `completed`.

- [ ] **Step 3: Clear inherited outer std handles immediately before worker spawn**

In `spawn_workflow_worker`, after fully configuring the worker command and before `command.spawn()`, add:

```rust
if let Err(error) =
    orca_platform::process::clear_current_process_std_handle_inheritance()
{
    eprintln!(
        "orca: failed to isolate workflow worker standard handles: {error}"
    );
    return 1;
}
```

Do not use `DETACHED_PROCESS`, do not change the launch-response pipe, and do not weaken API-key stdin handoff or worker cleanup.

- [ ] **Step 4: Run workflow behavior gates**

Run:

```bash
cargo test --test workflow_cli_contract --locked -- --test-threads=1 --nocapture
cargo check --test workflow_cli_contract --target x86_64-pc-windows-msvc --locked
```

Expected: 10/10 native tests PASS and Windows cross-compilation PASS. The three live-control tests must remain enabled.

- [ ] **Step 5: Commit the workflow fix**

```bash
git add crates/orca-runtime/src/workflow/command.rs tests/workflow_cli_contract.rs
git diff --cached --check
git commit -m "fix(workflow): detach Windows launch capture lifetime"
```

### Task 4: Local integration verification and review

**Files:**
- Inspect: every file changed since `ddafc1c0`
- Update only if verification reveals a defect

- [ ] **Step 1: Format and run targeted static checks**

```bash
cargo fmt --all -- --check
cargo check -p orca-tools --all-targets --target x86_64-unknown-linux-gnu --locked
cargo check -p orca-platform --tests --target x86_64-pc-windows-msvc --locked
cargo check --test workflow_cli_contract --target x86_64-pc-windows-msvc --locked
node scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-windows-platform-boundaries.mjs
```

Expected: every command PASS. If formatting fails, run `cargo fmt --all`, inspect only the intended formatting changes, and rerun the check.

- [ ] **Step 2: Run focused behavioral suites**

```bash
cargo test -p orca-tools --lib sandbox --locked
cargo test -p orca-platform --locked
cargo test --test workflow_cli_contract --locked -- --test-threads=1 --nocapture
```

Expected: every command PASS.

- [ ] **Step 3: Run the Release-equivalent local workspace gate**

```bash
RUST_BACKTRACE=1 cargo test --workspace --all-targets --locked -- --test-threads=1
```

Expected: PASS. If an intermittent failure occurs in unchanged sources, rerun the exact failing test once, retain both logs, and describe it only as an intermittent failure in unchanged sources unless causality is established. Do not silently ignore or skip it.

- [ ] **Step 4: Inspect the complete change and request code review**

```bash
git diff ddafc1c0..HEAD --check
git diff --stat ddafc1c0..HEAD
git status --short --branch
```

Invoke `superpowers:requesting-code-review` with the design, plan, CI failure evidence, and diff from `ddafc1c0`. Resolve every confirmed blocker and rerun affected tests.

- [ ] **Step 5: Commit any review-only corrections**

If review requires changes, stage only the files already owned by this plan:

```bash
git add crates/orca-tools/src/sandbox/mod.rs \
  crates/orca-platform/src/process.rs \
  crates/orca-platform/tests/process_contract.rs \
  crates/orca-runtime/src/workflow/command.rs \
  tests/workflow_cli_contract.rs \
  docs/superpowers/specs/2026-08-01-actions-pipeline-recovery-design.md \
  docs/superpowers/plans/2026-08-01-actions-pipeline-recovery.md
git diff --cached --check
git commit -m "fix(ci): address pipeline recovery review"
```

If no corrections are required, do not create an empty commit.

### Task 5: Remote Actions verification

**Files:**
- Remote branch: `origin/main`
- Workflows: `.github/workflows/windows-ci.yml`, `.github/workflows/release.yml`

- [ ] **Step 1: Confirm push target and publish the current branch**

```bash
git status --short --branch
git log --oneline origin/main..HEAD
git push origin main
git rev-parse HEAD
git ls-remote origin refs/heads/main
```

Expected: clean worktree; local `HEAD`, `origin/main`, and remote SHA match exactly. The existing `v0.3.0` tag remains at `ddafc1c0`.

- [ ] **Step 2: Watch the push-triggered Windows workflow**

Resolve the run for the pushed SHA, then watch it:

```bash
sha=$(git rev-parse HEAD)
windows_run_id=$(gh run list \
  --repo echoVic/orca-agent \
  --workflow Windows \
  --commit "$sha" \
  --limit 5 \
  --json databaseId,headSha \
  --jq '.[0].databaseId')
test -n "$windows_run_id"
gh run watch "$windows_run_id" --repo echoVic/orca-agent --exit-status
```

Expected: both `native-x64` and `native-arm64` conclude `success`. Inspect job metadata to ensure the full-suite steps ran rather than being skipped.

- [ ] **Step 3: Trigger a non-publishing Release validation for the new SHA**

```bash
gh workflow run release.yml --repo echoVic/orca-agent --ref main -f version=0.3.0
```

Resolve the run whose `headSha` equals local `HEAD`, then watch it:

```bash
sha=$(git rev-parse HEAD)
release_run_id=$(gh run list \
  --repo echoVic/orca-agent \
  --workflow Release \
  --commit "$sha" \
  --event workflow_dispatch \
  --limit 5 \
  --json databaseId,headSha \
  --jq '.[0].databaseId')
test -n "$release_run_id"
gh run watch "$release_run_id" --repo echoVic/orca-agent --exit-status
```

Expected: `test`, `version`, and all six `build` matrix jobs succeed. Tag-only `npm-auth`, `release`, `npm`, `npm-release-assets`, `verify`, and `verify-windows` jobs are skipped by design; no package or GitHub Release is published.

- [ ] **Step 4: If a remote-only failure remains, return to root-cause analysis**

```bash
failed_run_id=$windows_run_id # or: failed_run_id=$release_run_id
gh run view "$failed_run_id" --repo echoVic/orca-agent --json jobs,headSha,conclusion,url
gh run view "$failed_run_id" --repo echoVic/orca-agent --log-failed
```

Expected: no failure. If there is one, form a single new hypothesis from the exact failed step, add or strengthen a regression before changing production code, push the focused correction, and repeat Steps 2-3. Do not rerun a red workflow as a substitute for a code fix.

### Task 6: Completion audit

**Files and evidence:**
- Design: `docs/superpowers/specs/2026-08-01-actions-pipeline-recovery-design.md`
- Plan: `docs/superpowers/plans/2026-08-01-actions-pipeline-recovery.md`
- Production files and tests listed in the File Map
- GitHub Windows and Release run URLs for the final SHA

- [ ] **Step 1: Build the objective-to-artifact checklist**

Record concrete evidence for each objective:

1. Release Linux compile/test failure fixed: Linux cross-target check plus green Release `test` job.
2. Windows x64 fixed: green `native-x64`, including full suite and workflow contract.
3. Windows ARM64 fixed: green `native-arm64`, including full suite and workflow contract.
4. Asynchronous user contract preserved: elapsed-time assertion and active-state observation.
5. Live controls preserved: stop/pause/resume/clone tests executed, not skipped.
6. No gate weakening: inspect workflow diff and job step conclusions.
7. No historical release mutation: compare `refs/tags/v0.3.0` to `ddafc1c0`; confirm dispatch run skipped publication jobs.

- [ ] **Step 2: Run final repository integrity checks**

```bash
git status --short --branch
git diff --check origin/main...HEAD
git rev-parse HEAD
git rev-parse v0.3.0
gh run list --repo echoVic/orca-agent --commit "$(git rev-parse HEAD)" --limit 10
```

Expected: clean and synchronized `main`; no diff whitespace failures; `v0.3.0` still resolves to `ddafc1c03bda625c67569ebb3bb33e2dd9cd2a93`; final Windows and Release runs are green and point to the exact final SHA.

- [ ] **Step 3: Mark the goal complete only if every checklist row has direct evidence**

If any matrix job, behavior gate, SHA match, or no-publication condition is absent or uncertain, keep working. Otherwise call `update_goal(status="complete")` and report the run URLs, exact final SHA, local verification commands, and elapsed goal time.
