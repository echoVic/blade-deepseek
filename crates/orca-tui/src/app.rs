use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tui_textarea::TextArea;

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::CancelToken;
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::Message;
use orca_core::model::ModelSelection;
use orca_runtime::history;

use crate::approval_dialog_actions::handle_approval_dialog_key;
use crate::background_approval::submit_background_approval_response_for_tui;
use crate::background_tasks::{
    foreground_task_for_tui, notify_recovered_background_approvals_for_tui, stop_task_for_tui,
};
use crate::bridge;
use crate::commands;
use crate::composer_textarea::{
    insert_pasted_text, make_setup_textarea, make_textarea, make_textarea_with_text, textarea_text,
};
use crate::idle_key_actions::handle_idle_key;
use crate::input_event_actions::{handle_mouse_event, handle_paste_event};
use crate::key_event_actions::{KeyEventFlow, handle_key_event_preflight};
use crate::running_actions::handle_running_shortcut;
use crate::runtime_event_actions::handle_runtime_event;
use crate::session_picker_actions::handle_session_picker_key;
use crate::setup_actions::{SetupFlow, handle_setup_key};
use crate::shortcuts::{RunningShortcut, running_shortcut};
use crate::slash_menu_actions::{REASONING_SUBMENU_TITLE, handle_slash_menu_key};
use crate::submitted_turn::SubmittedTurn;
use crate::theme::Theme;
use crate::types::{
    AppState, AppStatus, ChatMessage, SlashMenu, SlashMenuItem, SubMenu, TuiEvent, UserAction,
};
use crate::ui;
use crate::vim::VimState;
use crate::workflow_notifications::{
    drain_pending_workflow_notifications, is_workflow_notification_turn_boundary,
    queue_workflow_terminal_notification, remove_pending_workflow_notification_by_id,
    submit_pending_workflow_notification,
};
use crate::workflow_panel_actions::handle_workflows_panel_key;

pub fn run_tui(config: RunConfig) -> i32 {
    match run_tui_inner(config) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("TUI error: {e}");
            1
        }
    }
}

fn run_tui_inner(mut config: RunConfig) -> io::Result<i32> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    // No alt-screen, so the terminal keeps its normal scrollback buffer. We DO enable mouse
    // capture so the wheel scrolls the conversation in-app; copying is done with the terminal's
    // modifier-drag (Shift/Option+drag on most terminals), which bypasses mouse capture.
    // stdout.execute(EnterAlternateScreen)?;
    let mouse_captured = stdout.execute(EnableMouseCapture).is_ok();
    let bracketed_paste = stdout.execute(EnableBracketedPaste).is_ok();
    // Kitty keyboard protocol: push enhancement AFTER entering alternate screen,
    // otherwise the terminal may reset the keyboard state stack on screen switch.
    let kbd_enhanced = stdout
        .execute(PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
        ))
        .is_ok();

    let backend = CrosstermBackend::new(stdout);

    let (event_tx, event_rx) = mpsc::channel::<TuiEvent>();
    let (action_tx, action_rx) = mpsc::channel::<UserAction>();
    let pending_workflow_notifications: bridge::PendingWorkflowNotifications =
        bridge::PendingWorkflowNotifications::new();

    let model_name = config.model.display_name().to_string();

    let needs_setup = config.api_key.is_none();
    let should_show_picker = config.show_session_picker
        && !needs_setup
        && config.prompt.trim().is_empty()
        && !matches!(
            config.history_mode,
            HistoryMode::Resume(_) | HistoryMode::Fork(_)
        );
    let picker_sessions = if should_show_picker {
        orca_runtime::history::list_sessions(20).unwrap_or_default()
    } else {
        Vec::new()
    };

    let cwd_display = config
        .cwd
        .as_deref()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
        .display()
        .to_string();
    let cwd_display = shorten_home(&cwd_display);

    let mut state = AppState::new(
        action_tx.clone(),
        config.app_version.clone(),
        model_name,
        cwd_display,
    );
    state.approval_mode = config.approval_mode;
    state.reasoning_effort = config.reasoning_effort;
    let theme = Theme::named(config.theme);
    if should_show_picker && !picker_sessions.is_empty() {
        state.status = AppStatus::SessionPicker;
        state.session_picker_sessions = picker_sessions;
    }

    if needs_setup {
        state.status = AppStatus::Setup;
        state.setup_step = 0;
    }

    let initial_prompt = if config.prompt.trim().is_empty() {
        None
    } else {
        Some(config.prompt.clone())
    };

    let startup_preloaded_transcript = if matches!(
        config.history_mode,
        HistoryMode::Resume(_) | HistoryMode::Fork(_)
    ) {
        if let Ok(transcript) = orca_runtime::history::load_session(match &config.history_mode {
            HistoryMode::Resume(selector) | HistoryMode::Fork(selector) => selector,
            HistoryMode::Record | HistoryMode::Disabled => "",
        }) {
            for message in &transcript.messages {
                if let Some(chat_message) = chat_message_from_history(message.clone()) {
                    state.messages.push(chat_message);
                }
            }
            if let Some((explanation, plan)) = &transcript.plan {
                state.current_plan = Some((explanation.clone(), plan.clone()));
            }
            if !state.messages.is_empty() {
                let label = if matches!(config.history_mode, HistoryMode::Fork(_)) {
                    "Forked saved conversation."
                } else {
                    "Resumed saved conversation."
                };
                state.messages.push(ChatMessage::System(label.to_string()));
            }
            // The preloaded transcript is entirely past turns; freeze it so the next
            // turn (or an initial prompt) starts a fresh live suffix.
            state.finalized_count = state.messages.len();
            Some(transcript)
        } else {
            None
        }
    } else {
        None
    };

    let shared_config = Arc::new(Mutex::new(config.clone()));
    let agent_config = Arc::clone(&shared_config);
    let preloaded_transcript: Arc<Mutex<Option<history::SessionTranscript>>> =
        Arc::new(Mutex::new(startup_preloaded_transcript));
    let agent_preloaded = Arc::clone(&preloaded_transcript);
    let agent_event_tx = event_tx.clone();
    let cancel_token = CancelToken::new();
    let agent_cancel = cancel_token.clone();
    let agent_workflow_notifications = pending_workflow_notifications.clone();

    let _agent_handle = std::thread::spawn(move || {
        agent_loop_thread(
            agent_config,
            agent_preloaded,
            agent_event_tx,
            action_rx,
            agent_cancel,
            agent_workflow_notifications,
        );
    });

    let mut vim_state = VimState::new(config.vim_mode);
    let mut textarea = if needs_setup {
        make_setup_textarea(&theme)
    } else {
        if let Some(prompt) = initial_prompt.clone() {
            state.messages.push(ChatMessage::User(prompt.clone()));
            state.enter_running();
            let _ = action_tx.send(UserAction::Submit(prompt));
        }
        make_textarea(&vim_state, &theme)
    };

    // Fullscreen viewport: the UI occupies the whole terminal and is fully repainted every
    // frame. We deliberately do NOT enter the alternate screen (see the commented
    // EnterAlternateScreen above). Mouse capture IS on so the wheel scrolls the conversation;
    // copying uses the terminal's modifier-drag, which bypasses capture.
    let mut terminal = Terminal::new(backend)?;
    // Clear once on startup. Without the alternate screen, ratatui's diffing draw only writes
    // cells that differ from the previous frame; on the very first frame the "previous" buffer
    // is empty, and our blank trailing cells match it, so whatever the shell/cargo left on
    // screen would show through underneath our text. A full clear gives us a clean canvas.
    terminal.clear()?;

    let exit_code;

    terminal.draw(|f| ui::render(f, &mut state, &textarea, &theme))?;

    loop {
        state.advance_tick();

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;

            if handle_paste_event(&ev, &mut state, &config, &mut textarea) {
                continue;
            }

            if handle_mouse_event(&ev, &mut state, Instant::now()) {
                continue;
            }

            if let Event::Key(key) = &ev {
                match handle_key_event_preflight(
                    *key,
                    &mut state,
                    &mut config,
                    &shared_config,
                    &action_tx,
                    &cancel_token,
                    || clear_terminal_scrollback(&mut terminal),
                )? {
                    KeyEventFlow::Continue => continue,
                    KeyEventFlow::Exit(code) => {
                        exit_code = code;
                        break;
                    }
                    KeyEventFlow::Unhandled => {}
                }

                // Setup mode: step-by-step
                if state.status == AppStatus::Setup {
                    match handle_setup_key(
                        &ev,
                        key,
                        &mut state,
                        &mut config,
                        &shared_config,
                        &action_tx,
                        &mut textarea,
                        &vim_state,
                        &theme,
                        initial_prompt.clone(),
                    )? {
                        SetupFlow::Continue => {
                            continue;
                        }
                        SetupFlow::Exit(code) => {
                            exit_code = code;
                            break;
                        }
                    }
                }

                if state.status == AppStatus::SessionPicker {
                    handle_session_picker_key(
                        key,
                        &mut state,
                        &mut config,
                        &shared_config,
                        &preloaded_transcript,
                        || clear_terminal_scrollback(&mut terminal),
                    )?;
                    continue;
                }

                // Approval dialog: 4-option selection + direct-key shortcuts.
                if state.status == AppStatus::WaitingApproval {
                    handle_approval_dialog_key(key, &mut state, &action_tx);
                    continue;
                }

                // Normal Idle mode input
                if matches!(state.status, AppStatus::Idle | AppStatus::WaitingUserInput) {
                    handle_idle_key(
                        &ev,
                        key,
                        &mut state,
                        &mut config,
                        &shared_config,
                        &action_tx,
                        &mut textarea,
                        &mut vim_state,
                        &theme,
                    );
                } else if state.status == AppStatus::Running {
                    if let Some(shortcut) = running_shortcut(*key) {
                        handle_running_shortcut(shortcut, &mut state, &action_tx, &cancel_token);
                    }
                }
            }
        }

        while let Ok(tui_event) = event_rx.try_recv() {
            handle_runtime_event(
                tui_event,
                &mut state,
                &action_tx,
                &pending_workflow_notifications,
                &mut textarea,
                &mut vim_state,
                &theme,
            );
        }

        terminal.draw(|f| ui::render(f, &mut state, &textarea, &theme))?;
    }

    // Cleanup: pop keyboard enhancement, disable bracketed paste
    if kbd_enhanced {
        let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
    }
    if bracketed_paste {
        let _ = io::stdout().execute(DisableBracketedPaste);
    }
    if mouse_captured {
        let _ = io::stdout().execute(DisableMouseCapture);
    }
    // No alternate screen to leave.
    // io::stdout().execute(LeaveAlternateScreen)?;
    // Leave the cursor on a fresh line below the final frame so the shell prompt returns cleanly.
    drop(terminal);
    let _ = io::stdout().execute(crossterm::cursor::Show);
    terminal::disable_raw_mode()?;
    println!();

    Ok(exit_code)
}

