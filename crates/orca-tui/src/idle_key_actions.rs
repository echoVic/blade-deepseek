use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::TextArea;

use orca_core::config::RunConfig;

use crate::composer_input_actions::{
    apply_composer_key_input, insert_composer_newline, recall_next_history, recall_previous_history,
};
use crate::idle_navigation_actions::handle_idle_navigation_shortcut;
use crate::idle_submit_actions::handle_idle_submit;
use crate::mention_menu_actions::handle_mention_menu_key;
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
        return;
    }

    if (!state.mention.candidates.is_empty()
        || (state.mention.phase.is_some() && key.code == KeyCode::Esc))
        && handle_mention_menu_key(ev, key, state, textarea, vim_state, theme)
    {
        return;
    }

    if handle_workflows_panel_key(key.code, state, action_tx) {
        return;
    }

    match resolve_shortcut(ShortcutContext::Idle, *key) {
        Some(ShortcutAction::Idle(IdleShortcut::Submit)) => {
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
        Some(ShortcutAction::Idle(IdleShortcut::Newline)) => {
            insert_composer_newline(textarea, state);
        }
        Some(ShortcutAction::Idle(IdleShortcut::HistoryPrevious)) => {
            recall_previous_history(ev, key, state, textarea, vim_state, theme);
        }
        Some(ShortcutAction::Idle(IdleShortcut::HistoryNext)) => {
            recall_next_history(ev, key, state, textarea, vim_state, theme);
        }
        Some(ShortcutAction::Idle(
            shortcut @ (IdleShortcut::ScrollUp
            | IdleShortcut::ScrollDown
            | IdleShortcut::PageUp
            | IdleShortcut::PageDown
            | IdleShortcut::HalfPageUp
            | IdleShortcut::HalfPageDown
            | IdleShortcut::Backtrack
            | IdleShortcut::ExpandToolOutput),
        )) => {
            handle_idle_navigation_shortcut(
                shortcut, ev, key, state, config, textarea, vim_state, theme, action_tx,
            );
        }
        Some(_) | None => {
            apply_composer_key_input(ev, key, state, config, textarea, vim_state, theme);
        }
    }
}
