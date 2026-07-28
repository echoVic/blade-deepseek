# TUI Vim Command Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add bounded counts, atomic `dd`/`yy`/`gg`/`G`, unnamed and lowercase named registers, and Normal-mode dot repeat while preserving Orca's existing input-routing and textarea undo contracts.

**Architecture:** A new pure `vim_command` parser turns bounded multi-key prefixes into typed commands. `VimState` owns register values, executes typed commands exclusively through public `tui-textarea` APIs, and stores a nonrecursive `RepeatableChange`. Existing preflight/menu/shortcut layers cancel incomplete parser state whenever they consume input before the composer, without clearing registers or repeat history.

**Tech Stack:** Rust 2024, tui-textarea 0.7 public cursor/selection/yank/history APIs, crossterm key events, existing Orca TUI status/idle/running routing.

---

## Scope and Baseline

Implementation baseline:

```text
dbb8408 docs(tui): design vim command core
```

Design authority:

```text
docs/superpowers/specs/2026-07-28-tui-vim-command-core-design.md
```

Do not implement:

- general operator-plus-motion such as `dw`, `d$`, `yw`, or `c2w`;
- insert-session dot replay;
- uppercase append, numbered, black-hole, expression, macro, or clipboard registers;
- configurable `jj -> Esc`;
- keybindings configuration or shortcut-definition changes;
- app-state, runtime, transcript, history-file, renderer, manifest, or lockfile changes.

Every commit must end exactly once with:

```text
Co-authored-by: TRAE CLI <noreply@bytedance.com>
```

## File Map

### Create

- `crates/orca-tui/src/vim_command.rs`
  - bounded count/register/prefix parser;
  - typed command, motion, selector, and visual-command values;
  - parser unit tests.

### Modify

- `crates/orca-tui/src/lib.rs`
  - register `vim_command`.
- `crates/orca-tui/src/vim.rs`
  - register bank;
  - count and atomic line execution;
  - visual register integration;
  - repeat IR and replay;
  - parser/reset test helpers and behavior tests.
- `crates/orca-tui/src/key_event_actions.rs`
  - pass Vim state through preflight;
  - cancel pending parser state at global/search/panel consumption.
- `crates/orca-tui/src/status_key_actions.rs`
  - cancel parser state at setup/picker/approval/search routing;
  - routing regression tests.
- `crates/orca-tui/src/idle_key_actions.rs`
  - cancel parser state after menu/panel/idle-shortcut consumption.
- `crates/orca-tui/src/queued_input_actions.rs`
  - cancel parser state after running menu/shortcut consumption.
- `crates/orca-tui/src/app.rs`
  - pass `VimState` into preflight;
  - cancel at paste, mouse, synthetic-enter, and wheel boundaries;
  - update focused key-preflight tests.

No other production file may change.

---

### Task 1: Add the Pure Bounded Vim Command Parser

**Files:**
- Create: `crates/orca-tui/src/vim_command.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [ ] **Step 1: Register the module and write RED parser shape/tests**

Add beside `mod vim;`:

```rust
mod vim_command;
```

Create `vim_command.rs` with target types:

```rust
use tui_textarea::{Input, Key};

