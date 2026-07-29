# TUI Provider, Model, and Theme Onboarding Design

## Goal

Expand first-run TUI onboarding from the current API-key-only flow into a
typed wizard that configures:

- the production provider;
- the model;
- the visual theme;
- the DeepSeek API key.

The selected provider, model, and theme become user defaults in
`~/.orca/config.toml` or `$ORCA_HOME/config.toml`. The API key remains isolated
in `auth.json`. Theme choices preview immediately inside the running setup
screen without restarting the TUI or probing the terminal again.

This is a separate sub-project from `/doctor` and the FPS HUD. It does not
change any completed P0 or P2 feature.

## Product Decisions

### First-run trigger

The wizard runs only when the effective `RunConfig.api_key` is missing, which
is the existing setup trigger.

An existing API key continues to skip onboarding. This project does not add a
settings command or a way to reopen onboarding from an active session.

### Wizard sequence

The ordered steps are:

```text
Welcome
Provider
API Key
Model
Theme
Review
Complete
```

The current `0`, `1`, and `2` numeric setup steps are removed.

- `Welcome` explains that the wizard configures local user defaults.
- `Provider` selects a production provider.
- `API Key` accepts the masked DeepSeek key.
- `Model` selects the default model.
- `Theme` selects and previews the requested theme.
- `Review` shows the non-secret choices before persistence.
- `Complete` reports save outcomes and starts the session on Enter.

### Provider scope

The provider picker contains only:

```text
DeepSeek
```

`Mock` and `DeepSeekFixture` remain hidden test implementations. They are not
displayed or persisted by onboarding.

The provider picker still uses a typed option list rather than hard-coded
step behavior, so another production provider can be added later without
changing wizard navigation.

This project does not add a custom OpenAI-compatible or DeepSeek-compatible
endpoint field. Existing `base_url` configuration remains available through
the config file and CLI.

### Model scope

The model picker reuses `orca_core::model::allowed_models()`:

```text
auto
deepseek-v4-flash
deepseek-v4-pro
```

The current effective model is selected initially. The wizard never accepts an
arbitrary model string.

### Theme scope and preview

The theme picker contains every `ThemeName`:

```text
auto
dark
light
solarized
catppuccin
```

Moving the selection immediately previews the candidate theme. Preview uses
the already captured startup `TerminalProfile`:

```rust
Theme::resolve(selected_theme, terminal_profile)
```

Preview does not:

- start another `InputRuntime`;
- call OSC 11 again;
- reread `COLORTERM`;
- call `system_color_level`;
- reconstruct terminal presentation;
- persist a value before the Review step is confirmed.

The current theme remains active when the user reaches Review and Complete.
If the process exits with Esc before Review is confirmed, neither `auth.json`
nor `config.toml` is changed. On Complete, Review has already committed the
choices, so Esc exits while retaining those confirmed values.

### Navigation

For option steps:

- Up and `k` select the previous option with wraparound.
- Down and `j` select the next option with wraparound.
- Enter confirms the current option and advances.
- Esc exits the TUI with code `0`.

For the API-key step:

- normal text, paste, selection, and deletion retain current textarea behavior;
- Enter confirms a non-empty trimmed key and advances;
- an empty key stays on the same step;
- Esc exits with code `0`.

Review uses Enter to apply the draft to the current session and attempt
persistence. Complete uses Enter to leave setup and optionally dispatch the
initial prompt exactly once.

This project does not add backward navigation. Avoiding backtracking keeps the
first implementation small. No disk write occurs until Review is confirmed,
so exit remains transactional even after an API key was entered.

## Considered Approaches

### Extend the numeric setup step

Add more integer values to `setup_step` and continue switching on numbers in
`setup_actions.rs` and `ui.rs`.

This is mechanically small, but step meanings become implicit and tests can
accidentally use invalid numbers. Provider/model/theme drafts and save outcomes
would become unrelated fields on `AppState`. This approach is rejected.

### Generic form engine

Build a reusable schema-driven forms framework with arbitrary field types,
validation, back navigation, and persistence callbacks.

This would support future settings screens, but it is substantially larger
than the requested onboarding flow and would obscure the exact terminal and
configuration contracts. This approach is rejected as YAGNI.

### Typed onboarding wizard

Add one focused wizard model with typed steps, bounded option lists, one draft,
and explicit persistence results. Keep input routing in `setup_actions.rs` and
rendering in `ui.rs`.

