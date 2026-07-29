# TUI Provider, Model, and Theme Onboarding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the numeric API-key-only setup flow with a typed seven-step wizard that selects the production provider, API key, model, and theme; previews themes from captured terminal capabilities; and safely persists user defaults.

**Architecture:** `orca-core` gains provider-aware layered configuration and hardened atomic user-file writers. `orca-tui` gains a pure `onboarding.rs` state model, while `setup_actions.rs` owns key routing, `app.rs` owns theme/capability effects, and `ui.rs` renders the typed wizard. API keys remain draft-only until Review, never enter review text or `config.toml`, and save failures are projected as fixed safe categories.

**Tech Stack:** Rust 2024, clap, serde, `toml_edit`, ratatui, crossterm, tui-textarea, existing `TerminalProfile`, `Theme`, `ModelSelection`, hosted TUI runtime, and strict RED/GREEN TDD.

---

## File Map

- Modify `Cargo.toml`
  - Add the direct workspace `toml_edit` dependency used by safe preference patching.
- Modify `crates/orca-core/Cargo.toml`
  - Consume `toml_edit`.
- Modify `crates/orca-core/src/config/file.rs`
  - Add persisted provider loading, provider overrides, typed persistence errors, atomic preference patching, and hardened auth persistence.
- Modify `src/cli.rs`
  - Make user-facing hidden provider flags optional and resolve provider through file/env/CLI precedence.
- Create `crates/orca-tui/src/onboarding.rs`
  - Own typed steps, private draft, closed options, selection movement, review rows, persistence outcomes, and safe labels.
- Modify `crates/orca-tui/src/lib.rs`
  - Register `onboarding`.
- Modify `crates/orca-tui/src/types.rs`
  - Replace `setup_step` with `OnboardingState`; expose central theme/diagnostic projection helpers.
- Modify `crates/orca-tui/src/setup_actions.rs`
  - Route setup keys, stage API key, atomically apply Review, invoke persistence, and finish setup.
- Modify `crates/orca-tui/src/status_key_actions.rs`
  - Propagate `PreviewTheme` from setup to the app.
- Modify `crates/orca-tui/src/input_event_actions.rs`
  - Permit setup paste only on the typed API-key step.
- Modify `crates/orca-tui/src/app.rs`
  - Initialize the wizard, retain captured `TerminalProfile`, mutate the active theme centrally, and apply preview effects without reprobe.
- Modify `crates/orca-tui/src/diagnostics.rs`
  - Update requested/resolved theme projections during preview.
- Modify `crates/orca-tui/src/ui.rs`
  - Render all seven typed setup steps with bounded geometry and API-key-only hardware cursor.
- Modify `README.md`
  - Document the first-run wizard and persistence contract.
- Modify `README.zh-CN.md`
  - Document the same contract in Chinese.

No MCP, workflow, history schema, session transcript, keybindings schema, terminal probe, or `/doctor` command changes are required.

### Task 1: Persist and Resolve the Production Provider

**Files:**
- Modify: `crates/orca-core/src/config/file.rs`
- Modify: `src/cli.rs`

- [ ] **Step 1: Write failing provider config and precedence tests**

In `crates/orca-core/src/config/file.rs`, add:

```rust
#[test]
fn provider_defaults_to_deepseek_and_parses_explicit_values() {
    assert_eq!(
        toml::from_str::<FileConfig>("").unwrap().provider,
        ProviderKind::DeepSeek,
    );
    assert_eq!(
        toml::from_str::<FileConfig>("provider = \"deep-seek\"")
            .unwrap()
            .provider,
        ProviderKind::DeepSeek,
    );
    assert_eq!(
        toml::from_str::<FileConfig>("provider = \"mock\"")
            .unwrap()
            .provider,
        ProviderKind::Mock,
    );
}

#[test]
fn provider_override_layers_follow_file_env_cli_order() {
    let base = FileConfig {
        provider: ProviderKind::DeepSeek,
        ..FileConfig::default()
    };
    let env = ConfigOverrides {
        provider: Some(ProviderKind::DeepSeekFixture),
        ..ConfigOverrides::default()
    };
    let cli = ConfigOverrides {
        provider: Some(ProviderKind::Mock),
        ..ConfigOverrides::default()
    };

    assert_eq!(
        apply_override_layers(base, env, cli).provider,
        ProviderKind::Mock,
    );
}

#[test]
fn trusted_project_config_cannot_override_user_provider() {
    let directory = tempfile::tempdir().unwrap();
    let user_dir = directory.path().join("user");
    let project = directory.path().join("project");
    std::fs::create_dir_all(&user_dir).unwrap();
    std::fs::create_dir_all(project.join(".orca")).unwrap();
    std::fs::write(
        user_dir.join("config.toml"),
        "provider = \"deep-seek\"\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".orca/config.toml"),
        "provider = \"mock\"\n",
    )
    .unwrap();
    crate::config::folder_trust::set_trust_with_config_dir(
        &project,
        &user_dir,
        crate::config::folder_trust::TrustLevel::Trusted,
    )
    .unwrap();

    let config =
        load_layered_config_from_paths(&user_dir.join("config.toml"), &project);
    assert_eq!(config.provider, ProviderKind::DeepSeek);
}
```

In `src/cli.rs`, add source and parser tests:

```rust
#[test]
fn user_facing_provider_flags_are_optional_but_workers_remain_resolved() {
    let source = include_str!("cli.rs");
    for struct_name in ["pub struct Cli", "struct ExecArgs", "struct WorkflowRunArgs"] {
        let start = source.find(struct_name).unwrap();
        let provider = source[start..].find("provider: Option<ProviderKind>").unwrap();
        assert!(provider < source[start..].find("\n}").unwrap());
    }
    for struct_name in ["struct WorkflowWorkerArgs", "struct SubagentWorkerArgs"] {
        let start = source.find(struct_name).unwrap();
        let provider = source[start..].find("provider: ProviderKind").unwrap();
        assert!(provider < source[start..].find("\n}").unwrap());
    }
}

#[test]
fn provider_precedence_resolves_cli_then_env_then_user_default() {
    let base = FileConfig {
        provider: ProviderKind::DeepSeek,
        ..FileConfig::default()
    };
    assert_eq!(
        apply_override_layers(
            base.clone(),
            ConfigOverrides {
                provider: Some(ProviderKind::DeepSeekFixture),
                ..ConfigOverrides::default()
            },
            ConfigOverrides {
                provider: Some(ProviderKind::Mock),
                ..ConfigOverrides::default()
            },
        )
        .provider,
        ProviderKind::Mock,
    );
    assert_eq!(
        apply_override_layers(
            base.clone(),
            ConfigOverrides {
                provider: Some(ProviderKind::DeepSeekFixture),
                ..ConfigOverrides::default()
            },
            ConfigOverrides::default(),
        )
        .provider,
        ProviderKind::DeepSeekFixture,
    );
    assert_eq!(
        apply_override_layers(
            base,
            ConfigOverrides::default(),
            ConfigOverrides::default(),
        )
        .provider,
        ProviderKind::DeepSeek,
    );
}
```

- [ ] **Step 2: Run provider tests and verify RED**

```bash
cargo test -p orca-core provider_defaults_to_deepseek -- --nocapture
cargo test -p orca-core provider_override_layers -- --nocapture
cargo test -p blade-deepseek provider_precedence_resolves -- --nocapture
```

Expected: compilation fails because `FileConfig.provider`,
`ConfigOverrides.provider`, and optional CLI provider fields do not exist.

- [ ] **Step 3: Add provider to layered config**

In `crates/orca-core/src/config/file.rs` import `ProviderKind`, then add:

```rust
fn default_provider() -> ProviderKind {
    ProviderKind::DeepSeek
}
```

Add this field to `FileConfig`:

```rust
pub provider: ProviderKind,
```

Add this field to `RawFileConfig`:

```rust
#[serde(default = "default_provider")]
pub provider: ProviderKind,
```

Initialize/copy it in `Default` and `From<RawFileConfig>`.

Extend `ConfigOverrides`:

```rust
pub provider: Option<ProviderKind>,
```

Apply it before the other override fields:

```rust
if let Some(provider) = overrides.provider {
    config.provider = provider;
}
```

Reject project-local provider:

```rust
table.remove("provider");
```

- [ ] **Step 4: Make user-facing CLI provider overrides optional**

In `src/cli.rs`, change only these fields:

```rust
// Cli, ExecArgs, WorkflowRunArgs
#[arg(long, value_enum, hide = true)]
provider: Option<ProviderKind>,
```

Keep these resolved fields unchanged:

```rust
// WorkflowWorkerArgs, SubagentWorkerArgs, WorkflowCliLaunchRecord
provider: ProviderKind,
```

Add provider to `env_overrides()`:

```rust
provider: match env::var("ORCA_PROVIDER") {
    Ok(value) => Some(
        ProviderKind::from_str(&value, true)
            .map_err(|_| format!("unsupported provider '{value}'"))?,
    ),
    Err(_) => None,
},
```

Pass CLI provider through `ConfigOverrides` at every user-facing load. Since
`apply_override_layers` already applies environment then CLI, each normal
`RunConfig` uses:

```rust
provider: file_config.provider,
```

Update every `ConfigOverrides` literal with either the actual provider or
`provider: None`.

`run_workflow_command` must pass the resolved provider rather than the raw
optional flag:

```rust
let run_config = match build_workflow_run_config(
    &cwd,
    args.provider,
    args.model.clone(),
    args.api_key.clone(),
    args.base_url.clone(),
) {
    Ok(config) => config,
    Err(error) => {
        eprintln!("orca: {error}");
        return 1;
    }
};
let resolved_provider = run_config.provider;

spawn_workflow_worker(
    &cwd,
    session_id,
    resolved_provider,
    args.model,
    run_config.api_key,
    args.base_url,
    &input,
)
```

Change `build_workflow_run_config` only at its user-facing call boundary:

```rust
fn build_workflow_run_config(
    cwd: &Path,
    provider_override: Option<ProviderKind>,
    model_override: Option<String>,
    api_key_override: Option<String>,
    base_url_override: Option<String>,
) -> Result<RunConfig, String> {
    let file_config = load_effective_file_config(
        cwd,
        ConfigOverrides {
            provider: provider_override,
            model: model_override,
            mode: None,
            api_key: api_key_override,
            base_url: base_url_override,
            reasoning_effort: None,
        },
    )?;
    if !file_config.workflows.resolved().enabled {
        return Err("workflows are disabled".to_string());
    }
    let model = ModelSelection::parse(file_config.model)?;
    Ok(RunConfig {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        prompt: String::new(),
        cwd: Some(cwd.to_path_buf()),
        output_format: OutputFormat::Jsonl,
        approval_mode: file_config.mode.unwrap_or_default(),
        provider: file_config.provider,
        verifier: None,
        model,
        model_runtime: file_config.model_runtime,
        reasoning_effort: file_config.reasoning_effort,
        api_key: file_config.api_key,
        base_url: file_config.base_url,
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: file_config.permission_profiles,
        runtime_workspace_roots: None,
        permission_rules: file_config.permissions,
        additional_working_directories: Vec::new(),
        max_budget_usd: None,
        mcp_servers: file_config.mcp_servers,
        hooks: file_config.hooks,
        external_tools: crate::tools::external::load_default_external_tools(),
        subagents: file_config.subagents.normalized(),
        tools: file_config.tools.normalized(),
        workflows: file_config.workflows.resolved(),
        theme: file_config.theme,
        vim_mode: file_config.vim_mode,
        vim_insert_escape: file_config.vim_insert_escape.clone(),
        update_check: file_config.update_check,
        desktop_notifications: false,
        terminal_notifications: false,
        auto_memory: file_config.auto_memory,
    })
}
```