pub(crate) const MAX_VIM_COUNT: usize = 9_999;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimRegisterSelector {
    Unnamed,
    Named(u8),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimMotion {
    Back,
    Down,
    Up,
    Forward,
    WordForward,
    WordEnd,
    WordBack,
    LineHead,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimCommand {
    Motion {
        motion: VimMotion,
        count: usize,
    },
    DeleteChars {
        count: usize,
        register: VimRegisterSelector,
    },
    DeleteToEnd {
        register: VimRegisterSelector,
    },
    ChangeToEnd {
        register: VimRegisterSelector,
    },
    DeleteLines {
        count: usize,
        register: VimRegisterSelector,
    },
    YankLines {
        count: usize,
        register: VimRegisterSelector,
    },
    Paste {
        count: usize,
        register: VimRegisterSelector,
    },
    GotoLine {
        one_based: Option<usize>,
    },
    Repeat {
        count: usize,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimCommandResolution {
    Pending,
    Execute(VimCommand),
    Consumed,
    Unhandled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimVisualCommand {
    Yank(VimRegisterSelector),
    Delete(VimRegisterSelector),
    Change(VimRegisterSelector),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimVisualResolution {
    Pending,
    Execute(VimVisualCommand),
    Consumed,
    Unhandled,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum VimPendingPrefix {
    #[default]
    None,
    Register,
    DeleteLine,
    YankLine,
    Goto,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct VimCommandParser {
    count: Option<usize>,
    selected_register: Option<VimRegisterSelector>,
    pending: VimPendingPrefix,
}

impl VimCommandParser {
    pub(crate) fn resolve_normal(&mut self, _input: Input) -> VimCommandResolution {
        unimplemented!()
    }

    pub(crate) fn resolve_visual(&mut self, _input: Input) -> VimVisualResolution {
        unimplemented!()
    }

    pub(crate) fn reset(&mut self) {
        self.count = None;
        self.selected_register = None;
        self.pending = VimPendingPrefix::None;
    }

    #[cfg(test)]
    pub(crate) fn has_pending(&self) -> bool {
        self.count.is_some()
            || self.selected_register.is_some()
            || self.pending != VimPendingPrefix::None
    }
}
```

Add test helpers:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn plain(character: char) -> Input {
        Input {
            key: Key::Char(character),
            ctrl: false,
            alt: false,
            shift: character.is_ascii_uppercase(),
        }
    }

    fn modified(character: char) -> Input {
        Input {
            key: Key::Char(character),
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    fn resolve_sequence(sequence: &str) -> VimCommandResolution {
        let mut parser = VimCommandParser::default();
        let mut resolution = VimCommandResolution::Unhandled;
        for character in sequence.chars() {
            resolution = parser.resolve_normal(plain(character));
        }
        resolution
    }
```

Add exact command tests:

```rust
    #[test]
    fn parses_counts_line_commands_registers_and_repeat() {
        assert_eq!(
            resolve_sequence("3h"),
            VimCommandResolution::Execute(VimCommand::Motion {
                motion: VimMotion::Back,
                count: 3,
            })
        );
        assert_eq!(
            resolve_sequence("2dd"),
            VimCommandResolution::Execute(VimCommand::DeleteLines {
                count: 2,
                register: VimRegisterSelector::Unnamed,
            })
        );
        assert_eq!(
            resolve_sequence("2yy"),
            VimCommandResolution::Execute(VimCommand::YankLines {
                count: 2,
                register: VimRegisterSelector::Unnamed,
            })
        );
        assert_eq!(
            resolve_sequence("4gg"),
            VimCommandResolution::Execute(VimCommand::GotoLine {
                one_based: Some(4),
            })
        );
        assert_eq!(
            resolve_sequence("G"),
            VimCommandResolution::Execute(VimCommand::GotoLine { one_based: None })
        );
        assert_eq!(
            resolve_sequence("3G"),
            VimCommandResolution::Execute(VimCommand::GotoLine {
                one_based: Some(3),
            })
        );
        assert_eq!(
            resolve_sequence("\"add"),
            VimCommandResolution::Execute(VimCommand::DeleteLines {
                count: 1,
                register: VimRegisterSelector::Named(0),
            })
        );
        assert_eq!(
            resolve_sequence("\"ayy"),
            VimCommandResolution::Execute(VimCommand::YankLines {
                count: 1,
                register: VimRegisterSelector::Named(0),
            })
        );
        assert_eq!(
            resolve_sequence("\"ap"),
            VimCommandResolution::Execute(VimCommand::Paste {
                count: 1,
                register: VimRegisterSelector::Named(0),
            })
        );
        assert_eq!(
            resolve_sequence("\"\"p"),
            VimCommandResolution::Execute(VimCommand::Paste {
                count: 1,
                register: VimRegisterSelector::Unnamed,
            })
        );
        assert_eq!(
            resolve_sequence("3."),
            VimCommandResolution::Execute(VimCommand::Repeat { count: 3 })
        );
    }

    #[test]
    fn count_and_register_prefixes_work_in_either_order() {
        for sequence in ["2\"add", "\"a2dd"] {
            assert_eq!(
                resolve_sequence(sequence),
                VimCommandResolution::Execute(VimCommand::DeleteLines {
                    count: 2,
                    register: VimRegisterSelector::Named(0),
                }),
                "{sequence}"
            );
        }
    }

    #[test]
    fn bare_zero_is_line_head_and_zero_extends_an_existing_count() {
        assert_eq!(
            resolve_sequence("0"),
            VimCommandResolution::Execute(VimCommand::Motion {
                motion: VimMotion::LineHead,
                count: 1,
            })
        );
        assert_eq!(
            resolve_sequence("20x"),
            VimCommandResolution::Execute(VimCommand::DeleteChars {
                count: 20,
                register: VimRegisterSelector::Unnamed,
            })
        );
    }

    #[test]
    fn count_saturates_at_the_hard_limit() {
        assert_eq!(
            resolve_sequence("999999x"),
            VimCommandResolution::Execute(VimCommand::DeleteChars {
                count: MAX_VIM_COUNT,
                register: VimRegisterSelector::Unnamed,
            })
        );
    }
```

Add fail-closed tests:

```rust
    #[test]
    fn invalid_pending_continuations_are_consumed_and_reset() {
        for sequence in ["dx", "yx", "gx", "\"1", "d2"] {
            let mut parser = VimCommandParser::default();
            let mut resolution = VimCommandResolution::Unhandled;
            for character in sequence.chars() {
                resolution = parser.resolve_normal(plain(character));
            }
            assert_eq!(resolution, VimCommandResolution::Consumed, "{sequence}");
            assert!(!parser.has_pending(), "{sequence}");
        }
    }

    #[test]
    fn explicit_register_rejects_non_register_commands() {
        assert_eq!(resolve_sequence("\"ah"), VimCommandResolution::Consumed);
    }

    #[test]
    fn modified_input_clears_pending_and_remains_unhandled() {
        let mut parser = VimCommandParser::default();
        assert_eq!(
            parser.resolve_normal(plain('2')),
            VimCommandResolution::Pending
        );
        assert_eq!(
            parser.resolve_normal(modified('r')),
            VimCommandResolution::Unhandled
        );
        assert!(!parser.has_pending());
    }

    #[test]
    fn visual_parser_supports_only_register_prefix_and_visual_operations() {
        let mut parser = VimCommandParser::default();
        assert_eq!(
            parser.resolve_visual(plain('"')),
            VimVisualResolution::Pending
        );
        assert_eq!(
            parser.resolve_visual(plain('b')),
            VimVisualResolution::Pending
        );
        assert_eq!(
            parser.resolve_visual(plain('d')),
            VimVisualResolution::Execute(VimVisualCommand::Delete(
                VimRegisterSelector::Named(1)
            ))
        );
        assert!(!parser.has_pending());
    }
}
```

- [ ] **Step 2: Run parser RED**

Run:

```sh
cargo test -p orca-tui vim_command::tests:: --lib -- --test-threads=1
```

Expected: FAIL because parser resolution functions are not implemented.

- [ ] **Step 3: Implement bounded parser state transitions**

Add helper methods:

```rust
impl VimCommandParser {
    fn append_count(&mut self, digit: usize) {
        self.count = Some(
            self.count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit)
                .min(MAX_VIM_COUNT),
        );
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }

    fn take_register(&mut self) -> VimRegisterSelector {
        self.selected_register
            .take()
            .unwrap_or(VimRegisterSelector::Unnamed)
    }

    fn finish(&mut self, command: VimCommand) -> VimCommandResolution {
        self.pending = VimPendingPrefix::None;
        self.count = None;
        self.selected_register = None;
        VimCommandResolution::Execute(command)
    }

    fn consume(&mut self) -> VimCommandResolution {
        self.reset();
        VimCommandResolution::Consumed
    }

    fn accept_register(character: char) -> Option<VimRegisterSelector> {
        match character {
            '"' => Some(VimRegisterSelector::Unnamed),
            'a'..='z' => Some(VimRegisterSelector::Named(
                (character as u8).saturating_sub(b'a'),
            )),
            _ => None,
        }
    }
}
```

Implement `resolve_normal` in this exact order:

```rust
pub(crate) fn resolve_normal(&mut self, input: Input) -> VimCommandResolution {
    if input.ctrl || input.alt {
        self.reset();
        return VimCommandResolution::Unhandled;
    }
    let Key::Char(character) = input.key else {
        self.reset();
        return VimCommandResolution::Unhandled;
    };

    match self.pending {
        VimPendingPrefix::Register => {
            let Some(register) = Self::accept_register(character) else {
                return self.consume();
            };
            self.selected_register = Some(register);
            self.pending = VimPendingPrefix::None;
            return VimCommandResolution::Pending;
        }
        VimPendingPrefix::DeleteLine => {
            if character != 'd' {
                return self.consume();
            }
            let command = VimCommand::DeleteLines {
                count: self.take_count(),
                register: self.take_register(),
            };
            return self.finish(command);
        }
        VimPendingPrefix::YankLine => {
            if character != 'y' {
                return self.consume();
            }
            let command = VimCommand::YankLines {
                count: self.take_count(),
                register: self.take_register(),
            };
            return self.finish(command);
        }
        VimPendingPrefix::Goto => {
            if character != 'g' {
                return self.consume();
            }
            if self.selected_register.is_some() {
                return self.consume();
            }
            let one_based = Some(self.take_count());
            return self.finish(VimCommand::GotoLine { one_based });
        }
        VimPendingPrefix::None => {}
    }

    if character.is_ascii_digit() {
        let digit = character.to_digit(10).unwrap_or_default() as usize;
        if digit != 0 || self.count.is_some() {
            self.append_count(digit);
            return VimCommandResolution::Pending;
        }
    }

    match character {
        '"' => {
            self.pending = VimPendingPrefix::Register;
            VimCommandResolution::Pending
        }
        'd' => {
            self.pending = VimPendingPrefix::DeleteLine;
            VimCommandResolution::Pending
        }
        'y' => {
            self.pending = VimPendingPrefix::YankLine;
            VimCommandResolution::Pending
        }
        'g' if self.selected_register.is_none() => {
            self.pending = VimPendingPrefix::Goto;
            VimCommandResolution::Pending
        }
        _ if self.selected_register.is_some()
            && !matches!(character, 'x' | 'D' | 'C' | 'p') =>
        {
            self.consume()
        }
        'h' | 'j' | 'k' | 'l' | 'w' | 'e' | 'b' | '0' => {
            let motion = match character {
                'h' => VimMotion::Back,
                'j' => VimMotion::Down,
                'k' => VimMotion::Up,
                'l' => VimMotion::Forward,
                'w' => VimMotion::WordForward,
                'e' => VimMotion::WordEnd,
                'b' => VimMotion::WordBack,
                '0' => VimMotion::LineHead,
                _ => unreachable!(),
            };
            let count = self.take_count();
            self.finish(VimCommand::Motion { motion, count })
        }
        'x' => {
            let command = VimCommand::DeleteChars {
                count: self.take_count(),
                register: self.take_register(),
            };
            self.finish(command)
        }
        'D' => {
            self.count = None;
            let register = self.take_register();
            self.finish(VimCommand::DeleteToEnd { register })
        }
        'C' => {
            self.count = None;
            let register = self.take_register();
            self.finish(VimCommand::ChangeToEnd { register })
        }
        'p' => {
            let command = VimCommand::Paste {
                count: self.take_count(),
                register: self.take_register(),
            };
            self.finish(command)
        }
        'G' if self.selected_register.is_none() => {
            let one_based = self.count.take();
            self.finish(VimCommand::GotoLine { one_based })
        }
        '.' if self.selected_register.is_none() => {
            let count = self.take_count();
            self.finish(VimCommand::Repeat { count })
        }
        _ => {
            self.reset();
            VimCommandResolution::Unhandled
        }
    }
}
```

Implement `resolve_visual`:

```rust
pub(crate) fn resolve_visual(&mut self, input: Input) -> VimVisualResolution {
    if input.ctrl || input.alt {
        self.reset();
        return VimVisualResolution::Unhandled;
    }
    let Key::Char(character) = input.key else {
        self.reset();
        return VimVisualResolution::Unhandled;
    };

    if self.pending == VimPendingPrefix::Register {
        let Some(register) = Self::accept_register(character) else {
            self.reset();
            return VimVisualResolution::Consumed;
        };
        self.selected_register = Some(register);
        self.pending = VimPendingPrefix::None;
        return VimVisualResolution::Pending;
    }

    if character == '"' {
        self.reset();
        self.pending = VimPendingPrefix::Register;
        return VimVisualResolution::Pending;
    }

    let command = match character {
        'y' => Some(VimVisualCommand::Yank(self.take_register())),
        'd' => Some(VimVisualCommand::Delete(self.take_register())),
        'c' => Some(VimVisualCommand::Change(self.take_register())),
        _ => None,
    };
    if let Some(command) = command {
        self.reset();
        return VimVisualResolution::Execute(command);
    }
    if self.selected_register.is_some() {
        self.reset();
        VimVisualResolution::Consumed
    } else {
        VimVisualResolution::Unhandled
    }
}
```

- [ ] **Step 4: Run parser GREEN and formatter**

Run:

```sh
cargo test -p orca-tui vim_command::tests:: --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: parser tests PASS.

- [ ] **Step 5: Commit parser**

Run:

```sh
git add crates/orca-tui/src/lib.rs crates/orca-tui/src/vim_command.rs
git commit \
  -m "feat(tui): parse vim command prefixes" \
  -m "Add bounded count, register, line-operator, goto, paste, and repeat parsing with fail-closed prefix state." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

Verify the required trailer appears exactly once.

---

### Task 2: Add Registers, Counts, and Atomic Line Operations

**Files:**
- Modify: `crates/orca-tui/src/vim.rs`

- [ ] **Step 1: Write RED state/register/operation tests**

Import parser types in `vim.rs`:

```rust
use crate::vim_command::{
    VimCommand, VimCommandParser, VimCommandResolution, VimMotion, VimRegisterSelector,
    VimVisualCommand, VimVisualResolution,
};
```

Add target register types before `VimState`:

```rust
const MAX_VIM_PASTE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimRegisterKind {
    Characterwise,
    Linewise,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VimRegisterValue {
    text: String,
    kind: VimRegisterKind,
}

#[derive(Clone, Debug)]
struct VimRegisterBank {
    unnamed: Option<VimRegisterValue>,
    named: [Option<VimRegisterValue>; 26],
}

impl Default for VimRegisterBank {
    fn default() -> Self {
        Self {
            unnamed: None,
            named: std::array::from_fn(|_| None),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VimCommandOutcome {
    redraw: bool,
    text_changed: bool,
}
```

Add fields:

```rust
parser: VimCommandParser,
registers: VimRegisterBank,
last_change: Option<RepeatableChange>,
```

Declare the final repeat value shape now; Task 3 wires recording and replay:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
enum RepeatableChange {
    DeleteChars {
        count: usize,
        register: VimRegisterSelector,
    },
    DeleteToEnd {
        register: VimRegisterSelector,
    },
    DeleteLines {
        count: usize,
        register: VimRegisterSelector,
    },
    Paste {
        count: usize,
        register: VimRegisterSelector,
    },
}
```

Initialize all three in `VimState::new`.

Add test helpers:

```rust
#[cfg(test)]
impl VimState {
    pub(crate) fn register_for_test(
        &self,
        selector: VimRegisterSelector,
    ) -> Option<(&str, VimRegisterKind)> {
        self.registers
            .read(selector)
            .map(|value| (value.text.as_str(), value.kind))
    }

    pub(crate) fn has_pending_command_for_test(&self) -> bool {
        self.parser.has_pending()
    }

    pub(crate) fn named_register_for_test(
        &self,
        name: u8,
    ) -> Option<(&str, bool)> {
        self.registers
            .read(VimRegisterSelector::Named(name))
            .map(|value| (value.text.as_str(), value.kind == VimRegisterKind::Linewise))
    }
}
```

Add tests:

```rust
#[test]
fn counted_motions_and_goto_commands_land_on_exact_positions() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["zero one two", "one", "two", "three"]);

    for character in "3l".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }
    assert_eq!(textarea.cursor(), (0, 3));

    for character in "3G".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }
    assert_eq!(textarea.cursor(), (2, 0));

    for character in "gg".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }
    assert_eq!(textarea.cursor(), (0, 0));

    state.handle(input('G'), &mut textarea, &theme);
    assert_eq!(textarea.cursor().0, 3);
}

#[test]
fn bare_zero_moves_to_current_line_head_without_crossing_lines() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["first", "second"]);
    textarea.move_cursor(CursorMove::Down);
    textarea.move_cursor(CursorMove::Forward);
    textarea.move_cursor(CursorMove::Forward);

    state.handle(input('0'), &mut textarea, &theme);

    assert_eq!(textarea.cursor(), (1, 0));
}

#[test]
fn dd_deletes_whole_lines_as_one_undoable_change() {
    let theme = Theme::named(ThemeName::Dark);
    for (start_row, sequence, expected, expected_cursor, expected_yank) in [
        (0, "dd", vec!["one", "two"], (0, 0), "zero"),
        (1, "dd", vec!["zero", "two"], (1, 0), "one"),
        (1, "2dd", vec!["zero"], (0, 4), "one\ntwo"),
    ] {
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["zero", "one", "two"]);
        for _ in 0..start_row {
            textarea.move_cursor(CursorMove::Down);
        }
        for character in sequence.chars() {
            state.handle(input(character), &mut textarea, &theme);
        }
        assert_eq!(textarea.lines(), expected.as_slice(), "{sequence}");
        assert_eq!(textarea.cursor(), expected_cursor, "{sequence}");
        assert_eq!(
            state.register_for_test(VimRegisterSelector::Unnamed),
            Some((expected_yank, VimRegisterKind::Linewise)),
            "{sequence}"
        );
        assert!(textarea.undo(), "{sequence}");
        assert_eq!(textarea.lines(), &["zero", "one", "two"], "{sequence}");
    }
}