This is the selected approach. It removes magic numbers, keeps the wizard
testable without a terminal, and does not create a general UI framework.

## Architecture

Add a focused module:

```rust
// crates/orca-tui/src/onboarding.rs

pub(crate) enum OnboardingStep {
    Welcome,
    Provider,
    ApiKey,
    Model,
    Theme,
    Review,
    Complete,
}

pub(crate) struct OnboardingDraft {
    provider: ProviderKind,
    model: String,
    theme: ThemeName,
    api_key: Option<String>,
}

pub(crate) struct OnboardingState {
    step: OnboardingStep,
    draft: OnboardingDraft,
    selected: usize,
    persistence: OnboardingPersistence,
    error: Option<String>,
}
```

Responsibilities:

- `onboarding.rs` owns steps, valid choices, selection movement, draft
  projection, review rows, and bounded persistence status labels.
- `AppState` owns one `OnboardingState`; it replaces `setup_step`.
- `setup_actions.rs` maps keys to wizard operations and applies confirmed
  values to `RunConfig`, `shared_config`, and `AppState`.
- `ui.rs` renders the current typed step.
- `app.rs` owns the effective `Theme`, captured `TerminalProfile`, and central
  reaction to a theme-preview effect.
- `orca-core::config::file` safely patches user preferences.
- `src/cli.rs` loads the persisted provider with the same precedence as other
  user preferences.

There is no new thread, channel, terminal owner, polling loop, or probe.

## Wizard State

### Draft initialization

`OnboardingState::new` receives the effective startup values:

```rust
OnboardingState::new(
    config.provider,
    config.model.display_name(),
    config.theme,
)
```

The draft is current-session state only until Review is confirmed.

`OnboardingDraft` does not derive `Debug`, `Serialize`, or `Clone`. Its API key
field is private, never returned by review-row helpers, and is consumed with
`take()` when Review is applied.

Provider initialization normalizes test-only providers to `DeepSeek` because
they are not valid user-facing choices. Tests that launch the TUI with
`ProviderKind::Mock` may continue using the mock runtime, but onboarding's
display draft and persisted provider remain `DeepSeek`.

Model initialization must already pass `ModelSelection` validation. Theme is a
closed enum.

### Selection ownership

Each option step derives its choices from stable functions:

```rust
production_provider_options()
orca_core::model::allowed_models()
theme_options()
```

`selected` is reset to the draft's current value whenever an option step is
entered. Moving selection mutates the draft immediately. This makes Theme
preview deterministic and lets Review use one source of truth.

`selected` is always normalized against a non-empty option list. No render or
input path indexes an unchecked empty vector.

### Persistence status

The Complete step projects two independent outcomes:

```rust
pub(crate) enum SaveOutcome {
    NotAttempted,
    Saved,
    Failed(UserConfigSaveError),
}

pub(crate) struct OnboardingPersistence {
    auth: SaveOutcome,
    preferences: SaveOutcome,
}
```

Both outcomes start as `NotAttempted`. They transition exactly once when
Review is successfully applied. Complete rendering treats `NotAttempted` as an
internal invariant violation and displays a bounded generic failure rather
than claiming a save.

`UserConfigSaveError` is a typed core error with stable categories:

```rust
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
```

The TUI renders fixed category labels such as `invalid existing config` or
`could not replace config`. It never renders the underlying path, raw OS error,
or API key. Core may retain the source error for debugging, but its public
`safe_label()` is the only text accepted by onboarding.

The UI shows generic destinations (`auth.json` and user `config.toml`), not an
expanded absolute home path.

## Runtime Data Flow

### Setup effects

Input routing returns explicit effects:

```rust
pub(crate) enum SetupFlow {
    Continue,
    PreviewTheme(ThemeName),
    Exit(i32),
}
```

`PreviewTheme` is emitted only when Theme selection changes. The app handles it
centrally:

1. resolve the selected theme against the startup `TerminalProfile`;
2. replace the mutable current `Theme`;
3. update syntax theme/color-level projections in `AppState`;
4. call
   `DiagnosticSnapshot::set_theme_projection(requested, resolved)` with the
   same values;
5. mark the scheduler dirty through the existing input-event path.

No leaf renderer or setup widget owns terminal capability state.