fn shorten_home(path: &str) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy();
        if let Some(rest) = path.strip_prefix(home.as_ref()) {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

type InlineTerminal = Terminal<CrosstermBackend<std::io::Stdout>>;

/// Erase the native scrollback and on-screen content. Used by the clear-screen shortcut so a
/// fresh session starts on a clean terminal instead of stacking under the old transcript.
fn clear_terminal_scrollback(terminal: &mut InlineTerminal) -> io::Result<()> {
    use crossterm::terminal::{Clear, ClearType};
    let stdout = terminal.backend_mut();
    stdout.execute(crossterm::cursor::MoveTo(0, 0))?;
    stdout.execute(Clear(ClearType::All))?;
    stdout.execute(Clear(ClearType::Purge))?;
    terminal.clear()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_actions::resolve_approval_option;
    use crate::slash_command_actions::handle_slash_command;
    use crate::types::ApprovalOption;
    use orca_core::config::{
        ModelRuntimeConfig, OutputFormat, ProviderKind, ThemeName, ToolConfig, WorkflowConfig,
    };
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_config(history_mode: HistoryMode) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("auto".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: Some("sk-test".to_string()),
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode,
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

    fn test_state() -> (AppState, mpsc::Receiver<UserAction>) {
        let (tx, rx) = mpsc::channel();
        (
            AppState::new(
                tx,
                "0.0.0-test".to_string(),
                "auto".to_string(),
                "/tmp".to_string(),
            ),
            rx,
        )
    }

    fn matching_task_update(
        event: TuiEvent,
        predicate: impl Fn(&orca_core::task_types::BackgroundTaskSummary) -> bool,
    ) -> Option<orca_core::task_types::BackgroundTaskSummary> {
        match event {
            TuiEvent::WorkflowTasksUpdated { tasks } => tasks.into_iter().find(predicate),
            TuiEvent::WorkflowTaskUpdated { task } if predicate(&task) => Some(task),
            _ => None,
        }
    }

    fn workflow_task(id: &str, name: &str) -> orca_core::task_types::BackgroundTaskSummary {
        orca_core::task_types::BackgroundTaskSummary {
            id: id.to_string(),
            task_type: orca_core::task_types::TaskType::Workflow,
            status: orca_core::task_types::TaskStatus::Running,
            is_backgrounded: false,
            description: name.to_string(),
            created_at_ms: 1_000,
            started_at_ms: Some(1_000),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: None,
            pending_tool_call: None,
            name: Some(name.to_string()),
            workflow_run_id: Some(format!("run-{id}")),
            phase_count: Some(1),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
        }
    }

    #[test]
    fn workflows_panel_keys_move_selected_task() {
        let (mut state, _rx) = test_state();
        state.show_workflows();
        state.workflow_panel.tasks = vec![
            workflow_task("task-1", "audit"),
            workflow_task("task-2", "repair"),
        ];

        let action_tx = state.event_tx.clone();

        assert!(handle_workflows_panel_key(
            KeyCode::Down,
            &mut state,
            &action_tx
        ));
        assert_eq!(state.workflow_panel.selected, 1);

        assert!(handle_workflows_panel_key(
            KeyCode::Up,
            &mut state,
            &action_tx
        ));
        assert_eq!(state.workflow_panel.selected, 0);
    }

    #[test]
    fn workflows_panel_enter_opens_selected_background_approval() {
        let (mut state, _rx) = test_state();
        let mut task = workflow_task("task-approval", "approval");
        task.task_type = orca_core::task_types::TaskType::MainSession;
        task.status = orca_core::task_types::TaskStatus::ApprovalRequired;
        task.is_backgrounded = true;
        task.pending_tool_call = Some(orca_core::task_types::PendingToolCallSummary {
            id: "mock-tool-1".to_string(),
            name: "task_list".to_string(),
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            arguments: "{}".to_string(),
        });
        state.show_workflows();
        state.workflow_panel.tasks = vec![task];

        let action_tx = state.event_tx.clone();
        assert!(handle_workflows_panel_key(
            KeyCode::Enter,
            &mut state,
            &action_tx
        ));

        let dialog = state.approval_dialog.as_ref().expect("approval dialog");
        assert_eq!(dialog.background_task_id.as_deref(), Some("task-approval"));
        assert_eq!(state.status, AppStatus::WaitingApproval);
    }

    #[test]
    fn workflows_panel_s_key_handles_selected_running_task() {
        let (mut state, rx) = test_state();
        let mut task = workflow_task("task-running", "running");
        task.status = orca_core::task_types::TaskStatus::Running;
        state.show_workflows();
        state.workflow_panel.tasks = vec![task];

        let action_tx = state.event_tx.clone();
        assert!(handle_workflows_panel_key(
            KeyCode::Char('s'),
            &mut state,
            &action_tx
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::StopTask { task_id }) if task_id == "task-running"
        ));
    }

    #[test]
    fn workflows_panel_f_key_handles_selected_backgrounded_main_session() {
        let (mut state, rx) = test_state();
        let mut task = workflow_task("task-main", "backgrounded");
        task.task_type = orca_core::task_types::TaskType::MainSession;
        task.status = orca_core::task_types::TaskStatus::Running;
        task.is_backgrounded = true;
        state.show_workflows();
        state.workflow_panel.tasks = vec![task];

        let action_tx = state.event_tx.clone();
        assert!(handle_workflows_panel_key(
            KeyCode::Char('f'),
            &mut state,
            &action_tx
        ));
        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::ForegroundTask { task_id }) if task_id == "task-main"
        ));
    }

    #[test]
    fn background_approval_resolution_sends_request_scoped_action() {
        let (mut state, rx) = test_state();
        let action_tx = state.event_tx.clone();
        state.approval_dialog = Some(crate::types::ApprovalDialog {
            id: "approval-background".to_string(),
            tool: "task_list".to_string(),
            target: None,
            background_task_id: Some("task-approval".to_string()),
            selected: 0,
            options: vec![ApprovalOption::Once, ApprovalOption::Deny],
            diff: None,
        });
        state.set_status(AppStatus::WaitingApproval);

        resolve_approval_option(&mut state, &action_tx, ApprovalOption::Once);

        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::ResolveBackgroundApproval { id, approved })
                if id == "approval-background" && approved
        ));
        assert_eq!(state.status, AppStatus::Idle);
        assert!(state.approval_dialog.is_none());
    }

    #[test]
    fn foreground_approval_resolution_sends_runtime_interaction_id() {
        let (mut state, rx) = test_state();
        let action_tx = state.event_tx.clone();
        state.update(TuiEvent::ApprovalNeeded {
            id: "approval-foreground".to_string(),
            tool: "bash".to_string(),
            target: Some("cargo test".to_string()),
            preview: None,
        });

        resolve_approval_option(&mut state, &action_tx, ApprovalOption::Once);

        assert!(matches!(
            rx.try_recv(),
            Ok(UserAction::Approve { id, approved })
                if id == "approval-foreground" && approved
        ));
        assert_eq!(state.status, AppStatus::Running);
        assert!(state.approval_dialog.is_none());
    }

    #[test]
    fn recovered_background_approval_notifies_tui_user() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(orca_core::task_types::PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();
        let (event_tx, event_rx) = mpsc::channel();

        assert_eq!(
            notify_recovered_background_approvals_for_tui(&registry, &event_tx),
            1
        );

        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1
                    && tasks[0].id == task.id
                    && tasks[0].status == orca_core::task_types::TaskStatus::ApprovalRequired
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Notice(message))
                if message.contains("Recovered background session")
                    && message.contains("task_list")
                    && message.contains("waiting for approval")
        ));
    }

    #[test]
    fn resumed_session_announces_recovered_background_approval_on_first_submit() {
        with_orca_home(|home| {
            let session_id = "resume-background-approval-session";
            let registry = orca_runtime::tasks::TaskRegistry::new_persistent(
                session_id.to_string(),
                home.join("task-sessions"),
            )
            .unwrap();
            let task = registry.create_main_session("Needs approval".to_string());
            let task_id = task.id.clone();
            registry.mark_running(&task.id).unwrap();
            registry.mark_backgrounded(&task.id).unwrap();
            registry
                .approval_required_for_pending_tool(
                    &task.id,
                    "approval_required".to_string(),
                    Some(orca_core::task_types::PendingToolCallSummary {
                        id: "mock-tool-1".to_string(),
                        name: "task_list".to_string(),
                        action: orca_core::approval_types::ActionKind::Read,
                        target: None,
                        arguments: "{}".to_string(),
                    }),
                )
                .unwrap();
            drop(registry);

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
                session_id.to_string(),
            ))));
            let mut transcript = transcript(session_id);
            transcript.path = home.join("resume-background-approval.jsonl");
            std::fs::write(&transcript.path, "").unwrap();
            let preloaded = Arc::new(Mutex::new(Some(transcript)));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit("hello".to_string()))
                .unwrap();

            let mut saw_task_refresh = false;
            let mut saw_notice = false;
            let mut seen = Vec::new();
            for _ in 0..20 {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::WorkflowTasksUpdated { tasks } => {
                        saw_task_refresh |= tasks.into_iter().any(|task| {
                            task.id == task_id
                                && task.status
                                    == orca_core::task_types::TaskStatus::ApprovalRequired
                                && task.is_backgrounded
                        });
                    }
                    TuiEvent::Notice(message)
                        if message.contains("Recovered background session")
                            && message.contains("task_list") =>
                    {
                        saw_notice = true;
                    }
                    event => seen.push(format!("{event:?}")),
                }
                if saw_task_refresh && saw_notice {
                    break;
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(
                saw_task_refresh,
                "missing recovered task refresh; saw {seen:?}"
            );
            assert!(
                saw_notice,
                "missing recovered approval notice; saw {seen:?}"
            );
        });
    }

    #[test]
    fn background_approval_action_denial_stops_task_and_refreshes_tasks() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(orca_core::task_types::PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();
        let (event_tx, event_rx) = mpsc::channel();

        let continuation_request = submit_background_approval_response_for_tui(
            Some(&registry),
            "mock-tool-1",
            false,
            &event_tx,
        );

        assert!(continuation_request.is_none());
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, orca_core::task_types::TaskStatus::Stopped);
        assert_eq!(record.pending_tool_call, None);
        assert_eq!(record.pending_tool_approval_response, None);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1
                    && tasks[0].status == orca_core::task_types::TaskStatus::Stopped
                    && tasks[0].pending_tool_call.is_none()
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Notice(message))
                if message.contains("Background approval denied")
        ));
    }

    #[test]
    fn stop_task_for_tui_requests_stop_and_refreshes_tasks() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Running in background".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let (event_tx, event_rx) = mpsc::channel();

        assert!(stop_task_for_tui(Some(&registry), &task.id, &event_tx));

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, orca_core::task_types::TaskStatus::Stopping);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1
                    && tasks[0].status == orca_core::task_types::TaskStatus::Stopping
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Notice(message))
                if message.contains("Task stop requested")
                    && message.contains(&task.id)
        ));
    }

    #[test]
    fn stop_task_for_tui_stops_approval_required_task_immediately() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(orca_core::task_types::PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();
        let (event_tx, event_rx) = mpsc::channel();

        assert!(stop_task_for_tui(Some(&registry), &task.id, &event_tx));

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, orca_core::task_types::TaskStatus::Stopped);
        assert_eq!(record.result.as_deref(), Some("Task stopped"));
        assert_eq!(record.pending_tool_call, None);
        assert_eq!(record.pending_tool_approval_response, None);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1
                    && tasks[0].status == orca_core::task_types::TaskStatus::Stopped
                    && tasks[0].pending_tool_call.is_none()
        ));
    }

    #[test]
    fn foreground_task_for_tui_marks_backgrounded_task_and_refreshes_tasks() {
        let registry = orca_runtime::tasks::TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Long answer".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let (event_tx, event_rx) = mpsc::channel();

        assert!(foreground_task_for_tui(
            Some(&registry),
            &task.id,
            &event_tx
        ));

        let record = registry.get(&task.id).unwrap();
        assert!(!record.is_backgrounded);
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::WorkflowTasksUpdated { tasks })
                if tasks.len() == 1 && !tasks[0].is_backgrounded
        ));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(TuiEvent::Notice(message)) if message.contains("returned to foreground")
        ));
    }

    fn transcript(session_id: &str) -> history::SessionTranscript {
        history::SessionTranscript {
            meta: history::SessionMeta {
                schema_version: 1,
                session_id: session_id.to_string(),
                cwd: "/tmp".to_string(),
                provider: "mock".to_string(),
                model: Some("auto".to_string()),
                title: "resumed goal".to_string(),
                created_at: chrono::Utc::now(),
                parent_id: None,
                forked: false,
                approval_mode: None,
                active_permission_profile: None,
                runtime_workspace_roots: Vec::new(),
                permission_rules: Default::default(),
                additional_working_directories: Vec::new(),
                network_domain_permissions: Default::default(),
            },
            messages: Vec::new(),
            compactions: Vec::new(),
            summaries: Vec::new(),
            usage: None,
            plan: None,
            completion_status: None,
            path: std::path::PathBuf::from("/tmp/resumed-goal.jsonl"),
        }
    }

    fn with_orca_home<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        let result = f(home.path());
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("ORCA_HOME", previous);
            } else {
                std::env::remove_var("ORCA_HOME");
            }
        }
        result
    }

    fn test_pending_workflow_notifications() -> bridge::PendingWorkflowNotifications {
        bridge::PendingWorkflowNotifications::new()
    }

    #[test]
    fn running_background_shortcut_dispatches_action_and_returns_to_idle_without_cancelling() {
        let (mut state, action_rx) = test_state();
        state.status = AppStatus::Running;
        let action_tx = state.event_tx.clone();
        let cancel = CancelToken::new();

        handle_running_shortcut(
            RunningShortcut::BackgroundCurrentTurn,
            &mut state,
            &action_tx,
            &cancel,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::BackgroundCurrentTurn)
        ));
        assert!(!cancel.is_cancelled());
        assert_eq!(state.status, AppStatus::Idle);
    }

    #[test]
    fn empty_recorded_session_goal_show_dispatches_agent_action() {
        let (mut state, rx) = test_state();
        let (action_tx, action_rx) = mpsc::channel();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));

        handle_slash_command("/goal", &mut config, &shared_config, &mut state, &action_tx);

        assert!(rx.try_recv().is_err());
        assert!(matches!(action_rx.try_recv(), Ok(UserAction::GoalShow)));
        assert_eq!(state.status, AppStatus::Running);
    }

    #[test]
    fn empty_recorded_agent_loop_goal_show_reports_no_goal() {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                agent_loop_thread(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx.send(UserAction::GoalShow).unwrap();
        let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        assert!(matches!(event, TuiEvent::GoalStatus(None)));
    }

    #[test]
    fn empty_recorded_agent_loop_goal_controls_report_session_not_started() {
        let cases = [
            UserAction::GoalEdit("better goal".to_string()),
            UserAction::GoalClear,
            UserAction::GoalPause,
        ];

        for action in cases {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(action).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            match event {
                TuiEvent::Error(message) => {
                    assert_eq!(
                        message,
                        "The session must start before you can change a goal."
                    );
                }
                other => panic!("expected goal control error, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_recorded_agent_loop_goal_resume_without_active_goal_reports_none() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(UserAction::GoalResume).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            cancel.cancel();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(matches!(event, TuiEvent::GoalStatus(None)));
        });
    }

    #[test]
    fn empty_recorded_agent_loop_goal_resume_restores_latest_active_goal() {
        with_orca_home(|home| {
            let mut writer =
                history::SessionWriter::start(home, "mock", Some("auto".to_string()), "goal")
                    .unwrap();
            writer
                .append_message(&orca_core::conversation::Message::user(
                    "previous goal work".to_string(),
                ))
                .unwrap();
            writer.complete("approval_required").unwrap();
            let old_session_id = history::load_session("latest").unwrap().meta.session_id;

            orca_runtime::goals::GoalStore::load_default()
                .replace(
                    &old_session_id,
                    "resume me",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                )
                .unwrap();

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(UserAction::GoalResume).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            cancel.cancel();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            let resumed_session_id = match event {
                TuiEvent::GoalUpdated(goal) => {
                    assert_eq!(goal.objective, "resume me");
                    assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Active);
                    // Resume continues the same thread: the goal must stay on
                    // the original session id; only fork mints a new one.
                    assert_eq!(goal.session_id, old_session_id);
                    goal.session_id
                }
                other => panic!("expected resumed goal update, got {other:?}"),
            };
            let store = orca_runtime::goals::GoalStore::load_default();
            assert_eq!(
                store.get(&resumed_session_id).unwrap().unwrap().status,
                orca_core::goal_types::ThreadGoalStatus::Active
            );
        });
    }

    #[test]
    fn preloaded_resume_goal_pause_updates_persisted_goal_before_live_session_exists() {
        with_orca_home(|_| {
            let session_id = "resume-goal-session";
            orca_runtime::goals::GoalStore::load_default()
                .replace(
                    session_id,
                    "resumed objective",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                )
                .unwrap();

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
                session_id.to_string(),
            ))));
            let preloaded = Arc::new(Mutex::new(Some(transcript(session_id))));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(UserAction::GoalPause).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            match event {
                TuiEvent::GoalUpdated(goal) => {
                    assert_eq!(goal.session_id, session_id);
                    assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Paused);
                }
                other => panic!("expected paused goal update, got {other:?}"),
            }
            let reloaded = orca_runtime::goals::GoalStore::load_default()
                .get(session_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                reloaded.status,
                orca_core::goal_types::ThreadGoalStatus::Paused
            );
        });
    }

    #[test]
    fn preloaded_resume_goal_show_reads_persisted_goal_before_live_session_exists() {
        with_orca_home(|_| {
            let session_id = "resume-goal-show-session";
            orca_runtime::goals::GoalStore::load_default()
                .replace(
                    session_id,
                    "show resumed objective",
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                )
                .unwrap();

            let config = Arc::new(Mutex::new(test_config(HistoryMode::Resume(
                session_id.to_string(),
            ))));
            let preloaded = Arc::new(Mutex::new(Some(transcript(session_id))));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx.send(UserAction::GoalShow).unwrap();
            let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            match event {
                TuiEvent::GoalStatus(Some(goal)) => {
                    assert_eq!(goal.session_id, session_id);
                    assert_eq!(goal.objective, "show resumed objective");
                    assert_eq!(goal.status, orca_core::goal_types::ThreadGoalStatus::Active);
                }
                other => panic!("expected resumed goal status, got {other:?}"),
            }
        });
    }

    #[test]
    fn disabled_history_goal_show_still_reports_recorded_history_requirement() {
        let config = Arc::new(Mutex::new(test_config(HistoryMode::Disabled)));
        let preloaded = Arc::new(Mutex::new(None));
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();

        let handle = std::thread::spawn({
            let config = Arc::clone(&config);
            let preloaded = Arc::clone(&preloaded);
            let cancel = cancel.clone();
            move || {
                agent_loop_thread(
                    config,
                    preloaded,
                    event_tx,
                    action_rx,
                    cancel,
                    test_pending_workflow_notifications(),
                )
            }
        });

        action_tx.send(UserAction::GoalShow).unwrap();
        let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        action_tx.send(UserAction::Cancel).unwrap();
        handle.join().unwrap();

        match event {
            TuiEvent::Error(message) => {
                assert_eq!(
                    message,
                    "persistent goals require recorded history; enable history before using /goal"
                );
            }
            other => panic!("expected recorded-history error, got {other:?}"),
        }
    }

    #[test]
    fn backgrounded_agent_loop_accepts_next_submit_before_first_turn_completes() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit("mock_stream_delay_ms 250".to_string()))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::MessageDelta(text) if text.contains("Mock slow stream started.") => {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();
            action_tx
                .send(UserAction::Submit("mock_history_echo".to_string()))
                .unwrap();

            let first_followup = loop {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::MessageDelta(text) if text.contains("Mock history users:") => {
                        break "next-submit";
                    }
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow stream completed.") =>
                    {
                        break "first-turn-completed";
                    }
                    _ => {}
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(
                first_followup, "next-submit",
                "backgrounding must let the next foreground submit run before the backgrounded turn finishes"
            );
        });
    }

    #[test]
    fn workflow_notification_submit_bypasses_user_file_mention_expansion() {
        with_orca_home(|_| {
            let temp = tempfile::tempdir().unwrap();
            let workspace = temp.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            std::fs::write(temp.path().join("outside.txt"), "outside").unwrap();

            let mut cfg = test_config(HistoryMode::Record);
            cfg.cwd = Some(workspace);
            let config = Arc::new(Mutex::new(cfg));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::SubmitWorkflowNotification(
                    crate::types::PendingWorkflowNotification {
                        id: "notification-1".to_string(),
                        prompt: "mock_history_echo\nread @../outside.txt".to_string(),
                    },
                ))
                .unwrap();

            let mut saw_history_echo = false;
            let mut unexpected_error = None;
            for _ in 0..10 {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::MessageDelta(text) if text.contains("Mock history users:") => {
                        saw_history_echo = true;
                        break;
                    }
                    TuiEvent::Error(message) => {
                        unexpected_error = Some(message);
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(unexpected_error, None);
            assert!(
                saw_history_echo,
                "workflow notifications should not be preprocessed as user-authored @file mentions"
            );
        });
    }

    #[test]
    fn workflow_notification_submit_uses_notification_task_label() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::SubmitWorkflowNotification(
                    crate::types::PendingWorkflowNotification {
                        id: "notification-1".to_string(),
                        prompt: "<task-notification>mock_history_echo</task-notification>"
                            .to_string(),
                    },
                ))
                .unwrap();

            let task = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                }) {
                    break task;
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(task.description, "Workflow notification notification-1");
        });
    }

    #[test]
    fn workflow_notification_first_turn_uses_notification_label_for_session_title() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::SubmitWorkflowNotification(
                    crate::types::PendingWorkflowNotification {
                        id: "notification-1".to_string(),
                        prompt: "<task-notification>mock_history_echo</task-notification>"
                            .to_string(),
                    },
                ))
                .unwrap();

            loop {
                let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                })
                .is_some()
                {
                    break;
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            let transcript = history::load_session("latest").expect("latest session");
            assert_eq!(
                transcript.meta.title,
                "Workflow notification notification-1"
            );
            assert!(!transcript.meta.title.contains("<task-notification>"));
        });
    }

    #[test]
    fn submitted_turn_boundary_owns_goal_loop_presentation_inputs() {
        let source = include_str!("app.rs");
        let goal_loop_start = source
            .rfind("fn run_goal_turns_for_tui(")
            .expect("goal loop function");
        let agent_loop_start = source
            .rfind("fn agent_loop_thread(")
            .expect("agent loop function");
        let goal_loop_section = &source[goal_loop_start..agent_loop_start];

        assert!(
            goal_loop_section.contains("fn run_goal_turns_for_tui(\n")
                && goal_loop_section.contains("    submitted_turn: SubmittedTurn,\n"),
            "goal loop entry should receive the typed submitted-turn boundary"
        );
        assert!(
            !goal_loop_section.contains("initial_task_description"),
            "task labels should stay inside SubmittedTurn presentation metadata"
        );
        assert!(
            !goal_loop_section.contains("initial_backtrack_target"),
            "backtrack policy should stay inside SubmittedTurn presentation metadata"
        );
        assert!(
            goal_loop_section.contains("submitted_turn.task_label()"),
            "goal loop should read the task label through the submitted-turn boundary"
        );
        assert!(
            goal_loop_section.contains("submitted_turn.is_backtrack_target()"),
            "goal loop should read backtrack policy through the submitted-turn boundary"
        );
        assert!(
            !goal_loop_section.contains(".presentation."),
            "goal loop should not reach into SubmittedTurnPresentation internals"
        );
    }

    #[test]
    fn submitted_turn_workflow_notification_carries_notification_boundary() {
        let source = std::fs::read_to_string(format!(
            "{}/src/submitted_turn.rs",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("submitted_turn source should be readable");
        let impl_start = source
            .find("impl SubmittedTurn {")
            .expect("SubmittedTurn impl");
        let submitted_turn_impl = &source[impl_start..];

        assert!(
            submitted_turn_impl
                .contains("fn workflow_notification(notification: PendingWorkflowNotification)"),
            "workflow notification submitted turns should carry the typed notification boundary"
        );
        assert!(
            !submitted_turn_impl.contains("fn workflow_notification(id: String, prompt: String)"),
            "submitted turns should not split workflow notification id and prompt at construction"
        );
    }

    #[test]
    fn submitted_turn_kind_owns_prompt_source_state() {
        let source = std::fs::read_to_string(format!(
            "{}/src/submitted_turn.rs",
            env!("CARGO_MANIFEST_DIR")
        ))
        .expect("submitted_turn source should be readable");
        let kind_start = source
            .rfind("enum SubmittedTurnKind {")
            .expect("SubmittedTurnKind enum");
        let submitted_turn_start = source
            .rfind("struct SubmittedTurn {")
            .expect("SubmittedTurn struct");
        let submitted_turn_section = &source[submitted_turn_start..];
        let struct_body = submitted_turn_section
            .split("}")
            .next()
            .expect("SubmittedTurn struct body");

        assert!(
            kind_start < submitted_turn_start,
            "submitted-turn kind should be declared before SubmittedTurn"
        );
        assert!(
            struct_body.contains("kind: SubmittedTurnKind"),
            "SubmittedTurn should store a single kind that owns the prompt/source data"
        );
        assert!(
            !struct_body.contains("prompt: String"),
            "prompt text should live inside SubmittedTurnKind variants"
        );
        assert!(
            !struct_body.contains("source: SubmittedTurnSource"),
            "source state should live inside SubmittedTurnKind variants"
        );
    }

    #[test]
    fn backgrounded_agent_loop_does_not_complete_unexecuted_tool_calls() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let status = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status != orca_core::task_types::TaskStatus::Running
                }) {
                    break task.status;
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_ne!(
                status,
                orca_core::task_types::TaskStatus::Completed,
                "background completion must not report success for tool calls that were not executed"
            );
        });
    }

    #[test]
    fn backgrounded_agent_loop_marks_unexecuted_tool_calls_approval_required() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let status = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status != orca_core::task_types::TaskStatus::Running
                }) {
                    break task.status;
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::json!("approval_required"),
                "backgrounded turns that stop before executing tool calls must be actionable"
            );
        });
    }

    #[test]
    fn backgrounded_agent_loop_reports_pending_tool_name() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let pending_tool = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
                }) {
                    break task.pending_tool_call;
                }
            };

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            let pending_tool = pending_tool.expect("pending tool call");
            assert_eq!(pending_tool.id, "mock-tool-1");
            assert_eq!(pending_tool.name, "task_list");
            assert_eq!(
                pending_tool.action,
                orca_core::approval_types::ActionKind::Read
            );
            assert_eq!(pending_tool.arguments, "{}");
        });
    }

    #[test]
    fn backgrounded_agent_loop_notifies_approval_required_in_user_language() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let mut notice = None;
            let mut seen = Vec::new();
            for _ in 0..20 {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::Notice(message) if message.starts_with("Background session") => {
                        notice = Some(message);
                        break;
                    }
                    TuiEvent::Notice(message) => {
                        seen.push(format!("notice: {message}"));
                    }
                    TuiEvent::WorkflowTasksUpdated { tasks } => {
                        let statuses = tasks
                            .into_iter()
                            .filter(|task| {
                                task.task_type == orca_core::task_types::TaskType::MainSession
                            })
                            .map(|task| format!("{:?}", task.status))
                            .collect::<Vec<_>>();
                        seen.push(format!("tasks: {}", statuses.join(",")));
                    }
                    TuiEvent::WorkflowTaskUpdated { task }
                        if task.task_type == orca_core::task_types::TaskType::MainSession =>
                    {
                        seen.push(format!("task: {:?}", task.status));
                    }
                    event => seen.push(format!("{event:?}")),
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert_eq!(
                notice.unwrap_or_else(|| panic!("missing background notice; saw {seen:?}")),
                "Background session needs approval for task_list before it can continue."
            );
        });
    }

    #[test]
    fn approved_background_tool_call_executes_and_completes_session() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 task_list".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let (task_id, approval_id) = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
                }) {
                    let approval_id = task
                        .pending_tool_call
                        .as_ref()
                        .expect("pending tool call")
                        .id
                        .clone();
                    break (task.id, approval_id);
                }
            };

            action_tx
                .send(UserAction::ResolveBackgroundApproval {
                    id: approval_id,
                    approved: true,
                })
                .unwrap();

            let mut saw_completion_message = false;
            let mut saw_completed_task = false;
            let mut seen = Vec::new();
            for _ in 0..40 {
                match event_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(TuiEvent::MessageDelta(text)) => {
                        if text.contains("Mock completed after tool execution.") {
                            saw_completion_message = true;
                        }
                        seen.push(format!("delta: {text}"));
                    }
                    Ok(TuiEvent::WorkflowTasksUpdated { tasks }) => {
                        saw_completed_task |= tasks.into_iter().any(|task| {
                            task.id == task_id
                                && task.status == orca_core::task_types::TaskStatus::Completed
                        });
                    }
                    Ok(TuiEvent::WorkflowTaskUpdated { task })
                        if task.id == task_id
                            && task.status == orca_core::task_types::TaskStatus::Completed =>
                    {
                        saw_completed_task = true;
                    }
                    Ok(event) => seen.push(format!("{event:?}")),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        seen.push("timeout".to_string());
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        panic!("agent event channel disconnected before background continuation")
                    }
                }
                if saw_completion_message && saw_completed_task {
                    break;
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(
                saw_completion_message,
                "approved background tool call should continue the model loop; saw {seen:?}"
            );
            assert!(
                saw_completed_task,
                "approved background tool call should complete the background task; saw {seen:?}"
            );
        });
    }

    #[test]
    fn approved_background_tool_call_does_not_prompt_again_for_same_tool() {
        with_orca_home(|_| {
            let config = Arc::new(Mutex::new(test_config(HistoryMode::Record)));
            let preloaded = Arc::new(Mutex::new(None));
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            let cancel = CancelToken::new();

            let handle = std::thread::spawn({
                let config = Arc::clone(&config);
                let preloaded = Arc::clone(&preloaded);
                let cancel = cancel.clone();
                move || {
                    agent_loop_thread(
                        config,
                        preloaded,
                        event_tx,
                        action_rx,
                        cancel,
                        test_pending_workflow_notifications(),
                    )
                }
            });

            action_tx
                .send(UserAction::Submit(
                    "mock_stream_tool_delay_ms 250 mcp__broken__tool".to_string(),
                ))
                .unwrap();

            loop {
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::MessageDelta(text)
                        if text.contains("Mock slow tool stream started.") =>
                    {
                        break;
                    }
                    _ => {}
                }
            }

            action_tx.send(UserAction::BackgroundCurrentTurn).unwrap();

            let approval_id = loop {
                let event = event_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                if let Some(task) = matching_task_update(event, |task| {
                    task.task_type == orca_core::task_types::TaskType::MainSession
                        && task.is_backgrounded
                        && task.status == orca_core::task_types::TaskStatus::ApprovalRequired
                        && task
                            .pending_tool_call
                            .as_ref()
                            .is_some_and(|tool| tool.name == "mcp__broken__tool")
                }) {
                    break task
                        .pending_tool_call
                        .as_ref()
                        .expect("pending tool call")
                        .id
                        .clone();
                }
            };

            action_tx
                .send(UserAction::ResolveBackgroundApproval {
                    id: approval_id,
                    approved: true,
                })
                .unwrap();

            let mut saw_tool_requested = false;
            let mut saw_second_approval = false;
            let mut seen = Vec::new();
            for _ in 0..20 {
                match event_rx.recv_timeout(Duration::from_secs(2)) {
                    Ok(TuiEvent::ToolRequested { name, .. }) if name == "mcp__broken__tool" => {
                        saw_tool_requested = true;
                        break;
                    }
                    Ok(TuiEvent::ApprovalNeeded { id, tool, .. }) => {
                        saw_second_approval = true;
                        seen.push(format!("approval: {tool}"));
                        action_tx
                            .send(UserAction::Approve {
                                id,
                                approved: false,
                            })
                            .unwrap();
                        break;
                    }
                    Ok(event) => seen.push(format!("{event:?}")),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        seen.push("timeout".to_string());
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        panic!("agent event channel disconnected before background tool execution")
                    }
                }
            }

            action_tx.send(UserAction::Cancel).unwrap();
            handle.join().unwrap();

            assert!(
                saw_tool_requested,
                "approved background tool should execute without a second approval; saw {seen:?}"
            );
            assert!(
                !saw_second_approval,
                "approved background tool should not prompt again for the same call"
            );
        });
    }

    #[test]
    fn idle_app_submits_pending_workflow_notification() {
        let (mut state, _rx) = test_state();
        let (action_tx, action_rx) = mpsc::channel();
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>done</task-notification>".to_string(),
            });

        submit_pending_workflow_notification(&mut state, &action_tx, true);

        assert_eq!(state.status, AppStatus::Running);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWorkflowNotification(notification))
                if notification.id == "notification-1"
                    && notification.prompt == "<task-notification>done</task-notification>"
        ));
    }

    #[test]
    fn tool_completion_is_not_a_workflow_notification_turn_boundary() {
        assert!(!is_workflow_notification_turn_boundary(
            &TuiEvent::ToolCompleted {
                id: "tool-1".to_string(),
                name: "bash".to_string(),
                status: "completed".to_string(),
                output: String::new(),
                diff: None,
                kind: None,
            }
        ));
        assert!(!is_workflow_notification_turn_boundary(
            &TuiEvent::SubagentCompleted {
                id: "agent-1".to_string(),
                description: "inspect".to_string(),
                status: "success".to_string(),
                output: None,
                error: None,
            }
        ));
    }

    #[test]
    fn session_completion_submits_pending_workflow_notification() {
        let (mut state, _rx) = test_state();
        let (action_tx, action_rx) = mpsc::channel();
        state.status = AppStatus::Running;
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
            });

        assert!(is_workflow_notification_turn_boundary(
            &TuiEvent::SessionCompleted {
                status: "success".to_string(),
            }
        ));
        submit_pending_workflow_notification(&mut state, &action_tx, false);

        assert_eq!(state.status, AppStatus::Running);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWorkflowNotification(notification))
                if notification.id == "notification-1"
                    && notification.prompt == "<task-notification>failed</task-notification>"
        ));
    }

    #[test]
    fn session_completion_drains_batch_boundary_queue_before_submitting_notification() {
        let (mut state, _rx) = test_state();
        let (action_tx, action_rx) = mpsc::channel();
        let queue = test_pending_workflow_notifications();
        assert!(
            queue.push_unique(crate::types::PendingWorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
            })
        );
        state.status = AppStatus::Running;

        drain_pending_workflow_notifications(&mut state, &queue);
        submit_pending_workflow_notification(&mut state, &action_tx, false);

        assert!(queue.is_empty());
        assert!(state.pending_workflow_notifications.is_empty());
        assert_eq!(state.status, AppStatus::Running);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SubmitWorkflowNotification(notification))
                if notification.id == "notification-1"
                    && notification.prompt == "<task-notification>failed</task-notification>"
        ));
    }

    #[test]
    fn terminal_workflow_notifications_enter_batch_boundary_queue() {
        let queue = test_pending_workflow_notifications();
        let queued = queue_workflow_terminal_notification(
            &TuiEvent::WorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>done</task-notification>".to_string(),
                status: "completed".to_string(),
                summary: "done".to_string(),
            },
            &queue,
            true,
        );
        assert_eq!(queued.as_deref(), Some("notification-1"));
        let notification = queue.pop_front().expect("notification");
        assert_eq!(notification.id, "notification-1");
        assert_eq!(
            notification.prompt,
            "<task-notification>done</task-notification>"
        );

        let queued = queue_workflow_terminal_notification(
            &TuiEvent::WorkflowNotification {
                id: "notification-2".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
                status: "failed".to_string(),
                summary: "failed".to_string(),
            },
            &queue,
            true,
        );
        assert_eq!(queued.as_deref(), Some("notification-2"));
        let notification = queue.pop_front().expect("notification");
        assert_eq!(notification.id, "notification-2");
        assert_eq!(
            notification.prompt,
            "<task-notification>failed</task-notification>"
        );

        let queued = queue_workflow_terminal_notification(
            &TuiEvent::WorkflowNotification {
                id: "notification-3".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
                status: "failed".to_string(),
                summary: "failed".to_string(),
            },
            &queue,
            false,
        );
        assert!(queued.is_none());
        assert!(queue.is_empty());
    }

    #[test]
    fn terminal_workflow_notifications_skip_duplicate_batch_queue_id() {
        let queue = test_pending_workflow_notifications();
        let event = TuiEvent::WorkflowNotification {
            id: "notification-1".to_string(),
            prompt: "<task-notification>done</task-notification>".to_string(),
            status: "completed".to_string(),
            summary: "done".to_string(),
        };

        assert_eq!(
            queue_workflow_terminal_notification(&event, &queue, true).as_deref(),
            Some("notification-1")
        );
        assert!(queue_workflow_terminal_notification(&event, &queue, true).is_none());
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn batch_queued_workflow_notification_is_removed_from_ui_pending_queue_by_id() {
        let (mut state, _rx) = test_state();
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "notification-1".to_string(),
                prompt: "<task-notification>completed</task-notification>".to_string(),
            });
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "notification-2".to_string(),
                prompt: "<task-notification>failed</task-notification>".to_string(),
            });

        remove_pending_workflow_notification_by_id(&mut state, "notification-2");

        assert_eq!(
            state
                .pending_workflow_notifications
                .iter()
                .map(|notification| notification.prompt.as_str())
                .collect::<Vec<_>>(),
            vec!["<task-notification>completed</task-notification>"]
        );
    }

    #[test]
    fn batch_queued_workflow_notification_removal_uses_notification_id() {
        let (mut state, _rx) = test_state();
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "workflow-run-1:tool-use-1".to_string(),
                prompt: "<task-notification>same</task-notification>".to_string(),
            });
        state
            .pending_workflow_notifications
            .push_back(crate::types::PendingWorkflowNotification {
                id: "workflow-run-2:tool-use-2".to_string(),
                prompt: "<task-notification>same</task-notification>".to_string(),
            });

        remove_pending_workflow_notification_by_id(&mut state, "workflow-run-2:tool-use-2");

        let pending = state
            .pending_workflow_notifications
            .iter()
            .map(|notification| notification.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(pending, vec!["workflow-run-1:tool-use-1"]);
    }

    #[test]
    fn settled_messages_remain_in_fullscreen_transcript_after_turn_end() {
        let theme = Theme::named(ThemeName::Dark);
        let (tx, _rx) = mpsc::channel();
        let mut state = AppState::new(
            tx,
            "0.0.0-test".to_string(),
            "auto".to_string(),
            "/tmp".to_string(),
        );
        state.messages.push(ChatMessage::User("hi".to_string()));
        state
            .messages
            .push(ChatMessage::Assistant("answer".to_string()));
        state.finalized_count = state.messages.len();
        state.status = AppStatus::Idle;

        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 10))
            .expect("test backend");

        terminal
            .draw(|frame| ui::render(frame, &mut state, &TextArea::default(), &theme))
            .expect("draw");

        assert_eq!(state.flushed_count, 0);
        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("hi"));
        assert!(rendered.contains("answer"));
    }

    #[test]
    fn slash_menu_tab_opens_history_picker_like_enter() {
        with_orca_home(|home| {
            orca_runtime::history::SessionWriter::start(
                home,
                "mock",
                Some("auto".to_string()),
                "history tab test",
            )
            .unwrap();

            let (mut state, _rx) = test_state();
            state.status = AppStatus::Idle;
            state.slash_menu = Some(SlashMenu {
                items: commands::all_commands()
                    .iter()
                    .map(|(command, description)| SlashMenuItem {
                        command: (*command).to_string(),
                        description: (*description).to_string(),
                    })
                    .collect(),
                selected: commands::all_commands()
                    .iter()
                    .position(|(command, _)| *command == "/history")
                    .unwrap(),
                sub_menu: None,
            });
            let mut config = test_config(HistoryMode::Record);
            let shared_config = Arc::new(Mutex::new(config.clone()));
            let (action_tx, _action_rx) = mpsc::channel();
            let theme = Theme::named(ThemeName::Dark);
            let mut textarea = make_textarea(&VimState::new(false), &theme);
            let vim_state = VimState::new(false);
            let event = Event::Key(crossterm::event::KeyEvent::new(
                KeyCode::Tab,
                crossterm::event::KeyModifiers::NONE,
            ));
            let key = match &event {
                Event::Key(key) => key,
                _ => unreachable!(),
            };

            assert!(handle_slash_menu_key(
                &event,
                key,
                &mut state,
                &mut config,
                &shared_config,
                &action_tx,
                &mut textarea,
                &vim_state,
                &theme,
            ));

            assert_eq!(state.status, AppStatus::SessionPicker);
            assert!(!state.session_picker_sessions.is_empty());
            assert!(state.slash_menu.is_none());
        });
    }

    #[test]
    fn slash_menu_tab_completes_goal_objective_prefix_without_dispatching() {
        let (mut state, _rx) = test_state();
        state.status = AppStatus::Idle;
        state.slash_menu = Some(SlashMenu {
            items: commands::all_commands()
                .iter()
                .map(|(command, description)| SlashMenuItem {
                    command: (*command).to_string(),
                    description: (*description).to_string(),
                })
                .collect(),
            selected: commands::all_commands()
                .iter()
                .position(|(command, _)| *command == "/goal")
                .unwrap(),
            sub_menu: None,
        });
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::channel();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = make_textarea(&VimState::new(false), &theme);
        let vim_state = VimState::new(false);
        let event = Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Tab,
            crossterm::event::KeyModifiers::NONE,
        ));
        let key = match &event {
            Event::Key(key) => key,
            _ => unreachable!(),
        };

        assert!(handle_slash_menu_key(
            &event,
            key,
            &mut state,
            &mut config,
            &shared_config,
            &action_tx,
            &mut textarea,
            &vim_state,
            &theme,
        ));

        assert_eq!(textarea_text(&textarea), "/goal ");
        assert_eq!(state.status, AppStatus::Idle);
        assert!(state.slash_menu.is_none());
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn slash_submenu_model_flow_asks_for_reasoning_effort_then_applies_both() {
        let (mut state, _rx) = test_state();
        state.slash_menu = Some(SlashMenu {
            items: Vec::new(),
            selected: 0,
            sub_menu: Some(SubMenu {
                title: "/model".to_string(),
                items: vec!["deepseek-v4-pro".to_string()],
                selected: 0,
                context: None,
            }),
        });
        let mut config = test_config(HistoryMode::Record);
        config.reasoning_effort = orca_core::config::ReasoningEffort::Max;
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::channel();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = make_textarea(&VimState::new(false), &theme);
        let vim_state = VimState::new(false);

        let press = |key_code: KeyCode,
                     state: &mut AppState,
                     config: &mut RunConfig,
                     textarea: &mut TextArea| {
            let event = Event::Key(crossterm::event::KeyEvent::new(
                key_code,
                crossterm::event::KeyModifiers::NONE,
            ));
            let key = match &event {
                Event::Key(key) => *key,
                _ => unreachable!(),
            };
            assert!(handle_slash_menu_key(
                &event,
                &key,
                state,
                config,
                &shared_config,
                &action_tx,
                textarea,
                &vim_state,
                &theme,
            ));
        };

        // Step 1: picking a model must NOT apply anything yet — it opens the
        // reasoning-effort picker, pre-selected on the current effort (max).
        press(KeyCode::Tab, &mut state, &mut config, &mut textarea);
        let sub = state
            .slash_menu
            .as_ref()
            .and_then(|menu| menu.sub_menu.as_ref())
            .expect("reasoning submenu should open");
        assert_eq!(sub.title, REASONING_SUBMENU_TITLE);
        assert_eq!(sub.context.as_deref(), Some("deepseek-v4-pro"));
        assert!(sub.items[sub.selected].starts_with("max"));
        assert_eq!(state.model_name, "auto", "not applied yet");

        // Step 2: pick "high" (first item), applying model + effort together.
        press(KeyCode::Up, &mut state, &mut config, &mut textarea);
        press(KeyCode::Enter, &mut state, &mut config, &mut textarea);

        assert_eq!(state.model_name, "deepseek-v4-pro");
        assert_eq!(
            state.reasoning_effort,
            orca_core::config::ReasoningEffort::High
        );
        assert_eq!(config.model.display_name(), "deepseek-v4-pro");
        assert_eq!(
            config.reasoning_effort,
            orca_core::config::ReasoningEffort::High
        );
        let shared = shared_config.lock().unwrap();
        assert_eq!(shared.model.display_name(), "deepseek-v4-pro");
        assert_eq!(
            shared.reasoning_effort,
            orca_core::config::ReasoningEffort::High
        );
        drop(shared);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::SetModel(model)) if model == "deepseek-v4-pro"
        ));
        assert!(state.slash_menu.is_none());
    }

    #[test]
    fn workflow_slash_command_dispatches_structured_run_action() {
        let (mut state, _rx) = test_state();
        let mut config = test_config(HistoryMode::Record);
        let shared_config = Arc::new(Mutex::new(config.clone()));
        let (action_tx, action_rx) = mpsc::channel();

        handle_slash_command(
            "/workflow:security-audit target=src maxAgents=8",
            &mut config,
            &shared_config,
            &mut state,
            &action_tx,
        );

        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::RunWorkflow { name, args })
                if name == "security-audit" && args.as_deref() == Some("target=src maxAgents=8")
        ));
    }

    #[test]
    fn bracketed_paste_inserts_multiline_text_without_submitting() {
        let (_state, _rx) = test_state();
        let (_action_tx, action_rx) = mpsc::channel::<UserAction>();
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = make_textarea(&VimState::new(false), &theme);

        assert!(insert_pasted_text(&mut textarea, "alpha\nbravo\ncharlie"));

        assert_eq!(textarea_text(&textarea), "alpha\nbravo\ncharlie");
        assert!(action_rx.try_recv().is_err());
    }

    #[test]
    fn bracketed_paste_can_insert_newline_after_existing_text() {
        let theme = Theme::named(ThemeName::Dark);
        let mut textarea = make_textarea_with_text("prefix", &VimState::new(false), &theme);

        assert!(insert_pasted_text(&mut textarea, "\nnext"));

        assert_eq!(textarea_text(&textarea), "prefix\nnext");
    }
}

