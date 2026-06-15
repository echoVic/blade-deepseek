use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tui_textarea::{Input, TextArea};

use crate::config::RunConfig;
use crate::tui::bridge;
use crate::tui::types::{AppState, AppStatus, ChatMessage, TuiEvent, UserAction};
use crate::tui::ui;

pub fn run_tui(config: RunConfig) -> i32 {
    match run_tui_inner(config) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("TUI error: {e}");
            1
        }
    }
}

fn run_tui_inner(config: RunConfig) -> io::Result<i32> {
    terminal::enable_raw_mode()?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (event_tx, event_rx) = mpsc::channel::<TuiEvent>();
    let (action_tx, action_rx) = mpsc::channel::<UserAction>();

    let model_name = config
        .model
        .clone()
        .unwrap_or_else(|| "deepseek-v4-flash".to_string());

    let mut state = AppState::new(action_tx.clone(), model_name);

    let initial_prompt = if config.prompt.trim().is_empty() {
        None
    } else {
        Some(config.prompt.clone())
    };

    let agent_config = config.clone();
    let agent_event_tx = event_tx.clone();
    thread::spawn(move || {
        agent_loop_thread(agent_config, agent_event_tx, action_rx);
    });

    if let Some(prompt) = initial_prompt {
        state.messages.push(ChatMessage::User(prompt.clone()));
        state.status = AppStatus::Running;
        let _ = action_tx.send(UserAction::Submit(prompt));
    }

    let mut textarea = make_textarea();
    let exit_code;

    loop {
        terminal.draw(|f| ui::render(f, &state, &textarea))?;

        if event::poll(Duration::from_millis(50))? {
            let ev = event::read()?;

            if let Event::Key(key) = &ev {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c')
                {
                    let _ = action_tx.send(UserAction::Cancel);
                    exit_code = 130;
                    break;
                }

                if state.status == AppStatus::WaitingApproval {
                    match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            let _ = action_tx.send(UserAction::Approve(true));
                            state.status = AppStatus::Running;
                            state.approval_info = None;
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') => {
                            let _ = action_tx.send(UserAction::Approve(false));
                            state.status = AppStatus::Idle;
                            state.approval_info = None;
                        }
                        _ => {}
                    }
                    continue;
                }

                if state.status == AppStatus::Idle {
                    match key.code {
                        KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                            let lines: Vec<String> = textarea.lines().to_vec();
                            let text = lines.join("\n").trim().to_string();
                            if !text.is_empty() {
                                state.messages.push(ChatMessage::User(text.clone()));
                                state.status = AppStatus::Running;
                                let _ = action_tx.send(UserAction::Submit(text));
                                textarea = make_textarea();
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
            }
        }

        while let Ok(tui_event) = event_rx.try_recv() {
            state.update(tui_event);
        }
    }

    terminal::disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    Ok(exit_code)
}

fn make_textarea<'a>() -> TextArea<'a> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text("Type a message... (Enter to send, Esc to quit)");
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    textarea.set_block(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .title(" Input "),
    );
    textarea
}

fn agent_loop_thread(
    config: RunConfig,
    event_tx: mpsc::Sender<TuiEvent>,
    action_rx: mpsc::Receiver<UserAction>,
) {
    loop {
        match action_rx.recv() {
            Ok(UserAction::Submit(prompt)) => {
                bridge::run_agent_for_tui(&config, &prompt, &event_tx, &action_rx);
            }
            Ok(UserAction::Cancel) | Err(_) => break,
            Ok(UserAction::Approve(_)) => {}
        }
    }
}
