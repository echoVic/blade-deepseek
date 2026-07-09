use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyEvent};
use tui_textarea::TextArea;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_runtime::history::SessionTranscript;

use crate::approval_dialog_actions::handle_approval_dialog_key;
use crate::idle_key_actions::handle_idle_key;
use crate::running_actions::handle_running_shortcut;
use crate::session_picker_actions::handle_session_picker_key;
use crate::setup_actions::{SetupFlow, handle_setup_key};
use crate::shortcuts::{ShortcutAction, ShortcutContext, resolve_shortcut};
use crate::theme::Theme;
use crate::types::{AppState, AppStatus, UserAction};
use crate::vim::VimState;

pub(crate) enum StatusKeyFlow {
    Continue,
    Exit(i32),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_status_key<F>(
    ev: &Event,
    key: &KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    cancel_token: &CancelToken,
    preloaded_transcript: &Arc<Mutex<Option<SessionTranscript>>>,
    textarea: &mut TextArea,
    vim_state: &mut VimState,
    theme: &Theme,
    initial_prompt: Option<String>,
    clear_terminal: F,
) -> io::Result<StatusKeyFlow>
where
    F: FnOnce() -> io::Result<()>,
{
    if state.status == AppStatus::Setup {
        return match handle_setup_key(
            ev,
            key,
            state,
            config,
            shared_config,
            action_tx,
            textarea,
            vim_state,
            theme,
            initial_prompt,
        )? {
            SetupFlow::Continue => Ok(StatusKeyFlow::Continue),
            SetupFlow::Exit(code) => Ok(StatusKeyFlow::Exit(code)),
        };
    }

    if state.status == AppStatus::SessionPicker {
        handle_session_picker_key(
            key,
            state,
            config,
            shared_config,
            preloaded_transcript,
            clear_terminal,
        )?;
        return Ok(StatusKeyFlow::Continue);
    }

    if state.status == AppStatus::WaitingApproval {
        handle_approval_dialog_key(key, state, action_tx);
        return Ok(StatusKeyFlow::Continue);
    }

    if matches!(state.status, AppStatus::Idle | AppStatus::WaitingUserInput) {
        handle_idle_key(
            ev,
            key,
            state,
            config,
            shared_config,
            action_tx,
            textarea,
            vim_state,
            theme,
        );
        return Ok(StatusKeyFlow::Continue);
    }

    if state.status == AppStatus::Running
        && let Some(ShortcutAction::Running(shortcut)) =
            resolve_shortcut(ShortcutContext::Running, *key)
    {
        handle_running_shortcut(shortcut, state, action_tx, cancel_token);
    }

    Ok(StatusKeyFlow::Continue)
}