`StatusKeyFlow` gains the corresponding `PreviewTheme(ThemeName)` variant so
`setup_actions.rs` can propagate the effect through `status_key_actions.rs`
without mutating terminal state. The main event closure applies the effect
immediately before returning from the routed input event.

### Applying Review

Confirming Review performs this sequence:

1. validate the draft provider/model/theme against closed option sets;
2. require a non-empty draft API key;
3. acquire `shared_config`; on lock failure, remain on Review without applying
   or persisting anything;
4. apply provider, model, theme, and API key to mutable `RunConfig`;
5. apply the same values to `shared_config` under that one lock;
6. update `AppState.model_name` and `AppState.auth_configured`;
7. persist the API key to `auth.json`;
8. persist provider/model/theme to user `config.toml`;
9. record both save outcomes;
10. clear the API key from the onboarding draft;
11. advance to Complete.

Validation and shared-config errors are stored in `OnboardingState.error`,
as a closed onboarding error enum and rendered with fixed labels on Review.
Moving a selection or successfully applying Review clears the error.

The API-key step trims the entered value, stores it only in the private draft,
clears the setup textarea, and advances. It does not update config or write a
file. The API key is never copied into `config.toml`, review rows, error text,
or chat messages.

The two persistence attempts are independent after the in-memory application
has succeeded. One may report `Saved` while the other reports `Failed`; neither
failure rolls back current-session values.

The hosted runtime process exists before setup, but no runtime thread exists
before a first-run prompt can be submitted. Updating `shared_config` before
Complete is therefore sufficient for provider and model selection; onboarding
does not send `UserAction::SetModel`. Initial prompt dispatch remains
exclusively on Complete Enter.

### Theme consistency

The app's current `theme` binding becomes mutable. Every normal render and
input handler continues borrowing the same current value.

When the selected theme changes:

- setup rendering changes on the next frame;
- capability adaptation still uses the original effective color level;
- final main textarea construction uses the previewed theme;
- transcript syntax theme projection is updated before the first normal frame;
- no terminal background or color capability is re-probed.

## User Configuration Persistence

### Provider in layered config

`FileConfig` and `RawFileConfig` gain:

```rust
pub provider: ProviderKind
```

The field uses an explicit serde default function:

```rust
fn default_provider() -> ProviderKind {
    ProviderKind::DeepSeek
}
```

`ProviderKind` remains capable of parsing hidden test providers for explicit
test/CLI usage, but onboarding only writes the serde/clap kebab-case value
`deep-seek`. Human-facing text remains `DeepSeek`; diagnostic/config display
may continue using `ProviderKind::as_str()` where it already reports
`deepseek`.

`ConfigOverrides` gains an optional provider. Effective precedence becomes:

```text
explicit CLI --provider
ORCA_PROVIDER
user config provider
DeepSeek default
```

The hidden provider arguments on `Cli`, `ExecArgs`, and `WorkflowRunArgs`
become `Option<ProviderKind>` rather than having a clap default.
`WorkflowWorkerArgs`, `SubagentWorkerArgs`, and `WorkflowCliLaunchRecord`
retain a required provider because their parent process passes or persists an
already resolved value. Every user-facing `RunConfig` construction resolves:

```rust
cli_provider
    .or(env_provider)
    .unwrap_or(file_config.provider)
```

This prevents clap's implicit default from masking the provider persisted by
onboarding. Existing tests that require Mock or Fixture continue passing an
explicit hidden provider.

As with `api_key`, `base_url`, and hooks, project-local config is not allowed
to override provider. `remove_project_denied_fields` removes `provider` before
the trusted project layer is merged, so only the user config contributes that
field.

### Safe root-key patch

Add:

```rust
pub struct UserPreferencePatch {
    provider: ProviderKind,
    model: String,
    theme: ThemeName,
}

pub enum UserPreferenceValidationError {
    UnsupportedProvider,
    UnsupportedModel,
}

impl UserPreferencePatch {
    pub fn new(
        provider: ProviderKind,
        model: impl Into<String>,
        theme: ThemeName,
    ) -> Result<Self, UserPreferenceValidationError>;
}

pub fn save_user_preferences(
    patch: &UserPreferencePatch,
) -> Result<(), UserConfigSaveError>;
```

The constructor accepts only `ProviderKind::DeepSeek` and values returned by
`allowed_models()`. The fields are private, and the type does not expose a way
to attach an API key.

