use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShortcutScope {
    Global,
    Idle,
    Running,
    Approval,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ShortcutHint {
    pub scope: ShortcutScope,
    pub keys: &'static str,
    pub action: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyBinding {
    key: KeyCode,
    modifiers: KeyModifiers,
}

impl KeyBinding {
    pub const fn new(key: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { key, modifiers }
    }

    pub fn is_press(&self, event: KeyEvent) -> bool {
        if !matches!(event.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return false;
        }

        normalize_key_parts(self.key, self.modifiers)
            == normalize_key_parts(event.code, event.modifiers)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GlobalShortcut {
    Cancel,
    ToggleShortcuts,
    ScrollBottom,
    ScrollTop,
    ClearScreen,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdleShortcut {
    Submit,
    Newline,
    HistoryPrevious,
    HistoryNext,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    Quit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunningShortcut {
    Interrupt,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalShortcut {
    SelectAllow,
    SelectDeny,
    ToggleSelection,
    Confirm,
    Approve,
    Deny,
}

pub fn global_shortcut(event: KeyEvent) -> Option<GlobalShortcut> {
    let bindings = [
        (
            GlobalShortcut::Cancel,
            KeyBinding::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        ),
        (
            GlobalShortcut::ToggleShortcuts,
            KeyBinding::new(KeyCode::F(1), KeyModifiers::NONE),
        ),
        (
            GlobalShortcut::ToggleShortcuts,
            KeyBinding::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        ),
        (
            GlobalShortcut::ScrollBottom,
            KeyBinding::new(KeyCode::End, KeyModifiers::CONTROL),
        ),
        (
            GlobalShortcut::ScrollTop,
            KeyBinding::new(KeyCode::Home, KeyModifiers::CONTROL),
        ),
        (
            GlobalShortcut::ClearScreen,
            KeyBinding::new(KeyCode::Char('l'), KeyModifiers::CONTROL),
        ),
    ];
    match_binding(event, &bindings)
}

pub fn idle_shortcut(event: KeyEvent) -> Option<IdleShortcut> {
    let bindings = [
        (
            IdleShortcut::Submit,
            KeyBinding::new(KeyCode::Enter, KeyModifiers::NONE),
        ),
        (
            IdleShortcut::Newline,
            KeyBinding::new(KeyCode::Enter, KeyModifiers::SHIFT),
        ),
        (
            IdleShortcut::Newline,
            KeyBinding::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ),
        (
            IdleShortcut::HistoryPrevious,
            KeyBinding::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        ),
        (
            IdleShortcut::HistoryNext,
            KeyBinding::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        ),
        (
            IdleShortcut::ScrollUp,
            KeyBinding::new(KeyCode::Up, KeyModifiers::NONE),
        ),
        (
            IdleShortcut::ScrollDown,
            KeyBinding::new(KeyCode::Down, KeyModifiers::NONE),
        ),
        (
            IdleShortcut::PageUp,
            KeyBinding::new(KeyCode::PageUp, KeyModifiers::NONE),
        ),
        (
            IdleShortcut::PageDown,
            KeyBinding::new(KeyCode::PageDown, KeyModifiers::NONE),
        ),
        (
            IdleShortcut::HalfPageUp,
            KeyBinding::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        ),
        (
            IdleShortcut::HalfPageDown,
            KeyBinding::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        ),
        (
            IdleShortcut::Quit,
            KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE),
        ),
    ];
    match_binding(event, &bindings)
}

pub fn running_shortcut(event: KeyEvent) -> Option<RunningShortcut> {
    let bindings = [
        (
            RunningShortcut::Interrupt,
            KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE),
        ),
        (
            RunningShortcut::Interrupt,
            KeyBinding::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        ),
        (
            RunningShortcut::ScrollUp,
            KeyBinding::new(KeyCode::Up, KeyModifiers::NONE),
        ),
        (
            RunningShortcut::ScrollDown,
            KeyBinding::new(KeyCode::Down, KeyModifiers::NONE),
        ),
        (
            RunningShortcut::PageUp,
            KeyBinding::new(KeyCode::PageUp, KeyModifiers::NONE),
        ),
        (
            RunningShortcut::PageDown,
            KeyBinding::new(KeyCode::PageDown, KeyModifiers::NONE),
        ),
        (
            RunningShortcut::HalfPageUp,
            KeyBinding::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
        ),
        (
            RunningShortcut::HalfPageDown,
            KeyBinding::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        ),
    ];
    match_binding(event, &bindings)
}

pub fn approval_shortcut(event: KeyEvent) -> Option<ApprovalShortcut> {
    let bindings = [
        (
            ApprovalShortcut::SelectAllow,
            KeyBinding::new(KeyCode::Up, KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::SelectAllow,
            KeyBinding::new(KeyCode::Char('k'), KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::SelectDeny,
            KeyBinding::new(KeyCode::Down, KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::SelectDeny,
            KeyBinding::new(KeyCode::Char('j'), KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::ToggleSelection,
            KeyBinding::new(KeyCode::Tab, KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::ToggleSelection,
            KeyBinding::new(KeyCode::BackTab, KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::ToggleSelection,
            KeyBinding::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        ),
        (
            ApprovalShortcut::Confirm,
            KeyBinding::new(KeyCode::Enter, KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::Approve,
            KeyBinding::new(KeyCode::Char('y'), KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::Approve,
            KeyBinding::new(KeyCode::Char('a'), KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::Deny,
            KeyBinding::new(KeyCode::Char('n'), KeyModifiers::NONE),
        ),
        (
            ApprovalShortcut::Deny,
            KeyBinding::new(KeyCode::Char('d'), KeyModifiers::NONE),
        ),
    ];
    match_binding(event, &bindings)
}

pub fn shortcut_lines(scopes: &[ShortcutScope]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let sections = [
        (ShortcutScope::Global, "Global"),
        (ShortcutScope::Idle, "Composer"),
        (ShortcutScope::Running, "Running"),
        (ShortcutScope::Approval, "Approval"),
    ];

    for (section_scope, title) in sections {
        if !scopes.is_empty() && !scopes.contains(&section_scope) {
            continue;
        }

        if !lines.is_empty() {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            title,
            Style::default().fg(Color::Cyan),
        )));
        for hint in SHORTCUT_HINTS
            .iter()
            .filter(|hint| hint.scope == section_scope)
        {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {:<18}", hint.keys),
                    Style::default().fg(Color::Yellow),
                ),
                Span::styled(hint.action, Style::default().fg(Color::White)),
            ]));
        }
    }

    lines
}

pub const SHORTCUT_HINTS: &[ShortcutHint] = &[
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "F1 / ctrl+k",
        action: "show or hide shortcuts",
    },
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "ctrl+c",
        action: "cancel and quit",
    },
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "ctrl+home/end",
        action: "jump to top or bottom",
    },
    ShortcutHint {
        scope: ShortcutScope::Global,
        keys: "ctrl+l",
        action: "clear screen",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "enter",
        action: "send message",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "shift+enter / ctrl+j",
        action: "insert newline",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "ctrl+p / ctrl+n",
        action: "previous or next prompt",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "up/down",
        action: "scroll one line",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "pgup/pgdn",
        action: "scroll one page",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "ctrl+u / ctrl+d",
        action: "scroll half page",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "esc",
        action: "quit",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "esc / ctrl+g",
        action: "interrupt current turn",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "up/down",
        action: "scroll one line",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "pgup/pgdn",
        action: "scroll one page",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "ctrl+u / ctrl+d",
        action: "scroll half page",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "up/down/j/k",
        action: "move selection",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "tab",
        action: "toggle selection",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "enter",
        action: "confirm selected action",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "y/a",
        action: "allow",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "n/d",
        action: "deny",
    },
];

fn match_binding<T: Copy>(event: KeyEvent, bindings: &[(T, KeyBinding)]) -> Option<T> {
    bindings
        .iter()
        .find(|(_, binding)| binding.is_press(event))
        .map(|(action, _)| *action)
}

fn normalize_key_parts(key: KeyCode, mut modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let KeyCode::Char(ch) = key else {
        return (key, modifiers);
    };

    if modifiers.is_empty() {
        if let Some(ctrl_char) = c0_control_char_to_ctrl_char(ch) {
            return (KeyCode::Char(ctrl_char), KeyModifiers::CONTROL);
        }
    }

    if ch.is_ascii_uppercase() {
        modifiers.insert(KeyModifiers::SHIFT);
        return (KeyCode::Char(ch.to_ascii_lowercase()), modifiers);
    }

    (key, modifiers)
}

fn c0_control_char_to_ctrl_char(ch: char) -> Option<char> {
    let code = u32::from(ch);
    match code {
        0x00 => Some(' '),
        0x01..=0x1a => char::from_u32(code - 0x01 + u32::from('a')),
        0x1c..=0x1f => char::from_u32(code - 0x1c + u32::from('4')),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn control_binding_matches_raw_c0_characters() {
        let binding = KeyBinding::new(KeyCode::Char('j'), KeyModifiers::CONTROL);

        assert!(binding.is_press(key(KeyCode::Char('\n'), KeyModifiers::NONE)));
    }

    #[test]
    fn shifted_binding_matches_uppercase_characters() {
        let binding = KeyBinding::new(KeyCode::Char('a'), KeyModifiers::SHIFT);

        assert!(binding.is_press(key(KeyCode::Char('A'), KeyModifiers::NONE)));
        assert!(binding.is_press(key(KeyCode::Char('A'), KeyModifiers::SHIFT)));
    }

    #[test]
    fn release_events_do_not_trigger_shortcuts() {
        let binding = KeyBinding::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..key(KeyCode::Char('c'), KeyModifiers::CONTROL)
        };

        assert!(!binding.is_press(release));
    }

    #[test]
    fn idle_shortcuts_resolve_history_navigation() {
        assert_eq!(
            idle_shortcut(key(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(IdleShortcut::HistoryPrevious)
        );
        assert_eq!(
            idle_shortcut(key(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Some(IdleShortcut::HistoryNext)
        );
    }
}
