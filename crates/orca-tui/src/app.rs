use std::io;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::ExecutableCommand;
use crossterm::event::{
    self, Event, KeyCode, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
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
    AppState, AppStatus, ChatMessage, PanelMode, SlashMenu, SlashMenuItem, SubMenu, TuiEvent,
    UserAction,
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
    stdout.execute(EnterAlternateScreen)?;
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
    let mut terminal = Terminal::new(backend)?;

    let (event_tx, event_rx) = mpsc::channel::<TuiEvent>();
    let (action_tx, action_rx) = mpsc::channel::<UserAction>();

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

    if matches!(
        config.history_mode,
        HistoryMode::Resume(_) | HistoryMode::Fork(_)
    ) {
        if let Ok(transcript) = orca_runtime::history::load_session(match &config.history_mode {
            HistoryMode::Resume(selector) | HistoryMode::Fork(selector) => selector,
            HistoryMode::Record | HistoryMode::Disabled => "",
        }) {
            for message in transcript.messages {
                if let Some(chat_message) = chat_message_from_history(message) {
                    state.messages.push(chat_message);
                }
            }
            if let Some((explanation, plan)) = transcript.plan {
                state.current_plan = Some((explanation, plan));
            }
            if !state.messages.is_empty() {
                let label = if matches!(config.history_mode, HistoryMode::Fork(_)) {
                    "Forked saved conversation."
                } else {
                    "Resumed saved conversation."
                };
                state.messages.push(ChatMessage::System(label.to_string()));
            }
        }
    }

    let shared_config = Arc::new(Mutex::new(config.clone()));
    let agent_config = Arc::clone(&shared_config);
    let preloaded_transcript: Arc<Mutex<Option<history::SessionTranscript>>> =
        Arc::new(Mutex::new(None));
    let agent_preloaded = Arc::clone(&preloaded_transcript);
    let agent_event_tx = event_tx.clone();
    let cancel_token = CancelToken::new();
    let agent_cancel = cancel_token.clone();

    let _agent_handle = std::thread::spawn(move || {
        agent_loop_thread(
            agent_config,
            agent_preloaded,
            agent_event_tx,
            action_rx,
            agent_cancel,
        );
    });

    let mut vim_state = VimState::new(config.vim_mode);
    let mut textarea = if needs_setup {
        make_setup_textarea(&theme)
    } else {
        if let Some(prompt) = initial_prompt.clone() {
            state.messages.push(ChatMessage::User(prompt.clone()));
            state.status = AppStatus::Running;
            let _ = action_tx.send(UserAction::Submit(prompt));
        }
        make_textarea(&vim_state, &theme)
    };

    let exit_code;

    loop {
        state.advance_tick();
        terminal.draw(|f| ui::render(f, &mut state, &textarea, &theme))?;

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;

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
                            state.scroll_offset = 0;
                            state.auto_scroll = true;
                            continue;
                        }
                    }
                }

                if state.show_shortcuts && key.code == KeyCode::Esc {
                    state.show_shortcuts = false;
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
                                    state.status = AppStatus::Idle;
                                    state.setup_step = 0;
                                    textarea = make_textarea(&vim_state, &theme);

                                    if let Some(prompt) = initial_prompt.clone() {
                                        state.messages.push(ChatMessage::User(prompt.clone()));
                                        state.status = AppStatus::Running;
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
                                    if let Ok(mut preloaded) = preloaded_transcript.lock() {
                                        *preloaded = Some(transcript);
                                    }
                                }
                                state.status = AppStatus::Idle;
                            }
                        }
                        KeyCode::Esc => {
                            state.status = AppStatus::Idle;
                            state.session_picker_sessions.clear();
                            state.session_picker_query.clear();
                        }
                        _ => {}
                    }
                    continue;
                }

                // Approval dialog: selection and direct approve/deny shortcuts
                if state.status == AppStatus::WaitingApproval {
                    match approval_shortcut(*key) {
                        Some(ApprovalShortcut::SelectAllow) => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                dialog.selected = 0;
                            }
                        }
                        Some(ApprovalShortcut::SelectDeny) => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                dialog.selected = 1;
                            }
                        }
                        Some(ApprovalShortcut::ToggleSelection) => {
                            if let Some(dialog) = &mut state.approval_dialog {
                                dialog.selected = 1usize.saturating_sub(dialog.selected);
                            }
                        }
                        Some(ApprovalShortcut::Confirm) => {
                            let approved = state
                                .approval_dialog
                                .as_ref()
                                .map(|d| d.selected == 0)
                                .unwrap_or(false);
                            resolve_approval(&mut state, &action_tx, approved);
                        }
                        Some(ApprovalShortcut::Approve) => {
                            resolve_approval(&mut state, &action_tx, true);
                        }
                        Some(ApprovalShortcut::Deny) => {
                            resolve_approval(&mut state, &action_tx, false);
                        }
                        None => {}
                    }
                    continue;
                }

                // Normal Idle mode input
                if state.status == AppStatus::Idle {
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
                                        SlashOutcome::Exit(code) => {
                                            exit_code = code;
                                            break;
                                        }
                                    }
                                }
                                state.record_prompt(text.clone());
                                state.messages.push(ChatMessage::User(text.clone()));
                                state.status = AppStatus::Running;
                                state.scroll_to_bottom();
                                let _ = action_tx.send(UserAction::Submit(text));
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
                                    update_slash_menu(&textarea, &mut state);
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
                                update_slash_menu(&textarea, &mut state);
                                update_mention_candidates(&textarea, &mut state, &config);
                            }
                        }
                    }
                } else if state.status == AppStatus::Running {
                    match running_shortcut(*key) {
                        Some(RunningShortcut::Interrupt) => {
                            cancel_token.cancel();
                            let _ = action_tx.send(UserAction::Interrupt);
                        }
                        Some(RunningShortcut::ScrollUp) => {
                            state.scroll_up(1);
                        }
                        Some(RunningShortcut::ScrollDown) => {
                            state.scroll_down(1);
                        }
                        Some(RunningShortcut::PageUp) => {
                            let page = state.visible_height.saturating_sub(2);
                            state.scroll_up(page);
                        }
                        Some(RunningShortcut::PageDown) => {
                            let page = state.visible_height.saturating_sub(2);
                            state.scroll_down(page);
                        }
                        Some(RunningShortcut::HalfPageUp) => {
                            let page = state.visible_height / 2;
                            state.scroll_up(page);
                        }
                        Some(RunningShortcut::HalfPageDown) => {
                            let page = state.visible_height / 2;
                            state.scroll_down(page);
                        }
                        None => {}
                    }
                }
            }
        }

        while let Ok(tui_event) = event_rx.try_recv() {
            let backtracked_prompt = match &tui_event {
                TuiEvent::Backtracked { prompt } => Some(prompt.clone()),
                _ => None,
            };
            state.update(tui_event);
            if let Some(prompt) = backtracked_prompt {
                vim_state.reset_insert(&mut textarea, &theme);
                textarea = make_textarea_with_text(&prompt, &vim_state, &theme);
            }
            if state.auto_scroll {
                state.scroll_to_bottom();
            }
        }
    }

    // Restore order: pop keyboard enhancement first, then leave alternate screen
    if kbd_enhanced {
        let _ = io::stdout().execute(PopKeyboardEnhancementFlags);
    }
    io::stdout().execute(LeaveAlternateScreen)?;
    terminal::disable_raw_mode()?;

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