#[test]
fn dd_on_the_only_line_leaves_one_empty_line_and_is_undoable() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["only"]);

    for character in "dd".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }

    assert_eq!(textarea.lines(), &[""]);
    assert_eq!(
        state.register_for_test(VimRegisterSelector::Unnamed),
        Some(("only", VimRegisterKind::Linewise))
    );
    assert!(textarea.undo());
    assert_eq!(textarea.lines(), &["only"]);
}

#[test]
fn yy_and_named_registers_preserve_linewise_text() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["zero", "one", "two"]);

    for character in "\"a2yy".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }

    assert_eq!(textarea.lines(), &["zero", "one", "two"]);
    assert_eq!(
        state.register_for_test(VimRegisterSelector::Named(0)),
        Some(("zero\none", VimRegisterKind::Linewise))
    );
    assert_eq!(
        state.register_for_test(VimRegisterSelector::Unnamed),
        Some(("zero\none", VimRegisterKind::Linewise))
    );
}

#[test]
fn named_character_delete_and_paste_use_one_shot_register_selection() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["abcd"]);

    for character in "\"a2x".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }
    assert_eq!(textarea.lines(), &["cd"]);
    assert_eq!(
        state.register_for_test(VimRegisterSelector::Named(0)),
        Some(("ab", VimRegisterKind::Characterwise))
    );

    textarea.move_cursor(CursorMove::End);
    for character in "\"ap".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }
    assert_eq!(textarea.lines(), &["cdab"]);
    assert!(!state.has_pending_command_for_test());
    assert!(textarea.undo());
    assert_eq!(textarea.lines(), &["cd"]);
}

