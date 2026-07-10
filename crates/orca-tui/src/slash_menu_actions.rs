use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use crossterm::event::{Event, KeyCode, KeyEvent};
use tui_textarea::{Input, TextArea};

use orca_core::config::{ReasoningEffort, RunConfig};
use orca_core::model::ModelSelection;

use crate::commands;
use crate::composer_textarea::{make_textarea, make_textarea_with_text, textarea_text};
use crate::slash_command_actions::{SlashOutcome, handle_slash_command, parse_approval_mode};
use crate::theme::Theme;
use crate::types::{
    AppState, AppStatus, ChatMessage, SlashMenu, SlashMenuItem, SubMenu, UserAction,
};
use crate::vim::VimState;

pub(crate) fn update_slash_menu(textarea: &TextArea, state: &mut AppState, config: &RunConfig) {
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_slash_menu_key(
    ev: &Event,
    key: &KeyEvent,
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
                if title == "/model" {
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
                        state.push_message(ChatMessage::System(format!(
                            "Model switched to {model} (reasoning effort: {}).",
                            effort.as_str()
                        )));
                        let _ = action_tx.send(UserAction::SetModel(model));
                    }
                } else if title == "/mode"
                    && let Some(mode) = parse_approval_mode(&chosen)
                {
                    config.approval_mode = mode;
                    if let Ok(mut cfg) = shared_config.lock() {
                        cfg.approval_mode = mode;
                    }
                    state.approval_mode = mode;
                    state.push_message(ChatMessage::System(format!(
                        "Approval mode switched to {chosen}."
                    )));
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
                    state.push_message(ChatMessage::System("No saved sessions.".to_string()));
                }
                Err(e) => {
                    state.push_message(ChatMessage::Error(format!("failed to list history: {e}")));
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

pub(crate) const REASONING_SUBMENU_TITLE: &str = "/model · reasoning effort";

fn reasoning_effort_submenu(pending_model: String, current: ReasoningEffort) -> SubMenu {
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

fn reasoning_effort_options() -> &'static [(ReasoningEffort, &'static str)] {
    &[
        (ReasoningEffort::High, "(faster, lighter reasoning)"),
        (ReasoningEffort::Max, "(deepest reasoning, default)"),
    ]
}

fn parse_reasoning_effort(choice: &str) -> Option<ReasoningEffort> {
    let token = choice.split_whitespace().next().unwrap_or(choice);
    reasoning_effort_options()
        .iter()
        .find(|(effort, _)| effort.as_str() == token)
        .map(|(effort, _)| *effort)
}
