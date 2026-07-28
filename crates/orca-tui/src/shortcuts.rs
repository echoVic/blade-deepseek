use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
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
pub struct ResolvedShortcutHint {
    pub scope: ShortcutScope,
    pub keys: &'static str,
    pub action: &'static str,
    pub has_registered_binding: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShortcutContext {
    Global,
    Idle,
    Running,
    Approval,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ShortcutAction {
    Global(GlobalShortcut),
    Idle(IdleShortcut),
    Running(RunningShortcut),
    Approval(ApprovalShortcut),
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum GlobalShortcut {
    Cancel,
    OpenTranscriptSearch,
    ToggleShortcuts,
    ScrollBottom,
    ScrollTop,
    ClearScreen,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum IdleShortcut {
    Submit,
    Newline,
    EditLatestQueued,
    HistoryPrevious,
    HistoryNext,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    Backtrack,
    ExpandToolOutput,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RunningShortcut {
    BackgroundCurrentTurn,
    Interrupt,
    SubmitQueued,
    Newline,
    EditLatestQueued,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ApprovalShortcut {
    SelectAllow,
    SelectDeny,
    ToggleSelection,
    Confirm,
    Approve,
    Deny,
}

const GLOBAL_BINDINGS: &[(GlobalShortcut, KeyBinding)] = &[
    (
        GlobalShortcut::Cancel,
        KeyBinding::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ),
    (
        GlobalShortcut::OpenTranscriptSearch,
        KeyBinding::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
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

const IDLE_BINDINGS: &[(IdleShortcut, KeyBinding)] = &[
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
        KeyBinding::new(KeyCode::Enter, KeyModifiers::ALT),
    ),
    (
        IdleShortcut::Newline,
        KeyBinding::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    ),
    (
        IdleShortcut::EditLatestQueued,
        KeyBinding::new(KeyCode::Up, KeyModifiers::ALT),
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
        IdleShortcut::HistoryPrevious,
        KeyBinding::new(KeyCode::Up, KeyModifiers::NONE),
    ),
    (
        IdleShortcut::HistoryNext,
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
        IdleShortcut::Backtrack,
        KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE),
    ),
    (
        IdleShortcut::ExpandToolOutput,
        KeyBinding::new(KeyCode::Char('e'), KeyModifiers::NONE),
    ),
];

const RUNNING_BINDINGS: &[(RunningShortcut, KeyBinding)] = &[
    (
        RunningShortcut::BackgroundCurrentTurn,
        KeyBinding::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
    ),
    (
        RunningShortcut::Interrupt,
        KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE),
    ),
    (
        RunningShortcut::Interrupt,
        KeyBinding::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
    ),
    (
        RunningShortcut::SubmitQueued,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::NONE),
    ),
    (
        RunningShortcut::Newline,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::SHIFT),
    ),
    (
        RunningShortcut::Newline,
        KeyBinding::new(KeyCode::Enter, KeyModifiers::ALT),
    ),
    (
        RunningShortcut::Newline,
        KeyBinding::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
    ),
    (
        RunningShortcut::EditLatestQueued,
        KeyBinding::new(KeyCode::Up, KeyModifiers::ALT),
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

const APPROVAL_BINDINGS: &[(ApprovalShortcut, KeyBinding)] = &[
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LegacyBinding {
    pub(crate) context: ShortcutContext,
    pub(crate) action: ShortcutAction,
    pub(crate) key: KeyCode,
    pub(crate) modifiers: KeyModifiers,
}

impl LegacyBinding {
    const fn new(context: ShortcutContext, action: ShortcutAction, binding: KeyBinding) -> Self {
        Self {
            context,
            action,
            key: binding.key,
            modifiers: binding.modifiers,
        }
    }

    pub(crate) const fn as_key_event(self) -> KeyEvent {
        KeyEvent::new(self.key, self.modifiers)
    }
}

pub(crate) fn configurable_legacy_bindings() -> impl Iterator<Item = LegacyBinding> {
    GLOBAL_BINDINGS
        .iter()
        .map(|(action, binding)| {
            LegacyBinding::new(
                ShortcutContext::Global,
                ShortcutAction::Global(*action),
                *binding,
            )
        })
        .chain(IDLE_BINDINGS.iter().map(|(action, binding)| {
            LegacyBinding::new(
                ShortcutContext::Idle,
                ShortcutAction::Idle(*action),
                *binding,
            )
        }))
        .chain(RUNNING_BINDINGS.iter().map(|(action, binding)| {
            LegacyBinding::new(
                ShortcutContext::Running,
                ShortcutAction::Running(*action),
                *binding,
            )
        }))
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
                .map(|(action, binding)| {
                    LegacyBinding::new(
                        ShortcutContext::Approval,
                        ShortcutAction::Approval(*action),
                        *binding,
                    )
                }),
        )
}

impl ShortcutAction {
    pub(crate) const fn configurable_id(self) -> Option<&'static str> {
        Some(match self {
            Self::Global(GlobalShortcut::Cancel) => "global.cancel",
            Self::Global(GlobalShortcut::OpenTranscriptSearch) => "global.open-transcript-search",
            Self::Global(GlobalShortcut::ToggleShortcuts) => "global.toggle-shortcuts",
            Self::Global(GlobalShortcut::ScrollBottom) => "global.scroll-bottom",
            Self::Global(GlobalShortcut::ScrollTop) => "global.scroll-top",
            Self::Global(GlobalShortcut::ClearScreen) => "global.clear-screen",
            Self::Idle(IdleShortcut::Submit) => "idle.submit",
            Self::Idle(IdleShortcut::Newline) => "idle.newline",
            Self::Idle(IdleShortcut::EditLatestQueued) => "idle.edit-latest-queued",
            Self::Idle(IdleShortcut::HistoryPrevious) => "idle.history-previous",
            Self::Idle(IdleShortcut::HistoryNext) => "idle.history-next",
            Self::Idle(IdleShortcut::ScrollUp) => "idle.scroll-up",
            Self::Idle(IdleShortcut::ScrollDown) => "idle.scroll-down",
            Self::Idle(IdleShortcut::PageUp) => "idle.page-up",
            Self::Idle(IdleShortcut::PageDown) => "idle.page-down",
            Self::Idle(IdleShortcut::HalfPageUp) => "idle.half-page-up",
            Self::Idle(IdleShortcut::HalfPageDown) => "idle.half-page-down",
            Self::Idle(IdleShortcut::Backtrack) => "idle.backtrack",
            Self::Idle(IdleShortcut::ExpandToolOutput) => "idle.expand-tool-output",
            Self::Running(RunningShortcut::BackgroundCurrentTurn) => {
                "running.background-current-turn"
            }
            Self::Running(RunningShortcut::Interrupt) => "running.interrupt",
            Self::Running(RunningShortcut::SubmitQueued) => "running.submit-queued",
            Self::Running(RunningShortcut::Newline) => "running.newline",
            Self::Running(RunningShortcut::EditLatestQueued) => "running.edit-latest-queued",
            Self::Running(RunningShortcut::ScrollUp) => "running.scroll-up",
            Self::Running(RunningShortcut::ScrollDown) => "running.scroll-down",
            Self::Running(RunningShortcut::PageUp) => "running.page-up",
            Self::Running(RunningShortcut::PageDown) => "running.page-down",
            Self::Running(RunningShortcut::HalfPageUp) => "running.half-page-up",
            Self::Running(RunningShortcut::HalfPageDown) => "running.half-page-down",
            Self::Approval(ApprovalShortcut::SelectAllow) => "approval.select-allow",
            Self::Approval(ApprovalShortcut::SelectDeny) => "approval.select-deny",
            Self::Approval(ApprovalShortcut::ToggleSelection) => "approval.toggle-selection",
            Self::Approval(ApprovalShortcut::Confirm) => "approval.confirm",
            Self::Approval(ApprovalShortcut::Approve | ApprovalShortcut::Deny) => return None,
        })
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
    configurable_legacy_bindings()
        .map(|binding| binding.action)
        .find(|action| action.configurable_id() == Some(id))
}

pub fn resolve_shortcut(context: ShortcutContext, event: KeyEvent) -> Option<ShortcutAction> {
    if let Some(shortcut) = global_shortcut(event) {
        return Some(ShortcutAction::Global(shortcut));
    }

    match context {
        ShortcutContext::Global => None,
        ShortcutContext::Idle => idle_shortcut(event).map(ShortcutAction::Idle),
        ShortcutContext::Running => running_shortcut(event).map(ShortcutAction::Running),
        ShortcutContext::Approval => approval_shortcut(event).map(ShortcutAction::Approval),
    }
}

pub fn global_shortcut(event: KeyEvent) -> Option<GlobalShortcut> {
    match_binding(event, GLOBAL_BINDINGS)
}

pub fn idle_shortcut(event: KeyEvent) -> Option<IdleShortcut> {
    match_binding(event, IDLE_BINDINGS)
}

pub fn running_shortcut(event: KeyEvent) -> Option<RunningShortcut> {
    match_binding(event, RUNNING_BINDINGS)
}

pub fn approval_shortcut(event: KeyEvent) -> Option<ApprovalShortcut> {
    match_binding(event, APPROVAL_BINDINGS)
}

pub fn shortcut_hints() -> impl Iterator<Item = ResolvedShortcutHint> {
    SHORTCUT_HINTS.iter().map(|hint| ResolvedShortcutHint {
        scope: hint.scope,
        keys: hint.keys,
        action: hint.action,
        has_registered_binding: scope_has_registered_binding(hint.scope),
    })
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
        for hint in shortcut_hints().filter(|hint| hint.scope == section_scope) {
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
        keys: "ctrl+f",
        action: "find in transcript",
    },
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
        scope: ShortcutScope::Global,
        keys: "shift+tab",
        action: "cycle approval mode",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "enter",
        action: "send message",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "alt+enter / shift+enter",
        action: "insert newline",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "alt+up",
        action: "edit latest queued message",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "up/down / ctrl+p/ctrl+n",
        action: "previous or next prompt",
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
        action: "backtrack previous prompt",
    },
    ShortcutHint {
        scope: ShortcutScope::Idle,
        keys: "e",
        action: "expand latest tool output",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "ctrl+b",
        action: "background current turn",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "esc / ctrl+g",
        action: "interrupt current turn",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "enter",
        action: "queue follow-up",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "alt+enter / shift+enter",
        action: "insert newline",
    },
    ShortcutHint {
        scope: ShortcutScope::Running,
        keys: "alt+up",
        action: "edit latest queued message",
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
        keys: "1/2/3",
        action: "allow options",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "4",
        action: "deny",
    },
    ShortcutHint {
        scope: ShortcutScope::Approval,
        keys: "y/A/a/n",
        action: "legacy direct keys",
    },
];

#[derive(Clone, Copy, Debug)]
pub(crate) struct ShortcutDescriptor {
    pub(crate) scope: ShortcutScope,
    pub(crate) legacy_keys: &'static str,
    pub(crate) label: &'static str,
    pub(crate) actions: &'static [ShortcutAction],
}

const OPEN_SEARCH: &[ShortcutAction] =
    &[ShortcutAction::Global(GlobalShortcut::OpenTranscriptSearch)];
const TOGGLE_SHORTCUTS: &[ShortcutAction] =
    &[ShortcutAction::Global(GlobalShortcut::ToggleShortcuts)];
const CANCEL: &[ShortcutAction] = &[ShortcutAction::Global(GlobalShortcut::Cancel)];
const SCROLL_EDGES: &[ShortcutAction] = &[
    ShortcutAction::Global(GlobalShortcut::ScrollTop),
    ShortcutAction::Global(GlobalShortcut::ScrollBottom),
];
const CLEAR_SCREEN: &[ShortcutAction] = &[ShortcutAction::Global(GlobalShortcut::ClearScreen)];
const IDLE_SUBMIT: &[ShortcutAction] = &[ShortcutAction::Idle(IdleShortcut::Submit)];
const IDLE_NEWLINE: &[ShortcutAction] = &[ShortcutAction::Idle(IdleShortcut::Newline)];
const IDLE_EDIT_QUEUED: &[ShortcutAction] = &[ShortcutAction::Idle(IdleShortcut::EditLatestQueued)];
const IDLE_HISTORY: &[ShortcutAction] = &[
    ShortcutAction::Idle(IdleShortcut::HistoryPrevious),
    ShortcutAction::Idle(IdleShortcut::HistoryNext),
];
const IDLE_PAGE: &[ShortcutAction] = &[
    ShortcutAction::Idle(IdleShortcut::PageUp),
    ShortcutAction::Idle(IdleShortcut::PageDown),
];
const IDLE_HALF_PAGE: &[ShortcutAction] = &[
    ShortcutAction::Idle(IdleShortcut::HalfPageUp),
    ShortcutAction::Idle(IdleShortcut::HalfPageDown),
];
const IDLE_BACKTRACK: &[ShortcutAction] = &[ShortcutAction::Idle(IdleShortcut::Backtrack)];
const IDLE_EXPAND: &[ShortcutAction] = &[ShortcutAction::Idle(IdleShortcut::ExpandToolOutput)];
const RUNNING_BACKGROUND: &[ShortcutAction] = &[ShortcutAction::Running(
    RunningShortcut::BackgroundCurrentTurn,
)];
const RUNNING_INTERRUPT: &[ShortcutAction] = &[ShortcutAction::Running(RunningShortcut::Interrupt)];
const RUNNING_SUBMIT: &[ShortcutAction] = &[ShortcutAction::Running(RunningShortcut::SubmitQueued)];
const RUNNING_NEWLINE: &[ShortcutAction] = &[ShortcutAction::Running(RunningShortcut::Newline)];
const RUNNING_EDIT_QUEUED: &[ShortcutAction] =
    &[ShortcutAction::Running(RunningShortcut::EditLatestQueued)];
const RUNNING_SCROLL: &[ShortcutAction] = &[
    ShortcutAction::Running(RunningShortcut::ScrollUp),
    ShortcutAction::Running(RunningShortcut::ScrollDown),
];
const RUNNING_PAGE: &[ShortcutAction] = &[
    ShortcutAction::Running(RunningShortcut::PageUp),
    ShortcutAction::Running(RunningShortcut::PageDown),
];
const RUNNING_HALF_PAGE: &[ShortcutAction] = &[
    ShortcutAction::Running(RunningShortcut::HalfPageUp),
    ShortcutAction::Running(RunningShortcut::HalfPageDown),
];
const APPROVAL_MOVE: &[ShortcutAction] = &[
    ShortcutAction::Approval(ApprovalShortcut::SelectAllow),
    ShortcutAction::Approval(ApprovalShortcut::SelectDeny),
];
const APPROVAL_TOGGLE: &[ShortcutAction] =
    &[ShortcutAction::Approval(ApprovalShortcut::ToggleSelection)];
const APPROVAL_CONFIRM: &[ShortcutAction] = &[ShortcutAction::Approval(ApprovalShortcut::Confirm)];

const SHORTCUT_DESCRIPTORS: &[ShortcutDescriptor] = &[
    ShortcutDescriptor {
        scope: ShortcutScope::Global,
        legacy_keys: "ctrl+f",
        label: "find in transcript",
        actions: OPEN_SEARCH,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Global,
        legacy_keys: "F1 / ctrl+k",
        label: "show or hide shortcuts",
        actions: TOGGLE_SHORTCUTS,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Global,
        legacy_keys: "ctrl+c",
        label: "cancel and quit",
        actions: CANCEL,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Global,
        legacy_keys: "ctrl+home/end",
        label: "jump to top or bottom",
        actions: SCROLL_EDGES,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Global,
        legacy_keys: "ctrl+l",
        label: "clear screen",
        actions: CLEAR_SCREEN,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Global,
        legacy_keys: "shift+tab",
        label: "cycle approval mode",
        actions: &[],
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Idle,
        legacy_keys: "enter",
        label: "send message",
        actions: IDLE_SUBMIT,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Idle,
        legacy_keys: "alt+enter / shift+enter",
        label: "insert newline",
        actions: IDLE_NEWLINE,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Idle,
        legacy_keys: "alt+up",
        label: "edit latest queued message",
        actions: IDLE_EDIT_QUEUED,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Idle,
        legacy_keys: "up/down / ctrl+p/ctrl+n",
        label: "previous or next prompt",
        actions: IDLE_HISTORY,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Idle,
        legacy_keys: "pgup/pgdn",
        label: "scroll one page",
        actions: IDLE_PAGE,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Idle,
        legacy_keys: "ctrl+u / ctrl+d",
        label: "scroll half page",
        actions: IDLE_HALF_PAGE,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Idle,
        legacy_keys: "esc",
        label: "backtrack previous prompt",
        actions: IDLE_BACKTRACK,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Idle,
        legacy_keys: "e",
        label: "expand latest tool output",
        actions: IDLE_EXPAND,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Running,
        legacy_keys: "ctrl+b",
        label: "background current turn",
        actions: RUNNING_BACKGROUND,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Running,
        legacy_keys: "esc / ctrl+g",
        label: "interrupt current turn",
        actions: RUNNING_INTERRUPT,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Running,
        legacy_keys: "enter",
        label: "queue follow-up",
        actions: RUNNING_SUBMIT,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Running,
        legacy_keys: "alt+enter / shift+enter",
        label: "insert newline",
        actions: RUNNING_NEWLINE,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Running,
        legacy_keys: "alt+up",
        label: "edit latest queued message",
        actions: RUNNING_EDIT_QUEUED,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Running,
        legacy_keys: "up/down",
        label: "scroll one line",
        actions: RUNNING_SCROLL,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Running,
        legacy_keys: "pgup/pgdn",
        label: "scroll one page",
        actions: RUNNING_PAGE,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Running,
        legacy_keys: "ctrl+u / ctrl+d",
        label: "scroll half page",
        actions: RUNNING_HALF_PAGE,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Approval,
        legacy_keys: "up/down/j/k",
        label: "move selection",
        actions: APPROVAL_MOVE,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Approval,
        legacy_keys: "tab",
        label: "toggle selection",
        actions: APPROVAL_TOGGLE,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Approval,
        legacy_keys: "enter",
        label: "confirm selected action",
        actions: APPROVAL_CONFIRM,
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Approval,
        legacy_keys: "1/2/3",
        label: "allow options",
        actions: &[],
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Approval,
        legacy_keys: "4",
        label: "deny",
        actions: &[],
    },
    ShortcutDescriptor {
        scope: ShortcutScope::Approval,
        legacy_keys: "y/A/a/n",
        label: "legacy direct keys",
        actions: &[],
    },
];

pub(crate) fn shortcut_descriptors() -> impl Iterator<Item = &'static ShortcutDescriptor> {
    SHORTCUT_DESCRIPTORS.iter()
}

fn scope_has_registered_binding(scope: ShortcutScope) -> bool {
    match scope {
        ShortcutScope::Global => !GLOBAL_BINDINGS.is_empty(),
        ShortcutScope::Idle => !IDLE_BINDINGS.is_empty(),
        ShortcutScope::Running => !RUNNING_BINDINGS.is_empty(),
        ShortcutScope::Approval => !APPROVAL_BINDINGS.is_empty(),
    }
}

fn match_binding<T: Copy>(event: KeyEvent, bindings: &[(T, KeyBinding)]) -> Option<T> {
    bindings
        .iter()
        .find(|(_, binding)| binding.is_press(event))
        .map(|(action, _)| *action)
}

pub(crate) fn normalize_key_parts(
    key: KeyCode,
    mut modifiers: KeyModifiers,
) -> (KeyCode, KeyModifiers) {
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
        assert_eq!(
            idle_shortcut(key(KeyCode::Up, KeyModifiers::NONE)),
            Some(IdleShortcut::HistoryPrevious)
        );
        assert_eq!(
            idle_shortcut(key(KeyCode::Down, KeyModifiers::NONE)),
            Some(IdleShortcut::HistoryNext)
        );
    }

    #[test]
    fn idle_shortcuts_distinguish_enter_from_shift_enter() {
        assert_eq!(
            idle_shortcut(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(IdleShortcut::Submit)
        );
        assert_eq!(
            idle_shortcut(key(KeyCode::Enter, KeyModifiers::SHIFT)),
            Some(IdleShortcut::Newline)
        );
    }

    #[test]
    fn idle_shortcuts_resolve_tool_output_expand() {
        assert_eq!(
            idle_shortcut(key(KeyCode::Char('e'), KeyModifiers::NONE)),
            Some(IdleShortcut::ExpandToolOutput)
        );
    }

    #[test]
    fn running_shortcuts_resolve_background_current_turn() {
        assert_eq!(
            running_shortcut(key(KeyCode::Char('b'), KeyModifiers::CONTROL)),
            Some(RunningShortcut::BackgroundCurrentTurn)
        );
    }

    #[test]
    fn queued_message_shortcuts_are_context_specific() {
        assert_eq!(
            resolve_shortcut(ShortcutContext::Idle, key(KeyCode::Up, KeyModifiers::ALT)),
            Some(ShortcutAction::Idle(IdleShortcut::EditLatestQueued))
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Running,
                key(KeyCode::Up, KeyModifiers::ALT)
            ),
            Some(ShortcutAction::Running(RunningShortcut::EditLatestQueued))
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Running,
                key(KeyCode::Enter, KeyModifiers::NONE)
            ),
            Some(ShortcutAction::Running(RunningShortcut::SubmitQueued))
        );
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            assert_eq!(
                resolve_shortcut(ShortcutContext::Running, key(KeyCode::Enter, modifiers)),
                Some(ShortcutAction::Running(RunningShortcut::Newline))
            );
        }
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Running,
                key(KeyCode::Char('j'), KeyModifiers::CONTROL)
            ),
            Some(ShortcutAction::Running(RunningShortcut::Newline))
        );
    }

    #[test]
    fn global_ctrl_f_opens_transcript_search() {
        assert_eq!(
            global_shortcut(key(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Some(GlobalShortcut::OpenTranscriptSearch)
        );
    }

    #[test]
    fn search_shortcut_hint_is_backed_by_a_binding() {
        assert!(shortcut_hints().any(|hint| {
            hint.scope == ShortcutScope::Global
                && hint.keys == "ctrl+f"
                && hint.has_registered_binding
        }));
    }

    #[test]
    fn shortcut_resolver_prioritizes_global_bindings() {
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Idle,
                key(KeyCode::Char('k'), KeyModifiers::CONTROL)
            ),
            Some(ShortcutAction::Global(GlobalShortcut::ToggleShortcuts))
        );
    }

    #[test]
    fn shortcut_resolver_interprets_same_key_by_context() {
        assert_eq!(
            resolve_shortcut(ShortcutContext::Idle, key(KeyCode::Up, KeyModifiers::NONE)),
            Some(ShortcutAction::Idle(IdleShortcut::HistoryPrevious))
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Running,
                key(KeyCode::Up, KeyModifiers::NONE)
            ),
            Some(ShortcutAction::Running(RunningShortcut::ScrollUp))
        );
        assert_eq!(
            resolve_shortcut(
                ShortcutContext::Approval,
                key(KeyCode::Up, KeyModifiers::NONE)
            ),
            Some(ShortcutAction::Approval(ApprovalShortcut::SelectAllow))
        );
    }

    #[test]
    fn shortcut_hints_are_backed_by_registered_bindings() {
        for hint in shortcut_hints() {
            assert!(
                hint.has_registered_binding,
                "shortcut hint '{}' in {:?} must be backed by a resolver binding",
                hint.keys, hint.scope
            );
        }
    }
}
