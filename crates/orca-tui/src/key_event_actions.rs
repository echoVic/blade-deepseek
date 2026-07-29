use std::io;
use std::sync::{Arc, Mutex};

use crossbeam_channel as mpsc;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use orca_core::config::RunConfig;

use crate::approval_mode_actions::cycle_approval_mode;
use crate::global_actions::{GlobalShortcutFlow, handle_global_shortcut};
use crate::keybindings::{
    InputOwnerFingerprint, KeymapRuntime, ShortcutInvocation, ShortcutResolution,
};
use crate::operation_controller::TuiOperationInterrupt;
use crate::shortcuts::ShortcutAction;
#[cfg(test)]
use crate::shortcuts::{GlobalShortcut, ShortcutContext, resolve_shortcut};
use crate::types::{AppState, AppStatus, PanelMode, UserAction};
use crate::vim::VimState;

#[cfg(test)]
#[allow(dead_code)]
pub(crate) enum KeyEventFlow {
    Continue,
    Exit(i32),
    Unhandled,
}

pub(crate) enum DynamicKeyEventFlow {
    Continue,
    Exit(i32),
    Context(ShortcutInvocation),
    Unhandled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SearchKeyFlow {
    NotSearch,
    Handled,
}

pub(crate) fn handle_transcript_search_key(key: KeyEvent, state: &mut AppState) -> SearchKeyFlow {
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        || !state.transcript_search.open
    {
        return SearchKeyFlow::NotSearch;
    }

    match key.code {
        KeyCode::Esc => state.close_transcript_search(),
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            state.search_previous();
        }
        KeyCode::Enter => state.search_next(),
        KeyCode::Char('g') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                state.search_previous();
            } else {
                state.search_next();
            }
        }
        KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.transcript_search.clear_query();
            state.refresh_transcript_search();
        }
        KeyCode::Backspace => {
            if state.transcript_search.backspace() {
                state.refresh_transcript_search();
            }
        }
        KeyCode::Left => state.transcript_search.move_left(),
        KeyCode::Right => state.transcript_search.move_right(),
        KeyCode::Home => state.transcript_search.move_home(),
        KeyCode::End => state.transcript_search.move_end(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            state.transcript_search.insert_char(character);
            state.refresh_transcript_search();
        }
        _ => {}
    }
    SearchKeyFlow::Handled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    use crate::test_support::{TestOperationInterrupt, test_run_config};
    use crate::theme::Theme;
    use crate::transcript_view::TranscriptRenderContext;
    use crate::types::ChatMessage;
    use crate::ui::build_lines_for_messages;

    fn state_with_search_matches() -> AppState {
        let (tx, _rx) = mpsc::unbounded();
        let mut state = AppState::new(
            tx,
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.push_message(ChatMessage::System("alpha one".to_string()));
        state.push_message(ChatMessage::System("alpha two".to_string()));
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let messages = &state.messages;
        let revisions = &state.message_revisions;
        state.transcript_render_cache.prepare(
            messages,
            revisions,
            TranscriptRenderContext::new(&theme, 40, 0, false),
            |_, message, theme, width, tick, force_expand| {
                build_lines_for_messages(
                    std::slice::from_ref(message),
                    theme,
                    width,
                    tick,
                    force_expand,
                )
            },
        );
        state.open_transcript_search();
        state.replace_transcript_search_query("alpha");
        state.refresh_transcript_search();
        state
    }

    #[test]
    fn active_search_keys_edit_close_and_navigate_without_fallthrough() {
        let mut state = state_with_search_matches();
        assert_eq!(
            handle_transcript_search_key(
                KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE),
                &mut state,
            ),
            SearchKeyFlow::Handled
        );
        assert_eq!(state.transcript_search.query(), "alphaz");
        handle_transcript_search_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &mut state);
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut state,
        );
        assert_eq!(state.transcript_search.query(), "alphz");
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert_eq!(state.transcript_search.query(), "");

        state.replace_transcript_search_query("alpha");
        state.refresh_transcript_search();
        let first = state.transcript_search.active_ordinal();
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut state,
        );
        assert_ne!(state.transcript_search.active_ordinal(), first);
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            &mut state,
        );
        assert_eq!(state.transcript_search.active_ordinal(), first);
        handle_transcript_search_key(
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            &mut state,
        );
        assert_ne!(state.transcript_search.active_ordinal(), first);
        handle_transcript_search_key(
            KeyEvent::new(
                KeyCode::Char('g'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            &mut state,
        );
        assert_eq!(state.transcript_search.active_ordinal(), first);

        handle_transcript_search_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut state);
        assert!(!state.transcript_search.open);
    }

    #[test]
    fn search_ctrl_g_precedes_running_interrupt_and_ctrl_c_stays_global() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = state_with_search_matches();
        state.enter_running();
        let operation = TestOperationInterrupt::default();
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let mut vim = crate::vim::VimState::new(false);

        let ctrl_g = KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL);
        assert!(matches!(
            handle_key_event_preflight(
                ctrl_g,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &operation,
                &mut vim,
                || Ok(()),
            )
            .unwrap(),
            KeyEventFlow::Continue
        ));
        assert_eq!(operation.call_count(), 0);
        assert!(action_rx.try_recv().is_err());

        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        handle_key_event_preflight(
            ctrl_c,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &operation,
            &mut vim,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(operation.call_count(), 1);
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

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
        assert_eq!(vim.named_register_for_test(0), Some(("saved", false)));
        assert!(vim.has_repeat_for_test());
    }

    #[test]
    fn release_and_unknown_search_keys_do_not_mutate_query() {
        let mut state = state_with_search_matches();
        let before = state.transcript_search.query().to_string();
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)
        };
        assert_eq!(
            handle_transcript_search_key(release, &mut state),
            SearchKeyFlow::NotSearch
        );
        assert_eq!(
            handle_transcript_search_key(
                KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE),
                &mut state,
            ),
            SearchKeyFlow::Handled
        );
        assert_eq!(state.transcript_search.query(), before);
    }

    #[test]
    fn dynamic_global_chord_replaces_default_and_opens_search() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let operation = TestOperationInterrupt::default();
        let mut vim = VimState::new(false);
        let keymap = crate::keybindings::parse_keymap(
            br#"{"version":1,"bindings":{"global.open-transcript-search":["ctrl+x ctrl+f"]}}"#,
        )
        .unwrap();
        let mut runtime = crate::keybindings::KeymapRuntime::new(keymap);
        let owner = crate::app::input_owner_fingerprint(&state, &vim);
        let now = std::time::Instant::now();

        assert!(matches!(
            handle_key_event_preflight_dynamic(
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
                now,
                owner,
                &mut runtime,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &operation,
                &mut vim,
                || Ok(()),
            )
            .unwrap(),
            DynamicKeyEventFlow::Unhandled,
        ));
        assert!(!state.transcript_search.open);

        assert!(matches!(
            handle_key_event_preflight_dynamic(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
                now,
                owner,
                &mut runtime,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &operation,
                &mut vim,
                || Ok(()),
            )
            .unwrap(),
            DynamicKeyEventFlow::Continue,
        ));
        assert!(matches!(
            handle_key_event_preflight_dynamic(
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
                now + std::time::Duration::from_millis(1),
                owner,
                &mut runtime,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &operation,
                &mut vim,
                || Ok(()),
            )
            .unwrap(),
            DynamicKeyEventFlow::Continue,
        ));
        assert!(state.transcript_search.open);
    }
}

