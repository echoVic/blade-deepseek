# TUI Custom Keybindings Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add context-aware custom TUI keybindings, sequential chords, dynamic shortcut guidance, and bounded live reload from `keybindings.json` without changing default behavior.

**Architecture:** Keep semantic shortcut actions and their execution in the existing shortcut/handler modules. Add a focused `keybindings` directory with an immutable effective `Keymap`, a deterministic chord `KeymapRuntime`, and a request-driven reload worker; `app.rs` owns one runtime and passes pre-resolved `ShortcutInvocation`s into action-only handlers.

**Tech Stack:** Rust 2024, crossterm key events, serde/serde_json, crossbeam-channel, ratatui, tempfile, existing FrameScheduler and TUI event loop.

---

## File Map

- Create `crates/orca-tui/src/keybindings/mod.rs`
  - Public module surface and shared keymap types.
- Create `crates/orca-tui/src/keybindings/config.rs`
  - JSON schema, key syntax, built-in merge, conflict validation, canonical formatting, and help descriptors.
- Create `crates/orca-tui/src/keybindings/runtime.rs`
  - Pending chord state, input-owner fences, deterministic resolution, and deadlines.
- Create `crates/orca-tui/src/keybindings/reload.rs`
  - `ORCA_HOME` path resolution, bounded request-driven loader, observation deduplication, and last-known-good application.
- Modify `crates/orca-tui/src/lib.rs`
  - Register the internal `keybindings` module.
- Modify `crates/orca-tui/src/shortcuts.rs`
  - Retain semantic enums, built-in definitions, labels, and legacy help strings; remove static resolution ownership.
- Modify `crates/orca-tui/src/global_actions.rs`
  - Keep semantic Global action execution unchanged.
- Modify `crates/orca-tui/src/idle_key_actions.rs`
  - Separate resolution from action-only execution.
- Modify `crates/orca-tui/src/idle_navigation_actions.rs`
  - Define raw-key compatibility versus chord semantics.
- Modify `crates/orca-tui/src/queued_input_actions.rs`
  - Add action-only Running execution.
- Modify `crates/orca-tui/src/status_key_actions.rs`
  - Dispatch pre-resolved Idle, Running, Compacting, and Approval invocations.
- Modify `crates/orca-tui/src/approval_dialog_actions.rs`
  - Preserve fixed direct option keys and execute configurable Approval actions separately.
- Modify `crates/orca-tui/src/key_event_actions.rs`
  - Run emergency cancel before pending chord advancement and accept pre-resolved Global invocations.
- Modify `crates/orca-tui/src/app.rs`
  - Own `KeymapRuntime`, poll reload results, maintain input-owner fingerprints, cap waits by chord deadlines, and clear chords at non-key/suspend boundaries.
- Modify `crates/orca-tui/src/ui.rs`
  - Read active keymap help for the overlay, welcome tips, and status hint.

No `RunConfig` field is added. The keybindings file is TUI-local and loaded only by `run_tui_inner`.

### Task 1: Define Key Syntax and Exact Built-in Keymap

**Files:**
- Create: `crates/orca-tui/src/keybindings/mod.rs`
- Create: `crates/orca-tui/src/keybindings/config.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/shortcuts.rs`

- [ ] **Step 1: Write failing default-parity and key-syntax tests**

In `crates/orca-tui/src/keybindings/config.rs`, add tests that import every current binding from `shortcuts.rs` and assert exact normalized parity:

```rust
#[test]
fn built_in_keymap_matches_all_legacy_bindings() {
    let keymap = Keymap::built_in();
    for binding in crate::shortcuts::configurable_legacy_bindings() {
        assert_eq!(
            keymap.resolve_single(binding.context, binding.as_key_event()),
            Some(binding.action),
            "{:?} {:?} must keep {binding:?}",
            binding.context,
            binding.action,
        );
    }
    assert_eq!(
        keymap.binding_count(),
        crate::shortcuts::configurable_legacy_bindings().count(),
    );
}

#[test]
fn key_strokes_parse_and_format_canonically() {
    for (source, canonical) in [
        ("CTRL+X", "ctrl+x"),
        ("alt+SHIFT+enter", "alt+shift+enter"),
        ("BackTab", "backtab"),
        ("space", "space"),
        ("f24", "f24"),
        ("ctrl+界", "ctrl+界"),
    ] {
        assert_eq!(KeyStroke::parse(source).unwrap().to_string(), canonical);
    }
}

#[test]
fn invalid_key_strokes_are_rejected() {
    for source in ["", "ctrl", "ctrl+", "ctrl+ctrl+x", "wat+x", "a+b", "f25", "\n"] {
        assert!(KeyStroke::parse(source).is_err(), "{source:?}");
    }
}

#[test]
fn normalization_keeps_c0_and_shifted_character_compatibility() {
    let ctrl_j = KeyStroke::parse("ctrl+j").unwrap();
    assert!(ctrl_j.matches(KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE)));
    let shift_a = KeyStroke::parse("shift+a").unwrap();
    assert!(shift_a.matches(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE)));
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test -p orca-tui keybindings::config::tests -- --nocapture
```

Expected: compilation fails because `keybindings`, `Keymap`, `KeyStroke`, and `legacy_bindings` do not exist.

- [ ] **Step 3: Add semantic metadata and key types**

Keep the existing action enums in `shortcuts.rs`. Add:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShortcutContext {
    Global,
    Idle,
    Running,
    Approval,
}

pub(crate) struct LegacyBinding {
    pub(crate) context: ShortcutContext,
    pub(crate) action: ShortcutAction,
    pub(crate) key: KeyCode,
    pub(crate) modifiers: KeyModifiers,
}

impl LegacyBinding {
    pub(crate) const fn as_key_event(self) -> KeyEvent {
        KeyEvent::new(self.key, self.modifiers)
    }
}