#[test]
fn named_delete_and_change_to_end_write_register_and_clear_count() {
    let theme = Theme::named(ThemeName::Dark);

    let mut delete_state = VimState::new(true);
    let mut delete_area = TextArea::from(["abcd"]);
    delete_area.move_cursor(CursorMove::Forward);
    for character in "2\"aD".chars() {
        delete_state.handle(input(character), &mut delete_area, &theme);
    }
    assert_eq!(delete_area.lines(), &["a"]);
    assert_eq!(
        delete_state.register_for_test(VimRegisterSelector::Named(0)),
        Some(("bcd", VimRegisterKind::Characterwise))
    );
    assert!(!delete_state.has_pending_command_for_test());

    let mut change_state = VimState::new(true);
    let mut change_area = TextArea::from(["wxyz"]);
    change_area.move_cursor(CursorMove::Forward);
    for character in "\"bC".chars() {
        change_state.handle(input(character), &mut change_area, &theme);
    }
    assert_eq!(change_area.lines(), &["w"]);
    assert_eq!(change_state.mode, VimMode::Insert);
    assert_eq!(
        change_state.register_for_test(VimRegisterSelector::Named(1)),
        Some(("xyz", VimRegisterKind::Characterwise))
    );

    let mut newline_state = VimState::new(true);
    let mut newline_area = TextArea::from(["left", "right"]);
    newline_area.move_cursor(CursorMove::End);
    for character in "\"cD".chars() {
        newline_state.handle(input(character), &mut newline_area, &theme);
    }
    assert_eq!(newline_area.lines(), &["leftright"]);
    assert_eq!(
        newline_state.register_for_test(VimRegisterSelector::Named(2)),
        Some(("\n", VimRegisterKind::Characterwise))
    );
}

#[test]
fn linewise_counted_paste_inserts_below_as_one_undoable_change() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["zero", "one"]);
    for character in "yy".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }

    for character in "2p".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }

    assert_eq!(textarea.lines(), &["zero", "zero", "zero", "one"]);
    assert!(textarea.undo());
    assert_eq!(textarea.lines(), &["zero", "one"]);
}

#[test]
fn counted_paste_above_one_mib_is_a_handled_noop() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    state.registers.write(
        VimRegisterSelector::Unnamed,
        VimRegisterValue {
            text: "x".repeat(1024),
            kind: VimRegisterKind::Characterwise,
        },
    );
    let mut textarea = TextArea::from(["keep"]);

    for character in "9999p".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }

    assert_eq!(textarea.lines(), &["keep"]);
    assert!(!textarea.undo());
}
```

Add visual named-register test:

```rust
#[test]
fn visual_yank_and_delete_write_selected_named_register() {
    let theme = Theme::named(ThemeName::Dark);
    let mut yank_state = VimState::new(true);
    let mut yank_area = TextArea::from(["abcd"]);
    yank_state.handle(input('v'), &mut yank_area, &theme);
    yank_state.handle(input('l'), &mut yank_area, &theme);
    for character in "\"by".chars() {
        yank_state.handle(input(character), &mut yank_area, &theme);
    }
    assert_eq!(
        yank_state.register_for_test(VimRegisterSelector::Named(1)),
        Some(("a", VimRegisterKind::Characterwise))
    );
    assert_eq!(yank_area.lines(), &["abcd"]);

    let mut delete_state = VimState::new(true);
    let mut delete_area = TextArea::from(["abcd"]);
    delete_state.handle(input('v'), &mut delete_area, &theme);
    delete_state.handle(input('l'), &mut delete_area, &theme);
    for character in "\"bd".chars() {
        delete_state.handle(input(character), &mut delete_area, &theme);
    }
    assert_eq!(delete_area.lines(), &["bcd"]);
    assert_eq!(
        delete_state.register_for_test(VimRegisterSelector::Named(1)),
        Some(("a", VimRegisterKind::Characterwise))
    );
}
```

- [ ] **Step 2: Run execution RED**

Run:

```sh
cargo test -p orca-tui vim::tests::counted_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::dd_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::yy_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::named_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::named_delete_and_change_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::linewise_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::counted_paste_above_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::visual_ --lib -- --test-threads=1
```

Expected: compile or assertion failures because `VimState` does not own parser/register execution.

- [ ] **Step 3: Implement the register bank**

Implement:

```rust
impl VimRegisterBank {
    fn read(&self, selector: VimRegisterSelector) -> Option<&VimRegisterValue> {
        match selector {
            VimRegisterSelector::Unnamed => self.unnamed.as_ref(),
            VimRegisterSelector::Named(index) => {
                self.named.get(index as usize).and_then(Option::as_ref)
            }
        }
    }

    fn write(&mut self, selector: VimRegisterSelector, value: VimRegisterValue) {
        self.unnamed = Some(value.clone());
        if let VimRegisterSelector::Named(index) = selector
            && let Some(slot) = self.named.get_mut(index as usize)
        {
            *slot = Some(value);
        }
    }
}
```

- [ ] **Step 4: Implement movement and exact range helpers**

Add:

```rust
fn move_to_line_head(textarea: &mut TextArea<'_>) {
    let (_, col) = textarea.cursor();
    for _ in 0..col {
        textarea.move_cursor(CursorMove::Back);
    }
}

fn move_to_line_end(textarea: &mut TextArea<'_>) {
    let (row, col) = textarea.cursor();
    let remaining = textarea.lines()[row].chars().count().saturating_sub(col);
    for _ in 0..remaining {
        textarea.move_cursor(CursorMove::Forward);
    }
}

fn move_to_row_head(textarea: &mut TextArea<'_>, row: usize) {
    textarea.move_cursor(CursorMove::Top);
    for _ in 0..row.min(textarea.lines().len().saturating_sub(1)) {
        textarea.move_cursor(CursorMove::Down);
    }
    move_to_line_head(textarea);
}

fn execute_motion(
    textarea: &mut TextArea<'_>,
    motion: VimMotion,
    count: usize,
    visual: bool,
) -> bool {
    if motion == VimMotion::LineHead {
        if visual && textarea.selection_range().is_none() {
            textarea.start_selection();
        }
        move_to_line_head(textarea);
        return true;
    }
    let movement = match motion {
        VimMotion::Back => CursorMove::Back,
        VimMotion::Down => CursorMove::Down,
        VimMotion::Up => CursorMove::Up,
        VimMotion::Forward => CursorMove::Forward,
        VimMotion::WordForward => CursorMove::WordForward,
        VimMotion::WordEnd => CursorMove::WordEnd,
        VimMotion::WordBack => CursorMove::WordBack,
        VimMotion::LineHead => unreachable!(),
    };
    for _ in 0..count {
        move_cursor(textarea, movement, visual);
    }
    true
}
```

- [ ] **Step 5: Implement atomic delete/yank helpers**

Add:

```rust
fn selected_line_text(textarea: &TextArea<'_>, count: usize) -> (usize, String) {
    let start = textarea.cursor().0;
    let available = textarea.lines().len().saturating_sub(start);
    let count = count.max(1).min(available);
    let text = textarea.lines()[start..start + count].join("\n");
    (count, text)
}

