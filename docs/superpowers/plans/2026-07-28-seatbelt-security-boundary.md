# Seatbelt Security Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make macOS Seatbelt an explicit fail-closed shell security boundary, prevent broad writable grants from reopening workspace metadata, and harden policy construction and behavior tests.

**Architecture:** Keep one sandbox process per shell command. Route ordinary writable roots and explicitly approved metadata roots through separate fields from permission resolution to the platform sandbox, enforce the same distinction on macOS and Linux, and construct macOS policy from an embedded static base plus `sandbox-exec -DKEY=value` path parameters. `DangerFullAccess` remains the only mode that launches a plain shell.

**Tech Stack:** Rust 2024, macOS Seatbelt SBPL, Linux bubblewrap policy builder, Cargo unit and integration tests.

---

### Task 1: Finish R1 fail-closed command construction

**Files:**
- Modify: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Test: `crates/orca-tools/src/sandbox/seatbelt.rs`

- [ ] **Step 1: Add command-construction regression tests**

Add tests that inspect `Command::get_program()` and execute a sandboxed command with a fake `sandbox-exec` first in `PATH`. Assert the program is `/usr/bin/sandbox-exec`, the fake executable never runs, and the actual command succeeds.

- [ ] **Step 2: Run the tests to verify the old implementation fails**

Run:

```bash
cargo test -p orca-tools sandbox::seatbelt::tests::seatbelt_command_uses_trusted_absolute_executable -- --exact
cargo test -p orca-tools sandbox::seatbelt::tests::path_cannot_override_seatbelt_executable -- --exact
```

Expected before the R1 implementation: the command program is `sandbox-exec` and the fake executable is selected from `PATH`.

- [ ] **Step 3: Use the trusted executable without fallback**

Keep:

```rust
const SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";
```

Construct workspace-write, read-only, and availability probes with `Command::new(SEATBELT_EXECUTABLE)`. Do not call a plain-shell fallback from either sandboxed constructor.

- [ ] **Step 4: Verify R1 tests pass**

Run the two exact tests above and expect both to pass.

### Task 2: Separate ordinary and metadata writable grants

**Files:**
- Modify: `crates/orca-tools/src/sandbox/mod.rs`
- Modify: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Modify: `crates/orca-runtime/src/shell_session.rs`
- Modify: `crates/orca-runtime/src/server/command_exec_sandbox.rs`
- Modify: `crates/orca-runtime/src/runtime_bash.rs`
- Modify: `crates/orca-runtime/src/runtime_permission.rs`
- Modify: shell command construction call sites found by `rg "ShellSessionCommand \\{" crates tests`
- Test: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Test: `crates/orca-runtime/src/server/command_exec_sandbox.rs`
- Test: `crates/orca-runtime/src/runtime_permission.rs`

- [ ] **Step 1: Write failing classification and policy tests**

Cover these cases:

```text
/repo/.git              -> metadata writable root
/repo/.agents           -> metadata writable root
/repo/.codex            -> metadata writable root
/repo                    -> ordinary writable root
/repo/.git/config       -> ordinary writable root
```

Add behavior tests proving that a workspace parent supplied as an ordinary writable root cannot write `.git`, `.agents`, or `.codex`, while an exact metadata grant can write the matching metadata target.

- [ ] **Step 2: Run focused tests and verify they fail**

Run:

```bash
cargo test -p orca-tools sandbox::seatbelt::tests::workspace_write_sandbox_parent_root_cannot_write_workspace_metadata -- --exact
cargo test -p orca-runtime runtime_permission::tests::approved_exact_metadata_roots_are_kept_separate -- --exact
```

Expected: missing metadata classification or a broad ordinary root reopens metadata.

- [ ] **Step 3: Carry two writable-root channels**

Add `metadata_writable_roots` beside `additional_writable_roots` in the resolved command sandbox and add `metadata_writable_directories` beside `additional_working_directories` in shell spawn data. Partition only exact final path components named `.git`, `.agents`, or `.codex` into the metadata channel after an explicit permission/profile grant. Keep repository parents and descendants such as `.git/config` in the ordinary channel.

- [ ] **Step 4: Enforce the split on macOS and Linux**

On macOS, emit ordinary allow rules before metadata deny rules and exact metadata allow rules after them. Deny both the exact metadata path and its subtree.

On Linux, mount existing protected metadata read-only regardless of ordinary writable roots. Skip that read-only protection only when the exact metadata path appears in `metadata_writable_roots`.

- [ ] **Step 5: Verify focused cross-layer tests pass**

Run:

```bash
cargo test -p orca-tools sandbox::seatbelt::tests -- --test-threads=1
cargo test -p orca-runtime runtime_permission::tests -- --test-threads=1
cargo test -p orca-runtime server::command_exec_sandbox -- --test-threads=1
```

Expected: all selected tests pass.

### Task 3: Move static policy to SBPL and parameterize paths

**Files:**
- Create: `crates/orca-tools/src/sandbox/seatbelt_base_policy.sbpl`
- Modify: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Test: `crates/orca-tools/src/sandbox/seatbelt.rs`

- [ ] **Step 1: Write failing policy-construction tests**

Add tests asserting:

```text
the embedded policy contains the static deny/process/device baseline
dynamic path rules use (param "KEY")
every filesystem path is passed as a -DKEY=value argument
a path containing a newline or SBPL-looking text never appears in policy source
```

- [ ] **Step 2: Run focused tests and verify they fail**

Run:

```bash
cargo test -p orca-tools sandbox::seatbelt::tests::filesystem_paths_are_passed_as_seatbelt_parameters -- --exact
```

Expected: current policy source contains interpolated paths.

- [ ] **Step 3: Embed the static base and build parameterized dynamic rules**

Load:

```rust
const SEATBELT_BASE_POLICY: &str = include_str!("seatbelt_base_policy.sbpl");
```

Represent a built profile as policy text plus ordered `(key, PathBuf)` parameters. Append parameters to `sandbox-exec` as `-DKEY=value` OS-string arguments, then `--`, `/bin/sh`, `-c`, and the original command.

- [ ] **Step 4: Mark nested sandbox execution**

Set:

```rust
cmd.env("ORCA_SANDBOX", "seatbelt");
```

for both workspace-write and read-only Seatbelt commands. Add an execution test that prints the variable and expects `seatbelt`.

- [ ] **Step 5: Verify policy tests pass**

Run:

```bash
cargo test -p orca-tools sandbox::seatbelt::tests -- --test-threads=1
```

Expected: all Seatbelt unit and behavior tests pass.

### Task 4: Make platform removal and profile failure visible

**Files:**
- Modify: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Modify: `crates/orca-tools/src/sandbox/mod.rs`
- Test: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Test: `crates/orca-tools/src/sandbox/mod.rs`

- [ ] **Step 1: Replace availability skips with an assertion gate**

Replace every:

```rust
if !available() {
    return;
}
```

with a helper that asserts `/usr/bin/sandbox-exec` can compile and run the probe policy. A macOS runner without Seatbelt must fail rather than report skipped success.

- [ ] **Step 2: Add a profile compilation failure behavior test**

Launch `/usr/bin/sandbox-exec` with an invalid policy and a shell command that would create a marker. Assert a nonzero exit and that the marker does not exist.

- [ ] **Step 3: Add denial diagnostic coverage**

Execute a forbidden metadata write, feed stdout/stderr into `diagnose_sandbox_denial`, and assert the denied metadata path and exact suggested metadata root are reported when macOS emits a recognizable permission error.

- [ ] **Step 4: Run focused tests**

Run:

```bash
cargo test -p orca-tools sandbox::seatbelt::tests -- --test-threads=1
cargo test -p orca-runtime sandbox_denial::tests -- --test-threads=1
```

Expected: no availability skip remains and all selected tests pass.

### Task 5: Add real security-boundary behavior tests

**Files:**
- Modify: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Test: `crates/orca-tools/src/sandbox/seatbelt.rs`

- [ ] **Step 1: Cover symlink and `.git` pointer-file bypasses**

Create a symlinked ordinary writable root resolving to workspace metadata and assert the write is denied. Create `.git` as a pointer file, grant the workspace parent as an ordinary writable root, and assert overwriting the file is denied.

- [ ] **Step 2: Cover network denial**

Start a loopback TCP listener in the test process, run a sandboxed client with `network_access: false`, and assert the connection is denied.

- [ ] **Step 3: Cover Unix socket restriction**

Run a sandboxed helper that binds or connects below an approved Unix socket root and assert success. Repeat outside the approved root and assert denial.

- [ ] **Step 4: Run the behavior suite serially**

Run:

```bash
cargo test -p orca-tools sandbox::seatbelt::tests -- --test-threads=1
```

Expected: PATH injection, parent grant, symlink, pointer-file, Unix socket, network denial, and invalid-profile tests all pass.

### Task 6: Validate the complete change

**Files:**
- Verify: all modified Rust and SBPL files

- [ ] **Step 1: Format and inspect**

Run:

```bash
cargo fmt --all -- --check
git diff --check
```

Expected: both commands exit successfully.

- [ ] **Step 2: Run package tests**

Run:

```bash
cargo test -p orca-tools -- --test-threads=1
cargo test -p orca-runtime shell_session -- --test-threads=1
cargo test -p orca-runtime runtime_permission -- --test-threads=1
```

Expected: all selected tests pass with zero failures.

- [ ] **Step 3: Run the workspace gate**

Run:

```bash
cargo test --workspace --all-targets --locked -- --test-threads=1
```

Expected: the complete workspace test suite exits successfully.

- [ ] **Step 4: Audit every objective requirement**

Confirm from source and fresh command output that:

```text
only /usr/bin/sandbox-exec is used on macOS
WorkspaceWrite and ReadOnly never fall back to a plain shell
only DangerFullAccess selects plain_bash_command
ordinary writable roots cannot reopen .git/.agents/.codex
only exact explicitly granted metadata roots can reopen their target
the static policy lives in seatbelt_base_policy.sbpl
all dynamic paths use -D parameters
ORCA_SANDBOX=seatbelt is present in sandbox children
Seatbelt unavailability fails macOS tests
all requested real behavior scenarios are covered
```

The future `SandboxEnforcement` enum and VM/remote backends remain later architectural work; this change preserves an interface path for them without adding unused abstractions now.