fn ensure_tui_session(
    session: &mut Option<bridge::TuiConversationSession>,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    prompt_for_title: &str,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Option<String> {
    if session.is_none() {
        let cfg = config.lock().unwrap().clone();
        let transcript = preloaded.lock().unwrap().take();
        *session = match bridge::TuiConversationSession::new_with_preloaded(
            &cfg,
            prompt_for_title,
            transcript,
        ) {
            Ok(session) => Some(session),
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(format!(
                    "failed to initialize conversation history: {error}"
                )));
                None
            }
        };
        if let Some(session) = session.as_ref() {
            notify_recovered_background_approvals_for_tui(session.task_registry(), event_tx);
        }
    }
    match session.as_ref().and_then(|session| session.session_id()) {
        Some(session_id) => Some(session_id.to_string()),
        None => {
            let _ = event_tx.send(TuiEvent::Error(
                "persistent goals require recorded history; enable history before using /goal"
                    .to_string(),
            ));
            None
        }
    }
}

fn update_goal_status_for_session(
    session_id: Option<&str>,
    status: orca_core::goal_types::ThreadGoalStatus,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    let Some(session_id) = session_id else {
        let _ = event_tx.send(TuiEvent::Error(
            "persistent goals require a saved session".to_string(),
        ));
        return;
    };
    let mut store = orca_runtime::goals::GoalStore::load_default();
    match store.update(
        session_id,
        orca_core::goal_types::GoalUpdate {
            objective: None,
            status: Some(status),
            token_budget: None,
        },
    ) {
        Ok(Some(goal)) => {
            let _ = event_tx.send(TuiEvent::GoalUpdated(goal));
        }
        Ok(None) => {
            let _ = event_tx.send(TuiEvent::Error("no goal is currently set".to_string()));
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!("failed to update goal: {error}")));
        }
    }
}

