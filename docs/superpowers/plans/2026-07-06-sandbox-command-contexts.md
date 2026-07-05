# Sandbox Command Contexts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace long sandbox bash command constructor argument lists with grouped context structs while preserving existing shell, command/exec, and bash sandbox behavior.

**Architecture:** `orca-tools::sandbox` owns public command-construction context types and passes them down to the platform implementation. The macOS Seatbelt backend owns matching lower-level profile context types so profile generation no longer needs long helper arguments. Existing convenience wrappers can remain as compatibility shims only when they delegate to the grouped context API.

**Tech Stack:** Rust, `orca-tools`, macOS Seatbelt sandbox profiles, existing cargo unit/integration tests, release docs/site metadata.

---

### Task 1: Add Sandbox Ownership Test

**Files:**
- Modify: `crates/orca-tools/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Add a source ownership test that requires:

```rust
#[test]
fn sandbox_command_constructors_use_grouped_contexts() {
    let sandbox_source = include_str!("sandbox/mod.rs");
    let seatbelt_source = include_str!("sandbox/seatbelt.rs");

    for marker in [
        "pub struct WorkspaceWriteSandboxCommandContext",
        "pub struct ReadOnlySandboxCommandContext",
        "pub fn workspace_write_bash_command(context: WorkspaceWriteSandboxCommandContext",
        "pub fn read_only_bash_command(context: ReadOnlySandboxCommandContext",
        "fn workspace_write_profile(context: WorkspaceWriteProfileContext",
        "fn read_only_profile(context: ReadOnlyProfileContext",
    ] {
        assert!(
            sandbox_source.contains(marker) || seatbelt_source.contains(marker),
            "sandbox command construction must use grouped context marker {marker}"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run:

```bash
cargo test -p orca-tools sandbox_command_constructors_use_grouped_contexts -- --nocapture
```

Expected: fail because `WorkspaceWriteSandboxCommandContext` does not exist yet.

### Task 2: Implement Grouped Sandbox Contexts

**Files:**
- Modify: `crates/orca-tools/src/sandbox/mod.rs`
- Modify: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Modify: `crates/orca-runtime/src/shell_session.rs`

- [ ] **Step 1: Add public sandbox context structs**

Add borrowed command-construction contexts:

```rust
pub struct WorkspaceWriteSandboxCommandContext<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub readable_roots: &'a [PathBuf],
    pub additional_roots: &'a [PathBuf],
    pub denied_roots: &'a [PathBuf],
    pub network_access: bool,
    pub exclude_tmpdir_env_var: bool,
    pub exclude_slash_tmp: bool,
    pub allowed_unix_socket_roots: &'a [PathBuf],
}

pub struct ReadOnlySandboxCommandContext<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub readable_roots: &'a [PathBuf],
    pub additional_roots: &'a [PathBuf],
    pub denied_roots: &'a [PathBuf],
    pub network_access: bool,
    pub allow_global_read: bool,
    pub allowed_unix_socket_roots: &'a [PathBuf],
}
```

- [ ] **Step 2: Rewrite constructors around contexts**

Change the main `workspace_write_bash_command` and `read_only_bash_command` functions to accept those contexts, prepare non-interactive commands, and delegate to platform functions with the same context.

- [ ] **Step 3: Keep compatibility shims narrow**

Keep `bash_command`, `plain_bash_command`, and `bash_command_with_additional_roots` stable. Remove the old public long-argument `*_with_unix_sockets` entrypoints by updating runtime call sites to use contexts directly.

- [ ] **Step 4: Group Seatbelt profile generation**

Add internal profile contexts in `seatbelt.rs` and rewrite `workspace_write_profile` / `read_only_profile` to take a single context each.

### Task 3: Verify Behavior And Commit

**Files:**
- Test: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Test: `crates/orca-runtime/src/shell_session.rs`
- Test: `crates/orca-runtime/src/server.rs`

- [ ] **Step 1: Run focused tests**

```bash
cargo test -p orca-tools sandbox_command_constructors_use_grouped_contexts -- --nocapture
cargo test -p orca-tools sandbox -- --nocapture
cargo test --test shell_session_contract shell_session -- --nocapture
cargo test --test session_server_contract command_exec -- --nocapture
```

- [ ] **Step 2: Run package checks**

```bash
cargo test -p orca-tools --all-targets -- --test-threads=1
cargo test -p orca-runtime bash -- --nocapture
cargo fmt -- --check
git diff --check
cargo clippy -p orca-tools --all-targets
```

- [ ] **Step 3: Commit feature**

```bash
git add crates/orca-tools/src/lib.rs crates/orca-tools/src/sandbox/mod.rs crates/orca-tools/src/sandbox/seatbelt.rs crates/orca-runtime/src/shell_session.rs docs/superpowers/plans/2026-07-06-sandbox-command-contexts.md
git commit -m "refactor(tools): group sandbox command contexts"
```

### Task 4: Prepare Patch Release

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `npm/orca/package.json`
- Modify: `README.md`
- Modify: `site/src/shared.ts`
- Modify: `site/src/changelog/Changelog.tsx`
- Modify: `site/public/sitemap.xml`
- Modify: `docs/production-roadmap.md`
- Create: `docs/releases/v0.1.133.md`

- [ ] **Step 1: Update version metadata to `0.1.133`**

Update Rust, npm, README install pin, site release list, sitemap `lastmod`, roadmap baseline, and release notes.

- [ ] **Step 2: Run release-grade verification**

```bash
cargo fmt -- --check
git diff --check
cargo test -p orca-tools sandbox_command_constructors_use_grouped_contexts -- --nocapture
cargo test -p orca-tools sandbox -- --nocapture
cargo test --test session_server_contract command_exec -- --nocapture
cargo test -p orca-tools --all-targets -- --test-threads=1
cargo clippy -p orca-tools --all-targets
npm --prefix site run build
npm --prefix site run check:seo
node scripts/release/test-stage-npm.mjs
node scripts/release/test-verify-published.mjs
CARGO_TARGET_DIR=/tmp/blade-deepseek-target-133 cargo test -- --test-threads=1
CARGO_TARGET_DIR=/tmp/blade-deepseek-target-133 cargo clippy --workspace --all-targets
node scripts/release/real-api-e2e.mjs --timeout-ms 180000
```

- [ ] **Step 3: Commit, push, tag, and verify public artifacts**

```bash
git commit -m "docs(release): prepare v0.1.133"
git push origin main
git tag v0.1.133
git push origin v0.1.133
gh run watch <release-run-id> --repo echoVic/blade-deepseek --exit-status
gh run watch <pages-run-id> --repo echoVic/blade-deepseek --exit-status
node scripts/release/verify-published.mjs --version 0.1.133 --repo echoVic/blade-deepseek --package @blade-ai/orca --bin orca
```