fn delete_chars(
    textarea: &mut TextArea<'_>,
    count: usize,
) -> Option<String> {
    let start = textarea.cursor();
    textarea.start_selection();
    for _ in 0..count {
        let before = textarea.cursor();
        textarea.move_cursor(CursorMove::Forward);
        if textarea.cursor() == before {
            break;
        }
    }
    if textarea.cursor() == start {
        textarea.cancel_selection();
        return None;
    }
    textarea.cut().then(|| textarea.yank_text())
}

fn delete_to_end(textarea: &mut TextArea<'_>) -> Option<String> {
    let (row, col) = textarea.cursor();
    let suffix = textarea.lines()[row].chars().skip(col).collect::<String>();
    let deleted = if !suffix.is_empty() {
        suffix
    } else if row + 1 < textarea.lines().len() {
        "\n".to_string()
    } else {
        return None;
    };
    textarea.delete_line_by_end().then_some(deleted)
}

fn delete_lines(
    textarea: &mut TextArea<'_>,
    count: usize,
) -> Option<String> {
    let start_row = textarea.cursor().0;
    let (count, register_text) = selected_line_text(textarea, count);
    let reaches_end = start_row + count == textarea.lines().len();

    if start_row == 0 && reaches_end {
        textarea.select_all();
    } else if reaches_end {
        move_to_row_head(textarea, start_row - 1);
        move_to_line_end(textarea);
        textarea.start_selection();
        textarea.move_cursor(CursorMove::Bottom);
        move_to_line_end(textarea);
    } else {
        move_to_row_head(textarea, start_row);
        textarea.start_selection();
        for _ in 0..count {
            textarea.move_cursor(CursorMove::Down);
        }
        move_to_line_head(textarea);
    }

    textarea.cut().then_some(register_text)
}

fn yank_lines(textarea: &TextArea<'_>, count: usize) -> String {
    selected_line_text(textarea, count).1
}
```

- [ ] **Step 6: Implement bounded paste construction**

Add:

```rust
fn repeat_text(text: &str, separator: &str, count: usize) -> Option<String> {
    let count = count.max(1).min(crate::vim_command::MAX_VIM_COUNT);
    let content_bytes = text.len().checked_mul(count)?;
    let separator_bytes = separator
        .len()
        .checked_mul(count.saturating_sub(1))?;
    let total = content_bytes.checked_add(separator_bytes)?;
    if total > MAX_VIM_PASTE_BYTES {
        return None;
    }
    Some(std::iter::repeat_n(text, count).collect::<Vec<_>>().join(separator))
}

fn paste_register(
    textarea: &mut TextArea<'_>,
    value: &VimRegisterValue,
    count: usize,
) -> bool {
    let payload = match value.kind {
        VimRegisterKind::Characterwise => repeat_text(&value.text, "", count),
        VimRegisterKind::Linewise => {
            let normalized = value.text.strip_suffix('\n').unwrap_or(&value.text);
            repeat_text(normalized, "\n", count).map(|text| format!("\n{text}"))
        }
    };
    let Some(payload) = payload.filter(|payload| !payload.is_empty()) else {
        return false;
    };
    if value.kind == VimRegisterKind::Linewise {
        move_to_line_end(textarea);
    }
    textarea.set_yank_text(payload);
    textarea.paste()
}
```

- [ ] **Step 7: Integrate parser and existing modes without repeat**

Initialize `parser`, `registers`, and `last_change` in `VimState::new`.

Add:

```rust
pub(crate) fn cancel_pending_command(&mut self) {
    self.parser.reset();
}
```

At the start of `reset_insert`, call `cancel_pending_command`.

At the start of Esc handling in Insert/Normal/Visual, call `cancel_pending_command`.

Refactor Normal mode:

```rust
fn handle_normal(&mut self, input: Input, textarea: &mut TextArea<'_>) -> VimCommandOutcome {
    match self.parser.resolve_normal(input) {
        VimCommandResolution::Pending | VimCommandResolution::Consumed => {
            return VimCommandOutcome::default();
        }
        VimCommandResolution::Execute(command) => {
            return self.execute_command(command, textarea);
        }
        VimCommandResolution::Unhandled => {}
    }

    let redraw = match input {
        Input {
            key: Key::Char('^'),
            ..
        } => move_cursor(textarea, CursorMove::Head, false),
        Input {
            key: Key::Char('$'),
            ..
        } => move_cursor(textarea, CursorMove::End, false),
        Input {
            key: Key::Char('i'),
            ..
        } => {
            textarea.cancel_selection();
            self.mode = VimMode::Insert;
            true
        }
        Input {
            key: Key::Char('a'),
            ..
        } => {
            textarea.cancel_selection();
            textarea.move_cursor(CursorMove::Forward);
            self.mode = VimMode::Insert;
            true
        }
        Input {
            key: Key::Char('A'),
            ..
        } => {
            textarea.cancel_selection();
            textarea.move_cursor(CursorMove::End);
            self.mode = VimMode::Insert;
            true
        }
        Input {
            key: Key::Char('o'),
            ..
        } => {
            textarea.cancel_selection();
            textarea.move_cursor(CursorMove::End);
            textarea.insert_newline();
            self.mode = VimMode::Insert;
            true
        }
        Input {
            key: Key::Char('O'),
            ..
        } => {
            textarea.cancel_selection();
            textarea.move_cursor(CursorMove::Head);
            textarea.insert_newline();
            textarea.move_cursor(CursorMove::Up);
            self.mode = VimMode::Insert;
            true
        }
        Input {
            key: Key::Char('v'),
            ..
        } => {
            textarea.start_selection();
            self.mode = VimMode::Visual;
            true
        }
        Input {
            key: Key::Char('u'),
            ctrl: false,
            ..
        } => textarea.undo(),
        Input {
            key: Key::Char('r'),
            ctrl: true,
            ..
        } => textarea.redo(),
        _ => false,
    };
    VimCommandOutcome {
        redraw,
        text_changed: false,
    }
}
```

Implement `execute_command` for every command except Repeat. Task 3 renames
this exact function to `execute_command_without_repeat` before adding the final
wrapper, so the completed code contains only one function with each name:

```rust
fn execute_command(
    &mut self,
    command: VimCommand,
    textarea: &mut TextArea<'_>,
) -> VimCommandOutcome {
    match command {
        VimCommand::Motion { motion, count } => VimCommandOutcome {
            redraw: execute_motion(textarea, motion, count, false),
            text_changed: false,
        },
        VimCommand::GotoLine { one_based } => {
            match one_based {
                Some(line) => move_to_row_head(textarea, line.saturating_sub(1)),
                None => textarea.move_cursor(CursorMove::Bottom),
            }
            VimCommandOutcome {
                redraw: true,
                text_changed: false,
            }
        }
        VimCommand::DeleteChars { count, register } => {
            let deleted = delete_chars(textarea, count);
            if let Some(text) = deleted {
                self.registers.write(
                    register,
                    VimRegisterValue {
                        text,
                        kind: VimRegisterKind::Characterwise,
                    },
                );
                VimCommandOutcome {
                    redraw: true,
                    text_changed: true,
                }
            } else {
                VimCommandOutcome::default()
            }
        }
        VimCommand::DeleteToEnd { register } => {
            let deleted = delete_to_end(textarea);
            if let Some(text) = deleted.as_ref() {
                self.registers.write(
                    register,
                    VimRegisterValue {
                        text: text.clone(),
                        kind: VimRegisterKind::Characterwise,
                    },
                );
            }
            VimCommandOutcome {
                redraw: deleted.is_some(),
                text_changed: deleted.is_some(),
            }
        }
        VimCommand::ChangeToEnd { register } => {
            let deleted = delete_to_end(textarea);
            if let Some(text) = deleted.as_ref() {
                self.registers.write(
                    register,
                    VimRegisterValue {
                        text: text.clone(),
                        kind: VimRegisterKind::Characterwise,
                    },
                );
            }
            textarea.cancel_selection();
            self.mode = VimMode::Insert;
            VimCommandOutcome {
                redraw: true,
                text_changed: deleted.is_some(),
            }
        }
        VimCommand::DeleteLines { count, register } => {
            let deleted = delete_lines(textarea, count);
            if let Some(text) = deleted {
                self.registers.write(
                    register,
                    VimRegisterValue {
                        text,
                        kind: VimRegisterKind::Linewise,
                    },
                );
                VimCommandOutcome {
                    redraw: true,
                    text_changed: true,
                }
            } else {
                VimCommandOutcome::default()
            }
        }
        VimCommand::YankLines { count, register } => {
            self.registers.write(
                register,
                VimRegisterValue {
                    text: yank_lines(textarea, count),
                    kind: VimRegisterKind::Linewise,
                },
            );
            VimCommandOutcome {
                redraw: true,
                text_changed: false,
            }
        }
        VimCommand::Paste { count, register } => {
            let changed = self
                .registers
                .read(register)
                .cloned()
                .is_some_and(|value| paste_register(textarea, &value, count));
            VimCommandOutcome {
                redraw: changed,
                text_changed: changed,
            }
        }
        VimCommand::Repeat { .. } => VimCommandOutcome::default(),
    }
}
```

Add a complete Visual handler:

```rust
fn handle_visual(
    &mut self,
    input: Input,
    textarea: &mut TextArea<'_>,
) -> VimCommandOutcome {
    match self.parser.resolve_visual(input) {
        VimVisualResolution::Pending | VimVisualResolution::Consumed => {
            return VimCommandOutcome::default();
        }
        VimVisualResolution::Execute(command) => {
            let (register, change, delete) = match command {
                VimVisualCommand::Yank(register) => (register, false, false),
                VimVisualCommand::Delete(register) => (register, false, true),
                VimVisualCommand::Change(register) => (register, true, true),
            };
            let changed = if delete {
                textarea.cut()
            } else {
                textarea.copy();
                false
            };
            let text = textarea.yank_text();
            if !text.is_empty() {
                self.registers.write(
                    register,
                    VimRegisterValue {
                        text,
                        kind: VimRegisterKind::Characterwise,
                    },
                );
            }
            textarea.cancel_selection();
            self.mode = if change {
                VimMode::Insert
            } else {
                VimMode::Normal
            };
            return VimCommandOutcome {
                redraw: true,
                text_changed: changed,
            };
        }
        VimVisualResolution::Unhandled => {}
    }

    let redraw = match input {
        Input {
            key: Key::Char('h'),
            ..
        } => move_cursor(textarea, CursorMove::Back, true),
        Input {
            key: Key::Char('j'),
            ..
        } => move_cursor(textarea, CursorMove::Down, true),
        Input {
            key: Key::Char('k'),
            ..
        } => move_cursor(textarea, CursorMove::Up, true),
        Input {
            key: Key::Char('l'),
            ..
        } => move_cursor(textarea, CursorMove::Forward, true),
        Input {
            key: Key::Char('w'),
            ..
        } => move_cursor(textarea, CursorMove::WordForward, true),
        Input {
            key: Key::Char('e'),
            ..
        } => move_cursor(textarea, CursorMove::WordEnd, true),
        Input {
            key: Key::Char('b'),
            ctrl: false,
            ..
        } => move_cursor(textarea, CursorMove::WordBack, true),
        Input {
            key: Key::Char('^'),
            ..
        } => move_cursor(textarea, CursorMove::Head, true),
        Input {
            key: Key::Char('$'),
            ..
        } => move_cursor(textarea, CursorMove::End, true),
        Input {
            key: Key::Char('v'),
            ..
        } => {
            textarea.cancel_selection();
            self.mode = VimMode::Normal;
            true
        }
        _ => false,
    };
    VimCommandOutcome {
        redraw,
        text_changed: false,
    }
}
```

Refactor `handle_command` to dispatch by mode:

```rust
fn handle_command(&mut self, input: Input, textarea: &mut TextArea<'_>) -> bool {
    if input.key == Key::Esc {
        self.cancel_pending_command();
        self.mode = VimMode::Normal;
        textarea.cancel_selection();
        return true;
    }
    let outcome = match self.mode {
        VimMode::Normal => self.handle_normal(input, textarea),
        VimMode::Visual => self.handle_visual(input, textarea),
        VimMode::Insert => unreachable!(),
    };
    outcome.redraw
}
```

Have `handle` return `outcome.redraw` and continue calling `configure_block`.

- [ ] **Step 8: Run execution GREEN and existing Vim regressions**

Run:

```sh
cargo test -p orca-tui vim::tests:: --lib -- --test-threads=1
cargo test -p orca-tui status_key_actions::tests::vim_ --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: new operation/register tests and all existing Vim tests PASS.