The workspace adds a direct `toml_edit = "0.22"` dependency and `orca-core`
uses `DocumentMut` to patch only these root keys:

```toml
provider = "deep-seek"
model = "auto"
theme = "dark"
```

It preserves:

- comments;
- unknown root keys;
- nested tables;
- arrays and ordering not touched by the patch;
- existing valid values outside the three keys.

Safety rules:

- the user config input is capped at 1 MiB plus one sentinel byte;
- a missing file starts from an empty `DocumentMut`;
- invalid UTF-8 or invalid TOML returns an error and leaves the file unchanged;
- directories, symlinks, and non-regular files are rejected;
- Unix open uses `O_NOFOLLOW`;
- the parent directory is created when absent;
- output is written to a same-directory `create_new` temporary file;
- the temporary file is flushed and synced;
- rename replaces the destination atomically;
- temporary files are removed on failure;
- an existing regular file's permissions are retained; a new file uses
  user-only permissions on Unix;
- the API key is never accepted by this patch type.

The function returns errors instead of printing or swallowing them.

### API-key save result

Change:

```rust
pub fn save_api_key(api_key: &str) -> Result<(), UserConfigSaveError>
```

It retains the separate JSON map and never includes the key in an error.
Onboarding applies the key to the current session even when persistence fails.

The auth writer receives the same regular-file, symlink, size, atomic-replace,
sync, cleanup, and user-only Unix permission protections as the preference
writer. It preserves unrelated valid JSON map entries. Invalid JSON is
rejected and left byte-identical instead of being replaced with an empty map.

This design does not require migration of an existing legacy `api_key` field in
`config.toml`; layered loading continues to support it, while onboarding writes
only `auth.json`.

## Rendering

### Shared shell

All steps use a centered, bounded panel with:

- title ` Setup `;
- step indicator such as `3/7`;
- one-line instruction area;
- bounded content;
- one-line footer for navigation.

Panels are clamped by the existing `centered_rect` helper and must render
without panic at widths down to 20 cells and heights down to 6 rows. Content
may compact or omit descriptions, but selected values and navigation state
remain visible whenever the frame has at least one inner row.

### Option lists

Options render one row each when space permits:

```text
› auto                  Recommended
  deepseek-v4-flash     Faster
  deepseek-v4-pro       Highest quality
```

The selected row uses theme semantics and monochrome-safe reversal. Provider,
model, and theme descriptions are static, bounded strings.

Theme rows are rendered with the candidate preview theme after movement. The
selected theme's label remains legible under TrueColor, ANSI 256, ANSI 16, and
Monochrome adaptation.

### API key

The API-key surface reuses:

- `make_setup_textarea`;
- masked visual layout;
- paste handling;
- hardware cursor projection;
- narrow and wide grapheme guards.

Only `OnboardingStep::ApiKey` exposes the setup textarea hardware cursor.
Every other setup step returns no hardware cursor candidate.

### Review and Complete

Review displays only:

```text
Provider: DeepSeek
Model: auto
Theme: dark
API key: configured
```

Complete displays independent persistence results:

```text
API key: saved
Preferences: saved
```

or:

```text
API key: current session only — could not replace auth file
Preferences: current session only — invalid existing config
```

No secret or absolute path is rendered.

## Error Handling

- Empty API key: remain on API Key with no disk write.
- Invalid draft value: remain on Review and show a bounded internal validation
  error; this should only be reachable through corrupted state.
- Auth save failure: continue with the current-session key.
- Preference save failure: continue with current-session provider/model/theme.
- Shared config lock poison: remain on Review; keep the already visible theme
  preview, but do not update local/shared config, committed app projections,
  or disk.
- Invalid existing config TOML: do not overwrite it; report current-session
  preferences only.
- Symlink/special/oversized config: do not follow or overwrite it.
- Terminal too small: render a bounded compact message and never panic.

Failures do not append chat transcript messages during setup.

## Testing Strategy

Every production behavior is introduced through RED/GREEN tests.

### Wizard model

Table-driven tests cover:

- initial step and draft from effective config;
- persistence starts as `NotAttempted`;
- production provider list excludes Mock and Fixture;
- exact model list equals `allowed_models()`;
- exact theme list;
- Up/Down and `j/k` wraparound;
- Enter transition matrix for all seven steps;
- Esc exits from every step;
- invalid/empty option lists fail closed;
- invalid draft/provider/model stays on Review with a fixed safe error;
- Review contains no API-key content.