fn existing_goal_session_id(
    session: Option<&bridge::TuiConversationSession>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    config: &Arc<Mutex<RunConfig>>,
    event_tx: &mpsc::Sender<TuiEvent>,
) -> Option<String> {
    if let Some(session_id) = current_goal_session_id(session, preloaded) {
        return Some(session_id);
    }

    let history_mode = config.lock().unwrap().history_mode.clone();
    let message = if matches!(history_mode, HistoryMode::Disabled) {
        "persistent goals require recorded history; enable history before using /goal"
    } else {
        "The session must start before you can change a goal."
    };
    let _ = event_tx.send(TuiEvent::Error(message.to_string()));
    None
}

fn current_goal_session_id(
    session: Option<&bridge::TuiConversationSession>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
) -> Option<String> {
    if let Some(session_id) = session.and_then(|session| session.session_id().map(str::to_string)) {
        return Some(session_id);
    }
    preloaded
        .lock()
        .unwrap()
        .as_ref()
        .map(|transcript| transcript.meta.session_id.clone())
}

const MAX_GOAL_CONTINUATIONS: usize = 64;

fn goal_continuation_prompt(objective: &str, continuation: usize) -> String {
    format!(
        "[Goal continuation #{continuation}]\nContinue working on this persistent goal:\n{objective}\n\nWork from current evidence. Preserve the full objective, verify every requirement before completion, and call update_goal only with status \"complete\" when the goal is actually finished or status \"blocked\" after the same blocker has repeated for at least three consecutive goal turns."
    )
}