pub(crate) fn configurable_legacy_bindings() -> impl Iterator<Item = LegacyBinding> {
    GLOBAL_BINDINGS
        .iter()
        .map(|(action, binding)| LegacyBinding::global(*action, *binding))
        .chain(
            IDLE_BINDINGS
                .iter()
                .map(|(action, binding)| LegacyBinding::idle(*action, *binding)),
        )
        .chain(
            RUNNING_BINDINGS
                .iter()
                .map(|(action, binding)| LegacyBinding::running(*action, *binding)),
        )
        .chain(
            APPROVAL_BINDINGS
                .iter()
                .filter(|(action, _)| {
                    matches!(
                        action,
                        ApprovalShortcut::SelectAllow
                            | ApprovalShortcut::SelectDeny
                            | ApprovalShortcut::ToggleSelection
                            | ApprovalShortcut::Confirm
                    )
                })
                .map(|(action, binding)| LegacyBinding::approval(*action, *binding)),
        )
}
```

Expose stable action metadata:

```rust
impl ShortcutAction {
    pub(crate) const fn configurable_id(self) -> Option<&'static str> {
        match self {
            Self::Global(GlobalShortcut::Cancel) => Some("global.cancel"),
            Self::Global(GlobalShortcut::OpenTranscriptSearch) => {
                Some("global.open-transcript-search")
            }
            Self::Global(GlobalShortcut::ToggleShortcuts) => Some("global.toggle-shortcuts"),
            Self::Global(GlobalShortcut::ScrollBottom) => Some("global.scroll-bottom"),
            Self::Global(GlobalShortcut::ScrollTop) => Some("global.scroll-top"),
            Self::Global(GlobalShortcut::ClearScreen) => Some("global.clear-screen"),
            Self::Idle(IdleShortcut::Submit) => Some("idle.submit"),
            Self::Idle(IdleShortcut::Newline) => Some("idle.newline"),
            Self::Idle(IdleShortcut::EditLatestQueued) => Some("idle.edit-latest-queued"),
            Self::Idle(IdleShortcut::HistoryPrevious) => Some("idle.history-previous"),
            Self::Idle(IdleShortcut::HistoryNext) => Some("idle.history-next"),
            Self::Idle(IdleShortcut::ScrollUp) => Some("idle.scroll-up"),
            Self::Idle(IdleShortcut::ScrollDown) => Some("idle.scroll-down"),
            Self::Idle(IdleShortcut::PageUp) => Some("idle.page-up"),
            Self::Idle(IdleShortcut::PageDown) => Some("idle.page-down"),
            Self::Idle(IdleShortcut::HalfPageUp) => Some("idle.half-page-up"),
            Self::Idle(IdleShortcut::HalfPageDown) => Some("idle.half-page-down"),
            Self::Idle(IdleShortcut::Backtrack) => Some("idle.backtrack"),
            Self::Idle(IdleShortcut::ExpandToolOutput) => Some("idle.expand-tool-output"),
            Self::Running(RunningShortcut::BackgroundCurrentTurn) => {
                Some("running.background-current-turn")
            }
            Self::Running(RunningShortcut::Interrupt) => Some("running.interrupt"),
            Self::Running(RunningShortcut::SubmitQueued) => Some("running.submit-queued"),
            Self::Running(RunningShortcut::Newline) => Some("running.newline"),
            Self::Running(RunningShortcut::EditLatestQueued) => {
                Some("running.edit-latest-queued")
            }
            Self::Running(RunningShortcut::ScrollUp) => Some("running.scroll-up"),
            Self::Running(RunningShortcut::ScrollDown) => Some("running.scroll-down"),
            Self::Running(RunningShortcut::PageUp) => Some("running.page-up"),
            Self::Running(RunningShortcut::PageDown) => Some("running.page-down"),
            Self::Running(RunningShortcut::HalfPageUp) => Some("running.half-page-up"),
            Self::Running(RunningShortcut::HalfPageDown) => Some("running.half-page-down"),
            Self::Approval(ApprovalShortcut::SelectAllow) => Some("approval.select-allow"),
            Self::Approval(ApprovalShortcut::SelectDeny) => Some("approval.select-deny"),
            Self::Approval(ApprovalShortcut::ToggleSelection) => {
                Some("approval.toggle-selection")
            }
            Self::Approval(ApprovalShortcut::Confirm) => Some("approval.confirm"),
            Self::Approval(ApprovalShortcut::Approve)
            | Self::Approval(ApprovalShortcut::Deny) => None,
        }
    }

    pub(crate) const fn context(self) -> ShortcutContext {
        match self {
            Self::Global(_) => ShortcutContext::Global,
            Self::Idle(_) => ShortcutContext::Idle,
            Self::Running(_) => ShortcutContext::Running,
            Self::Approval(_) => ShortcutContext::Approval,
        }
    }
}

