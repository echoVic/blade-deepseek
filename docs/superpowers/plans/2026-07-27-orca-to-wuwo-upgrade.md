# Orca to Wuwo Upgrade Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task in an isolated worktree. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Wuwo 0.3.0 as the renamed product, register the bare `wuwo` npm package with a functional release candidate, migrate all supported Orca state safely, and preserve `orca` as a reversible compatibility command.

**Architecture:** Both `orca` and `wuwo` binaries are built from one root crate and select an immutable compile-time product identity at process startup. Shared code resolves every writable home/project path through that identity; a separate migration engine copies and validates legacy Orca state before a nonce-bound receipt allows the Orca transition binary to forward to Wuwo. The release pipeline builds and verifies both binary/npm families before moving either `latest` tag.

**Tech Stack:** Rust 2024, Clap, Tokio, Rusqlite backup API, Serde/JSON/TOML, Node.js ESM npm launchers, GitHub Actions, GitHub Releases, npm Registry.

## Global Constraints

- `0.2.54` is the last pure-Orca release; `0.3.0` is the first final Wuwo release.
- Public product, primary CLI, bare npm package, user home, and project directory are `Wuwo`, `wuwo`, `wuwo`, `~/.wuwo`/`WUWO_HOME`, and `.wuwo/`.
- `@blade-ai/orca@0.3.0` remains a complete transition runtime and owns the `orca` command.
- Internal `orca-*` Rust crate names and legacy hook ABI variables may remain unchanged.
- Orca source data is copy-only migration input and is never automatically deleted or rewritten.
- Non-interactive ACP, server, JSONL, worker, history, workflow, and `exec` paths never prompt or contaminate protocol stdout.
- Compatibility forwarding activates only after a versioned, nonce-bound, fully validated migration result.
- The canonical repository URL is `https://github.com/echoVic/wuwo`; the old repository name must never be recreated.
- Existing unrelated ACP/runtime worktree changes must be preserved and excluded from Wuwo commits.

---

## File Structure

### New focused modules

- `crates/orca-core/src/product.rs` — immutable product identity and pure path resolution.
- `crates/orca-runtime/src/migration/mod.rs` — public migration orchestration API.
- `crates/orca-runtime/src/migration/inventory.rs` — supported legacy item discovery and source fingerprints.
- `crates/orca-runtime/src/migration/journal.rs` — resumable migration journal and destination lock.
- `crates/orca-runtime/src/migration/copy.rs` — confined staging, atomic commit, SQLite backup, and symlink policy.
- `crates/orca-runtime/src/migration/validation.rs` — post-copy semantic validation and final report.
- `src/lib.rs` — shared application entrypoint selected by binary identity.
- `src/bin/orca.rs` — Orca transition binary entrypoint.
- `src/bin/wuwo.rs` — Wuwo primary binary entrypoint.
- `src/migration.rs` — CLI prompt, handoff, compatibility receipt, forwarding, and rollback orchestration.
- `npm/wuwo/package.json` — bare Wuwo npm package metadata.
- `npm/wuwo/bin/wuwo.js` — platform binary resolver and stdio/signal-preserving launcher.
- `tests/product_identity_contract.rs` — dual binary/identity and forbidden-path-literal contract tests.
- `tests/migration_contract.rs` — inventory, copy, validation, resume, conflict, and security tests.
- `tests/wuwo_cli_contract.rs` — startup discovery, handoff, forwarding, rollback, and protocol-output tests.
- `scripts/release/test-wuwo-migration-e2e.mjs` — installed-package transition smoke test.

### Existing files changed by responsibility

- `Cargo.toml`, `Cargo.lock` — Wuwo package/version, dual binaries, SQLite backup feature.
- `src/cli.rs`, `src/config/mod.rs`, `src/tools/mod.rs` — dynamic identity, commands, environment precedence.
- `crates/orca-core/src/config/file.rs`, `folder_trust.rs` — identity-owned user/project config.
- `crates/orca-runtime/src/thread_store.rs`, `history.rs`, `goal_store.rs`, `tasks.rs`, `memory.rs`, `instructions.rs`, `update_check.rs`, `workflow/script.rs` — identity-owned storage and compatibility reads.
- `crates/orca-tui/src/types.rs`, `commands/mod.rs`, `slash_command_actions.rs`, `ui.rs` — identity-owned input history, workflows, and copy.
- `npm/orca/*`, `npm/platform-package.json` — transition package metadata and canonical repository.
- `scripts/release/stage-npm.mjs`, `smoke-npm.mjs`, `verify-published.mjs` and tests — dual package staging and verification.
- `.github/workflows/release.yml`, `npm-token-check.yml`, `pages.yml` — candidate/final dual publication and canonical links.
- `install.sh` — explicit Orca-transition versus Wuwo asset/install modes.
- `README*.md`, `site/**` source/public metadata — Wuwo branding, commands, migration documentation, and canonical repository links.

---

### Task 1: Immutable Product Identity and Path Boundary

**Files:**
- Create: `crates/orca-core/src/product.rs`
- Modify: `crates/orca-core/src/lib.rs`
- Test: `crates/orca-core/src/product.rs`
- Test: `tests/product_identity_contract.rs`

**Interfaces:**
- Produces: `ProductKind::{OrcaTransition,Wuwo}`
- Produces: `ProductIdentity::{orca_transition,wuwo}`
- Produces: `ProductPaths::resolve(identity, explicit_home, system_home, cwd)`
- Produces: `install_process_identity(ProductIdentity) -> Result<(), ProductIdentityError>`
- Produces: `current_product() -> ProductIdentity`
- Consumes: no application modules; this is the dependency root for later tasks.

- [ ] **Step 1: Write the failing pure path-resolution tests**