### Config persistence

Filesystem tests with isolated `ORCA_HOME` cover:

- missing config creation;
- root provider/model/theme updates;
- comments and unknown fields remain byte-visible;
- nested tables remain semantically identical;
- repeated save is idempotent;
- invalid TOML remains byte-identical;
- oversized, directory, symlink, and non-regular paths are rejected;
- temporary file cleanup after failure;
- no auth key enters `config.toml`;
- `save_api_key` success and error reporting;
- user provider loads into `RunConfig`;
- CLI and `ORCA_PROVIDER` precedence;
- project-local provider is denied.

### Setup actions

Typed harness tests cover:

- each Enter transition;
- API key remains draft-only until Review;
- Review atomically updates config/shared-config/auth projection;
- provider/model/theme current-session projection;
- no `SetModel` action is dispatched before a runtime thread exists;
- preference save success and failure;
- auth save success and failure;
- Esc before Review performs no persistence;
- initial prompt submits only on Complete Enter;
- Complete Enter creates a textarea using the selected theme;
- no secret appears in messages or persistence errors.

Persistence calls are injected behind small function parameters in tests;
production uses the real core save functions. Tests do not mutate the
developer's home directory.

### Theme preview and UI

Tests cover:

- selection emits `PreviewTheme`;
- preview uses the captured profile without a new probe;
- every theme produces a distinct requested projection;
- Auto resolves from the captured background;
- `/doctor` requested/resolved theme changes with preview;
- syntax theme revision changes with preview;
- setup frame buffers change after theme selection;
- selected rows remain adapted at every color level;
- 20x6 through normal terminal sizes do not panic or escape the buffer;
- only API Key shows/moves the hardware cursor;
- Complete and option steps hide it without moving it;
- default non-setup frames remain byte-identical.

### Regression gates

Focused:

```bash
cargo test -p orca-core config::file -- --nocapture
cargo test -p orca-tui onboarding -- --nocapture
cargo test -p orca-tui setup_actions -- --nocapture
cargo test -p orca-tui setup_cursor -- --nocapture
cargo test -p orca-tui app::tests -- --nocapture
cargo test -p blade-deepseek cli::tests -- --nocapture
```

Full:

```bash
cargo test -p orca-core
cargo test -p orca-tui
cargo test --workspace --all-targets
cargo check --workspace
cargo fmt --all -- --check
git diff --check
```

Known unrelated process/deadline flakes may be skipped only after the relevant
source is proven unchanged from the sub-project baseline and the exact test
passes on a fresh rerun. No onboarding, setup, config-persistence, theme,
cursor, model, or provider failure may be skipped.

## Documentation

README documentation adds a concise first-run section that states:

- the seven-step flow;
- provider is currently DeepSeek;
- available models and themes;
- provider/model/theme are saved to user `config.toml`;
- the API key remains in `auth.json`;
- save failures still apply values to the current session.

No API key example uses a real credential.

## Non-Goals

This sub-project does not add:

- another production provider;
- a custom base URL form;
- organization/login discovery;
- API-key validation through a network request;
- provider-specific model discovery;
- arbitrary model names;
- reasoning-effort selection;
- Vim or keybinding setup;
- notification preferences;
- an onboarding replay/settings command;
- backward navigation;
- mouse selection of wizard options;
- animation or graphical artwork;
- a second terminal capability probe;
- project-local persistence.

## Acceptance Criteria

The sub-project is complete only when:

- first-run onboarding has seven typed steps;
- Provider exposes only DeepSeek;
- Model and Theme expose the exact supported values;
- theme movement previews immediately from the captured profile;
- no second terminal/input runtime or probe exists;
- Review never includes the API key;
- provider/model/theme apply to current and shared config before first prompt;
- user `config.toml` is patched atomically without destroying valid unrelated
  content or following unsafe paths;
- API key remains separate in `auth.json`;
- save failures are visible but nonfatal;
- initial prompt dispatches exactly once after Complete;
- setup remains narrow-terminal and hardware-cursor safe;
- layered provider precedence is correct;
- focused and full verification pass;
- independent spec and code-quality reviews have no open Critical or Important
  findings;
- every commit has exactly one final
  `Co-authored-by: TRAE CLI <noreply@bytedance.com>` trailer;
- local and remote branch SHAs match after push.