Keep the worker path resolved by passing:

```rust
provider_override: Some(args.provider)
```

or by using a separate internal helper whose provider parameter is
`ProviderKind`. Tests must cover `workflow run` with no provider flag using the
user-config provider and explicit hidden flags overriding it.

- [ ] **Step 5: Run provider and CLI tests GREEN**

```bash
cargo test -p orca-core config::file -- --nocapture
cargo test -p blade-deepseek cli::tests -- --nocapture
cargo check --workspace
```

Expected: provider defaults to DeepSeek, CLI/env precedence passes, trusted
project provider is ignored, worker provider handoff tests remain green, and
the workspace compiles without missing struct fields.

- [ ] **Step 6: Commit provider precedence**

```bash
git add crates/orca-core/src/config/file.rs src/cli.rs
git commit -m "feat(config): persist production provider defaults" \
  -m "Load provider from user configuration while preserving explicit environment, CLI, and worker overrides." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 2: Add Atomic User Preference Persistence

**Files:**
- Modify: `Cargo.toml`
- Modify: `crates/orca-core/Cargo.toml`
- Modify: `crates/orca-core/src/config/file.rs`

- [ ] **Step 1: Write failing patch validation and preservation tests**

Add to `crates/orca-core/src/config/file.rs`:

```rust
#[test]
fn preference_patch_accepts_only_production_provider_and_known_models() {
    assert!(UserPreferencePatch::new(
        ProviderKind::DeepSeek,
        "auto",
        ThemeName::Dark,
    )
    .is_ok());
    assert_eq!(
        UserPreferencePatch::new(
            ProviderKind::Mock,
            "auto",
            ThemeName::Dark,
        )
        .unwrap_err(),
        UserPreferenceValidationError::UnsupportedProvider,
    );
    assert_eq!(
        UserPreferencePatch::new(
            ProviderKind::DeepSeek,
            "unknown",
            ThemeName::Dark,
        )
        .unwrap_err(),
        UserPreferenceValidationError::UnsupportedModel,
    );
}

#[test]
fn preference_patch_preserves_comments_unknown_keys_and_nested_tables() {
    let source = "\
# keep me
unknown = \"value\"
model = \"deepseek-v4-flash\"

[tools]
max_read_parallel = 7
";
    let patch = UserPreferencePatch::new(
        ProviderKind::DeepSeek,
        "auto",
        ThemeName::Solarized,
    )
    .unwrap();
    let output = patch_user_preferences(source, &patch).unwrap();

    assert!(output.contains("# keep me"));
    assert!(output.contains("unknown = \"value\""));
    assert!(output.contains("[tools]"));
    assert!(output.contains("max_read_parallel = 7"));
    assert!(output.contains("provider = \"deep-seek\""));
    assert!(output.contains("model = \"auto\""));
    assert!(output.contains("theme = \"solarized\""));
    assert!(!output.contains("api_key"));
}

#[test]
fn invalid_existing_config_is_not_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    let original = b"this is not [valid toml {{{";
    std::fs::write(&path, original).unwrap();
    let patch = UserPreferencePatch::new(
        ProviderKind::DeepSeek,
        "auto",
        ThemeName::Dark,
    )
    .unwrap();

    assert_eq!(
        save_user_preferences_at(&path, &patch).unwrap_err(),
        UserConfigSaveError::InvalidExistingContent,
    );
    assert_eq!(std::fs::read(path).unwrap(), original);
}
```

Add table-driven unsafe-path tests:

```rust
#[test]
fn preference_writer_rejects_unsafe_and_oversized_existing_paths() {
    let patch = UserPreferencePatch::new(
        ProviderKind::DeepSeek,
        "auto",
        ThemeName::Dark,
    )
    .unwrap();

    let directory = tempfile::tempdir().unwrap();
    let dir_path = directory.path().join("directory");
    std::fs::create_dir(&dir_path).unwrap();
    assert_eq!(
        save_user_preferences_at(&dir_path, &patch).unwrap_err(),
        UserConfigSaveError::UnsafeExistingPath,
    );

    let oversized = directory.path().join("oversized.toml");
    std::fs::write(&oversized, vec![b'x'; MAX_USER_CONFIG_BYTES + 1]).unwrap();
    assert_eq!(
        save_user_preferences_at(&oversized, &patch).unwrap_err(),
        UserConfigSaveError::ExistingFileTooLarge,
    );

    #[cfg(unix)]
    {
        let target = directory.path().join("target.toml");
        std::fs::write(&target, "theme = \"dark\"").unwrap();
        let link = directory.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            save_user_preferences_at(&link, &patch).unwrap_err(),
            UserConfigSaveError::UnsafeExistingPath,
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "theme = \"dark\"");
    }
}
```

- [ ] **Step 2: Run preference persistence tests and verify RED**

```bash
cargo test -p orca-core preference_patch_accepts -- --nocapture
cargo test -p orca-core preference_patch_preserves -- --nocapture
cargo test -p orca-core preference_writer_rejects -- --nocapture
```

Expected: compilation fails because the patch/error types and writers do not
exist.

- [ ] **Step 3: Add `toml_edit` and typed errors**

In the workspace `Cargo.toml`:

```toml
toml_edit = "0.22"
```

In `crates/orca-core/Cargo.toml`:

```toml
toml_edit = { workspace = true }
```

In `config/file.rs` define:

```rust
pub const MAX_USER_CONFIG_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserPreferenceValidationError {
    UnsupportedProvider,
    UnsupportedModel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserConfigSaveError {
    ConfigDirectoryUnavailable,
    UnsafeExistingPath,
    ExistingFileTooLarge,
    InvalidExistingContent,
    CreateDirectoryFailed,
    CreateTemporaryFileFailed,
    ReadFailed,
    WriteFailed,
    ReplaceFailed,
}

impl UserConfigSaveError {
    pub const fn safe_label(self) -> &'static str {
        match self {
            Self::ConfigDirectoryUnavailable => "config directory unavailable",
            Self::UnsafeExistingPath => "unsafe existing config path",
            Self::ExistingFileTooLarge => "existing config is too large",
            Self::InvalidExistingContent => "invalid existing config",
            Self::CreateDirectoryFailed => "could not create config directory",
            Self::CreateTemporaryFileFailed => "could not create temporary config",
            Self::ReadFailed => "could not read existing config",
            Self::WriteFailed => "could not write config",
            Self::ReplaceFailed => "could not replace config",
        }
    }
}
```

Define a private-field patch:

```rust
pub struct UserPreferencePatch {
    provider: ProviderKind,
    model: String,
    theme: ThemeName,
}

impl UserPreferencePatch {
    pub fn new(
        provider: ProviderKind,
        model: impl Into<String>,
        theme: ThemeName,
    ) -> Result<Self, UserPreferenceValidationError> {
        if provider != ProviderKind::DeepSeek {
            return Err(UserPreferenceValidationError::UnsupportedProvider);
        }
        let model = model.into();
        if !crate::model::allowed_models().contains(&model.as_str()) {
            return Err(UserPreferenceValidationError::UnsupportedModel);
        }
        Ok(Self {
            provider,
            model,
            theme,
        })
    }
}
```

- [ ] **Step 4: Implement pure TOML patching**

Add:

```rust
fn patch_user_preferences(
    source: &str,
    patch: &UserPreferencePatch,
) -> Result<String, UserConfigSaveError> {
    use toml_edit::{value, DocumentMut};

    let mut document = if source.trim().is_empty() {
        DocumentMut::new()
    } else {
        source
            .parse::<DocumentMut>()
            .map_err(|_| UserConfigSaveError::InvalidExistingContent)?
    };
    document["provider"] = value("deep-seek");
    document["model"] = value(patch.model.as_str());
    document["theme"] = value(patch.theme.as_str());
    Ok(document.to_string())
}
```

- [ ] **Step 5: Implement bounded regular-file reads and atomic replacement**

Import:

```rust
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
```

Add:

```rust
static USER_FILE_TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

fn read_optional_regular_file(
    path: &Path,
    max_bytes: usize,
) -> Result<Option<(Vec<u8>, Option<std::fs::Permissions>)>, UserConfigSaveError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(_) => return Err(UserConfigSaveError::ReadFailed),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(UserConfigSaveError::UnsafeExistingPath);
    }
    if metadata.len() > max_bytes as u64 {
        return Err(UserConfigSaveError::ExistingFileTooLarge);
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|_| UserConfigSaveError::ReadFailed)?;
    let opened = file
        .metadata()
        .map_err(|_| UserConfigSaveError::ReadFailed)?;
    if !opened.is_file() {
        return Err(UserConfigSaveError::UnsafeExistingPath);
    }
    let mut bytes = Vec::with_capacity(opened.len().min(max_bytes as u64) as usize);
    file.take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| UserConfigSaveError::ReadFailed)?;
    if bytes.len() > max_bytes {
        return Err(UserConfigSaveError::ExistingFileTooLarge);
    }
    Ok(Some((bytes, Some(opened.permissions()))))
}

