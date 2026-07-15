use crossbeam_channel as mpsc;
use std::io;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyEvent};
use tui_textarea::TextArea;

use orca_core::cancel::OperationCancellation;
use orca_core::config::RunConfig;
use orca_runtime::history::SessionTranscript;

use crate::approval_dialog_actions::handle_approval_dialog_key;
use crate::idle_key_actions::handle_idle_key;
use crate::running_actions::handle_running_shortcut;
use crate::session_picker_actions::handle_session_picker_key;
use crate::setup_actions::{SetupFlow, handle_setup_key};
use crate::shortcuts::{RunningShortcut, ShortcutAction, ShortcutContext, resolve_shortcut};
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
    cancellation: &OperationCancellation,
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
        handle_running_shortcut(shortcut, state, action_tx, cancellation);
    }

    if state.status == AppStatus::Compacting
        && let Some(ShortcutAction::Running(shortcut)) =
            resolve_shortcut(ShortcutContext::Running, *key)
        && compacting_shortcut_allowed(shortcut)
    {
        handle_running_shortcut(shortcut, state, action_tx, cancellation);
    }

    Ok(StatusKeyFlow::Continue)
}

fn compacting_shortcut_allowed(shortcut: RunningShortcut) -> bool {
    match shortcut {
        RunningShortcut::Interrupt
        | RunningShortcut::ScrollUp
        | RunningShortcut::ScrollDown
        | RunningShortcut::PageUp
        | RunningShortcut::PageDown
        | RunningShortcut::HalfPageUp
        | RunningShortcut::HalfPageDown => true,
        RunningShortcut::BackgroundCurrentTurn => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, ThemeName, ToolConfig,
        WorkflowConfig,
    };
    use orca_core::model::ModelSelection;

    fn config() -> RunConfig {
        RunConfig {
            app_version: "test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("auto".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            subagents: Default::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn compacting_status_keeps_running_interrupt_shortcut() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.set_status(AppStatus::Compacting);
        let mut config = config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let cancellation = OperationCancellation::new();
        let operation = cancellation.start();
        let preloaded = Arc::new(Mutex::new(None));
        let mut textarea = TextArea::default();
        let mut vim_state = VimState::new(false);
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let event = Event::Key(key);

        handle_status_key(
            &event,
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &cancellation,
            &preloaded,
            &mut textarea,
            &mut vim_state,
            &theme,
            None,
            || Ok(()),
        )
        .expect("handle compacting shortcut");

        assert!(operation.token().is_cancelled());
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    fn esc_interrupts_running_without_exiting_or_marking_idle() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.enter_running();
        let mut config = config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let cancellation = OperationCancellation::new();
        let operation = cancellation.start();
        let preloaded = Arc::new(Mutex::new(None));
        let mut textarea = TextArea::default();
        let mut vim_state = VimState::new(false);
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let key = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let event = Event::Key(key);

        let flow = handle_status_key(
            &event,
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &cancellation,
            &preloaded,
            &mut textarea,
            &mut vim_state,
            &theme,
            None,
            || Ok(()),
        )
        .expect("handle running shortcut");

        assert!(matches!(flow, StatusKeyFlow::Continue));
        assert_eq!(state.status, AppStatus::Running);
        assert!(operation.token().is_cancelled());
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::Interrupt)));
    }

    #[test]
    fn compacting_status_rejects_background_current_turn_shortcut() {
        let (action_tx, action_rx) = mpsc::unbounded();
        let mut state = AppState::new(
            action_tx.clone(),
            "test".to_string(),
            "mock".to_string(),
            "/tmp".to_string(),
        );
        state.set_status(AppStatus::Compacting);
        let mut config = config();
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let cancellation = OperationCancellation::new();
        let preloaded = Arc::new(Mutex::new(None));
        let mut textarea = TextArea::default();
        let mut vim_state = VimState::new(false);
        let theme = Theme::named(orca_core::config::ThemeName::Dark);
        let key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        let event = Event::Key(key);

        handle_status_key(
            &event,
            &key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &cancellation,
            &preloaded,
            &mut textarea,
            &mut vim_state,
            &theme,
            None,
            || Ok(()),
        )
        .expect("handle compacting shortcut");

        assert_eq!(state.status, AppStatus::Compacting);
        assert!(action_rx.try_recv().is_err());
        assert!(cancellation.current_id().is_none());
    }
}