fn run_goal_turns_for_tui(
    config: &RunConfig,
    session: &mut bridge::TuiConversationSession,
    submitted_turn: SubmittedTurn,
    event_tx: &mpsc::Sender<TuiEvent>,
    action_rx: &mpsc::Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    cancel: &CancelToken,
    starting_continuation: usize,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    let Some(session_id) = session.session_id().map(str::to_string) else {
        let _ = event_tx.send(TuiEvent::Error(
            "persistent goals require recorded history".to_string(),
        ));
        return;
    };

    let mut submitted_turn = submitted_turn;
    let mut continuation = starting_continuation;
    loop {
        if let Ok(Some(goal)) = orca_runtime::goals::GoalStore::load_default().get(&session_id)
            && goal.status.should_continue()
        {
            session.replace_goal_context(
                orca_runtime::agent_common::format_goal_mode_instructions(&goal),
            );
        }
        let before_usage = session.usage_totals();
        let started_at = std::time::Instant::now();
        let prompt = submitted_turn.prompt().to_string();
        let turn_result = bridge::run_agent_for_tui_with_notification_queue(
            config,
            session,
            &prompt,
            event_tx,
            action_rx,
            pending_actions,
            cancel,
            true,
            submitted_turn.task_label(),
            submitted_turn.is_backtrack_target(),
            Some(pending_workflow_notifications),
        );
        let status = turn_result.status;
        let after_usage = session.usage_totals();
        let token_delta = after_usage
            .input_tokens
            .saturating_sub(before_usage.input_tokens)
            .saturating_add(
                after_usage
                    .output_tokens
                    .saturating_sub(before_usage.output_tokens),
            )
            .saturating_add(
                after_usage
                    .cache_tokens
                    .saturating_sub(before_usage.cache_tokens),
            ) as i64;
        let elapsed_delta = started_at.elapsed().as_secs().min(i64::MAX as u64) as i64;
        if token_delta > 0 || elapsed_delta > 0 {
            if let Ok(Some(goal)) = orca_runtime::goals::GoalStore::load_default().account_usage(
                &session_id,
                token_delta,
                elapsed_delta,
            ) {
                let _ = event_tx.send(TuiEvent::GoalStatus(Some(goal)));
            }
        }
        if status != "success" {
            if let Ok(Some(goal)) = orca_runtime::goals::GoalStore::load_default().get(&session_id)
                && goal.status.should_continue()
            {
                let _ = event_tx.send(TuiEvent::Notice(format!(
                    "Goal paused because the last turn ended with status `{status}`. Resolve the issue, then use /goal resume to continue."
                )));
            }
            break;
        }
        if let Some(continuation) = turn_result.continuation {
            // Workflow failure notifications are diagnostic follow-ups for the turn that just
            // finished, so they do not consume goal-continuation quota or wait for the next
            // goal-status poll.
            match continuation {
                bridge::TuiAgentTurnContinuation::WorkflowNotification(notification) => {
                    submitted_turn = SubmittedTurn::workflow_notification(notification);
                    continue;
                }
            }
        }
        if session.has_active_workflows() {
            let _ = event_tx.send(TuiEvent::Notice(
                "Goal is waiting for active workflow tasks to finish.".to_string(),
            ));
            break;
        }

        let goal = match orca_runtime::goals::GoalStore::load_default().get(&session_id) {
            Ok(Some(goal)) => goal,
            Ok(None) => break,
            Err(error) => {
                let _ = event_tx.send(TuiEvent::Error(format!("failed to read goal: {error}")));
                break;
            }
        };
        let _ = event_tx.send(TuiEvent::GoalStatus(Some(goal.clone())));
        if !goal.status.should_continue() {
            let label = orca_core::goal_types::goal_status_label(goal.status);
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Goal auto-continuation stopped because the goal is {label}."
            )));
            break;
        }
        continuation += 1;
        if continuation > MAX_GOAL_CONTINUATIONS {
            update_goal_status_for_session(
                Some(&session_id),
                orca_core::goal_types::ThreadGoalStatus::UsageLimited,
                event_tx,
            );
            let _ = event_tx.send(TuiEvent::Notice(
                "Goal auto-continuation stopped after reaching the continuation limit.".to_string(),
            ));
            break;
        }
        submitted_turn =
            SubmittedTurn::user(goal_continuation_prompt(&goal.objective, continuation));
    }
}

