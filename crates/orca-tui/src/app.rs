use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyboardEnhancementFlags, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tui_textarea::{CursorMove, Input, TextArea};

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::CancelToken;
use orca_core::config::file::save_api_key;
use orca_core::config::{HistoryMode, RunConfig};
use orca_core::conversation::Message;
use orca_core::model::ModelSelection;
use orca_runtime::history;
use orca_runtime::mentions;

use crate::bridge;
use crate::commands::{self, GoalSlashCommand, SlashCommand};
use crate::shortcuts::{
    ApprovalShortcut, GlobalShortcut, IdleShortcut, RunningShortcut, approval_shortcut,
    global_shortcut, idle_shortcut, running_shortcut,
};
use crate::theme::Theme;
use crate::types::{
    AppState, AppStatus, ApprovalOption, ChatMessage, PanelMode, SlashMenu, SlashMenuItem, SubMenu,
    TuiEvent, UserAction,
};
use crate::ui;
use crate::vim::VimState;

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
        Arc::new(Mutex::new(VecDeque::new()));

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
    let agent_workflow_notifications = Arc::clone(&pending_workflow_notifications);

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

            if let Event::Paste(pasted) = &ev {
                match state.status {
                    AppStatus::Setup if state.setup_step == 1 => {
                        insert_pasted_text(&mut textarea, pasted);
                    }
                    AppStatus::Idle | AppStatus::WaitingUserInput => {
                        if insert_pasted_text(&mut textarea, pasted) {
                            state.reset_history_navigation();
                            update_slash_menu(&textarea, &mut state, &config);
                            update_mention_candidates(&textarea, &mut state, &config);
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // Mouse wheel scrolls the conversation transcript. Other mouse events (clicks,
            // drags) are ignored — text selection/copy is handled by the terminal via
            // modifier-drag, which bypasses our capture. Scrolling only applies to the
            // conversation view; the panel dashboards have their own keyboard navigation.
            if let Event::Mouse(mouse) = &ev {
                if state.panel_mode == PanelMode::Conversation {
                    if state.accepts_mouse_scroll_at(Instant::now()) {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => state.scroll_up(3),
                            MouseEventKind::ScrollDown => state.scroll_down(3),
                            _ => {}
                        }
                    }
                }
                continue;
            }

            if let Event::Key(key) = &ev {
                if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }

                if let Some(shortcut) = global_shortcut(*key) {
                    match shortcut {
                        GlobalShortcut::Cancel => {
                            if state.status == AppStatus::Running {
                                cancel_token.cancel();
                                let _ = action_tx.send(UserAction::Interrupt);
                                continue;
                            }
                            let now = Instant::now();
                            if state
                                .last_ctrl_c
                                .is_some_and(|t| now.duration_since(t) < Duration::from_secs(2))
                            {
                                let _ = action_tx.send(UserAction::Cancel);
                                exit_code = 130;
                                break;
                            }
                            state.last_ctrl_c = Some(now);
                            state
                                .messages
                                .push(ChatMessage::System("Press Ctrl+C again to quit.".into()));
                            state.scroll_to_bottom();
                            continue;
                        }
                        GlobalShortcut::ToggleShortcuts => {
                            state.toggle_shortcuts();
                            continue;
                        }
                        GlobalShortcut::ScrollBottom => {
                            state.scroll_to_bottom();
                            continue;
                        }
                        GlobalShortcut::ScrollTop => {
                            state.scroll_to_top();
                            continue;
                        }
                        GlobalShortcut::ClearScreen => {
                            state.messages.clear();
                            state.finalized_count = 0;
                            state.flushed_count = 0;
                            state.scroll_offset = 0;
                            state.auto_scroll = true;
                            // Wipe terminal contents as well as the in-memory transcript.
                            // Without the alternate screen, previous frames live in the
                            // terminal's normal buffer even after `messages` is cleared.
                            clear_terminal_scrollback(&mut terminal)?;
                            continue;
                        }
                    }
                }

                if state.show_shortcuts && key.code == KeyCode::Esc {
                    state.show_shortcuts = false;
                    continue;
                }

                // Shift+Tab cycles the approval mode. Skipped while an approval dialog is open,
                // where BackTab moves the dialog selection instead.
                if key.code == KeyCode::BackTab
                    && matches!(
                        state.status,
                        AppStatus::Idle | AppStatus::Running | AppStatus::WaitingUserInput
                    )
                {
                    let next = config.approval_mode.next();
                    config.approval_mode = next;
                    if let Ok(mut cfg) = shared_config.lock() {
                        cfg.approval_mode = next;
                    }
                    state.approval_mode = next;
                    state.messages.push(ChatMessage::System(format!(
                        "Approval mode switched to {}.",
                        next.as_str()
                    )));
                    state.scroll_to_bottom();
                    continue;
                }

                if state.status == AppStatus::Idle
                    && state.panel_mode == PanelMode::Workflows
                    && key.code == KeyCode::Esc
                {
                    state.show_conversation();
                    continue;
                }

                // Setup mode: step-by-step
                if state.status == AppStatus::Setup {
                    match state.setup_step {
                        0 => {
                            // Welcome screen — Enter to continue, Esc to quit
                            match key.code {
                                KeyCode::Enter => {
                                    state.setup_step = 1;
                                    textarea = make_setup_textarea(&theme);
                                }
                                KeyCode::Esc => {
                                    exit_code = 0;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        1 => {
                            // API key input
                            match key.code {
                                KeyCode::Enter => {
                                    let lines: Vec<String> = textarea.lines().to_vec();
                                    let key_input = lines.join("").trim().to_string();
                                    if !key_input.is_empty() {
                                        save_api_key(&key_input);
                                        config.api_key = Some(key_input.clone());
                                        if let Ok(mut cfg) = shared_config.lock() {
                                            cfg.api_key = Some(key_input);
                                        }
                                        state.setup_step = 2;
                                    }
                                }
                                KeyCode::Esc => {
                                    exit_code = 0;
                                    break;
                                }
                                _ => {
                                    textarea.input(Input::from(ev));
                                }
                            }
                        }
                        2 => {
                            // Completion screen — Enter to start
                            match key.code {
                                KeyCode::Enter => {
                                    state.set_status(AppStatus::Idle);
                                    state.setup_step = 0;
                                    textarea = make_textarea(&vim_state, &theme);

                                    if let Some(prompt) = initial_prompt.clone() {
                                        state.messages.push(ChatMessage::User(prompt.clone()));
                                        state.enter_running();
                                        let _ = action_tx.send(UserAction::Submit(prompt));
                                    }
                                }
                                KeyCode::Esc => {
                                    exit_code = 0;
                                    break;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                if state.status == AppStatus::SessionPicker {
                    match key.code {
                        KeyCode::Up => state.select_previous_session(),
                        KeyCode::Down => state.select_next_session(),
                        KeyCode::Backspace => state.session_query_pop(),
                        KeyCode::Char(c) => state.session_query_push(c),
                        KeyCode::Enter => {
                            if let Some(session_id) = state.selected_session_id() {
                                config.history_mode = HistoryMode::Resume(session_id.clone());
                                if let Ok(mut cfg) = shared_config.lock() {
                                    cfg.history_mode = HistoryMode::Resume(session_id.clone());
                                }
                                if let Ok(transcript) = history::load_session(&session_id) {
                                    state.messages.clear();
                                    state.flushed_count = 0;
                                    state.scroll_offset = 0;
                                    state.auto_scroll = true;
                                    for message in &transcript.messages {
                                        if let Some(chat_message) =
                                            chat_message_from_history(message.clone())
                                        {
                                            state.messages.push(chat_message);
                                        }
                                    }
                                    if let Some((explanation, plan)) = &transcript.plan {
                                        state.current_plan =
                                            Some((explanation.clone(), plan.clone()));
                                    } else {
                                        state.current_plan = None;
                                    }
                                    state.messages.push(ChatMessage::System(
                                        "Resumed saved conversation.".to_string(),
                                    ));
                                    // A resumed transcript is entirely past turns; freeze
                                    // it so the live suffix starts empty for the next turn.
                                    state.finalized_count = state.messages.len();
                                    if let Ok(mut preloaded) = preloaded_transcript.lock() {
                                        *preloaded = Some(transcript);
                                    }
                                    // Without the alternate screen, the old frame remains in the
                                    // terminal's normal buffer. Clear it so the resumed session
                                    // redraws on a clean terminal.
                                    clear_terminal_scrollback(&mut terminal)?;
                                }
                                state.set_status(AppStatus::Idle);
                            }
                        }
                        KeyCode::Esc => {
                            state.set_status(AppStatus::Idle);
                            state.session_picker_sessions.clear();
                            state.session_picker_query.clear();
                        }
                        _ => {}
                    }
                    continue;
                }

                // Approval dialog: 4-option selection + direct-key shortcuts.
                if state.status == AppStatus::WaitingApproval {
                    // Direct option keys (y / a / A / n) resolve immediately.
                    if let KeyCode::Char(c) = key.code
                        && let Some(option) = state
                            .approval_dialog
                            .as_ref()
                            .and_then(|d| d.options.iter().copied().find(|o| o.key() == c))
                    {
                        resolve_approval_option(&mut state, &action_tx, option);
                        continue;
                    }
                    match approval_shortcut(*key) {
                        Some(ApprovalShortcut::SelectAllow) => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                dialog.selected = dialog.selected.saturating_sub(1);
                            }
                        }
                        Some(ApprovalShortcut::SelectDeny) => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                let last = dialog.options.len().saturating_sub(1);
                                dialog.selected = (dialog.selected + 1).min(last);
                            }
                        }
                        Some(ApprovalShortcut::ToggleSelection) => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                let len = dialog.options.len().max(1);
                                dialog.selected = (dialog.selected + 1) % len;
                            }
                        }
                        Some(ApprovalShortcut::Confirm) => {
                            let option = state.approval_dialog.as_ref().map(|d| d.current());
                            if let Some(option) = option {
                                resolve_approval_option(&mut state, &action_tx, option);
                            }
                        }
                        Some(ApprovalShortcut::Approve) => {
                            resolve_approval_option(&mut state, &action_tx, ApprovalOption::Once);
                        }
                        Some(ApprovalShortcut::Deny) => {
                            resolve_approval_option(&mut state, &action_tx, ApprovalOption::Deny);
                        }
                        None => {}
                    }
                    continue;
                }

                // Normal Idle mode input
                if matches!(state.status, AppStatus::Idle | AppStatus::WaitingUserInput) {
                    // Handle slash menu if open
                    if state.slash_menu.is_some() {
                        if handle_slash_menu_key(
                            &ev,
                            key,
                            &mut state,
                            &mut config,
                            &shared_config,
                            &action_tx,
                            &mut textarea,
                            &vim_state,
                            &theme,
                        ) {
                            continue;
                        }
                    }

                    if !state.mention_candidates.is_empty() {
                        if handle_mention_menu_key(
                            &ev,
                            key,
                            &mut state,
                            &config,
                            &mut textarea,
                            &vim_state,
                            &theme,
                        ) {
                            continue;
                        }
                    }

                    if handle_workflows_panel_key(key.code, &mut state) {
                        continue;
                    }

                    match idle_shortcut(*key) {
                        Some(IdleShortcut::Submit) => {
                            state.slash_menu = None;
                            let lines: Vec<String> = textarea.lines().to_vec();
                            let text = lines.join("\n").trim().to_string();
                            if !text.is_empty() {
                                if let Some(outcome) = handle_slash_command(
                                    &text,
                                    &mut config,
                                    &shared_config,
                                    &mut state,
                                    &action_tx,
                                ) {
                                    match outcome {
                                        SlashOutcome::Continue => {
                                            vim_state.reset_insert(&mut textarea, &theme);
                                            textarea = make_textarea(&vim_state, &theme);
                                            continue;
                                        }
                                    }
                                }
                                if state.status == AppStatus::WaitingUserInput {
                                    state.enter_running();
                                    state.scroll_to_bottom();
                                    let _ = action_tx.send(UserAction::RespondToUserInput(text));
                                } else {
                                    state.record_prompt(text.clone());
                                    state.messages.push(ChatMessage::User(text.clone()));
                                    state.enter_running();
                                    state.scroll_to_bottom();
                                    let _ = action_tx.send(UserAction::Submit(text));
                                }
                                vim_state.reset_insert(&mut textarea, &theme);
                                textarea = make_textarea(&vim_state, &theme);
                            }
                        }
                        Some(IdleShortcut::Newline) => {
                            textarea.insert_newline();
                            state.reset_history_navigation();
                        }
                        Some(IdleShortcut::HistoryPrevious) => {
                            if key.code == KeyCode::Up && textarea.lines().len() > 1 {
                                textarea.input(Input::from(ev));
                            } else {
                                let draft = textarea_text(&textarea);
                                if let Some(history) = state.history_previous(draft) {
                                    textarea =
                                        make_textarea_with_text(&history, &vim_state, &theme);
                                }
                            }
                        }
                        Some(IdleShortcut::HistoryNext) => {
                            if key.code == KeyCode::Down && textarea.lines().len() > 1 {
                                textarea.input(Input::from(ev));
                            } else if let Some(history) = state.history_next() {
                                textarea = make_textarea_with_text(&history, &vim_state, &theme);
                            }
                        }
                        Some(IdleShortcut::ScrollUp) => {
                            if textarea.lines().len() > 1 {
                                textarea.input(Input::from(ev));
                            } else {
                                state.scroll_up(1);
                            }
                        }
                        Some(IdleShortcut::ScrollDown) => {
                            if textarea.lines().len() > 1 {
                                textarea.input(Input::from(ev));
                            } else {
                                state.scroll_down(1);
                            }
                        }
                        Some(IdleShortcut::PageUp) => {
                            let page = state.visible_height.saturating_sub(2);
                            state.scroll_up(page);
                        }
                        Some(IdleShortcut::PageDown) => {
                            let page = state.visible_height.saturating_sub(2);
                            state.scroll_down(page);
                        }
                        Some(IdleShortcut::HalfPageUp) => {
                            let page = state.visible_height / 2;
                            state.scroll_up(page);
                        }
                        Some(IdleShortcut::HalfPageDown) => {
                            let page = state.visible_height / 2;
                            state.scroll_down(page);
                        }
                        Some(IdleShortcut::Backtrack) => {
                            let _ = action_tx.send(UserAction::Backtrack);
                        }
                        Some(IdleShortcut::ExpandToolOutput) => {
                            if textarea_text(&textarea).trim().is_empty()
                                && state.toggle_latest_tool_output()
                            {
                                state.scroll_to_bottom();
                            } else {
                                let changed = if vim_state.enabled {
                                    vim_state.handle(Input::from(ev), &mut textarea, &theme)
                                } else {
                                    textarea.input(Input::from(ev))
                                };
                                if changed {
                                    state.reset_history_navigation();
                                    update_slash_menu(&textarea, &mut state, &config);
                                    update_mention_candidates(&textarea, &mut state, &config);
                                }
                            }
                        }
                        None => {
                            let changed = if key.code == KeyCode::Tab {
                                let cwd = config
                                    .cwd
                                    .clone()
                                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                                let text = textarea_text(&textarea);
                                if let Some(completed) =
                                    mentions::complete_file_mention(&text, &cwd)
                                {
                                    textarea =
                                        make_textarea_with_text(&completed, &vim_state, &theme);
                                    true
                                } else {
                                    textarea.input(Input::from(ev))
                                }
                            } else if vim_state.enabled {
                                vim_state.handle(Input::from(ev), &mut textarea, &theme)
                            } else {
                                textarea.input(Input::from(ev))
                            };
                            if changed {
                                state.reset_history_navigation();
                                update_slash_menu(&textarea, &mut state, &config);
                                update_mention_candidates(&textarea, &mut state, &config);
                            }
                        }
                    }
                } else if state.status == AppStatus::Running {
                    if let Some(shortcut) = running_shortcut(*key) {
                        handle_running_shortcut(shortcut, &mut state, &action_tx, &cancel_token);
                    }
                }
            }
        }

        while let Ok(tui_event) = event_rx.try_recv() {
            // Auto-approve tools the user chose to "always allow" this session,
            // so a repeat request is granted without re-prompting.
            if let TuiEvent::ApprovalNeeded { tool, target, .. } = &tui_event
                && state.approval_is_allowlisted(tool, target.as_deref())
            {
                let _ = action_tx.send(UserAction::Approve(true));
                state.enter_running();
                continue;
            }
            let backtracked_prompt = match &tui_event {
                TuiEvent::Backtracked { prompt } => Some(prompt.clone()),
                _ => None,
            };
            let workflow_notification_turn_boundary =
                is_workflow_notification_turn_boundary(&tui_event);
            let batch_queued_workflow_notification = queue_workflow_terminal_notification(
                &tui_event,
                &pending_workflow_notifications,
                state.status == AppStatus::Running,
            );
            state.update(tui_event);
            if let Some(prompt) = batch_queued_workflow_notification {
                remove_pending_workflow_notification(&mut state, &prompt);
            }
            if let Some(prompt) = backtracked_prompt {
                vim_state.reset_insert(&mut textarea, &theme);
                textarea = make_textarea_with_text(&prompt, &vim_state, &theme);
            }
            if workflow_notification_turn_boundary {
                drain_pending_workflow_notifications(&mut state, &pending_workflow_notifications);
                submit_pending_workflow_notification(&mut state, &action_tx, false);
            } else {
                submit_pending_workflow_notification(&mut state, &action_tx, true);
            }
            if state.auto_scroll {
                state.scroll_to_bottom();
            }
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

fn submit_pending_workflow_notification(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    require_idle: bool,
) {
    if require_idle && state.status != AppStatus::Idle {
        return;
    }
    if let Some(prompt) = state.pending_workflow_notifications.pop_front() {
        state.enter_running();
        state.scroll_to_bottom();
        let _ = action_tx.send(UserAction::Submit(prompt));
    }
}

fn queue_workflow_terminal_notification(
    event: &TuiEvent,
    pending_notifications: &bridge::PendingWorkflowNotifications,
    batch_injection_enabled: bool,
) -> Option<String> {
    if !batch_injection_enabled {
        return None;
    }
    if let TuiEvent::WorkflowNotification { prompt, .. } = event
        && let Ok(mut queue) = pending_notifications.lock()
    {
        queue.push_back(prompt.clone());
        return Some(prompt.clone());
    }
    None
}

fn remove_pending_workflow_notification(state: &mut AppState, prompt: &str) {
    if let Some(index) = state
        .pending_workflow_notifications
        .iter()
        .position(|pending| pending == prompt)
    {
        state.pending_workflow_notifications.remove(index);
    }
}

fn drain_pending_workflow_notifications(
    state: &mut AppState,
    pending_notifications: &bridge::PendingWorkflowNotifications,
) {
    if let Ok(mut queue) = pending_notifications.lock() {
        while let Some(prompt) = queue.pop_front() {
            state.pending_workflow_notifications.push_back(prompt);
        }
    }
}

fn is_workflow_notification_turn_boundary(event: &TuiEvent) -> bool {
    matches!(event, TuiEvent::SessionCompleted { .. })
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

        assert!(handle_workflows_panel_key(KeyCode::Down, &mut state));
        assert_eq!(state.workflow_panel.selected, 1);

        assert!(handle_workflows_panel_key(KeyCode::Up, &mut state));
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

        assert!(handle_workflows_panel_key(KeyCode::Enter, &mut state));

        let dialog = state.approval_dialog.as_ref().expect("approval dialog");
        assert_eq!(dialog.background_task_id.as_deref(), Some("task-approval"));
        assert_eq!(state.status, AppStatus::WaitingApproval);
    }

    #[test]
    fn background_approval_resolution_sends_task_scoped_action() {
        let (mut state, rx) = test_state();
        let action_tx = state.event_tx.clone();
        state.approval_dialog = Some(crate::types::ApprovalDialog {
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
            Ok(UserAction::ResolveBackgroundApproval { task_id, approved })
                if task_id == "task-approval" && approved
        ));
        assert_eq!(state.status, AppStatus::Idle);
        assert!(state.approval_dialog.is_none());
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

        submit_background_approval_response_for_tui(Some(&registry), &task.id, false, &event_tx);

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
        Arc::new(Mutex::new(VecDeque::new()))
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
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::WorkflowTasksUpdated { tasks } => {
                        if let Some(task) = tasks.into_iter().find(|task| {
                            task.task_type == orca_core::task_types::TaskType::MainSession
                                && task.is_backgrounded
                                && task.status != orca_core::task_types::TaskStatus::Running
                        }) {
                            break task.status;
                        }
                    }
                    _ => {}
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
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::WorkflowTasksUpdated { tasks } => {
                        if let Some(task) = tasks.into_iter().find(|task| {
                            task.task_type == orca_core::task_types::TaskType::MainSession
                                && task.is_backgrounded
                                && task.status != orca_core::task_types::TaskStatus::Running
                        }) {
                            break task.status;
                        }
                    }
                    _ => {}
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
                match event_rx.recv_timeout(Duration::from_secs(2)).unwrap() {
                    TuiEvent::WorkflowTasksUpdated { tasks } => {
                        if let Some(task) = tasks.into_iter().find(|task| {
                            task.task_type == orca_core::task_types::TaskType::MainSession
                                && task.is_backgrounded
                                && task.status
                                    == orca_core::task_types::TaskStatus::ApprovalRequired
                        }) {
                            break task.pending_tool_call;
                        }
                    }
                    _ => {}
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
    fn idle_app_submits_pending_workflow_notification() {
        let (mut state, _rx) = test_state();
        let (action_tx, action_rx) = mpsc::channel();
        state
            .pending_workflow_notifications
            .push_back("<task-notification>done</task-notification>".to_string());

        submit_pending_workflow_notification(&mut state, &action_tx, true);

        assert_eq!(state.status, AppStatus::Running);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::Submit(prompt)) if prompt == "<task-notification>done</task-notification>"
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
            .push_back("<task-notification>failed</task-notification>".to_string());

        assert!(is_workflow_notification_turn_boundary(
            &TuiEvent::SessionCompleted {
                status: "success".to_string(),
            }
        ));
        submit_pending_workflow_notification(&mut state, &action_tx, false);

        assert_eq!(state.status, AppStatus::Running);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::Submit(prompt)) if prompt == "<task-notification>failed</task-notification>"
        ));
    }

    #[test]
    fn session_completion_drains_batch_boundary_queue_before_submitting_notification() {
        let (mut state, _rx) = test_state();
        let (action_tx, action_rx) = mpsc::channel();
        let queue = test_pending_workflow_notifications();
        queue
            .lock()
            .unwrap()
            .push_back("<task-notification>failed</task-notification>".to_string());
        state.status = AppStatus::Running;

        drain_pending_workflow_notifications(&mut state, &queue);
        submit_pending_workflow_notification(&mut state, &action_tx, false);

        assert!(queue.lock().unwrap().is_empty());
        assert!(state.pending_workflow_notifications.is_empty());
        assert_eq!(state.status, AppStatus::Running);
        assert!(matches!(
            action_rx.try_recv(),
            Ok(UserAction::Submit(prompt)) if prompt == "<task-notification>failed</task-notification>"
        ));
    }

    #[test]
    fn terminal_workflow_notifications_enter_batch_boundary_queue() {
        let queue = test_pending_workflow_notifications();
        let queued = queue_workflow_terminal_notification(
            &TuiEvent::WorkflowNotification {
                prompt: "<task-notification>done</task-notification>".to_string(),
                status: "completed".to_string(),
                summary: "done".to_string(),
            },
            &queue,
            true,
        );
        assert_eq!(
            queued.as_deref(),
            Some("<task-notification>done</task-notification>")
        );
        assert_eq!(
            queue.lock().unwrap().pop_front().as_deref(),
            Some("<task-notification>done</task-notification>")
        );

        let queued = queue_workflow_terminal_notification(
            &TuiEvent::WorkflowNotification {
                prompt: "<task-notification>failed</task-notification>".to_string(),
                status: "failed".to_string(),
                summary: "failed".to_string(),
            },
            &queue,
            true,
        );
        assert_eq!(
            queued.as_deref(),
            Some("<task-notification>failed</task-notification>")
        );
        assert_eq!(
            queue.lock().unwrap().pop_front().as_deref(),
            Some("<task-notification>failed</task-notification>")
        );

        let queued = queue_workflow_terminal_notification(
            &TuiEvent::WorkflowNotification {
                prompt: "<task-notification>failed</task-notification>".to_string(),
                status: "failed".to_string(),
                summary: "failed".to_string(),
            },
            &queue,
            false,
        );
        assert!(queued.is_none());
        assert!(queue.lock().unwrap().is_empty());
    }

    #[test]
    fn batch_queued_workflow_notification_is_removed_from_ui_pending_queue() {
        let (mut state, _rx) = test_state();
        state
            .pending_workflow_notifications
            .push_back("<task-notification>completed</task-notification>".to_string());
        state
            .pending_workflow_notifications
            .push_back("<task-notification>failed</task-notification>".to_string());

        remove_pending_workflow_notification(
            &mut state,
            "<task-notification>failed</task-notification>",
        );

        assert_eq!(
            state
                .pending_workflow_notifications
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["<task-notification>completed</task-notification>"]
        );
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

// --- Slash menu helpers ---

fn update_slash_menu(textarea: &TextArea, state: &mut AppState, config: &RunConfig) {
    let text = textarea_text(textarea);
    if textarea.lines().len() == 1 && text.starts_with('/') {
        let filter = &text;
        let cwd = config
            .cwd
            .as_deref()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let items: Vec<SlashMenuItem> = commands::available_commands(&cwd)
            .into_iter()
            .filter(|(cmd, _)| cmd.starts_with(filter))
            .map(|(cmd, desc)| SlashMenuItem {
                command: cmd,
                description: desc,
            })
            .collect();
        if items.is_empty() {
            state.slash_menu = None;
        } else {
            let selected = state
                .slash_menu
                .as_ref()
                .map(|m| m.selected.min(items.len().saturating_sub(1)))
                .unwrap_or(0);
            state.slash_menu = Some(SlashMenu {
                items,
                selected,
                sub_menu: None,
            });
        }
    } else {
        state.slash_menu = None;
    }
}

fn update_mention_candidates(textarea: &TextArea, state: &mut AppState, config: &RunConfig) {
    if state.slash_menu.is_some() {
        state.mention_candidates.clear();
        state.mention_selected = 0;
        return;
    }
    let text = textarea_text(textarea);
    let has_at_token = text.rfind('@').map_or(false, |pos| {
        pos == 0 || text.as_bytes()[pos - 1].is_ascii_whitespace()
    });
    if !has_at_token {
        state.mention_candidates.clear();
        state.mention_selected = 0;
        return;
    }
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let candidates = mentions::list_mention_candidates(&text, &cwd);
    if candidates != state.mention_candidates {
        state.mention_selected = 0;
    }
    state.mention_candidates = candidates;
}

fn handle_mention_menu_key(
    ev: &Event,
    key: &crossterm::event::KeyEvent,
    state: &mut AppState,
    config: &RunConfig,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) -> bool {
    match key.code {
        KeyCode::Up => {
            state.mention_selected = state.mention_selected.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            let max = state.mention_candidates.len().saturating_sub(1);
            if state.mention_selected < max {
                state.mention_selected += 1;
            }
            true
        }
        KeyCode::Tab | KeyCode::Enter => {
            if let Some(candidate) = state
                .mention_candidates
                .get(state.mention_selected)
                .cloned()
            {
                let text = textarea_text(textarea);
                let applied = mentions::apply_mention_selection(&text, &candidate);
                *textarea = make_textarea_with_text(&applied, vim_state, theme);
                state.mention_candidates.clear();
                state.mention_selected = 0;
                let cwd = config
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                if candidate.ends_with('/') {
                    state.mention_candidates = mentions::list_mention_candidates(&applied, &cwd);
                }
            }
            true
        }
        KeyCode::Esc => {
            state.mention_candidates.clear();
            state.mention_selected = 0;
            true
        }
        _ => {
            textarea.input(Input::from(ev.clone()));
            update_mention_candidates(textarea, state, config);
            true
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_slash_menu_key(
    ev: &Event,
    key: &crossterm::event::KeyEvent,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) -> bool {
    let menu = match &mut state.slash_menu {
        Some(m) => m,
        None => return false,
    };

    // Sub-menu mode
    if let Some(sub) = &mut menu.sub_menu {
        match key.code {
            KeyCode::Up => {
                sub.selected = sub.selected.saturating_sub(1);
                return true;
            }
            KeyCode::Down => {
                if sub.selected + 1 < sub.items.len() {
                    sub.selected += 1;
                }
                return true;
            }
            KeyCode::Tab | KeyCode::Enter => {
                let chosen = sub.items[sub.selected].clone();
                let title = sub.title.clone();
                let pending_model = sub.context.clone();
                // Execute the sub-command
                if title == "/model" {
                    // Step 1 of 2: remember the model, then ask for reasoning effort.
                    // Nothing is applied yet, so Esc still cancels the whole switch.
                    let chosen_model = chosen
                        .split_whitespace()
                        .next()
                        .unwrap_or(&chosen)
                        .to_string();
                    if let Ok(()) = commands::validate_model(&chosen_model) {
                        menu.sub_menu = Some(reasoning_effort_submenu(
                            chosen_model,
                            config.reasoning_effort,
                        ));
                        return true;
                    }
                } else if title == REASONING_SUBMENU_TITLE {
                    // Step 2 of 2: apply model + reasoning effort together.
                    if let (Some(model), Some(effort)) =
                        (pending_model, parse_reasoning_effort(&chosen))
                    {
                        config.model = ModelSelection::from_unchecked(Some(model.clone()));
                        config.reasoning_effort = effort;
                        if let Ok(mut cfg) = shared_config.lock() {
                            cfg.model = ModelSelection::from_unchecked(Some(model.clone()));
                            cfg.reasoning_effort = effort;
                        }
                        state.model_name = model.clone();
                        state.reasoning_effort = effort;
                        state.messages.push(ChatMessage::System(format!(
                            "Model switched to {model} (reasoning effort: {}).",
                            effort.as_str()
                        )));
                        let _ = action_tx.send(UserAction::SetModel(model));
                    }
                } else if title == "/mode" {
                    if let Some(mode) = parse_approval_mode(&chosen) {
                        config.approval_mode = mode;
                        if let Ok(mut cfg) = shared_config.lock() {
                            cfg.approval_mode = mode;
                        }
                        state.approval_mode = mode;
                        state.messages.push(ChatMessage::System(format!(
                            "Approval mode switched to {chosen}."
                        )));
                    }
                }
                state.slash_menu = None;
                *textarea = make_textarea(vim_state, theme);
                return true;
            }
            KeyCode::Esc => {
                state.slash_menu = None;
                *textarea = make_textarea(vim_state, theme);
                return true;
            }
            _ => return true,
        }
    }

    // Main menu mode
    match key.code {
        KeyCode::Up => {
            menu.selected = menu.selected.saturating_sub(1);
            true
        }
        KeyCode::Down => {
            if menu.selected + 1 < menu.items.len() {
                menu.selected += 1;
            }
            true
        }
        KeyCode::Tab => {
            let selected_cmd = menu.items[menu.selected].command.clone();
            if selected_cmd == "/goal" {
                *textarea = make_textarea_with_text("/goal ", vim_state, theme);
                state.slash_menu = None;
                return true;
            }
            select_slash_menu_command(
                selected_cmd,
                menu.items.clone(),
                menu.selected,
                state,
                config,
                shared_config,
                action_tx,
                textarea,
                vim_state,
                theme,
            );
            true
        }
        KeyCode::Enter => {
            let selected_cmd = menu.items[menu.selected].command.clone();
            select_slash_menu_command(
                selected_cmd,
                menu.items.clone(),
                menu.selected,
                state,
                config,
                shared_config,
                action_tx,
                textarea,
                vim_state,
                theme,
            );
            true
        }
        KeyCode::Esc => {
            state.slash_menu = None;
            true
        }
        _ => {
            // Pass key to textarea for filtering
            textarea.input(Input::from(ev.clone()));
            update_slash_menu(textarea, state, config);
            true
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn select_slash_menu_command(
    selected_cmd: String,
    menu_items: Vec<SlashMenuItem>,
    selected: usize,
    state: &mut AppState,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    action_tx: &mpsc::Sender<UserAction>,
    textarea: &mut TextArea,
    vim_state: &VimState,
    theme: &Theme,
) {
    match selected_cmd.as_str() {
        "/model" => {
            let models: Vec<String> = commands::available_models()
                .iter()
                .map(|s| match *s {
                    "auto" => "auto (pro + flash for aux)".to_string(),
                    other => other.to_string(),
                })
                .collect();
            state.slash_menu = Some(SlashMenu {
                items: menu_items,
                selected,
                sub_menu: Some(SubMenu {
                    title: "/model".to_string(),
                    items: models,
                    selected: 0,
                    context: None,
                }),
            });
        }
        "/mode" => {
            let modes = vec![
                "suggest".to_string(),
                "auto-edit".to_string(),
                "full-auto".to_string(),
                "plan".to_string(),
            ];
            state.slash_menu = Some(SlashMenu {
                items: menu_items,
                selected,
                sub_menu: Some(SubMenu {
                    title: "/mode".to_string(),
                    items: modes,
                    selected: 0,
                    context: None,
                }),
            });
        }
        "/remember" => {
            *textarea = make_textarea_with_text("/remember ", vim_state, theme);
            state.slash_menu = None;
        }
        "/history" => {
            state.slash_menu = None;
            *textarea = make_textarea(vim_state, theme);
            match orca_runtime::history::list_sessions(20) {
                Ok(sessions) if !sessions.is_empty() => {
                    state.session_picker_sessions = sessions;
                    state.session_picker_selected = 0;
                    state.status = AppStatus::SessionPicker;
                }
                Ok(_) => {
                    state
                        .messages
                        .push(ChatMessage::System("No saved sessions.".to_string()));
                }
                Err(e) => {
                    state
                        .messages
                        .push(ChatMessage::Error(format!("failed to list history: {e}")));
                }
            }
        }
        _ => {
            *textarea = make_textarea_with_text(&selected_cmd, vim_state, theme);
            state.slash_menu = None;
            if let Some(outcome) =
                handle_slash_command(&selected_cmd, config, shared_config, state, action_tx)
            {
                match outcome {
                    SlashOutcome::Continue => {
                        *textarea = make_textarea(vim_state, theme);
                    }
                }
            }
            *textarea = make_textarea(vim_state, theme);
        }
    }
}

fn make_textarea<'a>(vim_state: &VimState, theme: &Theme) -> TextArea<'a> {
    let mut textarea = TextArea::default();
    configure_textarea(&mut textarea, vim_state, theme);
    textarea
}

fn make_textarea_with_text<'a>(text: &str, vim_state: &VimState, theme: &Theme) -> TextArea<'a> {
    let lines: Vec<String> = if text.is_empty() {
        vec![String::new()]
    } else {
        text.lines().map(str::to_string).collect()
    };
    let mut textarea = TextArea::from(lines);
    configure_textarea(&mut textarea, vim_state, theme);
    textarea.move_cursor(CursorMove::Bottom);
    textarea.move_cursor(CursorMove::End);
    textarea
}

fn configure_textarea(textarea: &mut TextArea, vim_state: &VimState, theme: &Theme) {
    textarea.set_placeholder_text("Type a message... (Enter send, Alt+Enter newline)");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    vim_state.configure_block(textarea, theme);
}

fn textarea_text(textarea: &TextArea) -> String {
    textarea.lines().join("\n")
}

fn insert_pasted_text(textarea: &mut TextArea, pasted: &str) -> bool {
    if pasted.is_empty() {
        return false;
    }
    textarea.insert_str(pasted)
}

fn make_setup_textarea<'a>(theme: &Theme) -> TextArea<'a> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text("sk-...");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    textarea.set_mask_char('*');
    textarea.set_block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .title(" API Key ")
            .border_style(ratatui::style::Style::default().fg(theme.border)),
    );
    textarea
}

fn resolve_approval(state: &mut AppState, action_tx: &mpsc::Sender<UserAction>, approved: bool) {
    if let Some(task_id) = state
        .approval_dialog
        .as_ref()
        .and_then(|dialog| dialog.background_task_id.clone())
    {
        let _ = action_tx.send(UserAction::ResolveBackgroundApproval { task_id, approved });
        state.set_status(AppStatus::Idle);
    } else {
        let _ = action_tx.send(UserAction::Approve(approved));
        if approved {
            state.enter_running();
        } else {
            state.set_status(AppStatus::Idle);
        }
    }
    state.approval_dialog = None;
}

fn handle_workflows_panel_key(key_code: KeyCode, state: &mut AppState) -> bool {
    if state.panel_mode != PanelMode::Workflows {
        return false;
    }

    match key_code {
        KeyCode::Up => {
            state.select_previous_workflow_task();
            true
        }
        KeyCode::Down => {
            state.select_next_workflow_task();
            true
        }
        KeyCode::Enter => state.open_selected_background_approval_dialog(),
        _ => false,
    }
}

fn handle_running_shortcut(
    shortcut: RunningShortcut,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    cancel_token: &CancelToken,
) {
    match shortcut {
        RunningShortcut::BackgroundCurrentTurn => {
            let _ = action_tx.send(UserAction::BackgroundCurrentTurn);
            state.set_status(AppStatus::Idle);
        }
        RunningShortcut::Interrupt => {
            cancel_token.cancel();
            let _ = action_tx.send(UserAction::Interrupt);
        }
        RunningShortcut::ScrollUp => {
            state.scroll_up(1);
        }
        RunningShortcut::ScrollDown => {
            state.scroll_down(1);
        }
        RunningShortcut::PageUp => {
            let page = state.visible_height.saturating_sub(2);
            state.scroll_up(page);
        }
        RunningShortcut::PageDown => {
            let page = state.visible_height.saturating_sub(2);
            state.scroll_down(page);
        }
        RunningShortcut::HalfPageUp => {
            let page = state.visible_height / 2;
            state.scroll_up(page);
        }
        RunningShortcut::HalfPageDown => {
            let page = state.visible_height / 2;
            state.scroll_down(page);
        }
    }
}

/// Resolve the approval dialog by the chosen option. The "always allow"
/// options record a session allowlist entry so later matching approvals are
/// auto-granted (see the ApprovalNeeded handling in the event loop). The wire
/// protocol stays a simple allow/deny bool.
fn resolve_approval_option(
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
    option: ApprovalOption,
) {
    if let Some(dialog) = &state.approval_dialog {
        match option {
            ApprovalOption::AlwaysTool => {
                state
                    .approval_allowlist
                    .insert(AppState::approval_key_tool(&dialog.tool));
            }
            ApprovalOption::AlwaysTarget => {
                if let Some(target) = &dialog.target {
                    state
                        .approval_allowlist
                        .insert(AppState::approval_key_target(&dialog.tool, target));
                }
            }
            ApprovalOption::Once | ApprovalOption::Deny => {}
        }
    }
    resolve_approval(state, action_tx, option.is_approve());
}

fn submit_background_approval_response_for_tui(
    task_registry: Option<&orca_runtime::tasks::TaskRegistry>,
    task_id: &str,
    approved: bool,
    event_tx: &mpsc::Sender<TuiEvent>,
) {
    let Some(task_registry) = task_registry else {
        let _ = event_tx.send(TuiEvent::Error(
            "cannot resolve background approval before a session exists".to_string(),
        ));
        return;
    };

    match task_registry.submit_pending_tool_approval_response(task_id, approved) {
        Ok(()) => {
            if !approved
                && let Err(error) = task_registry.finish_denied_pending_tool_approval(task_id)
            {
                let _ = event_tx.send(TuiEvent::Error(error));
                return;
            }
            let _ = event_tx.send(TuiEvent::WorkflowTasksUpdated {
                tasks: task_registry.list(),
            });
            let decision = if approved { "approved" } else { "denied" };
            let _ = event_tx.send(TuiEvent::Notice(format!(
                "Background approval {decision} for {task_id}."
            )));
        }
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
        }
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
    initial_prompt: &str,
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

    let mut prompt = initial_prompt.to_string();
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
        let turn_result = bridge::run_agent_for_tui_with_notification_queue(
            config,
            session,
            &prompt,
            event_tx,
            action_rx,
            pending_actions,
            cancel,
            true,
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
        if let Some(next_prompt) = turn_result.next_prompt {
            // Workflow failure notifications are diagnostic follow-ups for the turn that just
            // finished, so they do not consume goal-continuation quota or wait for the next
            // goal-status poll.
            prompt = next_prompt;
            continue;
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
        prompt = goal_continuation_prompt(&goal.objective, continuation);
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
    let _ = event_tx.send(TuiEvent::GoalUpdated(active_goal.clone()));
    let _ = event_tx.send(TuiEvent::Notice(
        "Resumed latest active goal in a restored session.".to_string(),
    ));

    if let Some(session) = session.as_mut() {
        let prompt = goal_continuation_prompt(&active_goal.objective, 1);
        run_goal_turns_for_tui(
            &cfg,
            session,
            &prompt,
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
                cancel.reset();
                let cfg = config.lock().unwrap().clone();
                let cwd = cfg
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let prompt = match mentions::expand_file_mentions(&prompt, &cwd) {
                    Ok(prompt) => prompt,
                    Err(error) => {
                        let _ = event_tx.send(TuiEvent::Error(error));
                        continue;
                    }
                };
                if session.is_none() {
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
                }
                if let Some(session) = session.as_mut() {
                    for context in pending_pinned_context.drain(..) {
                        session.add_pinned_context(context);
                    }
                }
                run_goal_turns_for_tui(
                    &cfg,
                    session.as_mut().expect("session initialized"),
                    &prompt,
                    &event_tx,
                    &action_rx,
                    &pending_actions,
                    &cancel,
                    0,
                    &pending_workflow_notifications,
                );
                if cfg.desktop_notifications {
                    let _ = orca_runtime::notify::notify("Orca", "Task completed");
                }
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
            Ok(UserAction::ResolveBackgroundApproval { task_id, approved }) => {
                submit_background_approval_response_for_tui(
                    session.as_ref().map(|session| session.task_registry()),
                    &task_id,
                    approved,
                    &event_tx,
                );
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
                                &objective,
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
                            &prompt,
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
            Ok(UserAction::Approve(_) | UserAction::RespondToUserInput(_)) => {}
        }
    }
}

enum SlashOutcome {
    Continue,
}

fn handle_slash_command(
    text: &str,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> Option<SlashOutcome> {
    let cwd = config
        .cwd
        .as_deref()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let command = commands::parse_with_cwd(text, &cwd)?;
    match command {
        SlashCommand::Model(Some(model)) => match commands::validate_model(&model) {
            Ok(()) => {
                config.model = ModelSelection::from_unchecked(Some(model.clone()));
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.model = ModelSelection::from_unchecked(Some(model.clone()));
                }
                state.model_name = model.clone();
                state
                    .messages
                    .push(ChatMessage::System(format!("Model switched to {model}.")));
                let _ = action_tx.send(UserAction::SetModel(model));
            }
            Err(error) => state.messages.push(ChatMessage::Error(error)),
        },
        SlashCommand::Model(None) => {
            state.messages.push(ChatMessage::System(format!(
                "Current model: {} (reasoning effort: {}). Use the /model menu to change both.",
                state.model_name,
                state.reasoning_effort.as_str()
            )));
        }
        SlashCommand::Cost => {
            state.messages.push(ChatMessage::System(format!(
                "Session usage: {} input, {} output, {} cache tokens, estimated ${:.6}.",
                state.usage.input_tokens,
                state.usage.output_tokens,
                state.usage.cache_tokens,
                state.usage.estimated_cost_usd
            )));
        }
        SlashCommand::ConfigShow => {
            state
                .messages
                .push(ChatMessage::System(orca_core::config::format_config_show(
                    config,
                )));
        }
        SlashCommand::Mode(Some(mode)) => match parse_approval_mode(&mode) {
            Some(approval_mode) => {
                config.approval_mode = approval_mode;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = approval_mode;
                }
                state.approval_mode = approval_mode;
                state.messages.push(ChatMessage::System(format!(
                    "Approval mode switched to {mode}."
                )));
            }
            None => state.messages.push(ChatMessage::Error(
                "unsupported mode. Use suggest, auto-edit, full-auto, or plan.".to_string(),
            )),
        },
        SlashCommand::Mode(None) => {
            state.messages.push(ChatMessage::System(format!(
                "Current mode: {}",
                config.approval_mode.as_str()
            )));
        }
        SlashCommand::Plan(arg) => match arg.as_deref() {
            Some("off") => {
                config.approval_mode = ApprovalMode::Suggest;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = ApprovalMode::Suggest;
                }
                state.approval_mode = ApprovalMode::Suggest;
                state
                    .messages
                    .push(ChatMessage::System("Plan mode disabled.".to_string()));
            }
            None => {
                config.approval_mode = ApprovalMode::Plan;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = ApprovalMode::Plan;
                }
                state.approval_mode = ApprovalMode::Plan;
                state
                    .messages
                    .push(ChatMessage::System("Plan mode enabled.".to_string()));
            }
            Some(_) => state.messages.push(ChatMessage::Error(
                "unsupported plan command. Use /plan or /plan off.".to_string(),
            )),
        },
        SlashCommand::Goal(goal_command) => {
            let action = match goal_command {
                GoalSlashCommand::Show => UserAction::GoalShow,
                GoalSlashCommand::Set(objective) => UserAction::GoalSet(objective),
                GoalSlashCommand::Edit(objective) => UserAction::GoalEdit(objective),
                GoalSlashCommand::Clear => UserAction::GoalClear,
                GoalSlashCommand::Pause => UserAction::GoalPause,
                GoalSlashCommand::Resume => UserAction::GoalResume,
            };
            state.enter_running();
            let _ = action_tx.send(action);
        }
        SlashCommand::WorkflowList => {
            state.show_workflows();
        }
        SlashCommand::WorkflowRun { name, args } => {
            state.enter_running();
            let _ = action_tx.send(UserAction::RunWorkflow { name, args });
        }
        SlashCommand::AgentDashboard => {
            state.show_agents();
        }
        SlashCommand::Remember(note) => {
            let remembered_note = note
                .strip_prefix("project:")
                .map(str::trim)
                .unwrap_or(note.as_str())
                .to_string();
            let result = if let Some(project_note) = note.strip_prefix("project:") {
                let cwd = config
                    .cwd
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                orca_runtime::memory::remember_project(&cwd, project_note)
            } else {
                orca_runtime::memory::remember_user(&note)
            };
            match &result {
                Ok(path) => state.messages.push(ChatMessage::System(format!(
                    "Remembered in {}.",
                    path.display()
                ))),
                Err(error) => state
                    .messages
                    .push(ChatMessage::Error(format!("failed to remember: {error}"))),
            }
            if result.is_ok() {
                let _ = action_tx.send(UserAction::Remember(remembered_note));
            }
        }
        SlashCommand::Compact => {
            state.enter_running();
            let _ = action_tx.send(UserAction::Compact);
        }
        SlashCommand::History => match history::list_sessions(10) {
            Ok(sessions) if sessions.is_empty() => state
                .messages
                .push(ChatMessage::System("No saved sessions.".to_string())),
            Ok(sessions) => {
                let summary = sessions
                    .into_iter()
                    .map(|session| {
                        format!(
                            "{}  {}  {}",
                            session.session_id,
                            session.updated_at.format("%Y-%m-%d %H:%M"),
                            session.title
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                state
                    .messages
                    .push(ChatMessage::System(format!("Recent sessions:\n{summary}")));
            }
            Err(error) => state.messages.push(ChatMessage::Error(format!(
                "failed to list history: {error}"
            ))),
        },
    }
    state.scroll_to_bottom();
    Some(SlashOutcome::Continue)
}

fn parse_approval_mode(mode: &str) -> Option<ApprovalMode> {
    match mode {
        "suggest" => Some(ApprovalMode::Suggest),
        "auto-edit" => Some(ApprovalMode::AutoEdit),
        "full-auto" => Some(ApprovalMode::FullAuto),
        "plan" => Some(ApprovalMode::Plan),
        _ => None,
    }
}

/// Title of step 2 of the `/model` picker. The submenu key handler dispatches on it.
const REASONING_SUBMENU_TITLE: &str = "/model · reasoning effort";

/// Step 2 of the `/model` picker: choose reasoning effort for the model picked in
/// step 1 (carried in `context`). Pre-selects the currently configured effort.
fn reasoning_effort_submenu(
    pending_model: String,
    current: orca_core::config::ReasoningEffort,
) -> SubMenu {
    let items: Vec<String> = reasoning_effort_options()
        .iter()
        .map(|(effort, description)| format!("{} {description}", effort.as_str()))
        .collect();
    let selected = reasoning_effort_options()
        .iter()
        .position(|(effort, _)| *effort == current)
        .unwrap_or(0);
    SubMenu {
        title: REASONING_SUBMENU_TITLE.to_string(),
        items,
        selected,
        context: Some(pending_model),
    }
}

fn reasoning_effort_options() -> &'static [(orca_core::config::ReasoningEffort, &'static str)] {
    use orca_core::config::ReasoningEffort;
    &[
        (ReasoningEffort::High, "(faster, lighter reasoning)"),
        (ReasoningEffort::Max, "(deepest reasoning, default)"),
    ]
}

fn parse_reasoning_effort(choice: &str) -> Option<orca_core::config::ReasoningEffort> {
    let token = choice.split_whitespace().next().unwrap_or(choice);
    reasoning_effort_options()
        .iter()
        .find(|(effort, _)| effort.as_str() == token)
        .map(|(effort, _)| *effort)
}

fn chat_message_from_history(message: Message) -> Option<ChatMessage> {
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