#[cfg(test)]
pub(crate) fn handle_key_event_preflight<F>(
    key: KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    operation: &impl TuiOperationInterrupt,
    vim_state: &mut VimState,
    clear_terminal: F,
) -> io::Result<KeyEventFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(KeyEventFlow::Continue);
    }

    if let Some(ShortcutAction::Global(GlobalShortcut::Cancel)) =
        resolve_shortcut(ShortcutContext::Global, key)
    {
        vim_state.cancel_pending_command();
        return match handle_global_shortcut(
            GlobalShortcut::Cancel,
            state,
            action_tx,
            operation,
            clear_terminal,
        )? {
            GlobalShortcutFlow::Continue => Ok(KeyEventFlow::Continue),
            GlobalShortcutFlow::Exit(code) => Ok(KeyEventFlow::Exit(code)),
        };
    }

    if handle_transcript_search_key(key, state) == SearchKeyFlow::Handled {
        vim_state.cancel_pending_command();
        return Ok(KeyEventFlow::Continue);
    }

    if let Some(ShortcutAction::Global(shortcut)) = resolve_shortcut(ShortcutContext::Global, key) {
        vim_state.cancel_pending_command();
        return match handle_global_shortcut(shortcut, state, action_tx, operation, clear_terminal)?
        {
            GlobalShortcutFlow::Continue => Ok(KeyEventFlow::Continue),
            GlobalShortcutFlow::Exit(code) => Ok(KeyEventFlow::Exit(code)),
        };
    }

    if state.show_shortcuts && key.code == KeyCode::Esc {
        vim_state.cancel_pending_command();
        state.show_shortcuts = false;
        return Ok(KeyEventFlow::Continue);
    }

    // Esc dismisses an active mouse selection before any other Esc meaning
    // (cancel turn, close panel); a second Esc then does the usual thing.
    if key.code == KeyCode::Esc && state.selection.is_some() {
        vim_state.cancel_pending_command();
        state.invalidate_selection();
        return Ok(KeyEventFlow::Continue);
    }

    if key.code == KeyCode::BackTab
        && matches!(
            state.status,
            AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
        )
    {
        vim_state.cancel_pending_command();
        cycle_approval_mode(config, shared_config, state);
        return Ok(KeyEventFlow::Continue);
    }

    if state.status == AppStatus::Idle
        && state.panel_mode == PanelMode::Workflows
        && key.code == KeyCode::Esc
    {
        vim_state.cancel_pending_command();
        state.show_conversation();
        return Ok(KeyEventFlow::Continue);
    }

    Ok(KeyEventFlow::Unhandled)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_key_event_preflight_dynamic<F>(
    key: KeyEvent,
    now: std::time::Instant,
    owner: InputOwnerFingerprint,
    keymap: &mut KeymapRuntime,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    operation: &impl TuiOperationInterrupt,
    vim_state: &mut VimState,
    clear_terminal: F,
) -> io::Result<DynamicKeyEventFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(DynamicKeyEventFlow::Continue);
    }

    if let ShortcutResolution::Action(invocation) = keymap.resolve_cancel(key) {
        return execute_global_invocation(
            invocation,
            state,
            action_tx,
            operation,
            vim_state,
            clear_terminal,
        );
    }

    match keymap.advance_pending(owner, key, now) {
        ShortcutResolution::Action(invocation) => {
            return if matches!(invocation.action, ShortcutAction::Global(_)) {
                execute_global_invocation(
                    invocation,
                    state,
                    action_tx,
                    operation,
                    vim_state,
                    clear_terminal,
                )
            } else {
                Ok(DynamicKeyEventFlow::Context(invocation))
            };
        }
        ShortcutResolution::Pending => return Ok(DynamicKeyEventFlow::Continue),
        ShortcutResolution::RetryCurrentKey | ShortcutResolution::NoMatch => {}
    }

    if handle_transcript_search_key(key, state) == SearchKeyFlow::Handled {
        vim_state.cancel_pending_command();
        return Ok(DynamicKeyEventFlow::Continue);
    }

    match keymap.resolve_new_global(owner, key, now) {
        ShortcutResolution::Action(invocation) => {
            return execute_global_invocation(
                invocation,
                state,
                action_tx,
                operation,
                vim_state,
                clear_terminal,
            );
        }
        ShortcutResolution::Pending => return Ok(DynamicKeyEventFlow::Continue),
        ShortcutResolution::RetryCurrentKey | ShortcutResolution::NoMatch => {}
    }

    if state.show_shortcuts && key.code == KeyCode::Esc {
        vim_state.cancel_pending_command();
        state.show_shortcuts = false;
        return Ok(DynamicKeyEventFlow::Continue);
    }
    if key.code == KeyCode::Esc && state.selection.is_some() {
        vim_state.cancel_pending_command();
        state.invalidate_selection();
        return Ok(DynamicKeyEventFlow::Continue);
    }
    if key.code == KeyCode::BackTab
        && matches!(
            state.status,
            AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
        )
    {
        vim_state.cancel_pending_command();
        cycle_approval_mode(config, shared_config, state);
        return Ok(DynamicKeyEventFlow::Continue);
    }
    if state.status == AppStatus::Idle
        && state.panel_mode == PanelMode::Workflows
        && key.code == KeyCode::Esc
    {
        vim_state.cancel_pending_command();
        state.show_conversation();
        return Ok(DynamicKeyEventFlow::Continue);
    }
    Ok(DynamicKeyEventFlow::Unhandled)
}

fn execute_global_invocation<F>(
    invocation: ShortcutInvocation,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    operation: &impl TuiOperationInterrupt,
    vim_state: &mut VimState,
    clear_terminal: F,
) -> io::Result<DynamicKeyEventFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    let ShortcutAction::Global(shortcut) = invocation.action else {
        return Ok(DynamicKeyEventFlow::Unhandled);
    };
    vim_state.cancel_pending_command();
    match handle_global_shortcut(shortcut, state, action_tx, operation, clear_terminal)? {
        GlobalShortcutFlow::Continue => Ok(DynamicKeyEventFlow::Continue),
        GlobalShortcutFlow::Exit(code) => Ok(DynamicKeyEventFlow::Exit(code)),
    }
}
