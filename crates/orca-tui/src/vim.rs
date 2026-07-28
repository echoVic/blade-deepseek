use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders};
use tui_textarea::{CursorMove, Input, Key, TextArea};

use crate::theme::Theme;
use crate::vim_command::{
    VimCommand, VimCommandParser, VimCommandResolution, VimMotion, VimRegisterSelector,
    VimVisualCommand, VimVisualResolution,
};

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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct VimCommandOutcome {
    redraw: bool,
    text_changed: bool,
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VimTranscriptSearchIntent {
    Open,
    Next,
    Previous,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VimMode {
    Insert,
    Normal,
    Visual,
}

#[derive(Clone, Debug)]
pub struct VimState {
    pub enabled: bool,
    pub mode: VimMode,
    parser: VimCommandParser,
    registers: VimRegisterBank,
    last_change: Option<RepeatableChange>,
}

impl VimState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            mode: if enabled {
                VimMode::Normal
            } else {
                VimMode::Insert
            },
            parser: VimCommandParser::default(),
            registers: VimRegisterBank::default(),
            last_change: None,
        }
    }

    pub fn title(&self) -> &'static str {
        if !self.enabled {
            " Input "
        } else {
            match self.mode {
                VimMode::Insert => " Input [vi insert] ",
                VimMode::Normal => " Input [vi normal] ",
                VimMode::Visual => " Input [vi visual] ",
            }
        }
    }

    pub fn configure_block(&self, textarea: &mut TextArea<'_>, theme: &Theme) {
        textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title(self.title())
                .border_style(Style::default().fg(theme.border)),
        );
        let cursor_color = match self.mode {
            VimMode::Insert => theme.border,
            VimMode::Normal => theme.warning,
            VimMode::Visual => theme.approval,
        };
        textarea.set_cursor_style(
            Style::default()
                .fg(cursor_color)
                .add_modifier(Modifier::REVERSED),
        );
    }

    pub(crate) fn transcript_search_intent(
        &self,
        key: crossterm::event::KeyCode,
    ) -> Option<VimTranscriptSearchIntent> {
        if !self.enabled || self.mode != VimMode::Normal {
            return None;
        }
        match key {
            crossterm::event::KeyCode::Char('/') => Some(VimTranscriptSearchIntent::Open),
            crossterm::event::KeyCode::Char('n') => Some(VimTranscriptSearchIntent::Next),
            crossterm::event::KeyCode::Char('N') => Some(VimTranscriptSearchIntent::Previous),
            _ => None,
        }
    }

    pub fn reset_insert(&mut self, textarea: &mut TextArea<'_>, theme: &Theme) {
        self.cancel_pending_command();
        self.mode = if self.enabled {
            VimMode::Normal
        } else {
            VimMode::Insert
        };
        textarea.cancel_selection();
        self.configure_block(textarea, theme);
    }

    pub fn handle(&mut self, input: Input, textarea: &mut TextArea<'_>, theme: &Theme) -> bool {
        if !self.enabled {
            return textarea.input(input);
        }

        let changed = match self.mode {
            VimMode::Insert => self.handle_insert(input, textarea),
            VimMode::Normal | VimMode::Visual => self.handle_command(input, textarea),
        };
        self.configure_block(textarea, theme);
        changed
    }

    pub(crate) fn cancel_pending_command(&mut self) {
        self.parser.reset();
    }

    fn handle_insert(&mut self, input: Input, textarea: &mut TextArea<'_>) -> bool {
        if input.key == Key::Esc {
            self.cancel_pending_command();
            self.mode = VimMode::Normal;
            textarea.cancel_selection();
            return true;
        }
        textarea.input(input)
    }

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

    fn handle_normal(&mut self, input: Input, textarea: &mut TextArea<'_>) -> VimCommandOutcome {
        match self.parser.resolve_normal(input.clone()) {
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

    fn handle_visual(&mut self, input: Input, textarea: &mut TextArea<'_>) -> VimCommandOutcome {
        match self.parser.resolve_visual(input.clone()) {
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
}

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

fn selected_line_text(textarea: &TextArea<'_>, count: usize) -> (usize, String) {
    let start = textarea.cursor().0;
    let available = textarea.lines().len().saturating_sub(start);
    let count = count.max(1).min(available);
    let text = textarea.lines()[start..start + count].join("\n");
    (count, text)
}

fn delete_chars(textarea: &mut TextArea<'_>, count: usize) -> Option<String> {
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

fn delete_lines(textarea: &mut TextArea<'_>, count: usize) -> Option<String> {
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

fn repeat_text(text: &str, separator: &str, count: usize) -> Option<String> {
    let count = count.max(1).min(crate::vim_command::MAX_VIM_COUNT);
    let content_bytes = text.len().checked_mul(count)?;
    let separator_bytes = separator.len().checked_mul(count.saturating_sub(1))?;
    let total = content_bytes.checked_add(separator_bytes)?;
    if total > MAX_VIM_PASTE_BYTES {
        return None;
    }
    Some(
        std::iter::repeat_n(text, count)
            .collect::<Vec<_>>()
            .join(separator),
    )
}

fn paste_register(textarea: &mut TextArea<'_>, value: &VimRegisterValue, count: usize) -> bool {
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

fn move_cursor(textarea: &mut TextArea<'_>, movement: CursorMove, visual: bool) -> bool {
    if visual && textarea.selection_range().is_none() {
        textarea.start_selection();
    }
    if visual {
        textarea.move_cursor(movement);
    } else {
        textarea.move_cursor(movement);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::ThemeName;

    impl VimState {
        fn register_for_test(
            &self,
            selector: VimRegisterSelector,
        ) -> Option<(&str, VimRegisterKind)> {
            let value = match selector {
                VimRegisterSelector::Unnamed => self.registers.unnamed.as_ref(),
                VimRegisterSelector::Named(index) => self
                    .registers
                    .named
                    .get(index as usize)
                    .and_then(Option::as_ref),
            }?;
            Some((value.text.as_str(), value.kind))
        }

        fn has_pending_command_for_test(&self) -> bool {
            self.parser.has_pending()
        }
    }

    fn input(ch: char) -> Input {
        Input {
            key: Key::Char(ch),
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    fn handle_sequence(
        state: &mut VimState,
        textarea: &mut TextArea<'_>,
        theme: &Theme,
        sequence: &str,
    ) {
        for character in sequence.chars() {
            state.handle(input(character), textarea, theme);
        }
    }

    #[test]
    fn counted_motions_and_goto_commands_land_on_exact_positions() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(["zero one two", "one", "two", "three"]);

        handle_sequence(&mut state, &mut textarea, &theme, "3l");
        assert_eq!(textarea.cursor(), (0, 3));

        handle_sequence(&mut state, &mut textarea, &theme, "3G");
        assert_eq!(textarea.cursor(), (2, 0));

        handle_sequence(&mut state, &mut textarea, &theme, "gg");
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
            handle_sequence(&mut state, &mut textarea, &theme, sequence);
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

        handle_sequence(&mut state, &mut textarea, &theme, "dd");

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

        handle_sequence(&mut state, &mut textarea, &theme, "\"a2yy");

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

        handle_sequence(&mut state, &mut textarea, &theme, "\"a2x");
        assert_eq!(textarea.lines(), &["cd"]);
        assert_eq!(
            state.register_for_test(VimRegisterSelector::Named(0)),
            Some(("ab", VimRegisterKind::Characterwise))
        );

        textarea.move_cursor(CursorMove::End);
        handle_sequence(&mut state, &mut textarea, &theme, "\"ap");
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
        handle_sequence(&mut delete_state, &mut delete_area, &theme, "2\"aD");
        assert_eq!(delete_area.lines(), &["a"]);
        assert_eq!(
            delete_state.register_for_test(VimRegisterSelector::Named(0)),
            Some(("bcd", VimRegisterKind::Characterwise))
        );
        assert!(!delete_state.has_pending_command_for_test());

        let mut change_state = VimState::new(true);
        let mut change_area = TextArea::from(["wxyz"]);
        change_area.move_cursor(CursorMove::Forward);
        handle_sequence(&mut change_state, &mut change_area, &theme, "\"bC");
        assert_eq!(change_area.lines(), &["w"]);
        assert_eq!(change_state.mode, VimMode::Insert);
        assert_eq!(
            change_state.register_for_test(VimRegisterSelector::Named(1)),
            Some(("xyz", VimRegisterKind::Characterwise))
        );

        let mut newline_state = VimState::new(true);
        let mut newline_area = TextArea::from(["left", "right"]);
        newline_area.move_cursor(CursorMove::End);
        handle_sequence(&mut newline_state, &mut newline_area, &theme, "\"cD");
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
        handle_sequence(&mut state, &mut textarea, &theme, "yy");

        handle_sequence(&mut state, &mut textarea, &theme, "2p");

        assert_eq!(textarea.lines(), &["zero", "zero", "zero", "one"]);
        assert!(textarea.undo());
        assert_eq!(textarea.lines(), &["zero", "one"]);
    }

    #[test]
    fn counted_paste_above_one_mib_is_a_handled_noop() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        state.registers.unnamed = Some(VimRegisterValue {
            text: "x".repeat(1024),
            kind: VimRegisterKind::Characterwise,
        });
        let mut textarea = TextArea::from(["keep"]);

        handle_sequence(&mut state, &mut textarea, &theme, "9999p");

        assert_eq!(textarea.lines(), &["keep"]);
        assert!(!textarea.undo());
    }

    #[test]
    fn visual_yank_and_delete_write_selected_named_register() {
        let theme = Theme::named(ThemeName::Dark);
        let mut yank_state = VimState::new(true);
        let mut yank_area = TextArea::from(["abcd"]);
        yank_state.handle(input('v'), &mut yank_area, &theme);
        yank_state.handle(input('l'), &mut yank_area, &theme);
        handle_sequence(&mut yank_state, &mut yank_area, &theme, "\"by");
        assert_eq!(
            yank_state.register_for_test(VimRegisterSelector::Named(1)),
            Some(("a", VimRegisterKind::Characterwise))
        );
        assert_eq!(yank_area.lines(), &["abcd"]);

        let mut delete_state = VimState::new(true);
        let mut delete_area = TextArea::from(["abcd"]);
        delete_state.handle(input('v'), &mut delete_area, &theme);
        delete_state.handle(input('l'), &mut delete_area, &theme);
        handle_sequence(&mut delete_state, &mut delete_area, &theme, "\"bd");
        assert_eq!(delete_area.lines(), &["bcd"]);
        assert_eq!(
            delete_state.register_for_test(VimRegisterSelector::Named(1)),
            Some(("a", VimRegisterKind::Characterwise))
        );
    }

    #[test]
    fn vi_insert_esc_returns_to_normal() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::default();
        state.handle(input('i'), &mut textarea, &theme);
        assert_eq!(state.mode, VimMode::Insert);
        state.handle(
            Input {
                key: Key::Esc,
                ctrl: false,
                alt: false,
                shift: false,
            },
            &mut textarea,
            &theme,
        );
        assert_eq!(state.mode, VimMode::Normal);
    }

    #[test]
    fn vi_normal_x_deletes_character() {
        let theme = Theme::named(ThemeName::Dark);
        let mut state = VimState::new(true);
        let mut textarea = TextArea::from(vec!["abc".to_string()]);
        state.handle(input('x'), &mut textarea, &theme);
        assert_eq!(textarea.lines(), &["bc".to_string()]);
    }

    #[test]
    fn vim_normal_mode_resolves_transcript_search_intents() {
        let state = VimState::new(true);
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('/')),
            Some(VimTranscriptSearchIntent::Open)
        );
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('n')),
            Some(VimTranscriptSearchIntent::Next)
        );
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('N')),
            Some(VimTranscriptSearchIntent::Previous)
        );
    }

    #[test]
    fn vim_insert_and_visual_modes_do_not_resolve_search_intents() {
        let mut state = VimState::new(true);
        state.mode = VimMode::Insert;
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('/')),
            None
        );
        state.mode = VimMode::Visual;
        assert_eq!(
            state.transcript_search_intent(crossterm::event::KeyCode::Char('n')),
            None
        );
    }
}