fn resume_latest_active_goal_for_tui(
    session: &mut Option<bridge::TuiConversationSession>,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: &mpsc::Sender<TuiEvent>,
    action_rx: &mpsc::Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    cancel: &CancelToken,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    if matches!(config.lock().unwrap().history_mode, HistoryMode::Disabled) {
        let _ = event_tx.send(TuiEvent::Error(
            "persistent goals require recorded history; enable history before using /goal"
                .to_string(),
        ));
        return;
    }

    let mut store = orca_runtime::goals::GoalStore::load_default();
    let goal = match store.latest_active() {
        Ok(Some(goal)) => goal,
        Ok(None) => {
            let _ = event_tx.send(TuiEvent::GoalStatus(None));
            return;
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!("failed to read goals: {error}")));
            return;
        }
    };

    let transcript = match history::load_session(&goal.session_id) {
        Ok(transcript) => transcript,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to load goal session {}: {error}",
                goal.session_id
            )));
            return;
        }
    };

    let mut cfg = config.lock().unwrap().clone();
    cfg.history_mode = HistoryMode::Resume(goal.session_id.clone());
    if let Ok(mut shared) = config.lock() {
        shared.history_mode = cfg.history_mode.clone();
    }
    *preloaded.lock().unwrap() = None;

    let resumed = match bridge::TuiConversationSession::new_with_preloaded(
        &cfg,
        &goal.objective,
        Some(transcript),
    ) {
        Ok(session) => session,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to initialize resumed goal session: {error}"
            )));
            return;
        }
    };

    let Some(new_session_id) = resumed.session_id().map(str::to_string) else {
        let _ = event_tx.send(TuiEvent::Error(
            "persistent goals require recorded history; enable history before using /goal"
                .to_string(),
        ));
        return;
    };

    if new_session_id != goal.session_id {
        let _ = store.update(
            &goal.session_id,
            orca_core::goal_types::GoalUpdate {
                objective: None,
                status: Some(orca_core::goal_types::ThreadGoalStatus::Paused),
                token_budget: None,
            },
        );
    }
    let active_goal = match store.replace(
        &new_session_id,
        &goal.objective,
        orca_core::goal_types::ThreadGoalStatus::Active,
        goal.token_budget,
    ) {
        Ok(goal) => goal,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "failed to resume goal in new session: {error}"
            )));
            return;
        }
    };

    *session = Some(resumed);
    if let Some(session) = session.as_ref() {
        notify_recovered_background_approvals_for_tui(session.task_registry(), event_tx);
    }
    let _ = event_tx.send(TuiEvent::GoalUpdated(active_goal.clone()));
    let _ = event_tx.send(TuiEvent::Notice(
        "Resumed latest active goal in a restored session.".to_string(),
    ));

    if let Some(session) = session.as_mut() {
        let prompt = goal_continuation_prompt(&active_goal.objective, 1);
        run_goal_turns_for_tui(
            &cfg,
            session,
            SubmittedTurn::user(prompt),
            event_tx,
            action_rx,
            pending_actions,
            cancel,
            1,
            pending_workflow_notifications,
        );
    }
}

