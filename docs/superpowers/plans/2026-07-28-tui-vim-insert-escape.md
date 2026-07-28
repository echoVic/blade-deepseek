# TUI Vim Insert Escape Remap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a default-off, configurable two-character Vim Insert-mode escape remap with a fixed 500ms match window, lossless input-boundary flushing, and no change to paste, IME, shortcut, register, dot-repeat, or Vim-disabled behavior.

**Architecture:** `orca-core` owns a validated `VimInsertEscapeSequence` parsed from the top-level `vim_insert_escape` TOML field and propagated through `RunConfig`. `VimState` owns one held Insert prefix plus its `Instant` deadline and exposes deterministic `*_at` methods for tests. `app.rs` resolves an already-started sequence before existing key preflight, flushes non-key ownership boundaries, and polls expiry in the existing 16ms loop; existing action handlers keep their priority and do not each implement duplicate remap logic.

**Tech Stack:** Rust, serde/TOML, crossterm/qwertty input events, tui-textarea 0.7 history APIs, ratatui event loop, Cargo tests.

---

## Scope and File Map

### Production files

- `crates/orca-core/src/config/mod.rs`
  - validated `VimInsertEscapeSequence`;
  - `RunConfig.vim_insert_escape`;
  - `/config show` rendering.
- `crates/orca-core/src/config/file.rs`
  - `FileConfig` and `RawFileConfig` parsing/default propagation.
- `src/cli.rs`
  - production `RunConfig` propagation.
- `crates/orca-tui/src/vim.rs`
  - pending Insert prefix, fixed deadline, deterministic state machine.
- `crates/orca-tui/src/app.rs`
  - production construction, central pending-key preflight, timeout poll,
    paste/mouse/wheel flush.
- `crates/orca-tui/src/runtime_event_actions.rs`
  - flush on runtime status transitions and composer replacement.

### Mechanical fixture files

Every existing `RunConfig` literal must add `vim_insert_escape: None` unless the
test explicitly configures a sequence:

- `crates/orca-runtime/examples/goal_mode_realapi.rs`
- `crates/orca-runtime/src/approval_resolution.rs`
- `crates/orca-runtime/src/child_agent_tests.rs`
- `crates/orca-runtime/src/controller.rs`
- `crates/orca-runtime/src/lifecycle.rs`
- `crates/orca-runtime/src/memory.rs`
- `crates/orca-runtime/src/provider_turn.rs`
- `crates/orca-runtime/src/runtime_readonly_tool_turn.rs`
- `crates/orca-runtime/src/runtime_tool_scheduler.rs`
- `crates/orca-runtime/src/runtime_turn_loop.rs`
- `crates/orca-runtime/src/server.rs`
- `crates/orca-runtime/src/session.rs`
- `crates/orca-runtime/src/subagent_execution.rs`
- `crates/orca-runtime/src/thread.rs`
- `crates/orca-runtime/src/tool_execution.rs`
- `crates/orca-runtime/src/tool_invocation.rs`
- `crates/orca-runtime/src/tool_turn.rs`
- `crates/orca-runtime/src/workflow/runner.rs`
- `crates/orca-runtime/src/workflow_execution.rs`
- `crates/orca-runtime/tests/acp_agent.rs`
- `crates/orca-runtime/tests/runtime_host.rs`
- `crates/orca-tui/src/lib.rs`
- `crates/orca-tui/src/status_key_actions.rs`
- `tests/runtime_lifecycle_contract.rs`
- `tests/server_runtime_contract.rs`
- `tests/workflow_runtime_contract.rs`

No manifest, lockfile, shortcut registry, server protocol, history format, or
renderer file changes are allowed.

---

### Task 1: Parse and Validate the Configured Sequence

**Files:**
- Modify: `crates/orca-core/src/config/mod.rs`
- Modify: `crates/orca-core/src/config/file.rs`

- [ ] **Step 1: Write RED validation and TOML tests**

In `crates/orca-core/src/config/mod.rs` tests, add:

```rust
#[test]
fn vim_insert_escape_validates_exactly_two_printable_non_whitespace_scalars() {
    for (value, first, second) in [("jj", 'j', 'j'), ("jk", 'j', 'k'), ("你好", '你', '好')] {
        let sequence = VimInsertEscapeSequence::parse(value).unwrap();
        assert_eq!(sequence.first(), first);
        assert_eq!(sequence.second(), second);
        assert_eq!(sequence.as_str(), value);
    }

    for value in ["", "j", "jjj", "j ", " j", "\nj", "j\u{7f}"] {
        let error = VimInsertEscapeSequence::parse(value).unwrap_err();
        assert!(error.contains("exactly two"), "{value:?}: {error}");
    }
}
```

In `crates/orca-core/src/config/file.rs` tests, add:

