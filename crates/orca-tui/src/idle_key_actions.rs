use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::TextArea;

use orca_core::config::RunConfig;

use crate::composer_input_actions::{
    apply_composer_key_input, insert_composer_newline, recall_next_history, recall_previous_history,
};
use crate::composer_textarea::{make_textarea_with_text, textarea_text};
use crate::idle_navigation_actions::handle_idle_navigation_shortcut;
use crate::idle_submit_actions::handle_idle_submit;
use crate::keybindings::{InvocationOrigin, ShortcutInvocation};
use crate::mention_menu_actions::handle_mention_menu_key;
use crate::queued_input_actions::restore_latest_queued_message;
use crate::shortcuts::{IdleShortcut, ShortcutAction, ShortcutContext, resolve_shortcut};
use crate::slash_menu_actions::handle_slash_menu_key;
use crate::theme::Theme;
use crate::types::{AppState, UserAction};
use crate::vim::VimState;
use crate::workflow_panel_actions::handle_workflows_panel_key;

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_idle_key(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) {
    if state.slash_menu.is_some()
        && handle_slash_menu_key(
            ev,
            key,
            state,
            config,
            shared_config,
            action_tx,
            textarea,
            vim_state,
            theme,
        )
    {
        vim_state.cancel_pending_command();
        return;
    }

    if (!state.mention.candidates.is_empty()
        || (state.mention.phase.is_some() && key.code == KeyCode::Esc))
        && handle_mention_menu_key(ev, key, state, textarea, vim_state, theme)
    {
        vim_state.cancel_pending_command();
        return;
    }

    if handle_workflows_panel_key(key.code, state, action_tx) {
        vim_state.cancel_pending_command();
        return;
    }

    if let Some(action @ ShortcutAction::Idle(_)) = resolve_shortcut(ShortcutContext::Idle, *key) {
        handle_idle_shortcut_invocation(
            ShortcutInvocation::key(action, *key),
            state,
            config,
            shared_config,
            action_tx,
            textarea,
            vim_state,
            theme,
        );
    } else {
        apply_composer_key_input(ev, key, state, config, textarea, vim_state, theme);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_idle_shortcut_invocation(
    invocation: ShortcutInvocation,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
) -> bool {
    let ShortcutAction::Idle(shortcut) = invocation.action else {
        return false;
    };
    match shortcut {
        IdleShortcut::EditLatestQueued => {
            vim_state.cancel_pending_command();
            if state.status == crate::types::AppStatus::Idle {
                restore_latest_queued_message(state, textarea, vim_state, theme);
            }
        }
        IdleShortcut::Submit => {
            vim_state.cancel_pending_command();
            handle_idle_submit(
                textarea,
                vim_state,
                theme,
                state,
                config,
                shared_config,
                action_tx,
            );
        }
        IdleShortcut::Newline => {
            vim_state.cancel_pending_command();
            insert_composer_newline(textarea, state);
        }
        IdleShortcut::HistoryPrevious => {
            vim_state.cancel_pending_command();
            match invocation.origin {
                InvocationOrigin::Key(key) => recall_previous_history(
                    &Event::Key(key),
                    &key,
                    state,
                    textarea,
                    vim_state,
                    theme,
                ),
                InvocationOrigin::Chord => {
                    if let Some(history) = state.history_previous(textarea_text(textarea)) {
                        *textarea = make_textarea_with_text(&history, vim_state, theme);
                    }
                }
            }
        }
        IdleShortcut::HistoryNext => {
            vim_state.cancel_pending_command();
            match invocation.origin {
                InvocationOrigin::Key(key) => {
                    recall_next_history(&Event::Key(key), &key, state, textarea, vim_state, theme)
                }
                InvocationOrigin::Chord => {
                    if let Some(history) = state.history_next() {
                        *textarea = make_textarea_with_text(&history, vim_state, theme);
                    }
                }
            }
        }
        shortcut @ (IdleShortcut::ScrollUp
        | IdleShortcut::ScrollDown
        | IdleShortcut::PageUp
        | IdleShortcut::PageDown
        | IdleShortcut::HalfPageUp
        | IdleShortcut::HalfPageDown
        | IdleShortcut::Backtrack
        | IdleShortcut::ExpandToolOutput) => match invocation.origin {
            InvocationOrigin::Key(key) => {
                if shortcut != IdleShortcut::ExpandToolOutput {
                    vim_state.cancel_pending_command();
                }
                handle_idle_navigation_shortcut(
                    shortcut,
                    &Event::Key(key),
                    &key,
                    state,
                    config,
                    textarea,
                    vim_state,
                    theme,
                    action_tx,
                );
            }
            InvocationOrigin::Chord => {
                vim_state.cancel_pending_command();
                handle_idle_chord_navigation(shortcut, state, textarea, vim_state, action_tx);
            }
        },
    }
    true
}

fn handle_idle_chord_navigation(
    shortcut: IdleShortcut,
    state: &mut AppState,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    action_tx: &mpsc::Sender<UserAction>,
) {
    match shortcut {
        IdleShortcut::ScrollUp => state.scroll_up(1),
        IdleShortcut::ScrollDown => state.scroll_down(1),
        IdleShortcut::PageUp => state.scroll_up(state.visible_height.saturating_sub(2)),
        IdleShortcut::PageDown => state.scroll_down(state.visible_height.saturating_sub(2)),
        IdleShortcut::HalfPageUp => state.scroll_up(state.visible_height / 2),
        IdleShortcut::HalfPageDown => state.scroll_down(state.visible_height / 2),
        IdleShortcut::Backtrack => {
            let _ = action_tx.send(UserAction::Backtrack);
        }
        IdleShortcut::ExpandToolOutput => {
            if textarea_text(textarea).trim().is_empty() && state.toggle_latest_tool_output() {
                vim_state.cancel_pending_command();
                state.scroll_to_bottom();
            }
        }
        IdleShortcut::Submit
        | IdleShortcut::Newline
        | IdleShortcut::EditLatestQueued
        | IdleShortcut::HistoryPrevious
        | IdleShortcut::HistoryNext => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composer_textarea::{make_textarea_with_text, textarea_text};
    use crate::keybindings::ShortcutInvocation;
    use crate::test_support::test_run_config;
    use crate::types::TuiEvent;
    use crossterm::event::KeyModifiers;
    use orca_core::config::{ThemeName, VimInsertEscapeSequence};

    #[test]
    fn nonempty_composer_keeps_vim_count_when_e_matches_expand_shortcut() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        config.vim_mode = true;
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(true);
        let mut textarea = TextArea::from(["one two three"]);

        for code in [KeyCode::Char('2'), KeyCode::Char('e')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            handle_idle_key(
                &Event::Key(key),
                &key,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &mut textarea,
                &mut vim,
                &theme,
            );
        }

        assert_eq!(textarea.cursor(), (0, 6));
        assert!(!vim.has_pending_command_for_test());
    }

    #[test]
    fn empty_multiline_composer_keeps_vim_prefix_when_expand_has_no_tool() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        config.vim_mode = true;
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(true);
        let mut textarea = TextArea::from([" ", " ", " "]);

        for code in [KeyCode::Char('d'), KeyCode::Char('e')] {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            handle_idle_key(
                &Event::Key(key),
                &key,
                &mut state,
                &mut config,
                &shared,
                &action_tx,
                &mut textarea,
                &mut vim,
                &theme,
            );
        }

        assert_eq!(textarea.cursor(), (0, 0));
        assert!(!vim.has_pending_command_for_test());
    }

    #[test]
    fn configured_first_character_does_not_steal_consumed_idle_shortcut() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.update(TuiEvent::ToolRequested {
            id: "tool-1".to_string(),
            name: "grep".to_string(),
            target: None,
        });
        let mut config = test_run_config();
        config.vim_mode = true;
        config.vim_insert_escape = Some(VimInsertEscapeSequence::parse("ee").unwrap());
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::with_insert_escape(true, config.vim_insert_escape.clone());
        vim.mode = crate::vim::VimMode::Insert;
        let mut textarea = TextArea::default();
        let key = KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE);

        handle_idle_key(
            &Event::Key(key),
            &key,
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        );

        assert!(textarea.is_empty());
        assert!(!vim.has_pending_insert_escape_for_test());
        let crate::types::ChatMessage::ToolCall { expanded, .. } = &state.messages[0] else {
            panic!("expected tool call");
        };
        assert!(*expanded);
    }

    #[test]
    fn chord_expand_decline_does_not_insert_or_move_composer_text() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("first\nsecond", &vim, &theme);
        let before = (textarea_text(&textarea), textarea.cursor());

        assert!(handle_idle_shortcut_invocation(
            ShortcutInvocation::chord(ShortcutAction::Idle(IdleShortcut::ExpandToolOutput,)),
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        ));

        assert_eq!((textarea_text(&textarea), textarea.cursor()), before);
    }

    #[test]
    fn chord_scroll_uses_transcript_even_for_multiline_composer() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.scroll_offset = 2;
        state.auto_scroll = false;
        state.total_lines = 20;
        state.visible_height = 5;
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("first\nsecond", &vim, &theme);
        let before = (textarea_text(&textarea), textarea.cursor());

        assert!(handle_idle_shortcut_invocation(
            ShortcutInvocation::chord(ShortcutAction::Idle(IdleShortcut::ScrollUp)),
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        ));

        assert_eq!((textarea_text(&textarea), textarea.cursor()), before);
        assert_eq!(state.scroll_offset, 1);
    }

    #[test]
    fn chord_history_previous_recalls_history_from_multiline_composer() {
        let (action_tx, _action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.input_history = vec!["prior prompt".to_string()];
        let mut config = test_run_config();
        let shared = Arc::new(Mutex::new(config.clone()));
        let theme = Theme::named(ThemeName::Dark);
        let mut vim = VimState::new(false);
        let mut textarea = make_textarea_with_text("draft\ncontinued", &vim, &theme);

        assert!(handle_idle_shortcut_invocation(
            ShortcutInvocation::chord(ShortcutAction::Idle(IdleShortcut::HistoryPrevious,)),
            &mut state,
            &mut config,
            &shared,
            &action_tx,
            &mut textarea,
            &mut vim,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "prior prompt");
    }
}