pub(crate) fn action_for_id(id: &str) -> Option<ShortcutAction> {
    // Exhaustive inverse of ShortcutAction::configurable_id for configurable actions.
}
```

In `keybindings/config.rs`, implement:

```rust
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct KeyStroke {
    code: KeyCode,
    modifiers: KeyModifiers,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct KeySequence(Vec<KeyStroke>);

#[derive(Clone, Debug)]
pub(crate) struct Keymap {
    bindings: HashMap<ShortcutContext, Vec<(KeySequence, ShortcutAction)>>,
}
```

`KeyStroke::parse`, `Display`, and `matches` must:

- support all named keys from the spec;
- accept one Unicode scalar character;
- canonicalize modifier order;
- call the extracted existing normalization function;
- ignore Release events and accept Press/Repeat.

- [ ] **Step 4: Build the exact built-in map**

Implement:

```rust
impl Keymap {
    pub(crate) fn built_in() -> Arc<Self> {
        Arc::new(Self::from_legacy(
            crate::shortcuts::configurable_legacy_bindings(),
        ))
    }

    pub(crate) fn resolve_single(
        &self,
        context: ShortcutContext,
        event: KeyEvent,
    ) -> Option<ShortcutAction> {
        self.single_global(event).or_else(|| self.single_context(context, event))
    }
}
```

Do not delete the old `resolve_shortcut` functions yet; later tasks migrate call sites under test.

- [ ] **Step 5: Run focused tests and verify GREEN**

Run:

```bash
cargo test -p orca-tui keybindings::config::tests -- --nocapture
cargo test -p orca-tui shortcuts::tests -- --nocapture
```

Expected: all config and legacy shortcut tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-tui/src/lib.rs \
  crates/orca-tui/src/keybindings/mod.rs \
  crates/orca-tui/src/keybindings/config.rs \
  crates/orca-tui/src/shortcuts.rs
git commit -m "feat(tui): model configurable keymaps" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

Verify the trailer is a separate final line before continuing.

### Task 2: Parse, Merge, and Validate `keybindings.json`

**Files:**
- Modify: `crates/orca-tui/src/keybindings/config.rs`
- Modify: `crates/orca-tui/src/shortcuts.rs`

- [ ] **Step 1: Write failing schema and merge tests**

Add:

```rust
#[test]
fn omitted_actions_inherit_and_present_actions_replace_defaults() {
    let keymap = parse_keymap(br#"{
        "version": 1,
        "bindings": {
            "idle.submit": ["ctrl+s"],
            "idle.backtrack": []
        }
    }"#).unwrap();
    assert_eq!(
        keymap.resolve_single(
            ShortcutContext::Idle,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
        ),
        Some(ShortcutAction::Idle(IdleShortcut::Submit)),
    );
    assert_eq!(
        keymap.resolve_single(
            ShortcutContext::Idle,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ),
        None,
    );
    assert!(!keymap.has_action(ShortcutAction::Idle(IdleShortcut::Backtrack)));
    assert!(keymap.has_action(ShortcutAction::Global(GlobalShortcut::Cancel)));
}

#[test]
fn rejects_unknown_schema_and_action_ids() {
    assert_error_contains(
        br#"{"version":2,"bindings":{}}"#,
        "unsupported keybindings version 2",
    );
    assert_error_contains(
        br#"{"version":1,"extra":true,"bindings":{}}"#,
        "unknown field `extra`",
    );
    assert_error_contains(
        br#"{"version":1,"bindings":{"idle.missing":["x"]}}"#,
        "unknown action `idle.missing`",
    );
}
```

Add table-driven validation tests for:

```rust
[
    // Duplicate in one context.
    (json_with("idle.submit", "ctrl+x", "idle.newline", "ctrl+x"), "conflict"),
    // Global collides with Idle.
    (json_with("global.clear-screen", "ctrl+x", "idle.submit", "ctrl+x"), "conflict"),
    // Prefix ambiguity.
    (json_with("idle.submit", "ctrl+x", "idle.newline", "ctrl+x ctrl+j"), "prefix"),
    // Cancel cannot be disabled or multi-stroke.
    (replace("global.cancel", &[]), "single-stroke"),
    (replace("global.cancel", &["ctrl+x ctrl+c"]), "single-stroke"),
    // Cancel stroke cannot appear in another chord.
    (replace("idle.submit", &["ctrl+x ctrl+c"]), "reserved cancel"),
    // Global restrictions.
    (replace("global.clear-screen", &["esc"]), "configurable Global"),
    (replace("global.clear-screen", &["shift+x"]), "configurable Global"),
    // Approval direct keys reserve Approval and Global sequences.
    (replace("approval.confirm", &["1"]), "reserved Approval"),
    (replace("global.clear-screen", &["ctrl+x a"]), "reserved Approval"),
    // Textual prefixes outside Approval.
    (replace("idle.submit", &["g g"]), "non-textual"),
    // Length.
    (replace("idle.submit", &["ctrl+x ctrl+a ctrl+b ctrl+c ctrl+d"]), "at most four"),
]
```

Also assert:

- the same sequence can map differently in Idle and Running;
- bare-character prefixes work in Approval if not reserved;
- four-stroke sequences work;
- duplicate JSON keys are rejected by a custom bindings-map visitor, not silently overwritten.

- [ ] **Step 2: Run parser tests and verify RED**

Run:

```bash
cargo test -p orca-tui keybindings::config::tests -- --nocapture
```

Expected: new tests fail because only built-ins exist.

- [ ] **Step 3: Implement strict JSON parsing and replacement merge**

Add strict serde types:

```rust
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct KeybindingsFile {
    version: u32,
    bindings: StrictBindings,
}

struct StrictBindings(Vec<(String, Vec<String>)>);
```

Implement a map visitor for `StrictBindings` that rejects duplicate action IDs.

Implement:

```rust
pub(crate) fn parse_keymap(bytes: &[u8]) -> Result<Arc<Keymap>, KeymapError> {
    let file: KeybindingsFile = serde_json::from_slice(bytes)?;
    if file.version != 1 {
        return Err(KeymapError::unsupported_version(file.version));
    }
    let replacements = parse_replacements(file.bindings)?;
    Keymap::merge_with_built_ins(replacements).map(Arc::new)
}
```

Parse each sequence by `split_whitespace`, reject zero or more than four strokes, then validate the complete effective map.

- [ ] **Step 4: Implement deterministic conflict validation**

For each Global/Idle, Global/Running, and Global/Approval effective context:

1. build `(sequence, action)` rows;
2. reject identical sequences with different actions;
3. reject strict prefix relationships;
4. enforce non-textual intermediate strokes;
5. enforce Global stroke restrictions;
6. enforce cancel and Approval reservations.

Sort validation rows by canonical sequence and the action's non-`None`
`configurable_id` before reporting errors so tests and user notices are
deterministic.

- [ ] **Step 5: Add dynamic help descriptors**

Replace `SHORTCUT_HINTS` with descriptors that preserve the current order, labels, and legacy strings:

```rust
pub(crate) struct ShortcutDescriptor {
    pub(crate) scope: ShortcutScope,
    pub(crate) legacy_keys: &'static str,
    pub(crate) label: &'static str,
    pub(crate) actions: &'static [ShortcutAction],
}
```

Add:

```rust
impl Keymap {
    pub(crate) fn descriptor_keys(&self, descriptor: &ShortcutDescriptor) -> Option<String> {
        if self.actions_equal_built_ins(descriptor.actions) {
            return Some(descriptor.legacy_keys.to_string());
        }
        let keys = self.canonical_sequences_for(descriptor.actions);
        (!keys.is_empty()).then(|| keys.join(" / "))
    }
}
```

Tests must prove:

- unrelated customization does not alter any legacy descriptor string;
- disabling every referenced action omits the row;
- replacement and chord strings are canonical;
- the fixed approval help row stays `1/2/3/4` and `y/A/a/n`, while `d` remains functional.

- [ ] **Step 6: Run focused tests and verify GREEN**

Run:

```bash
cargo test -p orca-tui keybindings::config::tests -- --nocapture
cargo test -p orca-tui shortcuts::tests -- --nocapture
```

Expected: all parser, validation, help, and legacy tests pass.

- [ ] **Step 7: Commit**

```bash
git add crates/orca-tui/src/keybindings/config.rs crates/orca-tui/src/shortcuts.rs
git commit -m "feat(tui): parse custom keybinding files" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 3: Add the Deterministic Chord Runtime

**Files:**
- Create: `crates/orca-tui/src/keybindings/runtime.rs`
- Modify: `crates/orca-tui/src/keybindings/mod.rs`
- Modify: `crates/orca-tui/src/keybindings/config.rs`

- [ ] **Step 1: Write failing chord state-machine tests**

Add `ctrl`, `idle_owner`, and `runtime_with` test helpers that construct real
`KeyEvent`, `InputOwnerFingerprint`, and parsed `Keymap` values, then add:

```rust
#[test]
fn exact_chord_emits_once_and_resets() {
    let map = map_with("idle.submit", &["ctrl+x ctrl+s"]);
    let now = Instant::now();
    let owner = idle_owner();
    let mut runtime = KeymapRuntime::new(map);

    assert_eq!(
        runtime.resolve(owner, ctrl('x'), now),
        ShortcutResolution::Pending,
    );
    assert_eq!(
        runtime.resolve(owner, ctrl('s'), now + Duration::from_millis(10)),
        ShortcutResolution::Action(ShortcutInvocation::chord(
            ShortcutAction::Idle(IdleShortcut::Submit),
        )),
    );
    assert!(!runtime.has_pending_chord());
}

#[test]
fn mismatch_reroutes_current_key_once() {
    let mut runtime = runtime_with("idle.submit", &["ctrl+x ctrl+s"]);
    let now = Instant::now();
    assert_eq!(runtime.resolve(idle_owner(), ctrl('x'), now), ShortcutResolution::Pending);
    assert_eq!(
        runtime.resolve(idle_owner(), ctrl('f'), now + Duration::from_millis(1)),
        ShortcutResolution::RetryCurrentKey,
    );
    assert!(!runtime.has_pending_chord());
}

#[test]
fn cancel_clears_pending_before_action() {
    let mut runtime = runtime_with("idle.submit", &["ctrl+x ctrl+s"]);
    let now = Instant::now();
    runtime.resolve(idle_owner(), ctrl('x'), now);
    assert_eq!(
        runtime.resolve(idle_owner(), ctrl('c'), now + Duration::from_millis(1)),
        ShortcutResolution::Action(ShortcutInvocation::key(
            ShortcutAction::Global(GlobalShortcut::Cancel),
            ctrl('c'),
        )),
    );
    assert!(!runtime.has_pending_chord());
    assert_eq!(
        runtime.resolve(idle_owner(), ctrl('s'), now + Duration::from_millis(2)),
        ShortcutResolution::NoMatch,
    );
}
```

Add tests for:

- two-, three-, and four-stroke completion;
- accepted intermediate stroke resets the one-second deadline;
- expiry emits nothing and the next key is resolved normally;
- Release does not start, advance, mismatch, or clear;
- Repeat behaves like Press;
- owner fingerprint changes clear pending before routing;
- Vim mode, panel, search, slash, mention, approval, setup, picker, and shortcut-overlay changes alter the fingerprint;
- `clear_for_non_key`, `clear_for_suspend`, and keymap generation changes clear pending;
- `next_deadline` reports the current chord deadline.

- [ ] **Step 2: Run runtime tests and verify RED**

Run:

```bash
cargo test -p orca-tui keybindings::runtime::tests -- --nocapture
```

Expected: compilation fails because the runtime types do not exist.

- [ ] **Step 3: Implement invocation and owner types**

In `runtime.rs`:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvocationOrigin {
    Key(KeyEvent),
    Chord,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ShortcutInvocation {
    pub(crate) action: ShortcutAction,
    pub(crate) origin: InvocationOrigin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct InputOwnerFingerprint {
    pub(crate) context: ShortcutContext,
    pub(crate) modal: ModalOwner,
    pub(crate) panel: PanelMode,
    pub(crate) vim_mode: Option<VimMode>,
}
```

Use a small `ModalOwner` enum rather than booleans so only one owner is represented.

- [ ] **Step 4: Implement pending-candidate resolution**

```rust
pub(crate) enum ShortcutResolution {
    NoMatch,
    RetryCurrentKey,
    Pending,
    Action(ShortcutInvocation),
}

pub(crate) struct KeymapRuntime {
    keymap: Arc<Keymap>,
    generation: u64,
    pending: Option<PendingChord>,
}
```

Resolution order:

1. ignore Release;
2. clear on owner mismatch;
3. resolve immediate Global cancel and clear pending;
4. expire pending when `now > deadline`;
5. advance pending candidates;
6. on mismatch clear and return `RetryCurrentKey`;
7. without pending, match a complete single stroke or start candidates.

Do not read the clock internally.

- [ ] **Step 5: Run runtime and parser tests and verify GREEN**

Run:

```bash
cargo test -p orca-tui keybindings::runtime::tests -- --nocapture
cargo test -p orca-tui keybindings::config::tests -- --nocapture
```

Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add crates/orca-tui/src/keybindings/mod.rs \
  crates/orca-tui/src/keybindings/runtime.rs \
  crates/orca-tui/src/keybindings/config.rs
git commit -m "feat(tui): resolve sequential key chords" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 4: Add Bounded Eventual Hot Reload

**Files:**
- Create: `crates/orca-tui/src/keybindings/reload.rs`
- Modify: `crates/orca-tui/src/keybindings/mod.rs`
- Modify: `crates/orca-tui/src/keybindings/runtime.rs`

- [ ] **Step 1: Write failing path and loader tests**

Use `tempfile::TempDir` and the existing process-env lock:

```rust
#[test]
fn keybindings_path_uses_orca_home() {
    let _env = crate::test_support::lock_process_env();
    let home = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("ORCA_HOME", home.path()) };
    assert_eq!(keybindings_path().unwrap(), home.path().join("keybindings.json"));
    unsafe { std::env::remove_var("ORCA_HOME") };
}

#[test]
fn loader_reads_only_limit_plus_sentinel() {
    let file = write_bytes(MAX_KEYBINDINGS_BYTES + 1);
    assert!(matches!(
        load_observation(&file),
        FileObservation::Rejected(ref error) if error.contains("64 KiB"),
    ));
}

#[test]
fn symlink_and_special_file_are_rejected_before_read() {
    let directory = tempfile::tempdir().unwrap();
    assert!(matches!(
        load_observation(directory.path()),
        FileObservation::Rejected(ref error) if error.contains("regular file"),
    ));

    #[cfg(unix)]
    {
        let target = directory.path().join("target.json");
        let link = directory.path().join("keybindings.json");
        std::fs::write(&target, br#"{"version":1,"bindings":{}}"#).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(matches!(
            load_observation(&link),
            FileObservation::Rejected(ref error) if error.contains("symbolic link"),
        ));
    }
}
```

Add worker/runtime tests for:

- valid initial observation changes the map;
- missing initial observation keeps defaults;
- invalid initial observation yields one `Rejected` result and defaults;
- unchanged invalid bytes are reported once;
- in-place write, atomic rename, delete, and recreate are observed;
- deletion restores defaults;
- later valid bytes recover from rejection;
- requests coalesce while a load is in flight;
- `poll_request` is capped to 500ms;
- completed reload clears a pending chord;
- an injected loader closure blocked on a test channel never blocks
  input-loop progress or shutdown;
- worker Drop joins a completed worker but detaches an unfinished worker.

- [ ] **Step 2: Run reload tests and verify RED**

Run:

```bash
cargo test -p orca-tui keybindings::reload::tests -- --nocapture
```

Expected: compilation fails because reload types do not exist.

- [ ] **Step 3: Implement bounded observations**

Define:

```rust
const MAX_KEYBINDINGS_BYTES: usize = 64 * 1024;
const RELOAD_INTERVAL: Duration = Duration::from_millis(500);

enum FileObservation {
    Missing,
    Bytes(Vec<u8>),
    Rejected(String),
}

pub(crate) enum ReloadOutcome {
    Unchanged,
    Applied,
    RestoredDefaults,
    Rejected(String),
}
```

Use `symlink_metadata`, reject non-regular files, and on Unix open with `libc::O_NOFOLLOW` through `OpenOptionsExt::custom_flags`. Read with `.take((MAX_KEYBINDINGS_BYTES + 1) as u64)`.

- [ ] **Step 4: Implement the request-driven loader**

Use bounded channels:

```rust
pub(crate) struct KeymapReloader {
    request_tx: mpsc::Sender<()>,
    result_rx: mpsc::Receiver<FileObservation>,
    join: Option<JoinHandle<()>>,
    next_poll_at: Instant,
    last_observation: Option<ObservationFingerprint>,
}
```

Define the worker dependency explicitly:

```rust
type LoaderFn = Arc<dyn Fn(&Path) -> FileObservation + Send + Sync + 'static>;

impl KeymapReloader {
    pub(crate) fn start(path: PathBuf, now: Instant) -> Self {
        Self::start_with_loader(path, now, Arc::new(load_observation))
    }

    #[cfg(test)]
    fn start_with_loader(path: PathBuf, now: Instant, loader: LoaderFn) -> Self {
        // Spawn the request-driven worker with the injected loader.
    }
}
```

The worker blocks only on request reception and filesystem I/O. `request_reload` uses `try_send`; results use a capacity-one latest-value channel. The UI never waits for either.

Drop behavior:

- disconnect channels;
- if `join.is_finished()`, join and consume the result;
- otherwise drop the handle without joining.

- [ ] **Step 5: Integrate reload application into `KeymapRuntime`**

Add:

```rust
pub(crate) fn apply_observation(
    &mut self,
    observation: FileObservation,
) -> ReloadOutcome
```

It deduplicates observations, parses a complete candidate before swapping, increments generation, and clears pending only for Applied/RestoredDefaults.

- [ ] **Step 6: Run reload tests and verify GREEN**

Run:

```bash
cargo test -p orca-tui keybindings::reload::tests -- --nocapture
cargo test -p orca-tui keybindings::runtime::tests -- --nocapture
```

Expected: all tests pass without sleeping longer than the explicit 500ms polling test.

- [ ] **Step 7: Commit**

```bash
git add crates/orca-tui/src/keybindings/mod.rs \
  crates/orca-tui/src/keybindings/reload.rs \
  crates/orca-tui/src/keybindings/runtime.rs
git commit -m "feat(tui): hot reload custom keybindings" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 5: Split Resolution from Semantic Action Execution

**Files:**
- Modify: `crates/orca-tui/src/idle_key_actions.rs`
- Modify: `crates/orca-tui/src/idle_navigation_actions.rs`
- Modify: `crates/orca-tui/src/queued_input_actions.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`
- Modify: `crates/orca-tui/src/approval_dialog_actions.rs`
- Modify: `crates/orca-tui/src/key_event_actions.rs`

- [ ] **Step 1: Write failing action-only handler tests**

For every Idle action, invoke `ShortcutInvocation::chord(action)` with both a non-empty single-line composer and a multiline composer. Assert:

```rust
let before_text = textarea_text(&textarea);
let before_cursor = textarea.cursor();

handle_idle_shortcut_invocation(
    ShortcutInvocation::chord(action),
    &mut state,
    &mut config,
    &shared,
    &action_tx,
    &mut textarea,
    &mut vim,
    &theme,
);

if action == IdleShortcut::ExpandToolOutput && !before_text.trim().is_empty() {
    assert_eq!(textarea_text(&textarea), before_text);
    assert_eq!(textarea.cursor(), before_cursor);
}
assert_no_final_chord_character_was_inserted(&textarea);
```

Explicitly test:

- chord history actions navigate history even in multiline text;
- chord ScrollUp/ScrollDown scroll transcript rather than moving the textarea cursor;
- guarded ExpandToolOutput is a no-op when declined;
- Submit/Newline/EditLatestQueued preserve existing semantic behavior;
- Running chords execute or are rejected by Compacting's existing allowlist;
- Approval option direct keys resolve before configurable Approval actions;
- Approval action invocations move/toggle/confirm without a synthetic raw key;
- Global cancel first clears pending and then preserves interrupt/double-exit behavior.

- [ ] **Step 2: Run handler tests and verify RED**

Run:

```bash
cargo test -p orca-tui idle_key_actions::tests -- --nocapture
cargo test -p orca-tui queued_input_actions::tests -- --nocapture
cargo test -p orca-tui approval_dialog_actions::tests -- --nocapture
cargo test -p orca-tui status_key_actions::tests -- --nocapture
```

Expected: compilation fails because invocation-based handlers do not exist.

- [ ] **Step 3: Add action-only Idle execution**

Introduce:

```rust
pub(crate) fn handle_idle_shortcut_invocation(
    invocation: ShortcutInvocation,
    /* existing state/config arguments, but no synthetic Event */
) -> bool
```

For `InvocationOrigin::Key(event)`, preserve multiline Up/Down and guarded `e` fallback using the real event. For `InvocationOrigin::Chord`, execute the explicit semantics from the spec and never call `textarea.input`.

Keep menu and panel checks in `handle_idle_key` before asking the runtime to start a contextual chord.

- [ ] **Step 4: Add action-only Running and Approval execution**

Introduce:

```rust
pub(crate) fn handle_running_shortcut_invocation(
    invocation: ShortcutInvocation,
    /* existing arguments */
) -> bool

pub(crate) fn handle_approval_shortcut(
    shortcut: ApprovalShortcut,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
)
```

`handle_approval_dialog_key` must retain fixed `option_for_key` handling before asking the keymap runtime for configurable Approval bindings.

- [ ] **Step 5: Accept pre-resolved invocations in status/global dispatch**

Change status dispatch to accept an optional `ShortcutInvocation` produced by `app.rs`. Keep raw key paths only for modal/setup/picker/Vim behavior and fixed approval keys.

Do not create synthetic crossterm events for chord actions.

- [ ] **Step 6: Run handler tests and verify GREEN**

Run:

```bash
cargo test -p orca-tui idle_key_actions::tests -- --nocapture
cargo test -p orca-tui queued_input_actions::tests -- --nocapture
cargo test -p orca-tui approval_dialog_actions::tests -- --nocapture
cargo test -p orca-tui status_key_actions::tests -- --nocapture
cargo test -p orca-tui key_event_actions::tests -- --nocapture
```

Expected: all focused handler tests pass, including existing regression tests.

- [ ] **Step 7: Commit**

```bash
git add crates/orca-tui/src/idle_key_actions.rs \
  crates/orca-tui/src/idle_navigation_actions.rs \
  crates/orca-tui/src/queued_input_actions.rs \
  crates/orca-tui/src/status_key_actions.rs \
  crates/orca-tui/src/approval_dialog_actions.rs \
  crates/orca-tui/src/key_event_actions.rs
git commit -m "refactor(tui): dispatch resolved shortcut actions" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 6: Integrate Runtime Ownership, Reload, and Dynamic Help

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/key_event_actions.rs`
- Modify: `crates/orca-tui/src/idle_key_actions.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`
- Modify: `crates/orca-tui/src/queued_input_actions.rs`
- Modify: `crates/orca-tui/src/shortcuts.rs`

- [ ] **Step 1: Write failing app routing and reload integration tests**

Add an app-level harness with an injected `KeymapRuntime` and fixed time. Cover:

```rust
#[test]
fn first_key_uses_defaults_until_delayed_initial_map_arrives() {
    // Queue a blocked initial load.
    // Assert Enter submits through the built-in map.
    // Release a valid map replacing idle.submit with ctrl+s.
    // Assert Enter no longer submits and Ctrl+S does.
}

#[test]
fn menu_appearance_clears_pending_contextual_chord() {
    // Start ctrl+x in Idle, then install mention candidates before next input.
    // Assert the owner fingerprint change clears pending and the next key routes once.
}

#[test]
fn input_suspend_clears_pending_before_acknowledgement() {
    // Start a chord, deliver InputControl::Suspend, and inspect runtime state
    // before the acknowledgement is sent.
}
```

Also test:

- global chord begins in preflight;
- contextual chord begins only after slash, mention, and workflow handlers decline;
- mismatch `RetryCurrentKey` restarts routing exactly once;
- non-key input clears pending before existing handling;
- synthetic Enter clears pending;
- owner changes for status, panel, Vim mode, search, overlay, setup, picker, and approval;
- `receive_prioritized_input_or_control` wait is capped by `next_deadline`;
- expired chord is cleared at the next loop checkpoint without claiming a hard 16ms guarantee;
- valid reload marks dirty and updates open help atomically;
- invalid initial/later observations add one system notice and retain the prior map;
- blocked loader does not stall input, draw, cleanup, or process return.

- [ ] **Step 2: Write failing UI help tests**

Add snapshots/string assertions:

```rust
#[test]
fn built_in_keymap_keeps_all_current_help_strings() {
    let keymap = Keymap::built_in();
    assert_eq!(shortcut_text(&keymap, idle_state()), legacy_idle_shortcut_text());
    assert!(welcome_text(&keymap).contains(
        "Enter to send, Alt+Enter (or Shift+Enter) for newline"
    ));
    assert!(status_text(&keymap).contains("F1 shortcuts"));
}

#[test]
fn custom_map_updates_overlay_welcome_and_status_together() {
    let keymap = map_with_replacements(/* submit, newline, toggle shortcuts */);
    let combined = render_all_help_surfaces(&keymap);
    assert!(combined.contains("ctrl+s"));
    assert!(combined.contains("ctrl+x ctrl+k"));
    assert!(!combined.contains("F1 shortcuts"));
}
```

Test disabled actions and unrelated customizations retaining exact legacy descriptor strings.

- [ ] **Step 3: Run app/UI tests and verify RED**

Run:

```bash
cargo test -p orca-tui app::tests -- --nocapture
cargo test -p orca-tui ui::tests -- --nocapture
```

Expected: compilation or assertions fail because app/UI do not own a dynamic keymap.

- [ ] **Step 4: Initialize runtime and eventual reloader**

Before the initial frame:

```rust
let mut keymap_runtime = KeymapRuntime::built_in();
let mut keymap_reloader = KeymapReloader::start(keybindings_path(), Instant::now());
keymap_reloader.request_reload(Instant::now());
```

Pass `keymap_runtime.keymap()` to every render call.

At the top of each loop:

1. request reload when due;
2. drain at most one latest observation;
3. apply it;
4. push one System notice for `Rejected`;
5. mark the scheduler dirty for Applied/RestoredDefaults/Rejected.

- [ ] **Step 5: Integrate owner synchronization and resolution**

Add:

```rust
fn input_owner_fingerprint(
    state: &AppState,
    vim_state: &VimState,
) -> InputOwnerFingerprint
```

Before each key:

1. synchronize owner;
2. resolve Global cancel;
3. advance pending;
4. run global preflight;
5. let menus/panels run;
6. ask runtime to resolve/start contextual bindings;
7. dispatch an Action invocation or pass NoMatch to composer/Vim;
8. on RetryCurrentKey, restart once with no pending state.

Use an explicit two-pass enum/loop guard rather than recursive routing.

Clear pending before:

- paste;
- mouse;
- focus;
- resize;
- synthetic Enter;
- suspend acknowledgement;
- TUI exit.

Cap the receive timeout:

```rust
let timeout = keymap_runtime
    .next_deadline()
    .map(|deadline| deadline.saturating_duration_since(now))
    .map_or(frame_timeout, |chord_wait| frame_timeout.min(chord_wait));
```

- [ ] **Step 6: Make every visible configurable hint dynamic**

Change:

```rust
pub(crate) fn render(
    frame: &mut Frame,
    state: &mut AppState,
    textarea: &TextArea,
    theme: &Theme,
    keymap: &Keymap,
)
```

Thread `keymap` into:

- `render_shortcuts`;
- `build_welcome_lines`;
- status-line construction.

Use descriptor legacy strings only when referenced bindings equal defaults. Use canonical current strings otherwise. Use key-independent “shortcuts” when toggle help is disabled.

- [ ] **Step 7: Remove static resolution and run focused tests GREEN**

Delete production uses of:

```rust
resolve_shortcut
global_shortcut
idle_shortcut
running_shortcut
approval_shortcut
```

Retain only compatibility helpers needed by tests until their assertions have migrated, then remove dead functions and static-only hint types.

Run:

```bash
cargo test -p orca-tui app::tests -- --nocapture
cargo test -p orca-tui ui::tests -- --nocapture
cargo test -p orca-tui key_event_actions::tests -- --nocapture
cargo test -p orca-tui idle_key_actions::tests -- --nocapture
cargo test -p orca-tui status_key_actions::tests -- --nocapture
cargo test -p orca-tui queued_input_actions::tests -- --nocapture
cargo test -p orca-tui approval_dialog_actions::tests -- --nocapture
```

Expected: all focused integration and regression tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/orca-tui/src/app.rs \
  crates/orca-tui/src/ui.rs \
  crates/orca-tui/src/key_event_actions.rs \
  crates/orca-tui/src/idle_key_actions.rs \
  crates/orca-tui/src/status_key_actions.rs \
  crates/orca-tui/src/queued_input_actions.rs \
  crates/orca-tui/src/shortcuts.rs
git commit -m "feat(tui): integrate live custom keybindings" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

### Task 7: Documentation, Independent Review, and Full Verification

**Files:**
- Modify: `README.md`
- Modify: `README.zh-CN.md`

- [ ] **Step 1: Write failing documentation contract test**

If the repository has no documentation tests, add a focused Rust source assertion near keybindings config tests:

```rust
#[test]
fn readme_documents_keybindings_path_schema_and_reload() {
    let readme = include_str!("../../../../README.md");
    assert!(readme.contains("~/.orca/keybindings.json"));
    assert!(readme.contains("\"version\": 1"));
    assert!(readme.contains("reload"));
}
```

Add the equivalent assertion for `README.zh-CN.md`.

- [ ] **Step 2: Run the documentation test and verify RED**

Run:

```bash
cargo test -p orca-tui readme_documents_keybindings -- --nocapture
```

Expected: FAIL because the documentation does not mention the file.

- [ ] **Step 3: Document the user contract**

Document:

- path and `ORCA_HOME` override;
- schema and replacement semantics;
- action IDs or a pointer to a complete action table;
- one- to four-stroke syntax;
- context behavior;
- one-second chord timeout;
- fixed Approval keys and Global restrictions;
- eventual live reload and last-known-good rejection behavior;
- deletion restoring defaults;
- no project-local file.

- [ ] **Step 4: Run docs and focused crate tests GREEN**

Run:

```bash
cargo test -p orca-tui readme_documents_keybindings -- --nocapture
cargo test -p orca-tui keybindings -- --nocapture
cargo test -p orca-tui shortcuts -- --nocapture
```

Expected: all pass.

- [ ] **Step 5: Commit documentation**

```bash
git add README.md README.zh-CN.md
git commit -m "docs(tui): document custom keybindings" \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

- [ ] **Step 6: Run independent spec-compliance review**

Give a reviewer:

- `docs/superpowers/specs/2026-07-28-tui-keybindings-design.md`;
- this implementation plan;
- the full diff from the design commit;
- focused test results.

The review must check:

- every action ID and context;
- cancel and Approval reservations;
- no composer text loss or injected chord tail;
- menu/modal/Vim/status ownership fences;
- eventual reload and blocked-loader shutdown;
- last-known-good behavior;
- all visible help surfaces;
- default behavior parity.

Fix every Critical or Important finding with a new failing test before production changes.

- [ ] **Step 7: Run independent code-quality review**

Ask a different reviewer to inspect:

- parser strictness and duplicate JSON handling;
- deterministic errors;
- sequence conflict complexity;
- unsafe/Unix open behavior;
- channel bounds and thread lifecycle;
- event-loop fairness;
- stale pending state;
- tests that accidentally encode implementation details.

Again, fix Critical/Important findings through RED/GREEN.

- [ ] **Step 8: Run focused verification**

```bash
cargo test -p orca-tui keybindings -- --nocapture
cargo test -p orca-tui shortcuts -- --nocapture
cargo test -p orca-tui app::tests -- --nocapture
cargo test -p orca-tui ui::tests -- --nocapture
cargo test -p orca-tui idle_key_actions::tests -- --nocapture
cargo test -p orca-tui queued_input_actions::tests -- --nocapture
cargo test -p orca-tui approval_dialog_actions::tests -- --nocapture
cargo test -p orca-tui status_key_actions::tests -- --nocapture
```

Expected: all focused suites pass.

- [ ] **Step 9: Run crate and workspace verification**

```bash
cargo test -p orca-core
cargo test -p orca-tui
cargo test --workspace --all-targets
cargo check --workspace
cargo fmt --all -- --check
git diff --check
```

Expected: all pass. If one of the three previously identified unrelated flaky tests fails, capture its exact failure, verify its source blob is unchanged from `b11b0120bd29d314325886fed8409cf7da04f223`, and rerun only that exact test. Do not suppress any new keybinding-related failure.

- [ ] **Step 10: Verify commit history and trailers**

```bash
git status --short
git log --format='%H%n%B%n---' b11b0120bd29d314325886fed8409cf7da04f223..HEAD
```

Expected:

- worktree clean;
- every new commit has exactly one final
  `Co-authored-by: TRAE CLI <noreply@bytedance.com>` trailer;
- no unrelated file changes.

- [ ] **Step 11: Push and verify remote SHA**

```bash
git push origin feature/tui-syntax-highlighting
LOCAL_SHA=$(git rev-parse HEAD)
REMOTE_SHA=$(git ls-remote origin refs/heads/feature/tui-syntax-highlighting | awk '{print $1}')
test "$LOCAL_SHA" = "$REMOTE_SHA"
printf 'local=%s\nremote=%s\n' "$LOCAL_SHA" "$REMOTE_SHA"
```

Expected: local and remote SHA are identical.

## Completion Criteria

The sub-project is complete only when:

- no file keeps current default behavior and exact legacy help;
- valid replacements work in Global, Idle, Running, and Approval contexts;
- two- through four-stroke chords execute semantic actions without text loss;
- cancel remains immediate and clears pending state;
- fixed Approval keys retain their exact meanings;
- owner changes and non-key boundaries clear pending chords;
- valid reload applies atomically, invalid reload retains last-known-good, and deletion restores defaults;
- blocked reload I/O cannot block terminal cleanup or app return;
- overlay, welcome, and status guidance agree with the active map;
- both independent reviews approve;
- focused, crate, workspace, check, format, and diff gates pass;
- all commits have the exact co-author trailer once;
- the pushed branch SHA equals local HEAD.