```rust
#[test]
fn vim_insert_escape_defaults_to_none_and_parses_valid_sequence() {
    let omitted: FileConfig = toml::from_str("").unwrap();
    let configured: FileConfig = toml::from_str(
        r#"
vim_mode = true
vim_insert_escape = "jj"
"#,
    )
    .unwrap();

    assert_eq!(omitted.vim_insert_escape, None);
    assert_eq!(
        configured
            .vim_insert_escape
            .as_ref()
            .map(VimInsertEscapeSequence::as_str),
        Some("jj")
    );
}

#[test]
fn vim_insert_escape_rejects_invalid_effective_value() {
    let error = toml::from_str::<FileConfig>(r#"vim_insert_escape = "j""#).unwrap_err();
    assert!(error.to_string().contains("exactly two"));
}

#[test]
fn invalid_layered_vim_insert_escape_uses_existing_default_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let user_path = dir.path().join("config.toml");
    std::fs::write(&user_path, r#"vim_insert_escape = "j""#).unwrap();

    let config = load_layered_config_from_paths(&user_path, dir.path());

    assert_eq!(config.vim_insert_escape, None);
    assert_eq!(config.theme, ThemeName::Auto);
}
```

- [ ] **Step 2: Run RED**

Run:

```sh
cargo test -p orca-core vim_insert_escape --lib -- --test-threads=1
```

Expected: compile failure because `VimInsertEscapeSequence` and the config
field do not exist.

- [ ] **Step 3: Implement the strong type**

In `crates/orca-core/src/config/mod.rs`, add imports:

```rust
use serde::{Deserialize, Deserializer, Serialize};
```

Extend the existing serde import rather than duplicating it. Add:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VimInsertEscapeSequence {
    value: String,
    first: char,
    second: char,
}

impl VimInsertEscapeSequence {
    pub fn parse(value: &str) -> Result<Self, String> {
        let mut characters = value.chars();
        let Some(first) = characters.next() else {
            return Err(
                "vim_insert_escape must contain exactly two non-whitespace, non-control characters"
                    .to_string(),
            );
        };
        let Some(second) = characters.next() else {
            return Err(
                "vim_insert_escape must contain exactly two non-whitespace, non-control characters"
                    .to_string(),
            );
        };
        if characters.next().is_some()
            || [first, second]
                .into_iter()
                .any(|character| character.is_whitespace() || character.is_control())
        {
            return Err(
                "vim_insert_escape must contain exactly two non-whitespace, non-control characters"
                    .to_string(),
            );
        }
        Ok(Self {
            value: value.to_string(),
            first,
            second,
        })
    }

    pub fn first(&self) -> char {
        self.first
    }

    pub fn second(&self) -> char {
        self.second
    }

    pub fn as_str(&self) -> &str {
        &self.value
    }
}

impl<'de> Deserialize<'de> for VimInsertEscapeSequence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}
```

Do not derive `Serialize` unless another existing config API requires it.

- [ ] **Step 4: Add the file-config field**

In both `FileConfig` and `RawFileConfig` in
`crates/orca-core/src/config/file.rs`, add after `vim_mode`:

```rust
#[serde(default)]
pub vim_insert_escape: Option<crate::config::VimInsertEscapeSequence>,
```

In `FileConfig::default` add:

```rust
vim_insert_escape: None,
```

In `From<RawFileConfig> for FileConfig` add:

```rust
vim_insert_escape: raw.vim_insert_escape,
```

- [ ] **Step 5: Run GREEN and config regressions**

Run:

```sh
cargo test -p orca-core vim_insert_escape --lib -- --test-threads=1
cargo test -p orca-core config::file::tests::parse_experience_config --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: all pass.

- [ ] **Step 6: Commit config parsing**