// --- Slash menu helpers ---

fn update_slash_menu(textarea: &TextArea, state: &mut AppState) {
    let text = textarea_text(textarea);
    if textarea.lines().len() == 1 && text.starts_with('/') {
        let filter = &text;
        let items: Vec<SlashMenuItem> = commands::all_commands()
            .iter()
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
            KeyCode::Tab => {
                let chosen = sub.items[sub.selected].clone();
                let value = chosen.split_whitespace().next().unwrap_or(&chosen);
                *textarea =
                    make_textarea_with_text(&format!("{} {value}", sub.title), vim_state, theme);
                state.slash_menu = None;
                return true;
            }
            KeyCode::Enter => {
                let chosen = sub.items[sub.selected].clone();
                // Execute the sub-command
                if sub.title == "/model" {
                    let chosen_model = chosen
                        .split_whitespace()
                        .next()
                        .unwrap_or(&chosen)
                        .to_string();
                    if let Ok(()) = commands::validate_model(&chosen_model) {
                        config.model = ModelSelection::from_unchecked(Some(chosen_model.clone()));
                        if let Ok(mut cfg) = shared_config.lock() {
                            cfg.model = ModelSelection::from_unchecked(Some(chosen_model.clone()));
                        }
                        state.model_name = chosen_model.clone();
                        state.messages.push(ChatMessage::System(format!(
                            "Model switched to {chosen_model}."
                        )));
                        let _ = action_tx.send(UserAction::SetModel(chosen_model));
                    }
                } else if sub.title == "/mode" {
                    if let Some(mode) = parse_approval_mode(&chosen) {
                        config.approval_mode = mode;
                        if let Ok(mut cfg) = shared_config.lock() {
                            cfg.approval_mode = mode;
                        }
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
            let selected_cmd = menu.items[menu.selected].command;
            // Tab acts like Enter — directly execute the selected command
            match selected_cmd {
                "/model" => {
                    let models: Vec<String> = commands::available_models()
                        .iter()
                        .map(|s| match *s {
                            "auto" => "auto (pro + flash for aux)".to_string(),
                            other => other.to_string(),
                        })
                        .collect();
                    state.slash_menu = Some(SlashMenu {
                        items: menu.items.clone(),
                        selected: menu.selected,
                        sub_menu: Some(SubMenu {
                            title: "/model".to_string(),
                            items: models,
                            selected: 0,
                        }),
                    });
                }
                "/mode" => {
                    let modes = vec![
                        "suggest".to_string(),
                        "auto-edit".to_string(),
                        "full-auto".to_string(),
                    ];
                    state.slash_menu = Some(SlashMenu {
                        items: menu.items.clone(),
                        selected: menu.selected,
                        sub_menu: Some(SubMenu {
                            title: "/mode".to_string(),
                            items: modes,
                            selected: 0,
                        }),
                    });
                }
                "/remember" => {
                    *textarea = make_textarea_with_text("/remember ", vim_state, theme);
                    state.slash_menu = None;
                }
                _ => {
                    *textarea = make_textarea_with_text(selected_cmd, vim_state, theme);
                    state.slash_menu = None;
                    if let Some(outcome) =
                        handle_slash_command(selected_cmd, config, shared_config, state, action_tx)
                    {
                        match outcome {
                            SlashOutcome::Continue => {
                                *textarea = make_textarea(vim_state, theme);
                            }
                            SlashOutcome::Exit(_) => {}
                        }
                    }
                    *textarea = make_textarea(vim_state, theme);
                }
            }
            true
        }
        KeyCode::Enter => {
            let selected_cmd = menu.items[menu.selected].command;
            match selected_cmd {
                "/model" => {
                    let models: Vec<String> = commands::available_models()
                        .iter()
                        .map(|s| match *s {
                            "auto" => "auto (pro + flash for aux)".to_string(),
                            other => other.to_string(),
                        })
                        .collect();
                    state.slash_menu = Some(SlashMenu {
                        items: menu.items.clone(),
                        selected: menu.selected,
                        sub_menu: Some(SubMenu {
                            title: "/model".to_string(),
                            items: models,
                            selected: 0,
                        }),
                    });
                }
                "/mode" => {
                    let modes = vec![
                        "suggest".to_string(),
                        "auto-edit".to_string(),
                        "full-auto".to_string(),
                    ];
                    state.slash_menu = Some(SlashMenu {
                        items: menu.items.clone(),
                        selected: menu.selected,
                        sub_menu: Some(SubMenu {
                            title: "/mode".to_string(),
                            items: modes,
                            selected: 0,
                        }),
                    });
                }
                "/remember" => {
                    // Leave "/remember " in textarea for user to type the note
                    *textarea = make_textarea_with_text("/remember ", vim_state, theme);
                    state.slash_menu = None;
                }
                "/history" => {
                    // Open session picker directly
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
                    // Fill command into textarea and submit
                    *textarea = make_textarea_with_text(selected_cmd, vim_state, theme);
                    state.slash_menu = None;
                    // Auto-execute the command
                    if let Some(outcome) =
                        handle_slash_command(selected_cmd, config, shared_config, state, action_tx)
                    {
                        match outcome {
                            SlashOutcome::Continue => {
                                *textarea = make_textarea(vim_state, theme);
                            }
                            SlashOutcome::Exit(_) => {}
                        }
                    }
                    *textarea = make_textarea(vim_state, theme);
                }
            }
            true
        }
        KeyCode::Esc => {
            state.slash_menu = None;
            true
        }
        _ => {
            // Pass key to textarea for filtering
            textarea.input(Input::from(ev.clone()));
            update_slash_menu(textarea, state);
            true
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
    let _ = action_tx.send(UserAction::Approve(approved));
    if approved {
        state.status = AppStatus::Running;
    } else {
        state.status = AppStatus::Idle;
    }
    state.approval_dialog = None;
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

const MAX_GOAL_CONTINUATIONS: usize = 64;

fn goal_continuation_prompt(objective: &str, continuation: usize) -> String {
    format!(
        "[Goal continuation #{continuation}]\nContinue working on this persistent goal:\n{objective}\n\nIf the goal is complete, call update_goal with status \"complete\". If it is genuinely blocked, call update_goal with status \"blocked\" and explain why."
    )
}

fn run_goal_turns_for_tui(
    config: &RunConfig,
    session: &mut bridge::TuiConversationSession,
    initial_prompt: &str,
    event_tx: &mpsc::Sender<TuiEvent>,
    action_rx: &mpsc::Receiver<UserAction>,
    cancel: &CancelToken,
    starting_continuation: usize,
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
        let status =
            bridge::run_agent_for_tui(config, session, &prompt, event_tx, action_rx, cancel);
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
            break;
        }
        continuation += 1;
        if continuation > MAX_GOAL_CONTINUATIONS {
            update_goal_status_for_session(
                Some(&session_id),
                orca_core::goal_types::ThreadGoalStatus::UsageLimited,
                event_tx,
            );
            break;
        }
        prompt = goal_continuation_prompt(&goal.objective, continuation);
    }
}

fn agent_loop_thread(
    config: Arc<Mutex<RunConfig>>,
    preloaded: Arc<Mutex<Option<history::SessionTranscript>>>,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
    cancel: CancelToken,
) {
    let mut session: Option<bridge::TuiConversationSession> = None;
    let mut pending_pinned_context: Vec<String> = Vec::new();

    loop {
        match action_rx.recv() {
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
                    &cancel,
                    0,
                );
                if cfg.desktop_notifications {
                    let _ = orca_runtime::notify::notify("Orca", "Task completed");
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
            Ok(UserAction::GoalShow) => {
                let Some(session_id) = session
                    .as_ref()
                    .and_then(|s| s.session_id().map(str::to_string))
                else {
                    let _ = event_tx.send(TuiEvent::Error(
                        "persistent goals require a saved session; send a prompt first with history enabled".to_string(),
                    ));
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
                                &cfg, session, &objective, &event_tx, &action_rx, &cancel, 0,
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
                let Some(session_id) = session
                    .as_ref()
                    .and_then(|s| s.session_id().map(str::to_string))
                else {
                    let _ = event_tx.send(TuiEvent::Error(
                        "persistent goals require a saved session before editing".to_string(),
                    ));
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
                let Some(session_id) = session
                    .as_ref()
                    .and_then(|s| s.session_id().map(str::to_string))
                else {
                    let _ = event_tx.send(TuiEvent::Error(
                        "persistent goals require a saved session before clearing".to_string(),
                    ));
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
                update_goal_status_for_session(
                    session.as_ref().and_then(|s| s.session_id()),
                    orca_core::goal_types::ThreadGoalStatus::Paused,
                    &event_tx,
                );
            }
            Ok(UserAction::GoalResume) => {
                update_goal_status_for_session(
                    session.as_ref().and_then(|s| s.session_id()),
                    orca_core::goal_types::ThreadGoalStatus::Active,
                    &event_tx,
                );
                if let Some(session) = session.as_mut() {
                    if let Some(goal) = session
                        .session_id()
                        .and_then(|id| orca_runtime::goals::GoalStore::load_default().get(id).ok())
                        .flatten()
                    {
                        let cfg = config.lock().unwrap().clone();
                        let prompt = goal_continuation_prompt(&goal.objective, 1);
                        run_goal_turns_for_tui(
                            &cfg, session, &prompt, &event_tx, &action_rx, &cancel, 1,
                        );
                    }
                }
            }
            Ok(UserAction::Cancel) | Err(_) => break,
            Ok(UserAction::Approve(_)) => {}
        }
    }
}

enum SlashOutcome {
    Continue,
    Exit(i32),
}

fn handle_slash_command(
    text: &str,
    config: &mut RunConfig,
    shared_config: &Arc<Mutex<RunConfig>>,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> Option<SlashOutcome> {
    let command = commands::parse(text)?;
    match command {
        SlashCommand::Help => {
            state.messages.push(ChatMessage::System(
                "/help /model <name> /compact /clear /cost /config show /history /mode <suggest|auto-edit|full-auto> /plan [off] /workflows /remember <note> /exit"
                    .to_string(),
            ));
        }
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
                "Current model: {}",
                state.model_name
            )));
        }
        SlashCommand::Clear => {
            state.messages.clear();
            state.scroll_offset = 0;
            state.auto_scroll = true;
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
                state.messages.push(ChatMessage::System(format!(
                    "Approval mode switched to {mode}."
                )));
            }
            None => state.messages.push(ChatMessage::Error(
                "unsupported mode. Use suggest, auto-edit, or full-auto.".to_string(),
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
                state
                    .messages
                    .push(ChatMessage::System("Plan mode disabled.".to_string()));
            }
            None => {
                config.approval_mode = ApprovalMode::Plan;
                if let Ok(mut cfg) = shared_config.lock() {
                    cfg.approval_mode = ApprovalMode::Plan;
                }
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
            state.status = AppStatus::Running;
            let _ = action_tx.send(action);
        }
        SlashCommand::WorkflowList => {
            state.show_workflows();
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
            state.status = AppStatus::Running;
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
        SlashCommand::Exit => return Some(SlashOutcome::Exit(0)),
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
            expanded: false,
        }),
    }
}
