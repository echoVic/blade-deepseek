use std::io;
use std::sync::{Arc, Mutex, mpsc};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;

use crate::approval_mode_actions::cycle_approval_mode;
use crate::global_actions::{GlobalShortcutFlow, handle_global_shortcut};
use crate::shortcuts::global_shortcut;
use crate::types::{AppState, AppStatus, PanelMode, UserAction};

pub(crate) enum KeyEventFlow {
    Continue,
    Exit(i32),
    Unhandled,
}

pub(crate) fn handle_key_event_preflight<F>(
    key: KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    cancel_token: &CancelToken,
    clear_terminal: F,
) -> io::Result<KeyEventFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(KeyEventFlow::Continue);
    }

    if let Some(shortcut) = global_shortcut(key) {
        return match handle_global_shortcut(
            shortcut,
            state,
            action_tx,
            cancel_token,
            clear_terminal,
        )? {
            GlobalShortcutFlow::Continue => Ok(KeyEventFlow::Continue),
            GlobalShortcutFlow::Exit(code) => Ok(KeyEventFlow::Exit(code)),
        };
    }

    if state.show_shortcuts && key.code == KeyCode::Esc {
        state.show_shortcuts = false;
        return Ok(KeyEventFlow::Continue);
    }

    if key.code == KeyCode::BackTab
        && matches!(
            state.status,
            AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
        )
    {
        cycle_approval_mode(config, shared_config, state);
        return Ok(KeyEventFlow::Continue);
    }

    if state.status == AppStatus::Idle
        && state.panel_mode == PanelMode::Workflows
        && key.code == KeyCode::Esc
    {
        state.show_conversation();
        return Ok(KeyEventFlow::Continue);
    }

    Ok(KeyEventFlow::Unhandled)
}