- [ ] **Step 9: Commit register and operation core**

Run:

```sh
git add crates/orca-tui/src/vim.rs
git commit \
  -m "feat(tui): execute counted vim commands" \
  -m "Add atomic line delete/yank, bounded counted motions and paste, and unnamed or lowercase named registers." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 3: Wire Nonrecursive Dot Repeat

**Files:**
- Modify: `crates/orca-tui/src/vim.rs`

- [ ] **Step 1: Write RED repeat-recording and replay tests**

Use the `RepeatableChange` enum declared in Task 2:

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
enum RepeatableChange {
    DeleteChars {
        count: usize,
        register: VimRegisterSelector,
    },
    DeleteToEnd {
        register: VimRegisterSelector,
    },
    DeleteLines {
        count: usize,
        register: VimRegisterSelector,
    },
    Paste {
        count: usize,
        register: VimRegisterSelector,
    },
}
```

Add tests:

```rust
#[test]
fn dot_repeats_x_and_counted_line_delete() {
    let theme = Theme::named(ThemeName::Dark);

    let mut x_state = VimState::new(true);
    let mut x_area = TextArea::from(["abcd"]);
    x_state.handle(input('x'), &mut x_area, &theme);
    x_state.handle(input('.'), &mut x_area, &theme);
    assert_eq!(x_area.lines(), &["cd"]);

    let mut dd_state = VimState::new(true);
    let mut dd_area = TextArea::from(["zero", "one", "two", "three", "four"]);
    for character in "2dd.".chars() {
        dd_state.handle(input(character), &mut dd_area, &theme);
    }
    assert_eq!(dd_area.lines(), &["four"]);
}

#[test]
fn count_before_dot_multiplies_the_stored_count_with_a_bound() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["abcdefgh"]);
    state.handle(input('x'), &mut textarea, &theme);
    for character in "3.".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }
    assert_eq!(textarea.lines(), &["efgh"]);
}

#[test]
fn failed_change_movement_yank_and_undo_do_not_replace_repeat() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["abcd"]);
    state.handle(input('x'), &mut textarea, &theme);
    for character in "yy".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }
    state.handle(input('l'), &mut textarea, &theme);
    move_to_line_end(&mut textarea);
    state.handle(input('x'), &mut textarea, &theme);
    textarea.move_cursor(CursorMove::Back);
    state.handle(input('.'), &mut textarea, &theme);
    assert_eq!(textarea.lines(), &["bc"]);

    state.handle(input('u'), &mut textarea, &theme);
    state.handle(input('.'), &mut textarea, &theme);
    assert_eq!(textarea.lines(), &["bc"]);
}

#[test]
fn named_paste_repeat_reads_the_registers_current_value() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["abc"]);
    for character in "\"ax".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }
    textarea.move_cursor(CursorMove::End);
    for character in "\"ap".chars() {
        state.handle(input(character), &mut textarea, &theme);
    }
    state.registers.write(
        VimRegisterSelector::Named(0),
        VimRegisterValue {
            text: "Z".to_string(),
            kind: VimRegisterKind::Characterwise,
        },
    );
    state.handle(input('.'), &mut textarea, &theme);
    assert_eq!(textarea.lines(), &["bcaZ"]);
}

#[test]
fn dot_without_a_previous_change_is_a_safe_noop() {
    let theme = Theme::named(ThemeName::Dark);
    let mut state = VimState::new(true);
    let mut textarea = TextArea::from(["abc"]);
    assert!(!state.handle(input('.'), &mut textarea, &theme));
    assert_eq!(textarea.lines(), &["abc"]);
}
```