```rust
#[test]
fn identities_own_distinct_writable_namespaces() {
    let system_home = Path::new("/home/test");
    let cwd = Path::new("/repo");

    let orca = ProductPaths::resolve(
        ProductIdentity::orca_transition(),
        None,
        Some(system_home),
        cwd,
    ).unwrap();
    let wuwo = ProductPaths::resolve(
        ProductIdentity::wuwo(),
        None,
        Some(system_home),
        cwd,
    ).unwrap();

    assert_eq!(orca.user_home, Path::new("/home/test/.orca"));
    assert_eq!(orca.project_dir, Path::new("/repo/.orca"));
    assert_eq!(wuwo.user_home, Path::new("/home/test/.wuwo"));
    assert_eq!(wuwo.project_dir, Path::new("/repo/.wuwo"));
    assert_eq!(wuwo.identity.home_env, "WUWO_HOME");
    assert_eq!(wuwo.identity.npm_package, "wuwo");
}

#[test]
fn explicit_home_belongs_only_to_the_selected_identity() {
    let paths = ProductPaths::resolve(
        ProductIdentity::wuwo(),
        Some(Path::new("/custom/wuwo")),
        Some(Path::new("/ignored")),
        Path::new("/repo"),
    ).unwrap();
    assert_eq!(paths.user_home, Path::new("/custom/wuwo"));
    assert_eq!(paths.project_dir, Path::new("/repo/.wuwo"));
}
```

- [ ] **Step 2: Run the tests and verify RED**

Run: `cargo test -p orca-core product::tests:: -- --nocapture`

Expected: compilation fails because `product` and the identity/path types do not exist.