fn recv_next_user_action(
    action_rx: &mpsc::Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
) -> Result<UserAction, mpsc::RecvError> {
    if let Some(action) = pending_actions.borrow_mut().pop_front() {
        return Ok(action);
    }
    action_rx.recv()
}

fn handle_submitted_turn_for_tui(
    submitted_turn: SubmittedTurn,
    config: &Arc<Mutex<RunConfig>>,
    preloaded: &Arc<Mutex<Option<history::SessionTranscript>>>,
    session: &mut Option<bridge::TuiConversationSession>,
    pending_pinned_context: &mut Vec<String>,
    event_tx: &mpsc::Sender<TuiEvent>,
    action_rx: &mpsc::Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    cancel: &CancelToken,
    pending_workflow_notifications: &bridge::PendingWorkflowNotifications,
) {
    cancel.reset();
    let cfg = config.lock().unwrap().clone();
    let cwd = cfg
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let prompt = match submitted_turn.prompt_for_model(&cwd) {
        Ok(prompt) => prompt,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            return;
        }
    };
    if session.is_none() {
        let transcript = preloaded.lock().unwrap().take();
        let title_seed = submitted_turn.title_seed(&prompt);
        *session =
            match bridge::TuiConversationSession::new_with_preloaded(&cfg, &title_seed, transcript)
            {
                Ok(session) => Some(session),
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::Error(format!(
                        "failed to initialize conversation history: {error}"
                    )));
                    return;
                }
            };
        if let Some(session) = session.as_ref() {
            notify_recovered_background_approvals_for_tui(session.task_registry(), event_tx);
        }
    }
    if let Some(session) = session.as_mut() {
        for context in pending_pinned_context.drain(..) {
            session.add_pinned_context(context);
        }
    }
    run_goal_turns_for_tui(
        &cfg,
        session.as_mut().expect("session initialized"),
        submitted_turn.with_model_prompt(prompt),
        event_tx,
        action_rx,
        pending_actions,
        cancel,
        0,
        pending_workflow_notifications,
    );
    if cfg.desktop_notifications {
        let _ = orca_runtime::notify::notify("Orca", "Task completed");
    }
}