- [ ] **Step 2: Run dot-repeat RED**

Run:

```sh
cargo test -p orca-tui vim::tests::dot_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::count_before_dot_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::failed_change_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::named_paste_repeat_ --lib -- --test-threads=1
```

Expected: FAIL because Repeat resolves to a no-op and successful changes do not
record `last_change`.

- [ ] **Step 3: Record only successful repeatable changes**

Add:

```rust
fn repeatable_change(command: &VimCommand) -> Option<RepeatableChange> {
    match *command {
        VimCommand::DeleteChars { count, register } => {
            Some(RepeatableChange::DeleteChars { count, register })
        }
        VimCommand::DeleteToEnd { register } => {
            Some(RepeatableChange::DeleteToEnd { register })
        }
        VimCommand::DeleteLines { count, register } => {
            Some(RepeatableChange::DeleteLines { count, register })
        }
        VimCommand::Paste { count, register } => {
            Some(RepeatableChange::Paste { count, register })
        }
        _ => None,
    }
}
```

Rename the Task 2 executor from `execute_command` to
`execute_command_without_repeat`. Add a thin wrapper:

```rust
fn execute_command(
    &mut self,
    command: VimCommand,
    textarea: &mut TextArea<'_>,
) -> VimCommandOutcome {
    if let VimCommand::Repeat { count } = command {
        return self.repeat_change(textarea, count);
    }
    let repeatable = repeatable_change(&command);
    let outcome = self.execute_command_without_repeat(command, textarea);
    if outcome.text_changed
        && let Some(change) = repeatable
    {
        self.last_change = Some(change);
    }
    outcome
}
```

Keep `ChangeToEnd`, `YankLines`, motions, and gotos nonrepeatable.

- [ ] **Step 4: Implement bounded nonrecursive replay**

Add:

```rust
fn multiplied_count(base: usize, multiplier: usize) -> usize {
    base.saturating_mul(multiplier).min(crate::vim_command::MAX_VIM_COUNT)
}

fn repeat_change(
    &mut self,
    textarea: &mut TextArea<'_>,
    multiplier: usize,
) -> VimCommandOutcome {
    let Some(change) = self.last_change.clone() else {
        return VimCommandOutcome::default();
    };
    match change {
        RepeatableChange::DeleteChars { count, register } => {
            self.execute_command_without_repeat(
                VimCommand::DeleteChars {
                    count: multiplied_count(count, multiplier),
                    register,
                },
                textarea,
            )
        }
        RepeatableChange::DeleteToEnd { register } => {
            let mut outcome = VimCommandOutcome::default();
            for _ in 0..multiplier.min(crate::vim_command::MAX_VIM_COUNT) {
                let next = self.execute_command_without_repeat(
                    VimCommand::DeleteToEnd { register },
                    textarea,
                );
                outcome.redraw |= next.redraw;
                outcome.text_changed |= next.text_changed;
                if !next.text_changed {
                    break;
                }
            }
            outcome
        }
        RepeatableChange::DeleteLines { count, register } => {
            self.execute_command_without_repeat(
                VimCommand::DeleteLines {
                    count: multiplied_count(count, multiplier),
                    register,
                },
                textarea,
            )
        }
        RepeatableChange::Paste { count, register } => {
            self.execute_command_without_repeat(
                VimCommand::Paste {
                    count: multiplied_count(count, multiplier),
                    register,
                },
                textarea,
            )
        }
    }
}
```

In `execute_command_without_repeat`, keep the `VimCommand::Repeat { .. }` arm as
`unreachable!("repeat is handled by execute_command")`. Replay calls only
`execute_command_without_repeat`, so it never updates `last_change`.

- [ ] **Step 5: Run dot-repeat GREEN and all Vim tests**

Run:

```sh
cargo test -p orca-tui vim::tests::dot_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::count_before_dot_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::failed_change_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests::named_paste_repeat_ --lib -- --test-threads=1
cargo test -p orca-tui vim::tests:: --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: all dot-repeat and existing Vim tests PASS.

- [ ] **Step 6: Commit dot repeat**

Run:

```sh
git add crates/orca-tui/src/vim.rs
git commit \
  -m "feat(tui): repeat atomic vim changes" \
  -m "Record only successful normal-mode mutations and replay bounded delete or paste commands without recursive state updates." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 4: Cancel Pending Vim Prefixes at Input Ownership Boundaries

**Files:**
- Modify: `crates/orca-tui/src/key_event_actions.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`
- Modify: `crates/orca-tui/src/idle_key_actions.rs`
- Modify: `crates/orca-tui/src/queued_input_actions.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Write RED parser-lifecycle test helpers**

In `vim.rs` test helper impl, add:

```rust
pub(crate) fn seed_pending_count_for_test(&mut self) {
    let _ = self.parser.resolve_normal(Input {
        key: Key::Char('2'),
        ctrl: false,
        alt: false,
        shift: false,
    });
}

pub(crate) fn set_named_register_for_test(&mut self, name: u8, text: &str) {
    self.registers.write(
        VimRegisterSelector::Named(name),
        VimRegisterValue {
            text: text.to_string(),
            kind: VimRegisterKind::Characterwise,
        },
    );
}

pub(crate) fn set_repeat_for_test(&mut self) {
    self.last_change = Some(RepeatableChange::DeleteChars {
        count: 1,
        register: VimRegisterSelector::Unnamed,
    });
}