fn open_unique_user_temp(
    path: &Path,
) -> Result<(std::path::PathBuf, File), UserConfigSaveError> {
    let parent = path
        .parent()
        .ok_or(UserConfigSaveError::CreateDirectoryFailed)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(UserConfigSaveError::CreateTemporaryFileFailed)?;
    for _ in 0..64 {
        let counter = USER_FILE_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp_path = parent.join(format!(
            ".{name}.tmp-{}-{counter}",
            std::process::id(),
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temp_path) {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(UserConfigSaveError::CreateTemporaryFileFailed),
        }
    }
    Err(UserConfigSaveError::CreateTemporaryFileFailed)
}

fn atomic_replace_user_file(
    path: &Path,
    bytes: &[u8],
    existing_permissions: Option<std::fs::Permissions>,
) -> Result<(), UserConfigSaveError> {
    let parent = path
        .parent()
        .ok_or(UserConfigSaveError::CreateDirectoryFailed)?;
    fs::create_dir_all(parent)
        .map_err(|_| UserConfigSaveError::CreateDirectoryFailed)?;
    let (temp_path, mut temp) = open_unique_user_temp(path)?;
    let result = (|| {
        if let Some(permissions) = existing_permissions {
            temp.set_permissions(permissions)
                .map_err(|_| UserConfigSaveError::WriteFailed)?;
        }
        temp.write_all(bytes)
            .and_then(|()| temp.sync_all())
            .map_err(|_| UserConfigSaveError::WriteFailed)?;
        drop(temp);
        fs::rename(&temp_path, path)
            .map_err(|_| UserConfigSaveError::ReplaceFailed)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}
```

Expose testable and production entry points:

```rust
fn save_user_preferences_at(
    path: &Path,
    patch: &UserPreferencePatch,
) -> Result<(), UserConfigSaveError> {
    let existing = read_optional_regular_file(path, MAX_USER_CONFIG_BYTES)?;
    let (source, permissions) = match existing {
        Some((bytes, permissions)) => (
            String::from_utf8(bytes)
                .map_err(|_| UserConfigSaveError::InvalidExistingContent)?,
            permissions,
        ),
        None => (String::new(), None),
    };
    let output = patch_user_preferences(&source, patch)?;
    atomic_replace_user_file(path, output.as_bytes(), permissions)
}

pub fn save_user_preferences(
    patch: &UserPreferencePatch,
) -> Result<(), UserConfigSaveError> {
    let directory =
        config_dir().ok_or(UserConfigSaveError::ConfigDirectoryUnavailable)?;
    save_user_preferences_at(&directory.join("config.toml"), patch)
}
```

- [ ] **Step 6: Run preference persistence tests GREEN**

```bash
cargo test -p orca-core preference_patch -- --nocapture
cargo test -p orca-core preference_writer -- --nocapture
cargo test -p orca-core invalid_existing_config_is_not_replaced -- --nocapture
```

Expected: all patch, preservation, unsafe-path, size, and atomic-write tests
pass.

- [ ] **Step 7: Commit safe preference persistence**

```bash
git add Cargo.toml Cargo.lock crates/orca-core/Cargo.toml \
  crates/orca-core/src/config/file.rs
git commit -m "feat(config): atomically persist onboarding preferences" \
  -m "Patch only provider, model, and theme while preserving valid user TOML and rejecting unsafe existing paths." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 3: Harden API-Key Persistence

**Files:**
- Modify: `crates/orca-core/src/config/file.rs`

- [ ] **Step 1: Write failing auth persistence tests**

Add:

```rust
#[test]
fn auth_writer_preserves_unrelated_entries_and_never_reports_secret() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    std::fs::write(
        &path,
        r#"{"OTHER_TOKEN":"keep","DEEPSEEK_API_KEY":"old"}"#,
    )
    .unwrap();

    save_api_key_at(&path, "sk-new-secret").unwrap();
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(value["OTHER_TOKEN"], "keep");
    assert_eq!(value["DEEPSEEK_API_KEY"], "sk-new-secret");
}

#[test]
fn invalid_auth_json_is_left_byte_identical() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("auth.json");
    let original = b"{ invalid json";
    std::fs::write(&path, original).unwrap();

    assert_eq!(
        save_api_key_at(&path, "sk-secret").unwrap_err(),
        UserConfigSaveError::InvalidExistingContent,
    );
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[test]
fn auth_writer_rejects_symlink_without_touching_target() {
    #[cfg(unix)]
    {
        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        std::fs::write(&target, r#"{"OTHER_TOKEN":"keep"}"#).unwrap();
        let link = directory.path().join("auth.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert_eq!(
            save_api_key_at(&link, "sk-secret").unwrap_err(),
            UserConfigSaveError::UnsafeExistingPath,
        );
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            r#"{"OTHER_TOKEN":"keep"}"#,
        );
    }
}
```

- [ ] **Step 2: Run auth tests and verify RED**

```bash
cargo test -p orca-core auth_writer_preserves -- --nocapture
cargo test -p orca-core invalid_auth_json_is_left -- --nocapture
```

Expected: compilation fails because `save_api_key_at` does not exist and
`save_api_key` does not return a typed result.

- [ ] **Step 3: Implement hardened auth persistence**

Add:

```rust
pub const MAX_AUTH_FILE_BYTES: usize = 1024 * 1024;

fn save_api_key_at(
    path: &Path,
    api_key: &str,
) -> Result<(), UserConfigSaveError> {
    let existing = read_optional_regular_file(path, MAX_AUTH_FILE_BYTES)?;
    let (mut map, permissions) = match existing {
        Some((bytes, permissions)) => (
            serde_json::from_slice::<HashMap<String, String>>(&bytes)
                .map_err(|_| UserConfigSaveError::InvalidExistingContent)?,
            permissions,
        ),
        None => (HashMap::new(), None),
    };
    map.insert("DEEPSEEK_API_KEY".to_string(), api_key.to_string());
    let bytes = serde_json::to_vec_pretty(&map)
        .map_err(|_| UserConfigSaveError::WriteFailed)?;
    atomic_replace_user_file(path, &bytes, permissions)
}

pub fn save_api_key(api_key: &str) -> Result<(), UserConfigSaveError> {
    let directory =
        config_dir().ok_or(UserConfigSaveError::ConfigDirectoryUnavailable)?;
    save_api_key_at(&directory.join("auth.json"), api_key)
}
```

No error formatting may interpolate `api_key`.

- [ ] **Step 4: Update existing callers and run auth tests GREEN**

Existing production callers must handle the `Result`; setup will consume it in
Task 5. Any other caller that intentionally ignores persistence must use:

```rust
let _ = save_api_key(api_key);
```

Run:

```bash
cargo test -p orca-core auth_writer -- --nocapture
cargo test -p orca-core invalid_auth_json -- --nocapture
cargo test -p orca-core config::file -- --nocapture
```

Expected: auth preservation, invalid-content, symlink, size, and legacy load
tests pass.

- [ ] **Step 5: Commit hardened auth persistence**

```bash
git add crates/orca-core/src/config/file.rs crates/orca-tui/src/setup_actions.rs
git commit -m "fix(config): harden onboarding credential persistence" \
  -m "Preserve valid auth entries and reject unsafe or malformed files without exposing credential text." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 4: Model the Typed Onboarding Wizard

**Files:**
- Create: `crates/orca-tui/src/onboarding.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [ ] **Step 1: Write failing step, option, and secret-boundary tests**

Create `onboarding.rs`, register it in `lib.rs`, and add:

```rust
#[test]
fn onboarding_has_exact_seven_step_order() {
    assert_eq!(
        OnboardingStep::ALL,
        [
            OnboardingStep::Welcome,
            OnboardingStep::Provider,
            OnboardingStep::ApiKey,
            OnboardingStep::Model,
            OnboardingStep::Theme,
            OnboardingStep::Review,
            OnboardingStep::Complete,
        ],
    );
}

#[test]
fn onboarding_choices_are_closed_and_production_safe() {
    assert_eq!(production_provider_options(), [ProviderKind::DeepSeek]);
    assert_eq!(model_options(), orca_core::model::allowed_models());
    assert_eq!(
        theme_options(),
        [
            ThemeName::Auto,
            ThemeName::Dark,
            ThemeName::Light,
            ThemeName::Solarized,
            ThemeName::Catppuccin,
        ],
    );
}

#[test]
fn test_provider_is_normalized_only_in_onboarding_draft() {
    let state = OnboardingState::new(
        ProviderKind::Mock,
        "auto",
        ThemeName::Auto,
    );
    assert_eq!(state.draft_provider(), ProviderKind::DeepSeek);
}

#[test]
fn review_rows_never_include_api_key() {
    let mut state = OnboardingState::new(
        ProviderKind::DeepSeek,
        "auto",
        ThemeName::Dark,
    );
    state.set_api_key("sk-do-not-render".to_string());
    let review = state.review_rows().join("\n");
    assert!(review.contains("API key: configured"));
    assert!(!review.contains("sk-do-not-render"));
    assert!(!format!("{:?}", state.review_rows()).contains("sk-do-not-render"));
}
```

Add navigation tests:

```rust
#[test]
fn option_selection_wraps_and_updates_draft() {
    let mut state = OnboardingState::new(
        ProviderKind::DeepSeek,
        "auto",
        ThemeName::Auto,
    );
    state.set_step_for_test(OnboardingStep::Theme);
    assert_eq!(state.selected_theme(), ThemeName::Auto);
    assert_eq!(state.move_previous(), Some(ThemeName::Catppuccin));
    assert_eq!(state.move_next(), Some(ThemeName::Auto));
    assert_eq!(state.move_next(), Some(ThemeName::Dark));
}

#[test]
fn persistence_starts_not_attempted_and_errors_are_safe() {
    let state = OnboardingState::new(
        ProviderKind::DeepSeek,
        "auto",
        ThemeName::Auto,
    );
    assert_eq!(state.auth_outcome(), &SaveOutcome::NotAttempted);
    assert_eq!(state.preferences_outcome(), &SaveOutcome::NotAttempted);
    for error in [
        UserConfigSaveError::ConfigDirectoryUnavailable,
        UserConfigSaveError::UnsafeExistingPath,
        UserConfigSaveError::ExistingFileTooLarge,
        UserConfigSaveError::InvalidExistingContent,
        UserConfigSaveError::CreateDirectoryFailed,
        UserConfigSaveError::CreateTemporaryFileFailed,
        UserConfigSaveError::ReadFailed,
        UserConfigSaveError::WriteFailed,
        UserConfigSaveError::ReplaceFailed,
    ] {
        let label = error.safe_label();
        assert!(!label.contains('/'));
        assert!(!label.contains("sk-"));
        assert!(!label.chars().any(char::is_control));
    }
}
```

- [ ] **Step 2: Run onboarding model tests and verify RED**

```bash
cargo test -p orca-tui onboarding::tests::onboarding_has_exact -- --nocapture
cargo test -p orca-tui onboarding::tests::review_rows_never -- --nocapture
cargo test -p orca-tui onboarding::tests::option_selection_wraps -- --nocapture
```

Expected: compilation fails because the onboarding types and module do not
exist.

- [ ] **Step 3: Implement closed wizard types**

Define:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnboardingStep {
    Welcome,
    Provider,
    ApiKey,
    Model,
    Theme,
    Review,
    Complete,
}

impl OnboardingStep {
    pub(crate) const ALL: [Self; 7] = [
        Self::Welcome,
        Self::Provider,
        Self::ApiKey,
        Self::Model,
        Self::Theme,
        Self::Review,
        Self::Complete,
    ];

    pub(crate) const fn ordinal(self) -> usize {
        match self {
            Self::Welcome => 1,
            Self::Provider => 2,
            Self::ApiKey => 3,
            Self::Model => 4,
            Self::Theme => 5,
            Self::Review => 6,
            Self::Complete => 7,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum SaveOutcome {
    NotAttempted,
    Saved,
    Failed(UserConfigSaveError),
}

impl From<Result<(), UserConfigSaveError>> for SaveOutcome {
    fn from(result: Result<(), UserConfigSaveError>) -> Self {
        match result {
            Ok(()) => Self::Saved,
            Err(error) => Self::Failed(error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnboardingError {
    MissingApiKey,
    UnsupportedProvider,
    UnsupportedModel,
    SharedConfigUnavailable,
}

impl OnboardingError {
    pub(crate) const fn safe_label(self) -> &'static str {
        match self {
            Self::MissingApiKey => "API key is required",
            Self::UnsupportedProvider => "unsupported provider selection",
            Self::UnsupportedModel => "unsupported model selection",
            Self::SharedConfigUnavailable => "shared configuration unavailable",
        }
    }
}
```

Do not derive `Debug`, `Clone`, or serialization traits on
`OnboardingDraft`/`OnboardingState`.

Add closed options:

```rust
const PROVIDER_OPTIONS: [ProviderKind; 1] = [ProviderKind::DeepSeek];
const THEME_OPTIONS: [ThemeName; 5] = [
    ThemeName::Auto,
    ThemeName::Dark,
    ThemeName::Light,
    ThemeName::Solarized,
    ThemeName::Catppuccin,
];

pub(crate) const fn production_provider_options() -> &'static [ProviderKind] {
    &PROVIDER_OPTIONS
}

pub(crate) fn model_options() -> &'static [&'static str] {
    orca_core::model::allowed_models()
}

pub(crate) const fn theme_options() -> &'static [ThemeName] {
    &THEME_OPTIONS
}
```

Implement the private state:

```rust
struct OnboardingDraft {
    provider: ProviderKind,
    model: String,
    theme: ThemeName,
    api_key: Option<String>,
}

pub(crate) struct OnboardingState {
    step: OnboardingStep,
    draft: OnboardingDraft,
    selected: usize,
    auth_outcome: SaveOutcome,
    preferences_outcome: SaveOutcome,
    error: Option<OnboardingError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OnboardingOptionRow {
    pub(crate) label: &'static str,
    pub(crate) description: &'static str,
    pub(crate) selected: bool,
}

impl OnboardingState {
    pub(crate) fn new(
        provider: ProviderKind,
        model: &str,
        theme: ThemeName,
    ) -> Self {
        let provider = if production_provider_options().contains(&provider) {
            provider
        } else {
            ProviderKind::DeepSeek
        };
        let model = if model_options().contains(&model) {
            model.to_string()
        } else {
            orca_core::model::AUTO_MODEL.to_string()
        };
        Self {
            step: OnboardingStep::Welcome,
            draft: OnboardingDraft {
                provider,
                model,
                theme,
                api_key: None,
            },
            selected: 0,
            auth_outcome: SaveOutcome::NotAttempted,
            preferences_outcome: SaveOutcome::NotAttempted,
            error: None,
        }
    }

    pub(crate) const fn step(&self) -> OnboardingStep {
        self.step
    }

    pub(crate) const fn draft_provider(&self) -> ProviderKind {
        self.draft.provider
    }

    pub(crate) fn draft_model(&self) -> &str {
        &self.draft.model
    }

    pub(crate) const fn selected_theme(&self) -> ThemeName {
        self.draft.theme
    }

    pub(crate) fn set_api_key(&mut self, api_key: String) {
        self.draft.api_key = Some(api_key);
        self.error = None;
    }

    pub(crate) fn api_key(&self) -> Option<&str> {
        self.draft.api_key.as_deref()
    }

    pub(crate) fn take_api_key(&mut self) -> Option<String> {
        self.draft.api_key.take()
    }

    pub(crate) fn review_rows(&self) -> Vec<String> {
        vec![
            "Provider: DeepSeek".to_string(),
            format!("Model: {}", self.draft.model),
            format!("Theme: {}", self.draft.theme.as_str()),
            format!(
                "API key: {}",
                if self.draft.api_key.is_some() {
                    "configured"
                } else {
                    "missing"
                },
            ),
        ]
    }

    pub(crate) fn option_rows(&self) -> Vec<OnboardingOptionRow> {
        match self.step {
            OnboardingStep::Provider => PROVIDER_OPTIONS
                .iter()
                .enumerate()
                .map(|(index, _)| OnboardingOptionRow {
                    label: "DeepSeek",
                    description: "Production provider",
                    selected: index == self.selected,
                })
                .collect(),
            OnboardingStep::Model => model_options()
                .iter()
                .enumerate()
                .map(|(index, model)| OnboardingOptionRow {
                    label: model,
                    description: match *model {
                        orca_core::model::AUTO_MODEL => "Recommended",
                        orca_core::model::FLASH_MODEL => "Faster",
                        orca_core::model::PRO_MODEL => "Highest quality",
                        _ => "Unsupported",
                    },
                    selected: index == self.selected,
                })
                .collect(),
            OnboardingStep::Theme => THEME_OPTIONS
                .iter()
                .enumerate()
                .map(|(index, theme)| OnboardingOptionRow {
                    label: theme.as_str(),
                    description: match theme {
                        ThemeName::Auto => "Match terminal background",
                        ThemeName::Dark => "Dark",
                        ThemeName::Light => "Light",
                        ThemeName::Solarized => "Solarized dark",
                        ThemeName::Catppuccin => "Catppuccin Mocha",
                    },
                    selected: index == self.selected,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    pub(crate) fn completion_rows(&self) -> Vec<String> {
        fn row(name: &str, outcome: &SaveOutcome) -> String {
            match outcome {
                SaveOutcome::NotAttempted => {
                    format!("{name}: current session only — save not attempted")
                }
                SaveOutcome::Saved => format!("{name}: saved"),
                SaveOutcome::Failed(error) => {
                    format!("{name}: current session only — {}", error.safe_label())
                }
            }
        }
        vec![
            row("API key", &self.auth_outcome),
            row("Preferences", &self.preferences_outcome),
        ]
    }

    pub(crate) const fn error_label(&self) -> Option<&'static str> {
        match self.error {
            Some(error) => Some(error.safe_label()),
            None => None,
        }
    }

    pub(crate) const fn auth_outcome(&self) -> &SaveOutcome {
        &self.auth_outcome
    }

    pub(crate) const fn preferences_outcome(&self) -> &SaveOutcome {
        &self.preferences_outcome
    }

    pub(crate) const fn review_error(&self) -> Option<OnboardingError> {
        self.error
    }

    pub(crate) fn set_error(&mut self, error: OnboardingError) {
        self.error = Some(error);
    }

    pub(crate) fn finish_review(
        &mut self,
        auth: SaveOutcome,
        preferences: SaveOutcome,
    ) {
        self.auth_outcome = auth;
        self.preferences_outcome = preferences;
        self.draft.api_key = None;
        self.error = None;
        self.step = OnboardingStep::Complete;
    }

    pub(crate) fn advance(&mut self) -> bool {
        let next = match self.step {
            OnboardingStep::Welcome => OnboardingStep::Provider,
            OnboardingStep::Provider => OnboardingStep::ApiKey,
            OnboardingStep::ApiKey if self.draft.api_key.is_some() => {
                OnboardingStep::Model
            }
            OnboardingStep::Model => OnboardingStep::Theme,
            OnboardingStep::Theme => OnboardingStep::Review,
            OnboardingStep::ApiKey
            | OnboardingStep::Review
            | OnboardingStep::Complete => return false,
        };
        self.step = next;
        self.selected = self.index_for_current_value();
        self.error = None;
        true
    }

    pub(crate) fn move_previous(&mut self) -> Option<ThemeName> {
        self.move_by(-1)
    }

    pub(crate) fn move_next(&mut self) -> Option<ThemeName> {
        self.move_by(1)
    }

    fn move_by(&mut self, delta: isize) -> Option<ThemeName> {
        let len = self.option_len();
        if len == 0 {
            self.error = Some(OnboardingError::UnsupportedModel);
            return None;
        }
        self.selected =
            (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        self.error = None;
        match self.step {
            OnboardingStep::Provider => {
                self.draft.provider = PROVIDER_OPTIONS[self.selected];
                None
            }
            OnboardingStep::Model => {
                self.draft.model = model_options()[self.selected].to_string();
                None
            }
            OnboardingStep::Theme => {
                self.draft.theme = THEME_OPTIONS[self.selected];
                Some(self.draft.theme)
            }
            _ => None,
        }
    }

    fn option_len(&self) -> usize {
        match self.step {
            OnboardingStep::Provider => PROVIDER_OPTIONS.len(),
            OnboardingStep::Model => model_options().len(),
            OnboardingStep::Theme => THEME_OPTIONS.len(),
            _ => 0,
        }
    }

    fn index_for_current_value(&self) -> usize {
        match self.step {
            OnboardingStep::Provider => PROVIDER_OPTIONS
                .iter()
                .position(|value| *value == self.draft.provider)
                .unwrap_or(0),
            OnboardingStep::Model => model_options()
                .iter()
                .position(|value| *value == self.draft.model)
                .unwrap_or(0),
            OnboardingStep::Theme => THEME_OPTIONS
                .iter()
                .position(|value| *value == self.draft.theme)
                .unwrap_or(0),
            _ => 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn set_step_for_test(&mut self, step: OnboardingStep) {
        self.step = step;
        self.selected = self.index_for_current_value();
    }

    #[cfg(test)]
    pub(crate) fn set_outcomes_for_test(
        &mut self,
        auth: SaveOutcome,
        preferences: SaveOutcome,
    ) {
        self.auth_outcome = auth;
        self.preferences_outcome = preferences;
    }
}
```

- [ ] **Step 4: Add wizard state beside the legacy setup field**

In `types.rs` keep the legacy field temporarily:

```rust
pub setup_step: u8,
```

and add:

```rust
pub(crate) onboarding: OnboardingState,
```

Since `AppState::new` has no config input, initialize a deterministic safe
default:

```rust
onboarding: OnboardingState::new(
    ProviderKind::DeepSeek,
    orca_core::model::AUTO_MODEL,
    ThemeName::Auto,
),
```

Add:

```rust
pub(crate) fn initialize_onboarding(&mut self, config: &RunConfig) {
    self.onboarding = OnboardingState::new(
        config.provider,
        config.model.display_name(),
        config.theme,
    );
}
```

Do not update `setup_actions.rs`, `ui.rs`, `input_event_actions.rs`, or
`app.rs` in this task. Keeping `setup_step` makes this model-only commit
buildable. Task 5 performs the atomic consumer migration and then removes the
legacy field.

- [ ] **Step 5: Run onboarding model tests GREEN**

```bash
cargo test -p orca-tui onboarding -- --nocapture
cargo test -p orca-tui app_state_diagnostics_defaults_are_inert -- --nocapture
cargo check -p orca-tui
```

Expected: wizard model tests pass while the legacy setup implementation still
compiles:

```bash
cargo check -p orca-tui
```

- [ ] **Step 6: Commit typed wizard model**

```bash
git add crates/orca-tui/src/onboarding.rs crates/orca-tui/src/lib.rs \
  crates/orca-tui/src/types.rs
git commit -m "feat(tui): model typed first-run onboarding" \
  -m "Add closed provider, credential, model, theme, review, and completion state before migrating setup consumers." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 5: Route Wizard Actions and Apply Review

**Files:**
- Modify: `crates/orca-tui/src/setup_actions.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/input_event_actions.rs`
- Modify: `crates/orca-tui/src/ui.rs`

- [ ] **Step 1: Write failing setup transition tests**

Add a typed harness in `setup_actions.rs`:

```rust
#[derive(Default)]
struct SaveCalls {
    auth: usize,
    preferences: usize,
}

fn setup_harness() -> (
    AppState,
    RunConfig,
    Arc<Mutex<RunConfig>>,
    mpsc::Sender<UserAction>,
    mpsc::Receiver<UserAction>,
    TextArea<'static>,
    VimState,
) {
    let (action_tx, action_rx) = mpsc::unbounded();
    let mut config = crate::test_support::test_run_config();
    config.provider = ProviderKind::DeepSeek;
    config.model = ModelSelection::from_unchecked(Some("auto".to_string()));
    config.theme = ThemeName::Auto;
    config.api_key = None;
    let shared = Arc::new(Mutex::new(config.clone()));
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "auto".to_string(),
        "/tmp".to_string(),
    );
    state.status = AppStatus::Setup;
    state.initialize_onboarding(&config);
    let theme = Theme::named(ThemeName::Dark);
    let textarea = make_setup_textarea(&theme);
    (
        state,
        config,
        shared,
        action_tx,
        action_rx,
        textarea,
        VimState::new(false),
    )
}

#[allow(clippy::too_many_arguments)]
fn press_setup_key(
    code: KeyCode,
    state: &mut AppState,
    config: &mut RunConfig,
    shared: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim: &VimState,
    theme: &Theme,
    calls: &Arc<Mutex<SaveCalls>>,
    auth_result: Result<(), UserConfigSaveError>,
    preferences_result: Result<(), UserConfigSaveError>,
) -> SetupFlow {
    let key = KeyEvent::new(code, KeyModifiers::NONE);
    let auth_calls = Arc::clone(calls);
    let preference_calls = Arc::clone(calls);
    handle_setup_key_with_savers(
        &Event::Key(key),
        &key,
        state,
        config,
        shared,
        action_tx,
        textarea,
        vim,
        theme,
        None,
        move |_| {
            auth_calls.lock().unwrap().auth += 1;
            auth_result
        },
        move |_| {
            preference_calls.lock().unwrap().preferences += 1;
            preferences_result
        },
    )
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn drive_to_review(
    state: &mut AppState,
    config: &mut RunConfig,
    shared: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim: &VimState,
    theme: &Theme,
    calls: &Arc<Mutex<SaveCalls>>,
) {
    assert_eq!(state.onboarding.step(), OnboardingStep::Welcome);
    assert_eq!(
        press_setup_key(
            KeyCode::Enter,
            state,
            config,
            shared,
            action_tx,
            textarea,
            vim,
            theme,
            calls,
            Ok(()),
            Ok(()),
        ),
        SetupFlow::Continue,
    );
    assert_eq!(state.onboarding.step(), OnboardingStep::Provider);

    press_setup_key(
        KeyCode::Enter,
        state,
        config,
        shared,
        action_tx,
        textarea,
        vim,
        theme,
        calls,
        Ok(()),
        Ok(()),
    );
    assert_eq!(state.onboarding.step(), OnboardingStep::ApiKey);

    textarea.insert_str("sk-test-secret");
    press_setup_key(
        KeyCode::Enter,
        state,
        config,
        shared,
        action_tx,
        textarea,
        vim,
        theme,
        calls,
        Ok(()),
        Ok(()),
    );
    assert_eq!(state.onboarding.step(), OnboardingStep::Model);

    press_setup_key(
        KeyCode::Down,
        state,
        config,
        shared,
        action_tx,
        textarea,
        vim,
        theme,
        calls,
        Ok(()),
        Ok(()),
    );
    press_setup_key(
        KeyCode::Enter,
        state,
        config,
        shared,
        action_tx,
        textarea,
        vim,
        theme,
        calls,
        Ok(()),
        Ok(()),
    );
    assert_eq!(state.onboarding.step(), OnboardingStep::Theme);

    for _ in 0..3 {
        assert!(matches!(
            press_setup_key(
                KeyCode::Down,
                state,
                config,
                shared,
                action_tx,
                textarea,
                vim,
                theme,
                calls,
                Ok(()),
                Ok(()),
            ),
            SetupFlow::PreviewTheme(_),
        ));
    }
    press_setup_key(
        KeyCode::Enter,
        state,
        config,
        shared,
        action_tx,
        textarea,
        vim,
        theme,
        calls,
        Ok(()),
        Ok(()),
    );
    assert_eq!(state.onboarding.step(), OnboardingStep::Review);
}
```

Add:

```rust
#[test]
fn enter_advances_exact_wizard_sequence_without_early_persistence() {
    let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) =
        setup_harness();
    let theme = Theme::named(ThemeName::Dark);
    let calls = Arc::new(Mutex::new(SaveCalls::default()));

    drive_to_review(
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &mut textarea,
        &vim,
        &theme,
        &calls,
    );

    assert!(config.api_key.is_none());
    assert!(shared.lock().unwrap().api_key.is_none());
    assert!(!state.auth_configured);
    assert_eq!(calls.lock().unwrap().auth, 0);
    assert_eq!(calls.lock().unwrap().preferences, 0);
    assert!(
        !state
            .onboarding
            .review_rows()
            .join("\n")
            .contains("sk-test-secret")
    );
}
```

Add Review tests:

```rust
#[test]
fn review_applies_memory_before_independent_persistence_results() {
    let (
        mut state,
        mut config,
        shared,
        action_tx,
        action_rx,
        mut textarea,
        vim,
    ) = setup_harness();
    let theme = Theme::named(ThemeName::Dark);
    let calls = Arc::new(Mutex::new(SaveCalls::default()));
    drive_to_review(
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &mut textarea,
        &vim,
        &theme,
        &calls,
    );

    assert_eq!(
        press_setup_key(
            KeyCode::Enter,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &vim,
            &theme,
            &calls,
            Err(UserConfigSaveError::WriteFailed),
            Ok(()),
        ),
        SetupFlow::Continue,
    );
    assert_eq!(state.onboarding.step(), OnboardingStep::Complete);
    assert_eq!(config.provider, ProviderKind::DeepSeek);
    assert_eq!(config.model.display_name(), "deepseek-v4-flash");
    assert_eq!(config.theme, ThemeName::Solarized);
    assert_eq!(config.api_key.as_deref(), Some("sk-test-secret"));
    assert_eq!(shared.lock().unwrap().theme, ThemeName::Solarized);
    assert!(state.auth_configured);
    assert_eq!(state.model_name, "deepseek-v4-flash");
    assert_eq!(
        state.onboarding.auth_outcome(),
        &SaveOutcome::Failed(UserConfigSaveError::WriteFailed),
    );
    assert_eq!(
        state.onboarding.preferences_outcome(),
        &SaveOutcome::Saved,
    );
    assert_eq!(calls.lock().unwrap().auth, 1);
    assert_eq!(calls.lock().unwrap().preferences, 1);
    assert!(action_rx.try_recv().is_err());
}

#[test]
fn poisoned_shared_config_keeps_review_transaction_unapplied() {
    let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) =
        setup_harness();
    let theme = Theme::named(ThemeName::Dark);
    let calls = Arc::new(Mutex::new(SaveCalls::default()));
    drive_to_review(
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &mut textarea,
        &vim,
        &theme,
        &calls,
    );
    let poison = Arc::clone(&shared);
    let _ = std::thread::spawn(move || {
        let _guard = poison.lock().unwrap();
        panic!("poison shared config");
    })
    .join();

    press_setup_key(
        KeyCode::Enter,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &mut textarea,
        &vim,
        &theme,
        &calls,
        Ok(()),
        Ok(()),
    );

    assert_eq!(state.onboarding.step(), OnboardingStep::Review);
    assert!(config.api_key.is_none());
    assert!(!state.auth_configured);
    assert!(state.onboarding.review_error().is_some());
    assert_eq!(calls.lock().unwrap().auth, 0);
    assert_eq!(calls.lock().unwrap().preferences, 0);
}

#[test]
fn esc_before_review_exits_without_persistence() {
    for step in OnboardingStep::ALL.into_iter().take(6) {
        let (mut state, mut config, shared, action_tx, _rx, mut textarea, vim) =
            setup_harness();
        let theme = Theme::named(ThemeName::Dark);
        let calls = Arc::new(Mutex::new(SaveCalls::default()));
        state.onboarding.set_step_for_test(step);
        state.onboarding.set_api_key("sk-test-secret".to_string());

        assert_eq!(
            press_setup_key(
                KeyCode::Esc,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &mut textarea,
                &vim,
                &theme,
                &calls,
                Ok(()),
                Ok(()),
            ),
            SetupFlow::Exit(0),
            "{step:?}",
        );
        assert!(config.api_key.is_none());
        assert_eq!(calls.lock().unwrap().auth, 0);
        assert_eq!(calls.lock().unwrap().preferences, 0);
    }
}
```

- [ ] **Step 2: Run setup transition tests and verify RED**

```bash
cargo test -p orca-tui enter_advances_exact_wizard_sequence -- --nocapture
cargo test -p orca-tui review_applies_memory -- --nocapture
cargo test -p orca-tui poisoned_shared_config -- --nocapture
```

Expected: compilation fails because typed action routing and saver injection do
not exist.

- [ ] **Step 3: Implement typed setup key routing**

Extend:

```rust
pub(crate) enum SetupFlow {
    Continue,
    PreviewTheme(ThemeName),
    Exit(i32),
}
```

Add:

```rust
fn handle_setup_key_with_savers<AuthSave, PreferencesSave>(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
    initial_prompt: Option<String>,
    save_auth: AuthSave,
    save_preferences: PreferencesSave,
) -> io::Result<SetupFlow>
where
    AuthSave: FnOnce(&str) -> Result<(), UserConfigSaveError>,
    PreferencesSave:
        FnOnce(&UserPreferencePatch) -> Result<(), UserConfigSaveError>;
```

Production `handle_setup_key` calls it with:

```rust
orca_core::config::file::save_api_key
orca_core::config::file::save_user_preferences
```

Routing:

```rust
KeyCode::Esc => SetupFlow::Exit(0)
KeyCode::Up | KeyCode::Char('k') => move_previous()
KeyCode::Down | KeyCode::Char('j') => move_next()
KeyCode::Enter => advance/apply/complete based on step
other key on ApiKey => textarea.input(Input::from(ev.clone()))
```

Only Theme movement returns `PreviewTheme(selected_theme)`.

- [ ] **Step 4: Implement Review application**

On Review Enter:

```rust
let patch = match UserPreferencePatch::new(provider, model.clone(), theme) {
    Ok(patch) => patch,
    Err(UserPreferenceValidationError::UnsupportedProvider) => {
        state.onboarding.set_error(OnboardingError::UnsupportedProvider);
        return Ok(SetupFlow::Continue);
    }
    Err(UserPreferenceValidationError::UnsupportedModel) => {
        state.onboarding.set_error(OnboardingError::UnsupportedModel);
        return Ok(SetupFlow::Continue);
    }
};
let Some(api_key) = state.onboarding.api_key().map(str::to_string) else {
    state.onboarding.set_error(OnboardingError::MissingApiKey);
    return Ok(SetupFlow::Continue);
};
let Ok(mut shared) = shared_config.lock() else {
    state.onboarding.set_error(OnboardingError::SharedConfigUnavailable);
    return Ok(SetupFlow::Continue);
};

let selection = match ModelSelection::parse(Some(model.clone())) {
    Ok(selection) => selection,
    Err(_) => {
        state.onboarding.set_error(OnboardingError::UnsupportedModel);
        return Ok(SetupFlow::Continue);
    }
};

config.provider = provider;
config.model = selection.clone();
config.theme = theme;
config.api_key = Some(api_key.clone());

shared.provider = provider;
shared.model = selection;
shared.theme = theme;
shared.api_key = Some(api_key.clone());
drop(shared);

state.model_name = model;
state.auth_configured = true;
let auth_outcome = SaveOutcome::from(save_auth(&api_key));
let preferences_outcome = SaveOutcome::from(save_preferences(&patch));
state
    .onboarding
    .finish_review(auth_outcome, preferences_outcome);
```

Ensure the draft API key is taken/zeroed after both savers return. Do not send
`UserAction::SetModel`.

- [ ] **Step 5: Keep preview effects buildable until capability integration**

In `status_key_actions.rs`, keep the existing status-flow type:

```rust
pub(crate) enum StatusKeyFlow {
    Continue,
    Exit(i32),
}
```

Map setup flow temporarily:

```rust
SetupFlow::Continue | SetupFlow::PreviewTheme(_) => {
    StatusKeyFlow::Continue
}
SetupFlow::Exit(code) => StatusKeyFlow::Exit(code),
```

The wizard draft still changes theme selection. Task 6 introduces a failing
app-level preview test, adds `StatusKeyFlow::PreviewTheme`, and replaces this
temporary mapping. This keeps the Task 5 migration commit exhaustive and
compilable without prematurely changing terminal capability ownership.

- [ ] **Step 6: Implement Complete Enter behavior**

On Complete Enter:

```rust
state.set_status(AppStatus::Idle);
*textarea = make_textarea(vim_state, theme);
state.sync_vim_mode(vim_state);

if let Some(prompt) = initial_prompt {
    state.push_message(ChatMessage::User(prompt.clone()));
    state.enter_running();
    let _ = action_tx.send(UserAction::Submit(prompt));
}
```

No other step may dispatch the initial prompt.

- [ ] **Step 7: Atomically migrate all numeric setup consumers**

In `types.rs`, remove:

```rust
pub setup_step: u8,
```

and its initializer. Keep the `onboarding` field added in Task 4.

In `app.rs`, replace setup initialization:

```rust
if needs_setup {
    state.status = AppStatus::Setup;
    state.initialize_onboarding(&config);
}
```

In `input_event_actions.rs`, replace the numeric paste guard:

```rust
AppStatus::Setup
    if state.onboarding.step() == OnboardingStep::ApiKey =>
{
    insert_pasted_text(textarea, pasted);
}
```

In `ui.rs`, replace the numeric match with a minimal typed renderer so this
commit remains buildable before Task 7's visual expansion:

```rust
match state.onboarding.step() {
    OnboardingStep::Welcome => {
        render_setup_message(frame, area, theme, "Welcome", "Press Enter to continue");
        None
    }
    OnboardingStep::Provider => {
        render_setup_rows(frame, area, theme, "Provider", state.onboarding.option_rows());
        None
    }
    OnboardingStep::ApiKey => {
        render_setup_api_key(frame, area, textarea, theme)
    }
    OnboardingStep::Model => {
        render_setup_rows(frame, area, theme, "Model", state.onboarding.option_rows());
        None
    }
    OnboardingStep::Theme => {
        render_setup_rows(frame, area, theme, "Theme", state.onboarding.option_rows());
        None
    }
    OnboardingStep::Review => {
        render_setup_text_rows(
            frame,
            area,
            theme,
            "Review",
            state.onboarding.review_rows(),
        );
        None
    }
    OnboardingStep::Complete => {
        render_setup_text_rows(
            frame,
            area,
            theme,
            "Complete",
            state.onboarding.completion_rows(),
        );
        None
    }
}
```

The three minimal helpers use `centered_rect`, one rounded block, theme
semantic colors, saturating geometry, and no cursor except the API-key helper.
Task 7 replaces/expands their content without changing ownership.

Verify the migration is complete:

```bash
! rg "setup_step" crates/orca-tui/src
```

- [ ] **Step 8: Run setup tests GREEN**

```bash
cargo test -p orca-tui setup_actions -- --nocapture
cargo test -p orca-tui onboarding -- --nocapture
cargo test -p orca-tui submitted_doctor_command -- --nocapture
```

Expected: typed transitions, transaction boundary, independent save outcomes,
Esc behavior, and exact initial prompt dispatch pass.

- [ ] **Step 9: Commit setup actions and consumer migration**

```bash
git add crates/orca-tui/src/setup_actions.rs \
  crates/orca-tui/src/status_key_actions.rs \
  crates/orca-tui/src/onboarding.rs crates/orca-tui/src/types.rs \
  crates/orca-tui/src/app.rs crates/orca-tui/src/input_event_actions.rs \
  crates/orca-tui/src/ui.rs
git commit -m "feat(tui): route typed onboarding actions" \
  -m "Stage credentials until Review, apply current-session choices atomically, and project independent persistence outcomes." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 6: Integrate Capability-Preserving Theme Preview

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/diagnostics.rs`
- Modify: `crates/orca-tui/src/capability_backend.rs`

- [ ] **Step 1: Write failing preview projection tests**

In `diagnostics.rs` add:

```rust
#[test]
fn theme_projection_updates_requested_and_resolved_values() {
    let mut snapshot = DiagnosticSnapshot::default();
    snapshot.set_theme_projection(ThemeName::Auto, ThemeName::Light);
    assert_eq!(snapshot.requested_theme(), ThemeName::Auto);
    assert_eq!(snapshot.resolved_theme(), ThemeName::Light);
}
```

In `app.rs` add:

```rust
#[test]
fn onboarding_theme_preview_reuses_captured_profile_without_reprobe() {
    let source = production_app_source();
    let helper = source.find("fn apply_onboarding_theme_preview(").unwrap();
    let body = &source[helper..source.find("\nfn ", helper + 1).unwrap()];
    assert!(body.contains("Theme::resolve(requested, terminal_profile)"));
    assert!(!body.contains("InputRuntime::start"));
    assert!(!body.contains("identity_from_env"));
    assert!(!body.contains("system_color_level"));
    assert!(!body.contains("probe_"));
}

#[test]
fn preview_updates_theme_syntax_and_doctor_projection_together() {
    let profile = TerminalProfile {
        background: TerminalBackground::Light,
        color_level: TerminalColorLevel::Ansi256,
    };
    let (tx, _rx) = mpsc::unbounded();
    let mut state = AppState::new(
        tx,
        "test".to_string(),
        "auto".to_string(),
        "/tmp".to_string(),
    );
    let mut theme = Theme::resolve(ThemeName::Dark, profile);

    apply_onboarding_theme_preview(
        ThemeName::Auto,
        profile,
        &mut theme,
        &mut state,
    );

    assert_eq!(theme.color_level, TerminalColorLevel::Ansi256);
    assert_eq!(state.syntax_theme_for_test(), SyntaxTheme::OneHalfLight);
    assert_eq!(
        state.diagnostics.requested_theme(),
        ThemeName::Auto,
    );
    assert_eq!(
        state.diagnostics.resolved_theme(),
        ThemeName::Light,
    );
}
```

Add a backend invariant:

```rust
#[test]
fn theme_preview_never_changes_capability_backend_color_level() {
    let backend = CapabilityBackend::new(
        RecordingBackend::default(),
        TerminalColorLevel::Ansi16,
    );
    assert_eq!(
        backend.color_level_for_test(),
        TerminalColorLevel::Ansi16,
    );
}
```

- [ ] **Step 2: Run preview tests and verify RED**

```bash
cargo test -p orca-tui theme_projection_updates -- --nocapture
cargo test -p orca-tui preview_updates_theme_syntax -- --nocapture
```

Expected: compilation fails because projection setters and preview helper do
not exist.

- [ ] **Step 3: Add central projection helpers**

In `diagnostics.rs`:

```rust
pub(crate) fn set_theme_projection(
    &mut self,
    requested: ThemeName,
    resolved: ThemeName,
) {
    self.requested_theme = requested;
    self.resolved_theme = resolved;
}
```

In `types.rs`:

```rust
pub(crate) fn apply_theme_projection(&mut self, theme: &Theme) {
    self.syntax_theme = theme.syntax_theme;
    self.syntax_color_level = theme.color_level;
    self.applied_diff_highlights.clear();
}
```

Do not start or reset the edit-highlight worker merely for onboarding, because
no normal messages/tools exist before setup completes.

- [ ] **Step 4: Add and wire the preview helper**

In `app.rs`:

```rust
fn apply_onboarding_theme_preview(
    requested: ThemeName,
    terminal_profile: TerminalProfile,
    theme: &mut Theme,
    state: &mut AppState,
) {
    let resolved = resolve_base_theme(requested, terminal_profile.background);
    *theme = Theme::resolve(requested, terminal_profile);
    state.apply_theme_projection(theme);
    state
        .diagnostics
        .set_theme_projection(requested, resolved);
}
```

In `status_key_actions.rs`, upgrade the flow:

```rust
pub(crate) enum StatusKeyFlow {
    Continue,
    PreviewTheme(ThemeName),
    Exit(i32),
}
```

Replace Task 5's temporary setup mapping:

```rust
SetupFlow::Continue => StatusKeyFlow::Continue,
SetupFlow::PreviewTheme(theme) => StatusKeyFlow::PreviewTheme(theme),
SetupFlow::Exit(code) => StatusKeyFlow::Exit(code),
```

Make the startup binding mutable:

```rust
let mut theme = Theme::resolve(config.theme, terminal_profile);
```

Handle every `StatusKeyFlow::PreviewTheme(requested)` at both direct status
routes (normal Enter path and synthetic Enter path):

```rust
StatusKeyFlow::PreviewTheme(requested) => {
    apply_onboarding_theme_preview(
        requested,
        terminal_profile,
        &mut theme,
        &mut state,
    );
    return Ok(None);
}
```

The event iteration is already dirty because it processed input; do not add a
second scheduler call.

- [ ] **Step 5: Prove the capability backend remains startup-owned**

Add test-only:

```rust
#[cfg(test)]
pub(crate) const fn color_level_for_test(&self) -> TerminalColorLevel {
    self.color_level
}
```

Production never mutates `CapabilityBackend.color_level`; every previewed
`Theme` is already adapted to the same captured profile.

- [ ] **Step 6: Run preview and regression tests GREEN**

```bash
cargo test -p orca-tui onboarding_theme_preview -- --nocapture
cargo test -p orca-tui preview_updates_theme_syntax -- --nocapture
cargo test -p orca-tui diagnostics -- --nocapture
cargo test -p orca-tui capability_backend -- --nocapture
cargo test -p orca-tui transcript_view -- --nocapture
```

Expected: all previews update theme/syntax/doctor together, Auto uses captured
background, and no second probe/runtime call exists.

- [ ] **Step 7: Commit theme preview**

```bash
git add crates/orca-tui/src/app.rs crates/orca-tui/src/types.rs \
  crates/orca-tui/src/diagnostics.rs crates/orca-tui/src/capability_backend.rs
git commit -m "feat(tui): preview onboarding themes safely" \
  -m "Resolve setup previews from captured terminal capabilities and synchronize syntax and diagnostic projections." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 7: Render the Seven-Step Wizard

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/input_event_actions.rs`
- Modify: `crates/orca-tui/src/onboarding.rs`

- [ ] **Step 1: Write failing frame and geometry tests**

Replace numeric setup tests with typed table-driven tests:

```rust
#[test]
fn every_onboarding_step_renders_in_normal_and_compact_frames() {
    let theme = Theme::named(ThemeName::Dark);
    for step in OnboardingStep::ALL {
        for (width, height) in [(80, 24), (40, 12), (20, 6)] {
            let mut state = test_state();
            state.status = AppStatus::Setup;
            state.onboarding.set_step_for_test(step);
            if step == OnboardingStep::ApiKey {
                state.onboarding.set_api_key("draft".to_string());
            }
            let textarea = crate::composer_textarea::make_setup_textarea(&theme);
            let mut terminal = ratatui::Terminal::new(
                ratatui::backend::TestBackend::new(width, height),
            )
            .unwrap();

            terminal
                .draw(|frame| render(frame, &mut state, &textarea, &theme))
                .unwrap();
            assert_eq!(state.frame_area, Some(Rect::new(0, 0, width, height)));
        }
    }
}

#[test]
fn only_api_key_step_moves_hardware_cursor() {
    let theme = Theme::named(ThemeName::Dark);
    let textarea = crate::composer_textarea::make_setup_textarea(&theme);
    for step in OnboardingStep::ALL {
        let mut state = test_state();
        state.status = AppStatus::Setup;
        state.onboarding.set_step_for_test(step);
        let (backend, events) = RecordingBackend::new(60, 16);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| render(frame, &mut state, &textarea, &theme))
            .unwrap();
        let events = take_cursor_events(&events);
        if step == OnboardingStep::ApiKey {
            assert!(events.iter().any(|event| matches!(event, CursorEvent::Move(_))));
        } else {
            assert_eq!(events, [CursorEvent::Hide]);
        }
    }
}

#[test]
fn review_and_complete_never_render_secret_or_absolute_paths() {
    let theme = Theme::named(ThemeName::Dark);
    for step in [OnboardingStep::Review, OnboardingStep::Complete] {
        let mut state = test_state();
        state.status = AppStatus::Setup;
        state.onboarding.set_step_for_test(step);
        state.onboarding.set_api_key("sk-visible-secret".to_string());
        state.onboarding.set_outcomes_for_test(
            SaveOutcome::Failed(UserConfigSaveError::WriteFailed),
            SaveOutcome::Failed(UserConfigSaveError::ReplaceFailed),
        );
        let frame = render_setup_test_frame(&mut state, &theme, 80, 24);
        let text = format!("{:?}", frame.buffer);
        assert!(!text.contains("sk-visible-secret"));
        assert!(!text.contains("/Users/"));
        assert!(!text.contains("C:\\\\Users\\\\"));
    }
}
```

Add theme/color tests:

```rust
#[test]
fn onboarding_option_selection_is_capability_safe_for_every_color_level() {
    for color_level in [
        TerminalColorLevel::TrueColor,
        TerminalColorLevel::Ansi256,
        TerminalColorLevel::Ansi16,
        TerminalColorLevel::Monochrome,
    ] {
        let theme = Theme::resolve(
            ThemeName::Dark,
            TerminalProfile {
                background: TerminalBackground::Dark,
                color_level,
            },
        );
        let style = onboarding_selected_style(&theme);
        assert_eq!(color_level.adapt_style(style), style);
    }
}
```

- [ ] **Step 2: Run setup UI tests and verify RED**

```bash
cargo test -p orca-tui every_onboarding_step_renders -- --nocapture
cargo test -p orca-tui only_api_key_step_moves -- --nocapture
cargo test -p orca-tui review_and_complete_never -- --nocapture
```

Expected: the typed minimal renderer from Task 5 fails one or more complete
layout, compact-frame, cursor-event, or content assertions.

- [ ] **Step 3: Implement shared bounded setup shell**

Add pure helpers:

```rust
fn onboarding_panel_area(frame: Rect) -> Rect {
    centered_rect(
        frame,
        68.min(frame.width.saturating_sub(2)),
        18.min(frame.height.saturating_sub(2)),
    )
}

fn onboarding_selected_style(theme: &Theme) -> Style {
    match theme.color_level {
        TerminalColorLevel::Monochrome => {
            Style::default().add_modifier(Modifier::REVERSED)
        }
        _ => Style::default()
            .fg(theme.text)
            .bg(theme.selection_bg)
            .add_modifier(Modifier::BOLD),
    }
}
```

Render one rounded block, step label `N/7`, bounded title/instructions, content,
and footer. Every rectangle must use saturating/clamped geometry.

- [ ] **Step 4: Render each typed step**

Expand the typed branches to use these exact projections:

```rust
match state.onboarding.step() {
    OnboardingStep::Welcome => render_onboarding_text(
        frame,
        shell.content,
        theme,
        &[
            "A DeepSeek-native coding agent",
            "Configure local defaults for this device.",
        ],
    ),
    OnboardingStep::Provider
    | OnboardingStep::Model
    | OnboardingStep::Theme => render_onboarding_options(
        frame,
        shell.content,
        theme,
        &state.onboarding.option_rows(),
    ),
    OnboardingStep::ApiKey => {
        return render_onboarding_api_key(
            frame,
            shell.content,
            textarea,
            theme,
        );
    }
    OnboardingStep::Review => render_onboarding_owned_rows(
        frame,
        shell.content,
        theme,
        state.onboarding.review_rows(),
    ),
    OnboardingStep::Complete => render_onboarding_owned_rows(
        frame,
        shell.content,
        theme,
        state.onboarding.completion_rows(),
    ),
}
```

Use one `OnboardingShellGeometry`:

```rust
struct OnboardingShellGeometry {
    panel: Rect,
    header: Rect,
    instruction: Rect,
    content: Rect,
    error: Rect,
    footer: Rect,
}
```

`onboarding_shell_geometry(frame.area())` partitions the clamped panel with
`Layout::vertical` and shortens zero-height rows with `saturating_sub`. Header
renders `"{ordinal}/7 · {title}"`; footer renders `↑/↓ or j/k · Enter · Esc`.
On Review, render `state.onboarding.error_label()` in `shell.error` when set.
On Complete, footer is `Enter start · Esc exit`.

Use only theme semantic colors; remove hard-coded setup
`Cyan/White/Green/Blue/DarkGray` colors.

Only API Key calls:

```rust
render_textarea_surface(
    frame,
    input_area,
    textarea,
    None,
    None,
    theme,
    true,
)
```

and returns its cursor projection. Every other branch returns `None`.

- [ ] **Step 5: Update setup paste ownership**

In `input_event_actions.rs`:

```rust
AppStatus::Setup
    if state.onboarding.step() == OnboardingStep::ApiKey =>
{
    insert_pasted_text(textarea, pasted);
}
```

Pastes on all other setup steps are consumed without modifying textarea or
draft.

- [ ] **Step 6: Run setup UI and full cursor matrix GREEN**

```bash
cargo test -p orca-tui onboarding -- --nocapture
cargo test -p orca-tui setup_cursor -- --nocapture
cargo test -p orca-tui hardware_cursor -- --nocapture
cargo test -p orca-tui ui::tests -- --nocapture
```

Expected: all seven steps render at compact sizes, only API Key moves cursor,
secret/path scans pass, and the existing cursor matrix remains green.

- [ ] **Step 7: Commit wizard UI**

```bash
git add crates/orca-tui/src/ui.rs \
  crates/orca-tui/src/input_event_actions.rs \
  crates/orca-tui/src/onboarding.rs
git commit -m "feat(tui): render seven-step onboarding wizard" \
  -m "Add bounded provider, credential, model, theme, review, and completion surfaces with API-key-only cursor ownership." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 8: Documentation, Independent Review, and Full Verification

**Files:**
- Modify: `README.md`
- Modify: `README.zh-CN.md`
- Modify: `crates/orca-tui/src/onboarding.rs`

- [ ] **Step 1: Write failing README contract test**

In `onboarding.rs`:

```rust
#[test]
fn readmes_document_first_run_onboarding_and_persistence() {
    for (name, readme) in [
        ("README.md", include_str!("../../../README.md")),
        ("README.zh-CN.md", include_str!("../../../README.zh-CN.md")),
    ] {
        for required in [
            "Provider",
            "API Key",
            "Model",
            "Theme",
            "Review",
            "Complete",
            "DeepSeek",
            "deepseek-v4-flash",
            "deepseek-v4-pro",
            "config.toml",
            "auth.json",
            "current session",
        ] {
            assert!(readme.contains(required), "{name}: {required}");
        }
    }
}
```

- [ ] **Step 2: Run README test and verify RED**

```bash
cargo test -p orca-tui readmes_document_first_run -- --nocapture
```

Expected: FAIL because the expanded wizard is not documented.

- [ ] **Step 3: Document first-run behavior**

Add concise English and Chinese sections stating:

- the exact seven steps;
- DeepSeek is currently the only production provider;
- exact model and theme choices;
- provider/model/theme save to user `config.toml`;
- API key saves separately to `auth.json`;
- no network validation occurs;
- persistence failures still apply choices to the current session;
- Esc before Review writes nothing.

- [ ] **Step 4: Run focused suites GREEN**

```bash
cargo test -p orca-core config::file -- --nocapture
cargo test -p orca-tui onboarding -- --nocapture
cargo test -p orca-tui setup_actions -- --nocapture
cargo test -p orca-tui setup_cursor -- --nocapture
cargo test -p orca-tui app::tests -- --nocapture
cargo test -p blade-deepseek cli::tests -- --nocapture
cargo fmt --all -- --check
git diff --check
```

- [ ] **Step 5: Commit documentation**

```bash
git add README.md README.zh-CN.md crates/orca-tui/src/onboarding.rs
git commit -m "docs(tui): document expanded first-run onboarding" \
  -m "Describe provider, model, theme, credential, review, persistence, and current-session fallback behavior." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

- [ ] **Step 6: Independent spec-compliance review**

Provide the reviewer:

- `docs/superpowers/specs/2026-07-29-tui-onboarding-provider-model-theme-design.md`;
- this plan;
- the full diff from spec commit `0e1d69c087820d68488339857e4e59f10eed3e42`;
- focused test output.

Require review of:

- exact seven-step sequence and trigger;
- provider-only production scope;
- model/theme closed choices;
- API key never rendered or written to TOML;
- Review transaction boundary and shared-config failure;
- independent auth/preferences persistence outcomes;
- user-only provider precedence and project deny;
- atomic file/symlink/size/permission protections;
- theme preview reuse of captured profile;
- no second input runtime/probe;
- syntax/doctor projections;
- compact UI and cursor ownership;
- exact initial prompt timing;
- default non-setup behavior parity.

Fix every Critical/Important finding through RED/GREEN and request re-review.

- [ ] **Step 7: Independent code-quality review**

Use a different reviewer for:

- secret ownership and trait derivations;
- typed error safety;
- temp-name collision and cleanup;
- TOCTOU and Unix `O_NOFOLLOW`;
- permission preservation/new `0o600`;
- TOML comment/unknown-key preservation;
- provider migration blast radius;
- mutable theme borrow and runtime event flow;
- test-only injection versus production behavior;
- compact geometry and cursor matrix;
- source tests versus behavior tests.

Fix every Critical/Important finding through RED/GREEN and request re-review.

- [ ] **Step 8: Full verification**

Run:

```bash
cargo test -p orca-core
cargo test -p orca-tui
cargo test --workspace --all-targets
cargo check --workspace
cargo fmt --all -- --check
git diff --check
```

Known unrelated process/deadline flakes may be skipped only after:

1. the relevant source matches spec-commit baseline
   `0e1d69c087820d68488339857e4e59f10eed3e42`;
2. the exact test passes on a fresh rerun;
3. all remaining all-targets tests pass with explicit skip filters.

No onboarding, setup, config-persistence, provider, model, theme, cursor, or
documentation failure may be skipped.

- [ ] **Step 9: Audit requirements, history, and trailers**

Build a prompt-to-artifact checklist:

```text
provider choice -> onboarding.rs + provider config/CLI tests
model choice -> allowed_models() projection + wizard tests
theme choice -> preview helper + UI buffers + diagnostic projection
API key -> private draft + auth writer + secret scans
persistence -> toml_edit patch + atomic filesystem tests
cursor/narrow UI -> setup cursor matrix + compact frame tests
```

Then run:

```bash
git status --short
git log --format='%H%n%B%n---' \
  0e1d69c087820d68488339857e4e59f10eed3e42..HEAD
```

Require a clean worktree and exactly one final:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

on every new commit.

- [ ] **Step 10: Audit the complete P0/P2 roadmap**

Do not treat the Onboarding test suite as proof that the full thread goal is
complete. Build this prompt-to-artifact checklist from the original objective
and inspect the listed production files and focused evidence:

```text
P0.1 code/diff syntax highlighting
  -> crates/orca-tui/src/syntax_highlight.rs
  -> crates/orca-tui/src/transcript_view.rs syntax_theme_revision cache key
  -> crates/orca-tui/src/edit_highlight_worker.rs
  -> >512 KiB, >10k lines, >4 KiB line fallback tests

P0.2 IME hardware cursor
  -> crates/orca-tui/src/ui.rs HardwareCursorProjection/final setter
  -> setup/search/composer/modal cursor matrix

P0.3 Markdown theme colors
  -> crates/orca-tui/src/theme.rs markdown semantic fields
  -> markdown role tests for Dark/Light/Solarized/Catppuccin/Monochrome

P0.4 terminal capability detection/theme degradation
  -> crates/orca-tui/src/input_runtime.rs
  -> crates/orca-tui/src/terminal_capabilities.rs
  -> crates/orca-tui/src/capability_backend.rs
  -> truecolor/ANSI256/ANSI16/Monochrome and OSC 11 tests

P0.5 notifications/title/focus
  -> crates/orca-tui/src/terminal_presentation.rs
  -> crates/orca-tui/src/input_event_actions.rs focus projection
  -> OSC 9/tmux/BEL/title reset and state-title tests

P0.6 diff rendering
  -> crates/orca-tui/src/diff_highlight.rs
  -> changed backgrounds, dual gutters, hunk separator, inline changes
  -> fallback and terminal-color-level tests

P0.8 streaming checkpoints
  -> crates/orca-tui/src/streaming_markdown.rs
  -> crates/orca-tui/src/transcript_view.rs
  -> checkpoint freeze, newline gate, table holdback, bounded tail tests

P2 transcript search
  -> crates/orca-tui/src/transcript_search.rs
  -> search routing/render/cache/highlight/navigation tests

P2 queued-message visibility/edit
  -> crates/orca-tui/src/queued_input.rs
  -> queue preview geometry and restore-latest tests

P2 cwd/git status
  -> crates/orca-tui/src/workspace_status.rs
  -> status compaction/non-reprobe tests

P2 Vim command core + insert escape
  -> crates/orca-tui/src/vim.rs
  -> crates/orca-tui/src/vim_command.rs
  -> counts/dd/gg/G/register/dot/jj-style sequence tests

P2 custom keybindings
  -> crates/orca-tui/src/keybindings/
  -> context/chord/reload/help/last-known-good tests

P2 doctor/FPS
  -> crates/orca-tui/src/diagnostics.rs
  -> command/report/privacy/metrics/HUD/cursor tests

P2 onboarding
  -> crates/orca-tui/src/onboarding.rs
  -> provider/model/theme/auth/persistence/preview/UI tests from this plan
```

For every row:

1. inspect the current production artifact;
2. run or cite a focused test that directly covers the requested behavior;
3. verify the corresponding design and plan are committed;
4. record missing, weak, or proxy-only evidence;
5. continue implementation if any requirement is absent or uncertain.

Write the final audit to:

```text
docs/superpowers/audits/2026-07-29-tui-roadmap-completion-audit.md
```

The audit must include exact file paths, test commands/results, commit SHAs,
known unrelated flakes with their baseline/exact-rerun evidence, and an
explicit `Complete` or `Incomplete` result. Commit it only when every roadmap
item is proven:

```bash
git add docs/superpowers/audits/2026-07-29-tui-roadmap-completion-audit.md
git commit -m "docs(tui): audit terminal roadmap completion" \
  -m "Map every P0 and P2 requirement to production artifacts, direct tests, review evidence, and remote-ready commits." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

- [ ] **Step 11: Push and verify remote SHA**

```bash
git fetch origin feature/tui-syntax-highlighting
```

If remote history changed, compare tree hashes and rebase local commits onto
the fetched remote. Never force push.

```bash
git push origin feature/tui-syntax-highlighting
LOCAL_SHA=$(git rev-parse HEAD)
REMOTE_SHA=$(git ls-remote origin \
  refs/heads/feature/tui-syntax-highlighting | awk '{print $1}')
test "$LOCAL_SHA" = "$REMOTE_SHA"
printf 'local=%s\nremote=%s\n' "$LOCAL_SHA" "$REMOTE_SHA"
```

- [ ] **Step 12: Create or update the Pull Request**

Detect the base branch and GitHub authentication:

```bash
BASE_BRANCH=$(
  git remote show origin |
    sed -n '/HEAD branch/s/.*: //p'
)
test -n "$BASE_BRANCH"
gh auth status
```

Build a body file instead of inlining multiline text:

```bash
cat > /tmp/orca-tui-roadmap-pr.md <<'EOF'
## Summary

- add guarded code and diff syntax highlighting
- fix IME hardware cursor placement and theme/capability rendering
- improve notifications, terminal title, diff rendering, and streaming checkpoints
- add transcript search, queued-message preview, workspace status, Vim commands, keybindings, doctor/FPS diagnostics, and expanded onboarding

## Verification

- focused RED/GREEN suites for every sub-project
- `cargo test -p orca-core`
- `cargo test -p orca-tui`
- `cargo test --workspace --all-targets` with only individually proven unrelated deadline/process flakes explicitly skipped
- `cargo check --workspace`
- `cargo fmt --all -- --check`
- `git diff --check`

## Safety

- no API key or absolute user path is included in diagnostics, onboarding review, persistence errors, or this PR
- every commit carries the required TRAE CLI co-author trailer exactly once
EOF
```

Create the PR only when the branch has no existing PR:

```bash
if PR_URL=$(gh pr view \
  feature/tui-syntax-highlighting \
  --json url \
  --jq .url 2>/dev/null); then
  printf 'existing_pr=%s\n' "$PR_URL"
else
  PR_URL=$(gh pr create \
    --base "$BASE_BRANCH" \
    --head feature/tui-syntax-highlighting \
    --title "feat(tui): complete terminal experience roadmap" \
    --body-file /tmp/orca-tui-roadmap-pr.md)
  printf 'created_pr=%s\n' "$PR_URL"
fi
```

Verify the PR points to the pushed SHA:

```bash
PR_HEAD=$(gh pr view \
  feature/tui-syntax-highlighting \
  --json headRefOid \
  --jq .headRefOid)
REMOTE_SHA=$(git ls-remote origin \
  refs/heads/feature/tui-syntax-highlighting | awk '{print $1}')
test "$PR_HEAD" = "$REMOTE_SHA"
gh pr view feature/tui-syntax-highlighting \
  --json url,state,baseRefName,headRefName,headRefOid
```

## Completion Criteria

The sub-project is complete only when:

- seven typed wizard steps replace numeric setup state;
- only DeepSeek appears as a production provider;
- exact supported model/theme choices render and navigate;
- API key stays private until Review and never appears in UI/errors/TOML;
- provider/model/theme/API key apply atomically to current/shared config;
- auth and preferences persist independently with safe typed outcomes;
- user config patch preserves valid unrelated TOML and rejects unsafe paths;
- provider file/env/CLI precedence and project deny are verified;
- theme preview uses captured capabilities with no reprobe/runtime restart;
- syntax and `/doctor` theme projections update with preview;
- initial prompt submits exactly once after Complete;
- compact setup UI and hardware cursor matrices pass;
- English and Chinese docs match behavior;
- independent spec and quality reviews approve;
- focused, crate, workspace, check, format, and diff gates pass;
- every commit has exactly one required co-author trailer;
- local and remote branch SHAs match after push;
- a GitHub PR exists and its `headRefOid` equals the remote branch SHA.