```sh
git add crates/orca-core/src/config/mod.rs crates/orca-core/src/config/file.rs
git commit \
  -m "feat(config): parse vim insert escape sequence" \
  -m "Validate an optional two-character non-whitespace Insert-mode escape mapping while preserving the existing layered-config fallback contract." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 2: Propagate the Effective Setting Through RunConfig

**Files:**
- Modify: `crates/orca-core/src/config/mod.rs`
- Modify: `src/cli.rs`
- Modify: every mechanical fixture file listed in the file map

- [ ] **Step 1: Write RED effective-config display tests**

Extend `format_config_show_redacts_api_key_and_includes_effective_values` in
`crates/orca-core/src/config/mod.rs`:

```rust
config.vim_insert_escape = Some(VimInsertEscapeSequence::parse("j\\").unwrap());
let shown = format_config_show(&config);
assert!(shown.contains(r#"vim_insert_escape = "j\\""#));

config.vim_insert_escape = None;
let shown = format_config_show(&config);
assert!(shown.contains(r#"vim_insert_escape = "<unset>""#));
```

Add a source-structure test in `src/cli.rs`:

```rust
#[test]
fn production_run_configs_propagate_vim_insert_escape() {
    let production = include_str!("cli.rs")
        .split("\n#[cfg(test)]")
        .next()
        .expect("production cli source");
    assert!(
        production
            .matches("vim_insert_escape: file_config.vim_insert_escape.clone()")
            .count()
            == 6
    );
}
```

- [ ] **Step 2: Run RED**

Run:

```sh
cargo test -p orca-core format_config_show_redacts --lib -- --test-threads=1
cargo test production_run_configs_propagate_vim_insert_escape -- --test-threads=1
```

Expected: compile/assertion failure because `RunConfig` has no field and CLI
constructors do not propagate it.

- [ ] **Step 3: Add `RunConfig.vim_insert_escape`**

In `RunConfig`, add immediately after `vim_mode`:

```rust
pub vim_insert_escape: Option<VimInsertEscapeSequence>,
```

In `format_config_show`, add:

```rust
let vim_insert_escape = config
    .vim_insert_escape
    .as_ref()
    .map(|sequence| toml::Value::String(sequence.as_str().to_string()).to_string())
    .unwrap_or_else(|| "\"<unset>\"".to_string());
```

Add the format line immediately after `vim_mode`:

```rust
"vim_insert_escape = {}\n",
```

and pass `vim_insert_escape` in the corresponding argument position.

Verify `toml::Value::String(...).to_string()` produces one valid quoted TOML
basic string in the RED test. The workspace uses TOML 0.8 with its `display`
implementation, so this path emits an escaped quoted TOML value. Do not
hand-roll escaping.

- [ ] **Step 4: Propagate production CLI values**

For every production `RunConfig` in `src/cli.rs`, add after `vim_mode`:

```rust
vim_insert_escape: file_config.vim_insert_escape.clone(),
```

Use `clone()` uniformly so field order and later `file_config` reads never
change ownership behavior.

- [ ] **Step 5: Update every fixture literal**

In every mechanical fixture file from the file map, add after `vim_mode`:

```rust
vim_insert_escape: None,
```

Do not use `..Default::default()` and do not change unrelated fixture values.

The broad `cargo check --workspace --all-targets` in the next step is the
exhaustive fixture-literal gate: every missing `RunConfig` field is a compile
error. The CLI source-structure test separately proves production propagation.

- [ ] **Step 6: Run GREEN and broad compile gate**

Run:

```sh
cargo test -p orca-core vim_insert_escape --lib -- --test-threads=1
cargo test -p orca-core format_config_show_redacts --lib -- --test-threads=1
cargo test production_run_configs_propagate_vim_insert_escape -- --test-threads=1
cargo check --workspace --all-targets
cargo fmt --all -- --check
git diff --check
```

Expected: all pass with only pre-existing warnings.

- [ ] **Step 7: Commit propagation**

```sh
git add crates/orca-core/src/config/mod.rs src/cli.rs \
  crates/orca-runtime crates/orca-tui/src/lib.rs \
  crates/orca-tui/src/status_key_actions.rs tests
git commit \
  -m "feat(config): propagate vim insert escape mapping" \
  -m "Carry the validated optional mapping through every RunConfig constructor and expose its escaped effective value in config output." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Implement the Deterministic Vim Insert State Machine

**Files:**
- Modify: `crates/orca-tui/src/vim.rs`

- [ ] **Step 1: Write RED core behavior tests**

Add imports in the Vim tests:

```rust
use std::time::{Duration, Instant};
use orca_core::config::VimInsertEscapeSequence;
```

Add a helper without changing the existing `VimState::new(bool)` call sites:

```rust
fn insert_escape_state(value: &str) -> VimState {
    let mut state = VimState::with_insert_escape(
        true,
        Some(VimInsertEscapeSequence::parse(value).unwrap()),
    );
    state.mode = VimMode::Insert;
    state
}
```

Add:

```rust
#[test]
fn vim_insert_escape_exact_pair_exits_without_text_or_history() {
    let theme = Theme::named(ThemeName::Dark);
    let started = Instant::now();
    let mut state = insert_escape_state("jj");
    let mut textarea = TextArea::from(["draft"]);
    textarea.move_cursor(CursorMove::End);

    assert!(state.handle_at(input('j'), &mut textarea, &theme, started));
    assert_eq!(textarea.lines(), &["draft"]);
    assert_eq!(state.mode, VimMode::Insert);
    assert!(state.has_pending_insert_escape_for_test());

    assert!(state.handle_at(
        input('j'),
        &mut textarea,
        &theme,
        started + Duration::from_millis(500),
    ));
    assert_eq!(textarea.lines(), &["draft"]);
    assert_eq!(state.mode, VimMode::Normal);
    assert!(!state.has_pending_insert_escape_for_test());
    assert!(!textarea.undo());
}

#[test]
fn vim_insert_escape_mismatch_overlap_and_expiry_preserve_text_once() {
    let theme = Theme::named(ThemeName::Dark);
    let started = Instant::now();

    let mut mismatch = insert_escape_state("jk");
    let mut mismatch_area = TextArea::default();
    mismatch.handle_at(input('j'), &mut mismatch_area, &theme, started);
    mismatch.handle_at(
        input('x'),
        &mut mismatch_area,
        &theme,
        started + Duration::from_millis(1),
    );
    assert_eq!(mismatch_area.lines(), &["jx"]);

    let mut overlap = insert_escape_state("jk");
    let mut overlap_area = TextArea::default();
    for (character, millis) in [('j', 0), ('j', 1), ('k', 2)] {
        overlap.handle_at(
            input(character),
            &mut overlap_area,
            &theme,
            started + Duration::from_millis(millis),
        );
    }
    assert_eq!(overlap_area.lines(), &["j"]);
    assert_eq!(overlap.mode, VimMode::Normal);

    let mut expired = insert_escape_state("jj");
    let mut expired_area = TextArea::default();
    expired.handle_at(input('j'), &mut expired_area, &theme, started);
    assert!(expired.flush_expired_insert_escape(
        started + Duration::from_millis(501),
        &mut expired_area,
    ));
    assert_eq!(expired_area.lines(), &["j"]);
    assert!(!expired.flush_expired_insert_escape(
        started + Duration::from_secs(1),
        &mut expired_area,
    ));
}
```

Add:

```rust
#[test]
fn vim_insert_escape_disabled_modified_and_direct_esc_paths_preserve_behavior() {
    let theme = Theme::named(ThemeName::Dark);
    let started = Instant::now();

    let mut disabled = VimState::new(true);
    disabled.mode = VimMode::Insert;
    let mut disabled_area = TextArea::default();
    disabled.handle_at(input('j'), &mut disabled_area, &theme, started);
    disabled.handle_at(input('j'), &mut disabled_area, &theme, started);
    assert_eq!(disabled_area.lines(), &["jj"]);

    let mut modified = insert_escape_state("jj");
    let mut modified_area = TextArea::default();
    modified.handle_at(input('j'), &mut modified_area, &theme, started);
    modified.handle_at(
        Input {
            key: Key::Char('j'),
            ctrl: true,
            alt: false,
            shift: false,
        },
        &mut modified_area,
        &theme,
        started + Duration::from_millis(1),
    );
    assert_eq!(modified_area.lines(), &["j"]);
    assert_eq!(modified.mode, VimMode::Insert);

    let mut escaped = insert_escape_state("jj");
    let mut escaped_area = TextArea::default();
    escaped.handle_at(input('j'), &mut escaped_area, &theme, started);
    escaped.handle_at(
        Input {
            key: Key::Esc,
            ctrl: false,
            alt: false,
            shift: false,
        },
        &mut escaped_area,
        &theme,
        started + Duration::from_millis(1),
    );
    assert_eq!(escaped_area.lines(), &["j"]);
    assert_eq!(escaped.mode, VimMode::Normal);
}
```

- [ ] **Step 2: Run RED**

Run:

```sh
cargo test -p orca-tui vim_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui expired_insert_escape --lib -- --test-threads=1
```

Expected: compile failure because the constructor, pending state, deterministic
handler, and flush methods do not exist.

- [ ] **Step 3: Add state and flow types**

At the top of `vim.rs`:

```rust
use std::time::{Duration, Instant};
use orca_core::config::VimInsertEscapeSequence;

const VIM_INSERT_ESCAPE_WINDOW: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingInsertEscape {
    character: char,
    deadline: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingInsertEscapeFlow {
    NoPending,
    Flushed,
    Consumed,
}
```

Extend `VimState`:

```rust
insert_escape: Option<VimInsertEscapeSequence>,
pending_insert_escape: Option<PendingInsertEscape>,
```

Keep the existing constructor:

```rust
pub fn new(enabled: bool) -> Self {
    Self::with_insert_escape(enabled, None)
}

pub fn with_insert_escape(
    enabled: bool,
    insert_escape: Option<VimInsertEscapeSequence>,
) -> Self {
    Self {
        enabled,
        mode: if enabled { VimMode::Normal } else { VimMode::Insert },
        parser: VimCommandParser::default(),
        registers: VimRegisterBank::default(),
        last_change: None,
        insert_escape,
        pending_insert_escape: None,
    }
}
```

- [ ] **Step 4: Implement deterministic pending resolution**

Add:

```rust
pub(crate) fn resolve_pending_insert_escape(
    &mut self,
    input: &Input,
    now: Instant,
    textarea: &mut TextArea<'_>,
) -> PendingInsertEscapeFlow {
    let Some(pending) = self.pending_insert_escape.take() else {
        return PendingInsertEscapeFlow::NoPending;
    };
    if now <= pending.deadline
        && !input.ctrl
        && !input.alt
        && self
            .insert_escape
            .as_ref()
            .is_some_and(|sequence| input.key == Key::Char(sequence.second()))
    {
        self.mode = VimMode::Normal;
        textarea.cancel_selection();
        return PendingInsertEscapeFlow::Consumed;
    }
    textarea.insert_char(pending.character);
    PendingInsertEscapeFlow::Flushed
}

pub(crate) fn flush_pending_insert_escape(
    &mut self,
    textarea: &mut TextArea<'_>,
) -> bool {
    let Some(pending) = self.pending_insert_escape.take() else {
        return false;
    };
    textarea.insert_char(pending.character);
    true
}

pub(crate) fn flush_expired_insert_escape(
    &mut self,
    now: Instant,
    textarea: &mut TextArea<'_>,
) -> bool {
    if self
        .pending_insert_escape
        .is_some_and(|pending| now > pending.deadline)
    {
        return self.flush_pending_insert_escape(textarea);
    }
    false
}
```

Add deterministic entry:

```rust
pub fn handle_at(
    &mut self,
    input: Input,
    textarea: &mut TextArea<'_>,
    theme: &Theme,
    now: Instant,
) -> bool {
    if !self.enabled {
        return textarea.input(input);
    }

    let changed = match self.mode {
        VimMode::Insert => self.handle_insert_at(input, textarea, now),
        VimMode::Normal | VimMode::Visual => self.handle_command(input, textarea),
    };
    self.configure_block(textarea, theme);
    changed
}

pub fn handle(&mut self, input: Input, textarea: &mut TextArea<'_>, theme: &Theme) -> bool {
    self.handle_at(input, textarea, theme, Instant::now())
}
```

Implement Insert handling:

```rust
fn handle_insert_at(
    &mut self,
    input: Input,
    textarea: &mut TextArea<'_>,
    now: Instant,
) -> bool {
    match self.resolve_pending_insert_escape(&input, now, textarea) {
        PendingInsertEscapeFlow::Consumed => return true,
        PendingInsertEscapeFlow::Flushed | PendingInsertEscapeFlow::NoPending => {}
    }

    if input.key == Key::Esc {
        self.cancel_pending_command();
        self.mode = VimMode::Normal;
        textarea.cancel_selection();
        return true;
    }

    if !input.ctrl
        && !input.alt
        && let Some(sequence) = self.insert_escape.as_ref()
        && input.key == Key::Char(sequence.first())
    {
        self.pending_insert_escape = Some(PendingInsertEscape {
            character: sequence.first(),
            deadline: now + VIM_INSERT_ESCAPE_WINDOW,
        });
        return true;
    }

    textarea.input(input)
}
```

When `resolve_pending_insert_escape` returns `Flushed` and the current input is
the first sequence character, the code above starts a new pending prefix,
implementing overlap.

- [ ] **Step 5: Make reset semantics explicit**

At the start of `reset_insert`:

```rust
self.flush_pending_insert_escape(textarea);
```

This is the defensive no-drop guarantee. Submission and queue snapshot paths
will already have flushed centrally before calling reset; direct reset callers
still preserve the character once.

Add test-only helper:

```rust
pub(crate) fn has_pending_insert_escape_for_test(&self) -> bool {
    self.pending_insert_escape.is_some()
}
```

- [ ] **Step 6: Run GREEN and existing Vim regressions**

Run:

```sh
cargo test -p orca-tui vim_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui expired_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui vim::tests:: --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: all pass.

- [ ] **Step 7: Commit the Vim state machine**

```sh
git add crates/orca-tui/src/vim.rs
git commit \
  -m "feat(tui): recognize vim insert escape sequence" \
  -m "Hold one configured Insert-mode prefix for a fixed 500ms window, match without textarea edits, and replay mismatch or expiry exactly once." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Integrate Central Routing, Expiry, and Runtime Fences

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/runtime_event_actions.rs`
- Test: `crates/orca-tui/src/idle_key_actions.rs`

- [ ] **Step 1: Write RED app routing tests**

In `app.rs` tests, add a focused helper around a pure function that will be
introduced below:

```rust
use orca_core::config::VimInsertEscapeSequence;

fn vim_insert_input(character: char) -> tui_textarea::Input {
    tui_textarea::Input {
        key: tui_textarea::Key::Char(character),
        ctrl: false,
        alt: false,
        shift: false,
    }
}

#[test]
fn pending_insert_escape_preflight_precedes_shortcuts_only_after_sequence_started() {
    let theme = Theme::named(ThemeName::Dark);
    let sequence = VimInsertEscapeSequence::parse("jj").unwrap();
    let started = Instant::now();
    let mut vim = VimState::with_insert_escape(true, Some(sequence));
    vim.mode = crate::vim::VimMode::Insert;
    let mut textarea = TextArea::default();
    let mut state = test_state().0;
    let config = test_config(HistoryMode::Disabled);

    let first = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(
        resolve_pending_insert_escape_before_routing(
            &first,
            started,
            &mut vim,
            &mut textarea,
            &mut state,
            &config,
            &theme,
        ),
        PendingInsertEscapeRouting::Continue,
    );
    vim.handle_at(
        vim_insert_input('j'),
        &mut textarea,
        &theme,
        started,
    );

    let second = Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
    assert_eq!(
        resolve_pending_insert_escape_before_routing(
            &second,
            started + Duration::from_millis(1),
            &mut vim,
            &mut textarea,
            &mut state,
            &config,
            &theme,
        ),
        PendingInsertEscapeRouting::Consumed,
    );
    assert_eq!(vim.mode, crate::vim::VimMode::Normal);
    assert!(textarea.is_empty());
}
```

Add mismatch/submit ordering:

```rust
#[test]
fn pending_insert_escape_flushes_before_submit_and_paste_ownership() {
    let theme = Theme::named(ThemeName::Dark);
    let started = Instant::now();
    let sequence = VimInsertEscapeSequence::parse("jj").unwrap();

    let (action_tx, action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    let mut config = test_config(HistoryMode::Disabled);
    let shared = Arc::new(Mutex::new(config.clone()));
    let mut vim = VimState::with_insert_escape(true, Some(sequence.clone()));
    vim.mode = crate::vim::VimMode::Insert;
    let mut textarea = TextArea::default();
    vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

    let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        resolve_pending_insert_escape_before_routing(
            &enter,
            started + Duration::from_millis(1),
            &mut vim,
            &mut textarea,
            &mut state,
            &config,
            &theme,
        ),
        PendingInsertEscapeRouting::Continue,
    );
    assert!(handle_idle_submit(
        &mut textarea,
        &mut vim,
        &theme,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
    ));
    assert!(matches!(
        action_rx.try_recv(),
        Ok(UserAction::SubmitWithMentions { prompt, .. }) if prompt == "j"
    ));

    let mut paste_state = test_state().0;
    let paste_config = test_config(HistoryMode::Disabled);
    let mut paste_vim = VimState::with_insert_escape(true, Some(sequence));
    paste_vim.mode = crate::vim::VimMode::Insert;
    let mut paste_area = TextArea::default();
    paste_vim.handle_at(vim_insert_input('j'), &mut paste_area, &theme, started);
    assert!(flush_pending_insert_escape_before_non_key(
        &mut paste_vim,
        &mut paste_area,
        &mut paste_state,
        &paste_config,
    ));
    assert!(handle_paste_event(
        &Event::Paste("jj".to_string()),
        &mut paste_state,
        &paste_config,
        &mut paste_area,
    ));
    assert_eq!(textarea_text(&paste_area), "jjj");
}
```

Add timeout ordering:

```rust
#[test]
fn expired_insert_escape_flush_refreshes_input_state_once() {
    let theme = Theme::named(ThemeName::Dark);
    let config = test_config(HistoryMode::Disabled);
    let started = Instant::now();
    let mut vim = VimState::with_insert_escape(
        true,
        Some(VimInsertEscapeSequence::parse("jj").unwrap()),
    );
    vim.mode = crate::vim::VimMode::Insert;
    let mut textarea = TextArea::default();
    let mut state = test_state().0;
    vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

    assert!(flush_expired_insert_escape(
        started + Duration::from_millis(501),
        &mut vim,
        &mut textarea,
        &mut state,
        &config,
    ));
    assert_eq!(textarea_text(&textarea), "j");
    assert!(!vim.has_pending_insert_escape_for_test());
}
```

- [ ] **Step 2: Write RED runtime-transition tests**

Extend `runtime_event_actions.rs` tests:

```rust
use orca_core::config::VimInsertEscapeSequence;
use tui_textarea::CursorMove;

fn vim_insert_input(character: char) -> tui_textarea::Input {
    tui_textarea::Input {
        key: tui_textarea::Key::Char(character),
        ctrl: false,
        alt: false,
        shift: false,
    }
}

#[test]
fn runtime_status_transition_flushes_pending_insert_escape_before_new_owner() {
    let (action_tx, _action_rx) = mpsc::unbounded();
    let pending = bridge::PendingWorkflowNotifications::new();
    let theme = Theme::named(ThemeName::Dark);
    let mut presentation = test_presentation();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    state.enter_running();
    let started = Instant::now();
    let mut vim = VimState::with_insert_escape(
        true,
        Some(VimInsertEscapeSequence::parse("jj").unwrap()),
    );
    vim.mode = VimMode::Insert;
    let mut textarea = TextArea::from(["draft"]);
    textarea.move_cursor(CursorMove::End);
    vim.handle_at(vim_insert_input('j'), &mut textarea, &theme, started);

    handle_runtime_event(
        TuiEvent::UserInputRequested {
            key: interaction_key(TuiInteractionKind::UserInput, "input"),
            question: "question".to_string(),
            choices: Vec::new(),
        },
        &mut state,
        &action_tx,
        &pending,
        &mut textarea,
        &mut vim,
        &theme,
        &mut presentation,
    );

    assert_eq!(textarea_text(&textarea), "draftj");
    assert!(!vim.has_pending_insert_escape_for_test());
    assert_eq!(state.status, AppStatus::WaitingUserInput);
}
```

- [ ] **Step 3: Run routing RED**

Run:

```sh
cargo test -p orca-tui pending_insert_escape_preflight --lib -- --test-threads=1
cargo test -p orca-tui pending_insert_escape_flushes_before --lib -- --test-threads=1
cargo test -p orca-tui expired_insert_escape_flush_refreshes --lib -- --test-threads=1
cargo test -p orca-tui runtime_status_transition_flushes_pending_insert_escape --lib -- --test-threads=1
```

Expected: compile failures because app integration helpers do not exist and
runtime transitions still clear only Normal-mode parser state.

- [ ] **Step 4: Construct production Vim state with config**

In `run_tui_inner`:

```rust
let mut vim_state = VimState::with_insert_escape(
    config.vim_mode,
    config.vim_insert_escape.clone(),
);
```

- [ ] **Step 5: Add pure app routing helpers**

Import:

```rust
use crate::composer_input_actions::refresh_input_menus;
use crate::vim::{PendingInsertEscapeFlow, VimState};
```

Add:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingInsertEscapeRouting {
    Continue,
    Consumed,
}

fn refresh_after_insert_escape_flush(
    state: &mut AppState,
    config: &RunConfig,
    textarea: &TextArea<'_>,
) {
    state.reset_history_navigation();
    refresh_input_menus(textarea, state, config);
}

fn resolve_pending_insert_escape_before_routing(
    event: &Event,
    now: Instant,
    vim_state: &mut VimState,
    textarea: &mut TextArea<'_>,
    state: &mut AppState,
    config: &RunConfig,
    theme: &Theme,
) -> PendingInsertEscapeRouting {
    let Event::Key(key) = event else {
        return PendingInsertEscapeRouting::Continue;
    };
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return PendingInsertEscapeRouting::Continue;
    }
    match vim_state.resolve_pending_insert_escape(&Input::from(event.clone()), now, textarea) {
        PendingInsertEscapeFlow::Consumed => {
            vim_state.configure_block(textarea, theme);
            PendingInsertEscapeRouting::Consumed
        }
        PendingInsertEscapeFlow::Flushed => {
            refresh_after_insert_escape_flush(state, config, textarea);
            PendingInsertEscapeRouting::Continue
        }
        PendingInsertEscapeFlow::NoPending => PendingInsertEscapeRouting::Continue,
    }
}

fn flush_pending_insert_escape_before_non_key(
    vim_state: &mut VimState,
    textarea: &mut TextArea<'_>,
    state: &mut AppState,
    config: &RunConfig,
) -> bool {
    if !vim_state.flush_pending_insert_escape(textarea) {
        return false;
    }
    refresh_after_insert_escape_flush(state, config, textarea);
    true
}

fn flush_expired_insert_escape(
    now: Instant,
    vim_state: &mut VimState,
    textarea: &mut TextArea<'_>,
    state: &mut AppState,
    config: &RunConfig,
) -> bool {
    if !vim_state.flush_expired_insert_escape(now, textarea) {
        return false;
    }
    refresh_after_insert_escape_flush(state, config, textarea);
    true
}
```

Add needed imports:

```rust
use crossterm::event::KeyEventKind;
use tui_textarea::Input;
```

- [ ] **Step 6: Integrate deadline polling**

At the top of each main loop iteration, immediately after `let now`:

```rust
if flush_expired_insert_escape(
    now,
    &mut vim_state,
    &mut textarea,
    &mut state,
    &config,
) {
    scheduler.mark_dirty();
}
```

Do this before mention registry synchronization so refreshed composer text is
visible to all later logic in the same iteration.

- [ ] **Step 7: Integrate key preflight**

Inside `BatchedInputEvent::Event(ev)`, after focus handling and before paste,
mouse, or existing key preflight:

```rust
if resolve_pending_insert_escape_before_routing(
    &ev,
    Instant::now(),
    &mut vim_state,
    &mut textarea,
    &mut state,
    &config,
    &theme,
) == PendingInsertEscapeRouting::Consumed
{
    return Ok(None);
}
```

Because there is no pending prefix before the first configured character
reaches Vim, existing shortcuts keep their start-key priority.

- [ ] **Step 8: Flush non-key boundaries**

Before `handle_paste_event`:

```rust
if matches!(ev, Event::Paste(_)) {
    flush_pending_insert_escape_before_non_key(
        &mut vim_state,
        &mut textarea,
        &mut state,
        &config,
    );
}
```

Before every mouse event reaches `handle_mouse_event`:

```rust
if matches!(ev, Event::Mouse(_)) {
    flush_pending_insert_escape_before_non_key(
        &mut vim_state,
        &mut textarea,
        &mut state,
        &config,
    );
}
```

Before `BatchedInputEvent::ScrollLines` calls `handle_scroll_lines`:

```rust
flush_pending_insert_escape_before_non_key(
    &mut vim_state,
    &mut textarea,
    &mut state,
    &config,
);
```

Do not parse `Event::Paste("jj")` through `VimState`.

- [ ] **Step 9: Flush runtime status and replacement boundaries**

In `handle_runtime_event`, when `state.status != previous_status`, replace the
current parser-only reset with:

```rust
vim_state.flush_pending_insert_escape(textarea);
vim_state.cancel_pending_command();
```

Do the same for the final `initial_status` comparison.

Before the allowlisted-approval early return changes status:

```rust
vim_state.flush_pending_insert_escape(textarea);
vim_state.cancel_pending_command();
```

Immediately before every `reset_insert` that precedes textarea replacement,
call:

```rust
vim_state.flush_pending_insert_escape(textarea);
```

The replacement may intentionally discard old draft text, but no hidden prefix
state may carry into the new textarea.

- [ ] **Step 10: Run routing GREEN and regressions**

Run:

```sh
cargo test -p orca-tui pending_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui expired_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui runtime_event_actions::tests:: --lib -- --test-threads=1
cargo test -p orca-tui key_event_actions::tests:: --lib -- --test-threads=1
cargo test -p orca-tui status_key_actions::tests:: --lib -- --test-threads=1
cargo test -p orca-tui idle_key_actions::tests:: --lib -- --test-threads=1
cargo test -p orca-tui queued_input_actions::tests:: --lib -- --test-threads=1
cargo test -p orca-tui composer_input_actions::tests:: --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: all pass.

- [ ] **Step 11: Commit routing integration**

```sh
git add crates/orca-tui/src/app.rs crates/orca-tui/src/runtime_event_actions.rs
git commit \
  -m "feat(tui): route vim insert escape remap" \
  -m "Resolve initiated mappings before existing key routing, flush held text at non-key and runtime ownership boundaries, and expire prefixes in the existing frame loop." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Independent Review, Full Verification, and Push

**Files:**
- Verify all files in Tasks 1–4
- Verify: `docs/superpowers/specs/2026-07-28-tui-vim-insert-escape-design.md`
- Verify: `docs/superpowers/plans/2026-07-28-tui-vim-insert-escape.md`

- [ ] **Step 1: Run focused acceptance**

```sh
cargo test -p orca-core vim_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui vim_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui pending_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui expired_insert_escape --lib -- --test-threads=1
cargo test -p orca-tui vim::tests:: --lib -- --test-threads=1
cargo test -p orca-tui runtime_event_actions::tests:: --lib -- --test-threads=1
```

- [ ] **Step 2: Run scope audits**

```sh
git diff --name-only 1892545..HEAD
git diff --exit-code 1892545..HEAD -- Cargo.toml Cargo.lock \
  crates/orca-tui/Cargo.toml crates/orca-core/Cargo.toml
git diff -U0 1892545..HEAD -- crates/orca-tui/src crates/orca-core/src src/cli.rs \
  | rg '^\+.*(keybindings|hot.reload|insert.session.repeat|protocol)' \
  && exit 1 || true
```

Expected: only declared production/fixture files and the plan changed; no
manifest or excluded feature leaked.

- [ ] **Step 3: Request independent spec and quality reviews**

Spec review must verify:

- default-off and invalid-config fallback;
- exact 500ms inclusive boundary;
- start-key shortcut priority and initiated-sequence precedence;
- mismatch/overlap and batch ordering;
- submit/queue inclusion;
- replacement non-carry semantics;
- paste/IME exclusion;
- no undo pollution;
- every key/non-key/runtime ownership boundary.

Quality review must probe:

- one-character timeout at 500/501ms;
- same-batch pair handling;
- `jj`, `jk`, Unicode, Ctrl/Alt, Shift-adapted characters;
- Idle Backtrack and Running Interrupt;
- paste `"jj"` and large-paste placeholder;
- mouse pre-click cursor ordering;
- runtime streaming versus status transition;
- registers/dot-repeat persistence;
- Vim disabled and mapping omitted.

Resolve every Important/Critical issue with a new RED/GREEN cycle.

- [ ] **Step 4: Run package gates on committed HEAD**

```sh
cargo test -p orca-core -- --test-threads=1
cargo test -p orca-tui -- --test-threads=1
cargo check --workspace --all-targets
cargo fmt --all -- --check
git diff --check
test -z "$(git status --porcelain=v1 -uall)"
```

- [ ] **Step 5: Audit commit trailers**

```sh
git log --format='%H' 1892545..HEAD | while read -r commit; do
  test "$(git show -s --format=%B "$commit" \
    | grep -Fxc 'Co-authored-by: TRAE CLI <noreply@bytedance.com>')" -eq 1
  test "$(git show -s --format=%B "$commit" | sed '/^$/d' | tail -n 1)" \
    = 'Co-authored-by: TRAE CLI <noreply@bytedance.com>'
done
```

- [ ] **Step 6: Run workspace gate**

```sh
cargo test --workspace --all-targets -- --test-threads=1
```

If either unchanged macOS timing test fails:

```text
external::tests::external_tool_timeout_kills_descendant_processes
external::tests::external_tool_timeout_preserves_observed_exit_code
```

prove `crates/orca-tools/src/external.rs` has the same blob at baseline
`1892545` and `HEAD`, then rerun with only those exact tests skipped.

- [ ] **Step 7: Push and verify remote SHA**

```sh
branch=$(git branch --show-current)
local_sha=$(git rev-parse HEAD)
git push origin "$branch"
remote_sha=$(git ls-remote --heads origin "$branch" | awk '{print $1}')
test -n "$remote_sha"
test "$local_sha" = "$remote_sha"
test -z "$(git status --porcelain=v1 -uall)"
printf 'verified=%s\n' "$remote_sha"
```

Do not create a tag, release, PR, or worktree cleanup. After remote
verification, continue the remaining P2 roadmap audit rather than marking the
overall optimization goal complete.