- [ ] **Step 3: Implement the immutable identity**

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductKind {
    OrcaTransition,
    Wuwo,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProductIdentity {
    pub kind: ProductKind,
    pub display_name: &'static str,
    pub cli_name: &'static str,
    pub home_env: &'static str,
    pub default_home: &'static str,
    pub project_dir_name: &'static str,
    pub npm_package: &'static str,
    pub repository: &'static str,
}

impl ProductIdentity {
    pub const fn orca_transition() -> Self {
        Self {
            kind: ProductKind::OrcaTransition,
            display_name: "Orca",
            cli_name: "orca",
            home_env: "ORCA_HOME",
            default_home: ".orca",
            project_dir_name: ".orca",
            npm_package: "@blade-ai/orca",
            repository: "echoVic/wuwo",
        }
    }

    pub const fn wuwo() -> Self {
        Self {
            kind: ProductKind::Wuwo,
            display_name: "Wuwo",
            cli_name: "wuwo",
            home_env: "WUWO_HOME",
            default_home: ".wuwo",
            project_dir_name: ".wuwo",
            npm_package: "wuwo",
            repository: "echoVic/wuwo",
        }
    }
}
```

Use `OnceLock<ProductIdentity>` for the process identity. A second installation of a different identity returns `ProductIdentityError` rather than silently changing storage ownership.

- [ ] **Step 4: Add the production-literal inventory contract**

The integration test reads production `.rs` files and rejects new direct writable uses of `"ORCA_HOME"`, `".orca"`, `"WUWO_HOME"`, or `".wuwo"` outside:

```rust
const ALLOWED_OWNERS: &[&str] = &[
    "crates/orca-core/src/product.rs",
    "crates/orca-runtime/src/migration/",
    "src/migration.rs",
];
```

The failure includes the file and line so every later path migration is forced through the boundary.

- [ ] **Step 5: Run identity tests and verify GREEN**

Run: `cargo test -p orca-core product::tests:: -- --nocapture && cargo test --test product_identity_contract -- --nocapture`

Expected: all identity tests pass; the inventory contract initially reports the existing direct literals and remains ignored with a documented count until Task 3 removes them.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-core/src/product.rs crates/orca-core/src/lib.rs tests/product_identity_contract.rs
git commit -m "feat: define Orca and Wuwo product identities"
```

---

### Task 2: Shared Application Crate and Dual Binary Identity Probe

**Files:**
- Create: `src/lib.rs`
- Create: `src/bin/orca.rs`
- Create: `src/bin/wuwo.rs`
- Delete: `src/main.rs`
- Modify: `src/cli.rs`
- Modify: `Cargo.toml`
- Test: `tests/product_identity_contract.rs`

**Interfaces:**
- Consumes: `install_process_identity`, `ProductIdentity`
- Produces: `wuwo::run(ProductIdentity) -> i32`
- Produces: hidden `__identity` command returning `{product,version,migration_protocol,distribution}`
- Produces: executable targets `orca` and `wuwo`

- [ ] **Step 1: Add failing dual-binary tests**

```rust
#[test]
fn binaries_report_compile_time_identity() {
    let orca = Command::new(env!("CARGO_BIN_EXE_orca"))
        .arg("__identity").output().unwrap();
    let wuwo = Command::new(env!("CARGO_BIN_EXE_wuwo"))
        .arg("__identity").output().unwrap();

    let orca: Value = serde_json::from_slice(&orca.stdout).unwrap();
    let wuwo: Value = serde_json::from_slice(&wuwo.stdout).unwrap();
    assert_eq!(orca["product"], "orca_transition");
    assert_eq!(wuwo["product"], "wuwo");
    assert_eq!(orca["migration_protocol"], 1);
    assert_eq!(wuwo["migration_protocol"], 1);
}

#[test]
fn wuwo_help_uses_wuwo_identity() {
    let output = Command::new(env!("CARGO_BIN_EXE_wuwo"))
        .arg("--help").output().unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with("A DeepSeek-native coding agent"));
    assert!(text.contains("Usage: wuwo"));
    assert!(!text.contains("Usage: orca"));
}
```

- [ ] **Step 2: Run and verify RED**

Run: `cargo test --test product_identity_contract binaries_report_compile_time_identity -- --nocapture`

Expected: compilation fails because the `wuwo` binary target is absent.

- [ ] **Step 3: Move the shared module root to `src/lib.rs`**

Expose `pub fn run(identity: ProductIdentity) -> i32` that installs the immutable process identity and calls `cli::run(identity)`. Both binary files are minimal:

```rust
fn main() {
    std::process::exit(wuwo::run(
        orca_core::product::ProductIdentity::wuwo(),
    ));
}
```

The Orca entrypoint passes `ProductIdentity::orca_transition()`.

- [ ] **Step 4: Make Clap identity dynamic and add the probe**

Remove the hard-coded `#[command(name = "orca")]`. Build matches through `CommandFactory`, set `.name(identity.cli_name)`, and deserialize with `FromArgMatches`. Add:

```rust
#[derive(Serialize)]
struct IdentityProbe {
    product: ProductKind,
    version: &'static str,
    migration_protocol: u32,
    distribution: &'static str,
}
```

`distribution` is `"npm"` when the identity-specific managed-by-npm variable is set and `"direct"` otherwise. The probe trusts the binary-selected `ProductIdentity`, never `argv[0]`.

- [ ] **Step 5: Set Cargo package and binaries**

```toml
[package]
name = "wuwo"
version = "0.3.0-rc.0"
repository = "https://github.com/echoVic/wuwo"
description = "Wuwo: a DeepSeek-native coding agent"

[[bin]]
name = "orca"
path = "src/bin/orca.rs"

[[bin]]
name = "wuwo"
path = "src/bin/wuwo.rs"
```

- [ ] **Step 6: Run and verify GREEN**

Run: `cargo test --test product_identity_contract -- --nocapture && cargo run --bin orca -- __identity && cargo run --bin wuwo -- __identity`

Expected: tests pass and each probe reports its own identity at version `0.3.0-rc.0`.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/bin/orca.rs src/bin/wuwo.rs src/main.rs src/cli.rs tests/product_identity_contract.rs
git commit -m "feat: build Orca transition and Wuwo binaries"
```

---

### Task 3: Route All Writable Storage Through Product Paths

**Files:**
- Modify: `crates/orca-core/src/config/file.rs`
- Modify: `crates/orca-core/src/config/folder_trust.rs`
- Modify: `crates/orca-core/src/config/mod.rs`
- Modify: `crates/orca-provider/src/deepseek_http.rs`
- Modify: `crates/orca-provider/src/summary_cache.rs`
- Modify: `crates/orca-runtime/src/thread_store.rs`
- Modify: `crates/orca-runtime/src/thread_store/local.rs`
- Modify: `crates/orca-runtime/src/history.rs`
- Modify: `crates/orca-runtime/src/goal_store.rs`
- Modify: `crates/orca-runtime/src/tasks.rs`
- Modify: `crates/orca-runtime/src/memory.rs`
- Modify: `crates/orca-runtime/src/instructions.rs`
- Modify: `crates/orca-runtime/src/controller.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Modify: `crates/orca-runtime/src/mentions.rs`
- Modify: `crates/orca-runtime/src/runtime_bash.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Modify: `crates/orca-runtime/src/runtime_permission.rs`
- Modify: `crates/orca-runtime/src/runtime_special.rs`
- Modify: `crates/orca-runtime/src/server.rs`
- Modify: `crates/orca-runtime/src/server/command_exec_manager.rs`
- Modify: `crates/orca-runtime/src/session.rs`
- Modify: `crates/orca-runtime/src/thread.rs`
- Modify: `crates/orca-runtime/src/update_check.rs`
- Modify: `crates/orca-runtime/src/workflow/runner.rs`
- Modify: `crates/orca-runtime/src/workflow/script.rs`
- Modify: `crates/orca-runtime/src/workflow_execution.rs`
- Modify: `crates/orca-runtime/src/worktree.rs`
- Modify: `crates/orca-tools/src/external.rs`
- Modify: `crates/orca-tools/src/list_files.rs`
- Modify: `crates/orca-tools/src/registry.rs`
- Modify: `crates/orca-tools/src/sandbox/mod.rs`
- Modify: `crates/orca-tools/src/sandbox/seatbelt.rs`
- Modify: `crates/orca-tools/src/skills.rs`
- Modify: `crates/orca-tui/src/action_dispatcher.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/commands/mod.rs`
- Modify: `crates/orca-tui/src/mention_search_manager.rs`
- Modify: `crates/orca-tui/src/slash_command_actions.rs`
- Modify: `crates/orca-tui/src/surface_client.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Test: `tests/product_identity_contract.rs`

**Interfaces:**
- Consumes: `current_product`, `ProductPaths`
- Produces: identity-owned writable paths and trusted read-only `.orca` fallback for Wuwo
- Produces: environment precedence `WUWO_* > ORCA_* > DEEPSEEK_* > config/default`

- [ ] **Step 1: Add failing subprocess storage tests**

Each binary is run in a fresh process with isolated `HOME`, `ORCA_HOME`, and `WUWO_HOME`. Assertions cover API-key save, input history, session history, Goals, tasks, Memory, trust, rules, Skills, tools, and workflows:

```rust
#[test]
fn wuwo_never_writes_orca_home() {
    let sandbox = StorageSandbox::new();
    sandbox.run_wuwo(["config", "set-key", "sk-test"]).assert_success();
    assert!(sandbox.wuwo_home().join("auth.json").exists());
    assert!(!sandbox.orca_home().exists());
}

#[test]
fn orca_transition_never_writes_wuwo_home_before_migration() {
    let sandbox = StorageSandbox::new();
    sandbox.run_orca(["exec", "--no-history", "hello"]).assert_started();
    assert!(!sandbox.wuwo_home().exists());
}
```

- [ ] **Step 2: Verify RED**

Run: `cargo test --test product_identity_contract storage -- --nocapture`

Expected: Wuwo writes one or more artifacts under Orca paths or ignores `WUWO_HOME`.

- [ ] **Step 3: Replace path helpers with identity-owned resolution**

Every no-argument helper delegates to `current_product()`. Explicit-dir helpers remain for deterministic unit tests. Wuwo project reads follow:

```rust
pub enum ProjectConfigSource {
    Primary(PathBuf),
    LegacyReadOnly(PathBuf),
    None,
}
```

`.wuwo/` wins. Trusted `.orca/` is read-only fallback only when `.wuwo/` is absent. Writes always target `.wuwo/`.

- [ ] **Step 4: Implement environment compatibility**

For Wuwo, public values resolve in this order:

```rust
fn compatible_env(name: &str) -> Option<OsString> {
    env::var_os(format!("WUWO_{name}"))
        .or_else(|| env::var_os(format!("ORCA_{name}")))
        .or_else(|| env::var_os(format!("DEEPSEEK_{name}")))
}
```

`ORCA_HOME` is excluded from this generic aliasing and is used only by migration discovery. Hooks continue receiving legacy `ORCA_*` variables and receive matching `WUWO_*` aliases.

- [ ] **Step 5: Fix TUI input history ownership**

Replace the direct `dirs::home_dir().join(".orca/history.jsonl")` behavior with `ProductPaths.user_home.join("history.jsonl")`. Add a regression test proving custom `WUWO_HOME` is honored.

- [ ] **Step 6: Enable the forbidden-literal contract**

Remove the temporary ignore/count from Task 1. Allowed literal owners remain only product identity, migration compatibility, tests, fixtures, and explicit legacy ABI constants.

- [ ] **Step 7: Run and verify GREEN**

Run:

```bash
cargo test --test product_identity_contract -- --nocapture
cargo test -p orca-core config:: -- --nocapture
cargo test -p orca-runtime history:: -- --nocapture
cargo test -p orca-runtime tasks:: -- --nocapture
cargo test -p orca-runtime memory:: -- --nocapture
cargo test -p orca-runtime instructions:: -- --nocapture
cargo test -p orca-tools skills:: -- --nocapture
cargo test -p orca-tui --lib
```

Expected: identity ownership and legacy read-only fallback tests pass; the production literal inventory is clean.

- [ ] **Step 8: Commit**

```bash
git add crates/orca-core crates/orca-provider/src crates/orca-runtime/src crates/orca-tools/src crates/orca-tui/src tests/product_identity_contract.rs
git commit -m "refactor: route storage through product identity"
```

---

### Task 4: Legacy Inventory, Fingerprints, and Conflict Planning

**Files:**
- Create: `crates/orca-runtime/src/migration/mod.rs`
- Create: `crates/orca-runtime/src/migration/inventory.rs`
- Modify: `crates/orca-runtime/src/lib.rs`
- Test: `tests/migration_contract.rs`

**Interfaces:**
- Produces: `discover_legacy(LegacyDiscoveryRequest) -> Result<MigrationInventory, MigrationError>`
- Produces: `plan_migration(&MigrationInventory, &ProductPaths, ConflictPolicy) -> MigrationPlan`
- Produces: `SourceFingerprint {size,modified_ns,sha256}`
- Consumes: Wuwo destination paths from Task 1

- [ ] **Step 1: Write failing complete-inventory tests**

Build a temporary Orca home containing every supported source from the design. Assert exact item kinds, counts, sizes, unknown files, excluded cache/lock files, and default-history fallback:

```rust
assert_eq!(inventory.source_home, legacy_home.canonicalize().unwrap());
assert!(inventory.contains(MigrationItemKind::Config));
assert!(inventory.contains(MigrationItemKind::Credentials));
assert!(inventory.contains(MigrationItemKind::GoalDatabase));
assert!(inventory.contains(MigrationItemKind::InputHistory));
assert_eq!(inventory.unknown, vec![legacy_home.join("future-format.bin")]);
assert!(!inventory.paths().any(|p| p.ends_with("summary_cache")));
```

Add separate tests for empty/update-cache-only homes, explicit `ORCA_HOME`, duplicate canonical paths, inaccessible homes, current `.orca/` project state, and pre-existing `.wuwo/` conflicts.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test migration_contract inventory -- --nocapture`

Expected: compilation fails because migration inventory types do not exist.

- [ ] **Step 3: Implement typed supported-item discovery**

Use a closed mapping of legacy relative paths. Unknown paths are reported and never copied. Directory walking uses `symlink_metadata`; symlinks become `MigrationWarning::Symlink` and are not followed.

- [ ] **Step 4: Implement deterministic conflict planning**

```rust
pub enum ConflictChoice {
    KeepWuwo,
    ReplaceWithOrca,
    KeepBoth { alternate_name: OsString },
    Skip,
}
```

Config, credentials, Goals, trust, and permissions require an explicit per-item choice. No global replacement default exists for them.

- [ ] **Step 5: Run and verify GREEN**

Run: `cargo test --test migration_contract inventory -- --nocapture`

Expected: all inventory and conflict tests pass without writing the destination.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-runtime/src/migration crates/orca-runtime/src/lib.rs tests/migration_contract.rs
git commit -m "feat: inventory Orca migration state"
```

---

### Task 5: Transactional Copy, SQLite Backup, Journal, and Resume

**Files:**
- Create: `crates/orca-runtime/src/migration/journal.rs`
- Create: `crates/orca-runtime/src/migration/copy.rs`
- Modify: `crates/orca-runtime/src/migration/mod.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Test: `tests/migration_contract.rs`

**Interfaces:**
- Produces: `stage_migration(&MigrationPlan) -> Result<StagedMigration, MigrationError>`
- Produces: `resume_staging(&Path) -> Result<StagedMigration, MigrationError>`
- Produces: `commit_validated(ValidatedMigration) -> Result<MigrationExecution, MigrationError>`
- Produces journal states `Discovered -> Planned -> Copied`; Task 6 owns `Validated -> Committed`
- Consumes: `MigrationPlan`, `SourceFingerprint`

- [ ] **Step 1: Write failing transaction tests**

Cover interruption after every journal state, changed source fingerprints, existing destination conflicts, direct-install migration, live `goals.sqlite3` WAL writes, known runtime locks, concurrent session/task/workflow appends, permissions, and symlink escapes. A test-injected checkpoint callback returns an interruption error at a chosen state:

```rust
let interrupted = stage_with_checkpoint(&plan, |state| {
    (state == JournalItemState::Copied).then_some(MigrationError::Interrupted)
});
assert!(interrupted.is_err());
assert!(legacy_home.join("config.toml").exists());
assert!(!wuwo_home.join("config.toml").exists());

let resumed = resume_staging(&wuwo_home).unwrap();
assert!(resumed.items.iter().all(|item| item.state == JournalItemState::Copied));
assert!(!wuwo_home.join("config.toml").exists());
```

Create `goals.runtime.lock` and mutate append-only sources between fingerprint
checks in separate tests. They must produce `MigrationStatus::Incomplete` with
deferred items and must not produce a compatibility receipt.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test migration_contract transaction -- --nocapture`

Expected: compilation fails because execution and journal modules do not exist.

- [ ] **Step 3: Enable and implement consistent SQLite backup**

Update the workspace dependency:

```toml
rusqlite = { version = "0.32", features = ["bundled", "backup"] }
```

Open the legacy DB read-only and copy with `rusqlite::backup::Backup`; never copy `-wal` or `-shm` files.

- [ ] **Step 4: Implement confined staging and atomic commit**

Create `.migration/staging/<journal-id>` under the Wuwo destination filesystem with mode `0700`. Canonicalize every source/destination parent and reject escape. Copy regular files with restrictive credential permissions, fsync files and directories, recheck source fingerprints, then rename staged items atomically.

- [ ] **Step 5: Implement destination lock, live-writer handling, and resumable journal**

The destination lock uses create-new semantics and records PID/start metadata. Stale locks require verified dead process ownership before replacement. Detect `goals.runtime.lock` and known task/workflow writers; fingerprint append-only sources before and after copy, retry changed sources at most three times, then mark them deferred with a close-Orca-and-resume instruction. Journal writes use temp-file, fsync, and rename. Copied items are not recopied when fingerprints still match, and any deferred confirmed-plan item blocks validation and compatibility activation.

- [ ] **Step 6: Run and verify GREEN**

Run: `cargo test --test migration_contract transaction -- --nocapture`

Expected: staging, WAL, interruption, fingerprint, and symlink tests pass; destination files are not committed before validation and Orca sources remain byte-identical.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock crates/orca-runtime/src/migration tests/migration_contract.rs
git commit -m "feat: migrate Orca state transactionally"
```

---

### Task 6: Semantic Validation and Migration Reporting

**Files:**
- Create: `crates/orca-runtime/src/migration/validation.rs`
- Modify: `crates/orca-runtime/src/migration/mod.rs`
- Modify: `crates/orca-core/src/config/file.rs`
- Modify: `crates/orca-core/src/config/folder_trust.rs`
- Modify: `crates/orca-runtime/src/history.rs`
- Modify: `crates/orca-runtime/src/goal_store.rs`
- Modify: `crates/orca-runtime/src/tasks.rs`
- Modify: `crates/orca-runtime/src/workflow/script.rs`
- Modify: `crates/orca-tools/src/external.rs`
- Modify: `crates/orca-tools/src/skills.rs`
- Test: `tests/migration_contract.rs`

**Interfaces:**
- Produces: `validate_staged(&StagedMigration) -> Result<ValidatedMigration, MigrationReport>`
- Produces: `execute_migration(&MigrationPlan) -> Result<MigrationExecution, MigrationError>`
- Produces: `resume_migration(&Path) -> Result<MigrationExecution, MigrationError>`
- Produces: `MigrationReport {status,items,warnings,unknown,deferred,source_fingerprint}`
- Consumes: `stage_migration`, `commit_validated`, and existing config/history/Goal/task/workflow loaders through explicit paths

- [ ] **Step 1: Write failing semantic-validation tests**

Create valid and malformed variants for config, auth JSON structure, JSONL/Zstd sessions, active Goals, task indexes, workflows, tools, skills, rules, memory, trust, and project config. Assert malformed required state prevents completion:

```rust
let report = validate_staged(&staging_with_malformed_goal()).unwrap_err();
assert_eq!(report.status, MigrationStatus::Incomplete);
assert!(report.items.iter().any(|item|
    item.kind == MigrationItemKind::GoalDatabase
        && matches!(item.validation, ValidationResult::Failed(_))
));
```

- [ ] **Step 2: Verify RED**

Run: `cargo test --test migration_contract validation -- --nocapture`

Expected: compilation fails because the validator/report types do not exist.

- [ ] **Step 3: Implement validators using production loaders**

Expose explicit-path read-only loader entrypoints in the files listed for this task rather than duplicating their parsers. Authentication validation checks readability and the presence/type of supported keys without logging values. Active Goal validation preserves objective, status, elapsed time, tokens, budget, timestamps, and referenced session IDs.

- [ ] **Step 4: Persist a redacted report**

Write `.migration/reports/<journal-id>.json` and a human-readable text report. Reports contain relative item names/counts but no credentials, prompts, raw history, or absolute paths unless the user requests local inspection.

After every required item validates, construct `ValidatedMigration`, call
`commit_validated`, advance journal items through `Validated` and `Committed`,
and atomically rename staged items. Add checkpoint tests for interruption after
validation and during commit; resume must never duplicate committed sessions
or Goals.

- [ ] **Step 5: Run and verify GREEN**

Run: `cargo test --test migration_contract validation -- --nocapture`

Expected: all supported types validate; any required failure leaves the migration incomplete and prevents compatibility activation.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-core/src/config crates/orca-runtime/src/migration crates/orca-runtime/src/history.rs crates/orca-runtime/src/goal_store.rs crates/orca-runtime/src/tasks.rs crates/orca-runtime/src/workflow/script.rs crates/orca-tools/src/external.rs crates/orca-tools/src/skills.rs tests/migration_contract.rs
git commit -m "feat: validate migrated Wuwo state"
```

---

### Task 7: Guided Upgrade, Direct Discovery, and Decision Persistence

**Files:**
- Create: `src/migration.rs`
- Modify: `src/lib.rs`
- Modify: `src/cli.rs`
- Modify: `crates/orca-runtime/src/update_check.rs`
- Test: `tests/wuwo_cli_contract.rs`
- Test: `tests/tui_pty_contract.rs`

**Interfaces:**
- Produces: `orca migrate-to-wuwo`
- Produces: `wuwo migrate-from-orca [--legacy-home PATH] [--handoff PATH]`
- Produces: installation-method-specific Wuwo install and retry instructions
- Produces: interactive rename/discovery preflight
- Produces: persisted `Migrate`, `RemindUntil`, `StayOnOrca`, `StartFresh` decisions
- Consumes: migration engine Tasks 4–6

- [ ] **Step 1: Write failing PTY and non-interactive tests**

PTY tests select each prompt option. Protocol tests assert exact empty stdout additions for ACP/server/JSONL/worker paths:

```rust
#[test]
fn direct_wuwo_first_launch_discovers_orca_before_creating_state() {
    let run = PtyRun::wuwo_with_legacy_home();
    run.wait_for("Existing Orca data found");
    assert!(!run.wuwo_home().exists());
    run.send_key('3'); // Not Now
    run.assert_normal_start();
}

#[test]
fn server_mode_never_prompts_or_mutates_for_migration() {
    let result = run_wuwo(["--mode=server"], legacy_fixture());
    assert!(!result.stdout.contains("Existing Orca data"));
    assert!(!result.wuwo_home.exists());
}
```

- [ ] **Step 2: Verify RED**

Run: `cargo test --test wuwo_cli_contract preflight -- --nocapture && cargo test --test tui_pty_contract migration -- --nocapture`

Expected: no Wuwo migration commands or prompts exist.

- [ ] **Step 3: Add commands and preflight gating**

Automatic discovery runs only when product is Wuwo, there is no subcommand/prompt, both stdin/stdout are terminals, and normal runtime/config initialization has not started. Orca shows its rename prompt locally at every eligible startup according to its separate migration decision state, without requiring another network update check.

- [ ] **Step 4: Persist fingerprinted decisions**

Store migration decisions under the identity-owned update state. `Remind me later` uses a seven-day timestamp separate from `skip_until_version`. `Start Fresh` is keyed by canonical legacy home plus non-secret inventory fingerprint. Explicit migration commands ignore prior dismissal.

- [ ] **Step 5: Install and verify the matching Wuwo distribution**

For npm-managed Orca, execute `npm install -g wuwo@<matching-version>` with inherited terminal streams and retain `@blade-ai/orca`. For direct installs, invoke the checksum-verifying installer in Wuwo mode and target the running Orca binary's directory only when writable. Resolve the exact installed executable and require a successful `__identity` probe before creating a handoff. Network, permission, or probe failures keep Orca active and print an exact retry command; offline mode prints the version-matched download plus `wuwo migrate-from-orca`.

- [ ] **Step 6: Render inventory, conflicts, progress, and final report**

The prompt displays exact counts, bytes, destination, warnings, and per-item conflicts. It never logs credential contents. Cancellation leaves Orca unchanged and the journal resumable.

- [ ] **Step 7: Run and verify GREEN**

Run: `cargo test --test wuwo_cli_contract -- --nocapture && cargo test --test tui_pty_contract migration -- --nocapture`

Expected: all interactive choices and non-interactive determinism tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/migration.rs src/lib.rs src/cli.rs crates/orca-runtime/src/update_check.rs tests/wuwo_cli_contract.rs tests/tui_pty_contract.rs
git commit -m "feat: guide Orca users into Wuwo"
```

---

### Task 8: Nonce-Bound Handoff, Compatibility Forwarding, Repair, and Rollback

**Files:**
- Modify: `src/migration.rs`
- Modify: `src/lib.rs`
- Modify: `src/cli.rs`
- Test: `tests/wuwo_cli_contract.rs`

**Interfaces:**
- Produces: one-use `MigrationHandoffV1`
- Produces: `CompatibilityReceiptV1`
- Produces: `orca migrate rollback`
- Produces: `wuwo migrate repair-alias`
- Consumes: identity probe Task 2 and validated report Task 6

- [ ] **Step 1: Write failing handoff/forwarding tests**

Cover matching/mismatched nonce, expired/tampered handoff, wrong binary identity/version/path, generic exit zero without result, direct-Wuwo activation with compatible/old/unrelated/missing `orca` executables, full argument boundaries, cwd, environment mapping, stdin/stdout/stderr, PTY, signals, and exit codes:

```rust
#[test]
fn zero_exit_without_nonce_bound_success_does_not_activate_redirect() {
    let fixture = HandoffFixture::new();
    fixture.fake_wuwo_exits_zero_without_result();
    fixture.run_orca_migration().assert_failed();
    fixture.run_orca_identity().assert_product("orca_transition");
}

#[test]
fn active_receipt_forwards_all_arguments_and_exit_code() {
    let fixture = ForwardFixture::validated();
    let result = fixture.run_orca_os_args([
        OsString::from("exec"),
        OsString::from("argument with spaces"),
        OsString::from_bytes(b"non-utf8-\xff"),
    ]);
    result.assert_seen_by_wuwo_byte_for_byte();
    assert_eq!(result.exit_code, 37);
}
```

- [ ] **Step 2: Verify RED**

Run:

```bash
cargo test --test wuwo_cli_contract handoff -- --nocapture
cargo test --test wuwo_cli_contract forwarding -- --nocapture
cargo test --test wuwo_cli_contract rollback -- --nocapture
```

Expected: handoff, receipt, and compatibility commands do not exist.

- [ ] **Step 3: Implement secure handoff and result**

Handoff/result files are mode `0600`, contain no secrets, expire, bind expected executable canonical path, migration protocol, source version/install method, exact legacy/destination homes, cwd, and a random UUID nonce. Orca activates only after reading a successful result with the same nonce and validated report fingerprint.

- [ ] **Step 4: Implement trusted executable resolution**

For npm, resolve Wuwo from the installed package location/managed package metadata, never the current working directory. For direct installs, use the verified sibling path recorded in the receipt. Probe the exact path before every activation/repair.

- [ ] **Step 5: Activate compatible Orca after direct Wuwo migration**

After a direct Wuwo migration succeeds, inspect an installed `orca` command without replacing it. A compatible transition binary receives a fresh nonce-bound activation request and writes its own receipt. An older Orca is offered the exact transition-package or direct-launcher update before retry; an unrelated executable is reported and left untouched. If no `orca` exists, complete migration without creating an alias. `wuwo migrate repair-alias` repeats this flow idempotently.

- [ ] **Step 6: Implement forwarding and rollback**

On macOS/Linux use `CommandExt::exec` after environment mapping so stdio, PTY, cwd, and signals are inherited exactly. `orca migrate rollback` is intercepted before forwarding, disables only the receipt, warns about post-migration Wuwo-only writes, and returns future invocations to the full Orca transition runtime.

- [ ] **Step 7: Run and verify GREEN**

Run: `cargo test --test wuwo_cli_contract -- --nocapture`

Expected: forwarding is byte/exit/signal transparent; all tamper/failure cases keep Orca active; rollback and repair are idempotent.

- [ ] **Step 8: Commit**

```bash
git add src/migration.rs src/lib.rs src/cli.rs tests/wuwo_cli_contract.rs
git commit -m "feat: preserve Orca as a Wuwo compatibility command"
```

---

### Task 9: Dual npm Packages, Installers, and Local Artifact Smoke Tests

**Files:**
- Create: `npm/wuwo/package.json`
- Create: `npm/wuwo/bin/wuwo.js`
- Modify: `npm/orca/package.json`
- Modify: `npm/orca/bin/orca.js`
- Modify: `npm/platform-package.json`
- Modify: `install.sh`
- Modify: `scripts/release/stage-npm.mjs`
- Modify: `scripts/release/smoke-npm.mjs`
- Modify: `scripts/release/test-stage-npm.mjs`
- Create: `scripts/release/test-wuwo-migration-e2e.mjs`

**Interfaces:**
- Produces: `wuwo@VERSION` with `wuwo` bin
- Produces: `@blade-ai/orca@VERSION` with `orca` bin
- Produces: platform aliases resolving to prerelease variants of each main package
- Consumes: `orca-*` and `wuwo-*` native artifacts

- [ ] **Step 1: Extend staging tests and verify RED**

The expected Wuwo main metadata is:

```json
{
  "name": "wuwo",
  "version": "0.3.0-rc.0",
  "bin": {"wuwo": "bin/wuwo.js"},
  "repository": {
    "type": "git",
    "url": "git+https://github.com/echoVic/wuwo.git",
    "directory": "npm/wuwo"
  }
}
```

Expected optional aliases map `wuwo-darwin-arm64` to `npm:wuwo@VERSION-darwin-arm64` and equivalent targets. Orca aliases remain scoped and map to `npm:@blade-ai/orca@VERSION-SUFFIX`.

Run: `node scripts/release/test-stage-npm.mjs`

Expected: FAIL because no Wuwo staging/package support exists.

- [ ] **Step 2: Implement product-driven staging**

Represent Orca/Wuwo metadata as two product descriptors rather than duplicate scripts. Stage both binaries per target, main launchers, README, and LICENSE. `npm pack` produces all ten tarballs: four variants plus one main package for each product.

- [ ] **Step 3: Implement Wuwo Node launcher**

Mirror Orca's realpath-safe optional dependency lookup and signal forwarding. Set:

```js
const env = {
  ...process.env,
  WUWO_MANAGED_BY_NPM: "1",
  WUWO_MANAGED_PACKAGE_ROOT: realpathSync(path.join(__dirname, "..")),
  WUWO_NODE_PATH: process.env.WUWO_NODE_PATH || process.execPath
};
```

The error reinstall command is exactly `npm install -g wuwo`.

- [ ] **Step 4: Add explicit installer product mode**

`install.sh` accepts `WUWO_INSTALL_PRODUCT=orca|wuwo`, selects the corresponding asset/binary/install name, verifies SHA-256, and never replaces the other command. Existing Orca calls default to `orca` for compatibility.

- [ ] **Step 5: Add local tarball migration smoke**

Install `@blade-ai/orca@0.2.54`, upgrade it to the staged `0.3.0-rc.0` transition tarball, install staged Wuwo, seed an isolated full Orca home/project, drive migration, and assert:

- `wuwo --version` reports the candidate;
- every supported item validates under `WUWO_HOME`;
- `orca --version` resolves to Wuwo after migration;
- `orca migrate rollback` restores Orca transition identity;
- original Orca fixture hashes are unchanged.

- [ ] **Step 6: Run and verify GREEN**

Run: `node scripts/release/test-stage-npm.mjs && node scripts/release/smoke-npm.mjs --version 0.3.0-rc.0 --stage-dir dist/npm/stage --tarballs-dir dist/npm/tarballs && node scripts/release/test-wuwo-migration-e2e.mjs`

Expected: dual staging, both CLI smokes, migration, forwarding, and rollback pass.

- [ ] **Step 7: Commit**

```bash
git add npm install.sh scripts/release
git commit -m "build: package Orca transition and Wuwo"
```

---

### Task 10: Gated Dual-Product Release Workflow

**Files:**
- Modify: `.github/workflows/release.yml`
- Modify: `.github/workflows/npm-token-check.yml`
- Modify: `scripts/release/verify-published.mjs`
- Modify: `scripts/release/test-verify-published.mjs`
- Test: `tests/pages_workflow_contract.test.mjs`

**Interfaces:**
- Produces: candidate publication under `next`/`transition-next`
- Produces: final dual `latest` promotion only after verification
- Consumes: dual artifacts and smoke tests Task 9

- [ ] **Step 1: Write failing workflow contract tests**

Parse the workflow and assert:

- both binaries build on all four target triples;
- both asset/checksum families upload;
- candidate packages publish Wuwo variants/main before Orca variants/main;
- candidate tags are non-default;
- migration smoke gates final dist-tag promotion;
- `latest` is moved for both packages only after both verifications;
- canonical repo defaults use `echoVic/wuwo`.

- [ ] **Step 2: Verify RED**

Run: `node --test tests/pages_workflow_contract.test.mjs && node scripts/release/test-verify-published.mjs`

Expected: FAIL because the workflow only builds/publishes Orca.

- [ ] **Step 3: Build and package both binaries**

Each matrix job runs:

```bash
cargo build --release --target "$TARGET" --bin orca --bin wuwo
```

Create `orca-$TARGET.tar.gz` and `wuwo-$TARGET.tar.gz` with separate checksums.

- [ ] **Step 4: Implement candidate publication and verification gates**

For `v0.3.0-rc.0`, publish Wuwo variants then `wuwo` with `--tag next`; verify a clean `npm exec --package wuwo@0.3.0-rc.0 -- wuwo --version`; then publish Orca transition with `--tag transition-next`; run installed migration E2E. No `latest` tag changes on a prerelease.

- [ ] **Step 5: Implement final publication**

For `v0.3.0`, publish both families under candidate tags, verify exact registry versions, installed binaries, migration, forwarding, rollback, and GitHub assets, then perform the two controlled promotions:

```bash
npm dist-tag add wuwo@0.3.0 latest
npm dist-tag add @blade-ai/orca@0.3.0 latest
```

If either promotion fails, retry idempotently and report the asymmetric state; never unpublish or claim that the two registry mutations are transactional.

- [ ] **Step 6: Run and verify GREEN**

Run: `node --test tests/pages_workflow_contract.test.mjs && node scripts/release/test-verify-published.mjs && actionlint .github/workflows/release.yml .github/workflows/npm-token-check.yml`

Expected: workflow contracts and `actionlint` pass.

- [ ] **Step 7: Commit**

```bash
git add .github/workflows scripts/release/verify-published.mjs scripts/release/test-verify-published.mjs tests/pages_workflow_contract.test.mjs
git commit -m "ci: publish Wuwo and Orca transition atomically"
```

---

### Task 11: Public Branding, Repository URLs, Site, and Migration Documentation

**Files:**
- Modify: `Cargo.toml`
- Modify: `npm/orca/package.json`
- Modify: `npm/wuwo/package.json`
- Modify: `npm/platform-package.json`
- Modify: `crates/orca-runtime/src/update_check.rs`
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `README.es-419.md`
- Modify: `README.ja-JP.md`
- Modify: `README.ko-KR.md`
- Modify: `README.pt-BR.md`
- Modify: `README.vi.md`
- Modify: `site/src/App.tsx`
- Modify: `site/src/shared.ts`
- Modify: `site/src/changelog/Changelog.tsx`
- Modify: `site/public/robots.txt`
- Modify: `site/public/sitemap.xml`
- Modify: `site/scripts/check-seo.mjs`
- Modify: `.github/workflows/pages.yml`
- Test: site build and repository-string inventory

**Interfaces:**
- Produces: canonical user-facing Wuwo naming and repository URLs
- Produces: Orca-to-Wuwo migration/rollback instructions
- Consumes: final commands and package names from Tasks 7–10

- [ ] **Step 1: Add failing public-string inventory**

Create a script/test that scans public metadata, docs, and site sources. It rejects `echoVic/blade-deepseek` except inside the migration design/history note, rejects new install instructions for `@blade-ai/orca` as the primary product, and requires `npm install -g wuwo`, `wuwo`, `WUWO_HOME`, `.wuwo/`, migration, rollback, and compatibility sections.

- [ ] **Step 2: Verify RED**

Run: `rg -n 'echoVic/blade-deepseek|npm install -g @blade-ai/orca|\\borca exec\\b' README*.md site/src site/public Cargo.toml npm crates/orca-runtime/src/update_check.rs`

Expected: many legacy public references are reported.

- [ ] **Step 3: Update canonical metadata and documentation**

Lead with Wuwo and the bare package. Keep Orca only in migration/compatibility sections. Document:

```bash
npm install -g wuwo
wuwo
wuwo migrate-from-orca
orca migrate rollback
```

Explain that `orca` forwards only after validated migration and that `~/.orca` is retained.

- [ ] **Step 4: Update the site without changing its current custom domain**

Rebrand content/title/structured data to Wuwo, link GitHub to `echoVic/wuwo`, publish the migration guide and release notes, and keep `orcaagent.dev` as the current transitional host. Preserve whale visual assets until a separate visual-brand decision replaces them.

- [ ] **Step 5: Build and verify GREEN**

Run: `npm ci --prefix site && npm run build --prefix site && npm run check:seo --prefix site && git diff --check`

Expected: site build and SEO checks pass; public-string inventory contains legacy terms only in explicit migration context.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml npm README*.md site .github/workflows/pages.yml crates/orca-runtime/src/update_check.rs
git commit -m "docs: rebrand Orca as Wuwo"
```

---

### Task 12: Full Verification, Candidate Registration, and Final 0.3.0 Release

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: release notes/changelog sources
- No unrelated runtime files

**Interfaces:**
- Produces: verified `wuwo@0.3.0-rc.0` under `next`
- Produces: source-ready `0.3.0` final revision after candidate verification
- Consumes: all prior tasks

- [ ] **Step 1: Run the full local verification suite**

Run:

```bash
cargo fmt --all -- --check
cargo test --workspace -- --test-threads=1
node scripts/release/test-stage-npm.mjs
node scripts/release/test-wuwo-migration-e2e.mjs
node --test tests/pages_workflow_contract.test.mjs
node scripts/release/test-verify-published.mjs
npm ci --prefix site
npm run build --prefix site
npm run check:seo --prefix site
git diff --check
```

Expected: every command exits zero with no failed tests.

- [ ] **Step 2: Audit the implementation against every design requirement**

Create a requirement-to-evidence checklist from:

`docs/superpowers/specs/2026-07-27-orca-to-wuwo-upgrade-migration-design.md`

For each naming surface, migration item, prompt choice, conflict mode, failure behavior, security rule, compatibility guarantee, and release gate, record the exact passing test or command. Any missing evidence returns to the relevant task.

- [ ] **Step 3: Commit and push the release-candidate source**

Push the isolated Wuwo branch only after reviewing that commits exclude the user's unrelated ACP/runtime changes. Create/push tag `v0.3.0-rc.0` to trigger the candidate workflow.

- [ ] **Step 4: Verify and register the bare npm package**

After the workflow succeeds, run:

```bash
npm view wuwo@0.3.0-rc.0 version dist-tags --json
npm exec --yes --package wuwo@0.3.0-rc.0 -- wuwo --version
```

Expected: version is `0.3.0-rc.0`, `next` points to it, no `latest` tag exists yet, and the executable reports `wuwo 0.3.0-rc.0`.

- [ ] **Step 5: Run installed candidate migration verification**

In a clean temporary prefix/home, install `@blade-ai/orca@0.2.54`, upgrade to transition candidate, migrate every supported fixture to `wuwo@0.3.0-rc.0`, verify `orca` forwarding, then rollback. Hash the Orca source before/after and require equality.

- [ ] **Step 6: Prepare final `0.3.0` source**

Only after candidate evidence passes, change Cargo/npm/release metadata from `0.3.0-rc.0` to `0.3.0`, rerun the full verification suite, and commit:

```bash
git commit -m "chore: prepare Wuwo 0.3.0"
```

- [ ] **Step 7: Publish and verify final `0.3.0`**

Push the final commit and tag `v0.3.0`. Wait for the dual-product workflow, then verify GitHub assets, `wuwo@0.3.0`, `@blade-ai/orca@0.3.0`, both `latest` tags, a clean global Wuwo install, the `0.2.54 -> 0.3.0` migration, Orca forwarding, and rollback. If any external credential, workflow, or registry gate fails, report the exact evidence and leave existing `latest` tags unchanged until the idempotent release job is repaired and rerun.