fn agent_loop_thread(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    cancel: CancelToken,
    pending_workflow_notifications: bridge::PendingWorkflowNotifications,
) {
    let mut session: Option<bridge::TuiConversationSession> = None;
    let mut pending_pinned_context: Vec<String> = Vec::new();
    let pending_actions: RefCell<VecDeque<UserAction>> = RefCell::new(VecDeque::new());

    loop {
        match recv_next_user_action(&action_rx, &pending_actions) {
            Ok(UserAction::Submit(prompt)) => {
                handle_submitted_turn_for_tui(
                    SubmittedTurn::user(prompt),
                    &config,
                    &preloaded,
                    &mut session,
                    &mut pending_pinned_context,
                    &event_tx,
                    &action_rx,
                    &pending_actions,
                    &cancel,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::SubmitWorkflowNotification(notification)) => {
                handle_submitted_turn_for_tui(
                    SubmittedTurn::workflow_notification(notification),
                    &config,
                    &preloaded,
                    &mut session,
                    &mut pending_pinned_context,
                    &event_tx,
                    &action_rx,
                    &pending_actions,
                    &cancel,
                    &pending_workflow_notifications,
                );
            }
            Ok(UserAction::RunWorkflow { name, args }) => {
                cancel.reset();
                let cfg = config.lock().unwrap().clone();
                if session.is_none() {
                    let prompt = format!("Run saved workflow `{name}`");
                    let transcript = preloaded.lock().unwrap().take();
                    session = match bridge::TuiConversationSession::new_with_preloaded(
                        &cfg, &prompt, transcript,
                    ) {
                        Ok(session) => Some(session),
                        Err(error) => {
                            let _ = event_tx.send(TuiEvent::Error(format!(
                                "failed to initialize conversation history: {error}"
                            )));
                            continue;
                        }
                    };
                    if let Some(session) = session.as_ref() {
                        notify_recovered_background_approvals_for_tui(
                            session.task_registry(),
                            &event_tx,
                        );
                    }
                }
                if let Some(session) = session.as_ref() {
                    bridge::launch_saved_workflow_for_tui(
                        &cfg,
                        session,
                        &name,
                        args.as_deref(),
                        &event_tx,
                    );
                }
                if cfg.desktop_notifications {
                    let _ = orca_runtime::notify::notify("Orca", "Workflow launched");
                }
            }
            Ok(UserAction::Interrupt) => {
                // Cancel already set by TUI thread; just continue waiting for next Submit
            }
            Ok(UserAction::SetModel(model)) => {
                if let Some(session) = session.as_mut() {
                    session.set_model(Some(&model));
                }
            }
            Ok(UserAction::Remember(note)) => {
                let context = format!("[Pinned remembered note]\n{}", note.trim());
                if let Some(session) = session.as_mut() {
                    session.add_pinned_context(context);
                } else {
                    pending_pinned_context.push(context);
                }
            }
            Ok(UserAction::Compact) => {
                if let Some(session) = session.as_mut() {
                    let cfg = config.lock().unwrap().clone();
                    let cwd = cfg
                        .cwd
                        .clone()
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    let (before_messages, after_messages) = session.compact(&cfg, &cwd);
                    let _ = event_tx.send(TuiEvent::Compacted {
                        before_messages,
                        after_messages,
                    });
                } else {
                    let _ = event_tx.send(TuiEvent::Error("nothing to compact".to_string()));
                }
            }
            Ok(UserAction::Backtrack) => {
                if let Some(session) = session.as_mut() {
                    match session.backtrack_last_user() {
                        Some(prompt) => {
                            let _ = event_tx.send(TuiEvent::Backtracked { prompt });
                        }
                        None => {
                            let _ =
                                event_tx.send(TuiEvent::Error("nothing to backtrack".to_string()));
                        }
                    }
                } else {
                    let _ = event_tx.send(TuiEvent::Error("nothing to backtrack".to_string()));
                }
            }
            Ok(UserAction::BackgroundCurrentTurn) => {}
            Ok(UserAction::StopTask { task_id }) => {
                stop_task_for_tui(
                    session.as_ref().map(|session| session.task_registry()),
                    &task_id,
                    &event_tx,
                );
            }
            Ok(UserAction::ForegroundTask { task_id }) => {
                foreground_task_for_tui(
                    session.as_ref().map(|session| session.task_registry()),
                    &task_id,
                    &event_tx,
                );
            }
            Ok(UserAction::ResolveBackgroundApproval { id, approved }) => {
                let continuation_request = submit_background_approval_response_for_tui(
                    session.as_ref().map(|session| session.task_registry()),
                    &id,
                    approved,
                    &event_tx,
                );
                if approved
                    && let Some(continuation_request) = continuation_request
                    && let Some(session) = session.as_mut()
                {
                    let cfg = config.lock().unwrap().clone();
                    let result = bridge::continue_approved_background_turn_for_tui(
                        &cfg,
                        session,
                        &continuation_request,
                        &event_tx,
                        &action_rx,
                        &pending_actions,
                        &cancel,
                        Some(&pending_workflow_notifications),
                    );
                    if let Some(continuation) = result.continuation {
                        match continuation {
                            bridge::TuiAgentTurnContinuation::WorkflowNotification(
                                notification,
                            ) => {
                                pending_actions.borrow_mut().push_front(
                                    UserAction::SubmitWorkflowNotification(notification),
                                );
                            }
                        }
                    }
                }
            }
            Ok(UserAction::GoalShow) => {
                let session_id = session
                    .as_ref()
                    .and_then(|s| s.session_id().map(str::to_string))
                    .or_else(|| {
                        preloaded
                            .lock()
                            .unwrap()
                            .as_ref()
                            .map(|transcript| transcript.meta.session_id.clone())
                    });
                let Some(session_id) = session_id else {
                    let history_mode = config.lock().unwrap().history_mode.clone();
                    if matches!(history_mode, HistoryMode::Disabled) {
                        let _ = event_tx.send(TuiEvent::Error(
                            "persistent goals require recorded history; enable history before using /goal"
                                .to_string(),
                        ));
                    } else {
                        let _ = event_tx.send(TuiEvent::GoalStatus(None));
                    }
                    continue;
                };
                let store = orca_runtime::goals::GoalStore::load_default();
                match store.get(&session_id) {
                    Ok(goal) => {
                        let _ = event_tx.send(TuiEvent::GoalStatus(goal));
                    }
                    Err(error) => {
                        let _ =
                            event_tx.send(TuiEvent::Error(format!("failed to read goal: {error}")));
                    }
                }
            }
            Ok(UserAction::GoalSet(objective)) => {
                let Some(session_id) =
                    ensure_tui_session(&mut session, &config, &preloaded, &objective, &event_tx)
                else {
                    continue;
                };
                let mut store = orca_runtime::goals::GoalStore::load_default();
                match store.replace(
                    &session_id,
                    &objective,
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    None,
                ) {
                    Ok(goal) => {
                        let _ = event_tx.send(TuiEvent::GoalUpdated(goal));
                        let _ = event_tx.send(TuiEvent::Notice(
                            "Starting goal. Automatic continuation will keep running while it remains active.".to_string(),
                        ));
                        if let Some(session) = session.as_mut() {
                            let cfg = config.lock().unwrap().clone();
                            run_goal_turns_for_tui(
                                &cfg,
                                session,
                                SubmittedTurn::user(objective),
                                &event_tx,
                                &action_rx,
                                &pending_actions,
                                &cancel,
                                0,
                                &pending_workflow_notifications,
                            );
                        }
                    }
                    Err(error) => {
                        let _ =
                            event_tx.send(TuiEvent::Error(format!("failed to set goal: {error}")));
                    }
                }
            }
            Ok(UserAction::GoalEdit(objective)) => {
                let Some(session_id) =
                    existing_goal_session_id(session.as_ref(), &preloaded, &config, &event_tx)
                else {
                    continue;
                };
                let mut store = orca_runtime::goals::GoalStore::load_default();
                match store.update(
                    &session_id,
                    orca_core::goal_types::GoalUpdate {
                        objective: Some(objective),
                        status: Some(orca_core::goal_types::ThreadGoalStatus::Active),
                        token_budget: None,
                    },
                ) {
                    Ok(Some(goal)) => {
                        let _ = event_tx.send(TuiEvent::GoalUpdated(goal));
                    }
                    Ok(None) => {
                        let _ =
                            event_tx.send(TuiEvent::Error("no goal is currently set".to_string()));
                    }
                    Err(error) => {
                        let _ =
                            event_tx.send(TuiEvent::Error(format!("failed to edit goal: {error}")));
                    }
                }
            }
            Ok(UserAction::GoalClear) => {
                let Some(session_id) =
                    existing_goal_session_id(session.as_ref(), &preloaded, &config, &event_tx)
                else {
                    continue;
                };
                let mut store = orca_runtime::goals::GoalStore::load_default();
                match store.clear(&session_id) {
                    Ok(_) => {
                        let _ = event_tx.send(TuiEvent::GoalCleared);
                    }
                    Err(error) => {
                        let _ = event_tx
                            .send(TuiEvent::Error(format!("failed to clear goal: {error}")));
                    }
                }
            }
            Ok(UserAction::GoalPause) => {
                if let Some(session_id) =
                    existing_goal_session_id(session.as_ref(), &preloaded, &config, &event_tx)
                {
                    update_goal_status_for_session(
                        Some(&session_id),
                        orca_core::goal_types::ThreadGoalStatus::Paused,
                        &event_tx,
                    );
                }
            }
            Ok(UserAction::GoalResume) => {
                let Some(session_id) = current_goal_session_id(session.as_ref(), &preloaded) else {
                    resume_latest_active_goal_for_tui(
                        &mut session,
                        &config,
                        &preloaded,
                        &event_tx,
                        &action_rx,
                        &pending_actions,
                        &cancel,
                        &pending_workflow_notifications,
                    );
                    continue;
                };
                update_goal_status_for_session(
                    Some(&session_id),
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    &event_tx,
                );
                if let Some(session) = session.as_mut() {
                    if let Some(goal) = session
                        .session_id()
                        .filter(|id| *id == session_id)
                        .and_then(|id| orca_runtime::goals::GoalStore::load_default().get(id).ok())
                        .flatten()
                    {
                        let cfg = config.lock().unwrap().clone();
                        let prompt = goal_continuation_prompt(&goal.objective, 1);
                        run_goal_turns_for_tui(
                            &cfg,
                            session,
                            SubmittedTurn::user(prompt),
                            &event_tx,
                            &action_rx,
                            &pending_actions,
                            &cancel,
                            1,
                            &pending_workflow_notifications,
                        );
                    }
                }
            }
            Ok(UserAction::Cancel) | Err(_) => break,
            Ok(UserAction::Approve { .. } | UserAction::RespondToUserInput { .. }) => {}
        }
    }
}

pub(crate) fn chat_message_from_history(message: Message) -> Option<ChatMessage> {
    match message {
        Message::System { .. } => None,
        Message::User { content, .. } => Some(ChatMessage::User(content)),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            if let Some(content) = content.filter(|text| !text.trim().is_empty()) {
                Some(ChatMessage::Assistant(content))
            } else if let Some(reasoning) = reasoning_content.filter(|text| !text.trim().is_empty())
            {
                Some(ChatMessage::Reasoning(reasoning))
            } else if !tool_calls.is_empty() {
                let names = tool_calls
                    .iter()
                    .map(|tool| tool.function_name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(ChatMessage::System(format!(
                    "Previous assistant requested tools: {names}"
                )))
            } else {
                None
            }
        }
        Message::Tool {
            tool_call_id,
            content,
            ..
        } => Some(ChatMessage::ToolCall {
            id: tool_call_id.clone(),
            name: format!("tool:{tool_call_id}"),
            target: None,
            status: "completed".to_string(),
            output: Some(content),
            diff: None,
            kind: None,
            expanded: false,
        }),
    }
}