pub(crate) fn has_repeat_for_test(&self) -> bool {
    self.last_change.is_some()
}
```

Add `key_event_actions.rs` test:

```rust
#[test]
fn global_and_search_preflight_clear_only_pending_vim_command_state() {
    let (action_tx, _action_rx) = mpsc::unbounded();
    let mut state = state_with_search_matches();
    let mut config = test_run_config();
    let shared = Arc::new(Mutex::new(config.clone()));
    let operation = TestOperationInterrupt::default();
    let mut vim = crate::vim::VimState::new(true);
    vim.seed_pending_count_for_test();
    vim.set_named_register_for_test(0, "saved");
    vim.set_repeat_for_test();

    handle_key_event_preflight(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &mut vim,
        || Ok(()),
    )
    .unwrap();

    assert!(!vim.has_pending_command_for_test());
    assert_eq!(
        vim.named_register_for_test(0),
        Some(("saved", false))
    );
    assert!(vim.has_repeat_for_test());
}
```

Update every existing preflight test call to pass a mutable `VimState`.

Add `status_key_actions.rs` test:

```rust
#[test]
fn vim_search_intent_clears_pending_prefix_before_opening_search() {
    let (action_tx, _action_rx) = mpsc::unbounded();
    let mut state = AppState::new(
        action_tx.clone(),
        "test".to_string(),
        "mock".to_string(),
        "/tmp".to_string(),
    );
    let mut config = config();
    config.vim_mode = true;
    let shared = Arc::new(Mutex::new(config.clone()));
    let operation = TestOperationInterrupt::default();
    let preloaded = Arc::new(Mutex::new(None));
    let mut textarea = TextArea::from(["draft"]);
    let mut vim = VimState::new(true);
    vim.seed_pending_count_for_test();
    let theme = Theme::named(ThemeName::Dark);
    let key = KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE);

    handle_status_key(
        &Event::Key(key),
        &key,
        &mut state,
        &mut config,
        &shared,
        &action_tx,
        &operation,
        &preloaded,
        &mut textarea,
        &mut vim,
        &theme,
        None,
        || Ok(()),
    )
    .unwrap();

    assert!(state.transcript_search.open);
    assert!(!vim.has_pending_command_for_test());
}
```

- [ ] **Step 2: Run routing RED**

Run:

```sh
cargo test -p orca-tui global_and_search_preflight_clear_only_pending --lib -- --test-threads=1
cargo test -p orca-tui vim_search_intent_clears_pending_prefix --lib -- --test-threads=1
```

Expected: compile failures because preflight lacks `VimState` and pending
cancellation is not wired.

- [ ] **Step 3: Pass Vim state through preflight and cancel consumed branches**

Import `VimState` in `key_event_actions.rs` and add before `clear_terminal`:

```rust
vim_state: &mut VimState,
```

For every consumed branch, call:

```rust
vim_state.cancel_pending_command();
```

immediately before returning `Continue` or `Exit`. Apply to:

- global Cancel;
- active transcript search;
- every other global shortcut;
- shortcuts overlay Esc;
- mouse-selection Esc;
- BackTab approval-mode cycle;
- workflows-panel Esc.

Do not cancel on release events or the final `Unhandled`.

Update all callers/tests.

- [ ] **Step 4: Cancel in status/idle/running key ownership**

In `status_key_actions.rs`, call `cancel_pending_command` before branches that
fully consume setup, picker, approval, or Vim search intent input.

In `idle_key_actions.rs`:

- after slash-menu handler returns true, cancel then return;
- after mention-menu handler returns true, cancel then return;
- after workflow-panel handler returns true, cancel then return;
- before every resolved Idle shortcut branch, cancel;
- do not cancel in `Some(_) | None` before `apply_composer_key_input`.

In `queued_input_actions.rs`:

- after mention handler returns true, cancel then return;
- before every resolved Running shortcut, cancel;
- do not cancel before `apply_composer_key_input`.

- [ ] **Step 5: Cancel paste/mouse/wheel boundaries in the app loop**

In `app.rs`:

- before handling a batched `ScrollLines`, call `vim_state.cancel_pending_command()`;
- when `handle_paste_event` returns true, cancel before return;
- on `MouseFlow::Handled`, cancel before return;
- on `MouseFlow::SyntheticEnter`, cancel before invoking status Enter.

Pass `&mut vim_state` to `handle_key_event_preflight`.

Add a production-source test:

```rust
#[test]
fn non_composer_input_boundaries_cancel_pending_vim_commands() {
    let production = include_str!("app.rs")
        .split("\n#[cfg(test)]\nmod tests {")
        .next()
        .expect("production app source");
    assert!(
        production.matches("vim_state.cancel_pending_command()").count() >= 4
    );
    assert!(production.contains(
        "handle_key_event_preflight(\n                                        *key,"
    ));
}
```

- [ ] **Step 6: Run routing GREEN and broad focused regressions**

Run:

```sh
cargo test -p orca-tui key_event_actions::tests:: --lib -- --test-threads=1
cargo test -p orca-tui status_key_actions::tests::vim_ --lib -- --test-threads=1
cargo test -p orca-tui idle_key_actions --lib -- --test-threads=1
cargo test -p orca-tui queued_input_actions --lib -- --test-threads=1
cargo test -p orca-tui non_composer_input_boundaries_cancel_pending --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: all routing, search, queue, and prefix-reset tests PASS.

- [ ] **Step 7: Commit routing boundaries**

Run:

```sh
git add \
  crates/orca-tui/src/key_event_actions.rs \
  crates/orca-tui/src/status_key_actions.rs \
  crates/orca-tui/src/idle_key_actions.rs \
  crates/orca-tui/src/queued_input_actions.rs \
  crates/orca-tui/src/app.rs
git commit \
  -m "fix(tui): fence pending vim commands" \
  -m "Clear incomplete count, operator, and register prefixes whenever higher-priority input routing consumes a key, paste, mouse action, or scroll." \
  -m "Co-authored-by: TRAE CLI <noreply@bytedance.com>"
```

---

### Task 5: Independent Review, Full Verification, and Push

**Files:**
- Verify: `crates/orca-tui/src/vim_command.rs`
- Verify: `crates/orca-tui/src/vim.rs`
- Verify: `crates/orca-tui/src/key_event_actions.rs`
- Verify: `crates/orca-tui/src/status_key_actions.rs`
- Verify: `crates/orca-tui/src/idle_key_actions.rs`
- Verify: `crates/orca-tui/src/queued_input_actions.rs`
- Verify: `crates/orca-tui/src/app.rs`
- Verify: `docs/superpowers/specs/2026-07-28-tui-vim-command-core-design.md`
- Verify: `docs/superpowers/plans/2026-07-28-tui-vim-command-core.md`

- [ ] **Step 1: Run focused acceptance tests**

Run:

```sh
cargo test -p orca-tui vim_command --lib -- --test-threads=1
cargo test -p orca-tui vim::tests:: --lib -- --test-threads=1
cargo test -p orca-tui status_key_actions::tests::vim_ --lib -- --test-threads=1
cargo test -p orca-tui key_event_actions::tests:: --lib -- --test-threads=1
```

Expected: all focused tests PASS.

- [ ] **Step 2: Run scope and manifest audits**

Run:

```sh
git diff --name-only dbb8408..HEAD
git diff --exit-code dbb8408..HEAD -- Cargo.toml Cargo.lock crates/orca-tui/Cargo.toml
git diff -U0 dbb8408..HEAD -- \
  crates/orca-tui/src \
  | rg '^\+.*(jj|keybindings|macro|clipboard register|numbered register|operator.motion)' \
  && exit 1 || true
```

Expected:

- only declared Rust files plus the plan changed;
- manifest/lock diff is empty;
- no excluded Vim/config feature leaked.

- [ ] **Step 3: Request independent spec and quality reviews**

Spec review must verify all ten acceptance criteria and exact parser/register/
repeat/routing semantics.

Quality review must inspect:

- parser state reset and count saturation;
- explicit-vs-default unnamed register behavior;
- one-edit undo for `dd`, counted `x`, and counted paste;
- EOF line deletion and large composers;
- 1 MiB paste bound and overflow safety;
- repeat nonrecursion and failed-change retention;
- upper-layer routing cancellation completeness;
- compatibility of `/ n N`, Enter, Esc, mention, queue, undo/redo, and
  Vim-disabled input.

Resolve every Important/Critical issue with a new RED/GREEN cycle and focused
reverification.

- [ ] **Step 4: Run package gates on committed HEAD**

Run:

```sh
cargo test -p orca-tui -- --test-threads=1
cargo check -p orca-tui
cargo fmt --all -- --check
git diff --check
test -z "$(git status --porcelain=v1 -uall)"
```

Expected: all `orca-tui` tests PASS and the worktree is clean.

- [ ] **Step 5: Audit every sub-project commit trailer**

Run:

```sh
git log --format='%H' dbb8408..HEAD | while read -r commit; do
  test "$(git show -s --format=%B "$commit" | grep -Fxc 'Co-authored-by: TRAE CLI <noreply@bytedance.com>')" -eq 1
  test "$(git show -s --format=%B "$commit" | tail -n 2 | head -n 1)" = 'Co-authored-by: TRAE CLI <noreply@bytedance.com>'
done
```

Expected: exit 0.

- [ ] **Step 6: Run the workspace all-target gate**

Run:

```sh
cargo test --workspace --all-targets -- --test-threads=1
```

Expected: PASS.

If either exact unchanged macOS timing test fails:

```text
external::tests::external_tool_timeout_kills_descendant_processes
external::tests::external_tool_timeout_preserves_observed_exit_code
```

prove the source blob matches the pushed baseline, then rerun with only those
two exact tests skipped.

- [ ] **Step 7: Push and verify remote SHA**

Run:

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

Expected: remote SHA exactly equals local `HEAD`.

Do not create a release, tag, PR, or worktree cleanup. Continue immediately to
the separate configurable insert-mode `jj -> Esc` design.
