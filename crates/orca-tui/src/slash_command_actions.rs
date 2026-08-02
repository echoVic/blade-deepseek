use crossbeam_channel as mpsc;
use std::sync::{Arc, Mutex};

use orca_core::approval_types::ApprovalMode;
use orca_core::config::RunConfig;
use orca_runtime::surface::RuntimeSurfaceHostHandle;

use crate::commands::{self, GoalSlashCommand, SlashCommand, TrustSlashCommand};
use crate::surface_actions::TuiHostActions;
use crate::types::{AppState, ChatMessage, TuiMemoryScope, UserAction};

pub(crate) enum SlashOutcome {
    Continue,
}

pub(crate) fn handle_slash_command(
    text: &str,
    config: &mut RunConfig,
    _shared_config: &Arc<Mutex<RunConfig>>,
    state: &mut AppState,
    action_tx: &mpsc::Sender<UserAction>,
) -> Option<SlashOutcome> {
    let cwd = config
        .cwd
        .as_deref()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let command = commands::parse_with_cwd(text, &cwd)?;
    let mut pending_settings_action = None;
    match command {
        SlashCommand::Model(Some(model)) => match commands::validate_model(&model) {
            Ok(()) => {
                pending_settings_action = Some(UserAction::SetModel(model));
            }
            Err(error) => state.push_message(ChatMessage::Error(error)),
        },
        SlashCommand::Model(None) => {
            state.push_message(ChatMessage::System(format!(
                "Current model: {} (reasoning effort: {}). Use the /model menu to change both.",
                state.model_name,
                state.reasoning_effort.as_str()
            )));
        }
        SlashCommand::Cost => {
            state.push_message(ChatMessage::System(format!(
                "Session usage: {} input, {} output, {} cache tokens, estimated ${:.6}.",
                state.usage.input_tokens,
                state.usage.output_tokens,
                state.usage.cache_tokens,
                state.usage.estimated_cost_usd
            )));
        }
        SlashCommand::ConfigShow => {
            state.push_message(ChatMessage::System(orca_core::config::format_config_show(
                config,
            )));
        }
        SlashCommand::Mode(Some(mode)) => match parse_approval_mode(&mode) {
            Some(approval_mode) => {
                pending_settings_action = Some(UserAction::SetModel(encode_settings_intent(
                    None,
                    None,
                    Some(approval_mode),
                )));
            }
            None => state.push_message(ChatMessage::Error(
                "unsupported mode. Use suggest, auto-edit, full-auto, or plan.".to_string(),
            )),
        },
        SlashCommand::Mode(None) => {
            state.push_message(ChatMessage::System(format!(
                "Current mode: {}",
                config.approval_mode.as_str()
            )));
        }
        SlashCommand::Plan(arg) => match arg.as_deref() {
            Some("off") => {
                pending_settings_action = Some(UserAction::SetModel(encode_settings_intent(
                    None,
                    None,
                    Some(ApprovalMode::Suggest),
                )));
            }
            None => {
                pending_settings_action = Some(UserAction::SetModel(encode_settings_intent(
                    None,
                    None,
                    Some(ApprovalMode::Plan),
                )));
            }
            Some(_) => state.push_message(ChatMessage::Error(
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
        SlashCommand::SkillRun { id, args } => {
            let prompt = match args {
                Some(a) => format!("${id}:{a}"),
                None => format!("${id}"),
            };
            state.record_prompt(prompt.clone());
            state.push_message(ChatMessage::User(prompt.clone()));
            state.enter_running();
            let _ = action_tx.send(UserAction::Submit(prompt));
        }
        SlashCommand::WorkflowList => {
            state.show_workflows();
        }
        SlashCommand::SkillList => {
            let cwd = config
                .cwd
                .as_deref()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match orca_tools::skills::discover_from_env(&cwd) {
                Ok(skills) if skills.is_empty() => {
                    state.push_message(ChatMessage::System("No skills found. Add SKILL.md files under .orca/skills/ or .agents/skills/.".to_string()));
                }
                Ok(skills) => {
                    let list = skills
                        .iter()
                        .map(|s| {
                            format!(
                                "${} [{}] — {}",
                                s.id,
                                s.source.as_str(),
                                if s.description.is_empty() {
                                    &s.name
                                } else {
                                    &s.description
                                }
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    state.push_message(ChatMessage::System(format!("Available skills:\n{list}")));
                }
                Err(e) => {
                    state.push_message(ChatMessage::Error(format!("failed to list skills: {e}")))
                }
            }
        }
        SlashCommand::WorkflowRun { name, args } => {
            state.enter_running();
            let _ = action_tx.send(UserAction::RunWorkflow { name, args });
        }
        SlashCommand::AgentDashboard => {
            state.show_agents();
        }
        SlashCommand::Remember(note) => {
            let (scope, note) = if let Some(project_note) = note.strip_prefix("project:") {
                (TuiMemoryScope::Project, project_note.trim().to_string())
            } else {
                (TuiMemoryScope::User, note)
            };
            let _ = action_tx.send(UserAction::Remember { scope, note });
        }
        SlashCommand::Compact => {
            state.enter_running();
            let _ = action_tx.send(UserAction::Compact);
        }
        SlashCommand::ResumeOperation => {
            if let Some(operation_id) = state.recoverable_operation_id.clone() {
                state.enter_running();
                let _ = action_tx.send(UserAction::ResumeOperation { operation_id });
            } else {
                state.push_message(ChatMessage::Error(
                    "no recoverable operation is available".to_string(),
                ));
            }
        }
        SlashCommand::CancelOperation => {
            if let Some(operation_id) = state.recoverable_operation_id.clone() {
                state.enter_running();
                let _ = action_tx.send(UserAction::CancelOperation { operation_id });
            } else {
                state.push_message(ChatMessage::Error(
                    "no recoverable operation is available".to_string(),
                ));
            }
        }
        SlashCommand::History => match RuntimeSurfaceHostHandle::list_saved_sessions(10) {
            Ok(sessions) if sessions.is_empty() => {
                state.push_message(ChatMessage::System("No saved sessions.".to_string()))
            }
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
                state.push_message(ChatMessage::System(format!("Recent sessions:\n{summary}")));
            }
            Err(error) => state.push_message(ChatMessage::Error(format!(
                "failed to list history: {error}"
            ))),
        },
        SlashCommand::Trust(trust_command) => match trust_command {
            TrustSlashCommand::Show => {
                if TuiHostActions::folder_is_trusted(&cwd) {
                    state.push_message(ChatMessage::System(format!(
                            "{} is trusted; the OS sandbox honors the configured write and network policy.",
                            cwd.display()
                        )))
                } else {
                    state.push_message(ChatMessage::System(format!(
                            "{} is not trusted; commands run read-only with no network. Use /trust add to trust it.",
                            cwd.display()
                        )))
                }
            }
            TrustSlashCommand::Add => match TuiHostActions::set_folder_trust(&cwd, true) {
                Ok(()) => state.push_message(ChatMessage::System(format!(
                    "Trusted {}. Restart Orca to load project config from this folder.",
                    cwd.display()
                ))),
                Err(error) => state.push_message(ChatMessage::Error(format!(
                    "failed to trust folder: {error}"
                ))),
            },
            TrustSlashCommand::Remove => match TuiHostActions::set_folder_trust(&cwd, false) {
                Ok(()) => state.push_message(ChatMessage::System(format!(
                    "Removed trust for {}; commands now run read-only with no network.",
                    cwd.display()
                ))),
                Err(error) => state.push_message(ChatMessage::Error(format!(
                    "failed to update trust: {error}"
                ))),
            },
        },
    }
    if let Some(action) = pending_settings_action {
        let _ = action_tx.send(action);
    }
    state.scroll_to_bottom();
    Some(SlashOutcome::Continue)
}

pub(crate) fn parse_approval_mode(mode: &str) -> Option<ApprovalMode> {
    match mode {
        "suggest" => Some(ApprovalMode::Suggest),
        "auto-edit" => Some(ApprovalMode::AutoEdit),
        "full-auto" => Some(ApprovalMode::FullAuto),
        "plan" => Some(ApprovalMode::Plan),
        _ => None,
    }
}

const SETTINGS_INTENT_PREFIX: &str = "__orca_runtime_settings__:";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SettingsIntent {
    pub model: Option<String>,
    pub reasoning_effort: Option<orca_core::config::ReasoningEffort>,
    pub approval_mode: Option<ApprovalMode>,
}

pub(crate) fn encode_settings_intent(
    model: Option<&str>,
    reasoning_effort: Option<orca_core::config::ReasoningEffort>,
    approval_mode: Option<ApprovalMode>,
) -> String {
    format!(
        "{SETTINGS_INTENT_PREFIX}{}|{}|{}",
        model.unwrap_or("-"),
        reasoning_effort.map_or("-", orca_core::config::ReasoningEffort::as_str),
        approval_mode.map_or("-", ApprovalMode::as_str),
    )
}

pub(crate) fn decode_settings_intent(value: &str) -> Option<SettingsIntent> {
    let fields = value
        .strip_prefix(SETTINGS_INTENT_PREFIX)?
        .split('|')
        .collect::<Vec<_>>();
    if fields.len() != 3 {
        return None;
    }
    let model = match fields[0] {
        "-" => None,
        model if orca_core::model::validate_model(model).is_ok() => Some(model.to_string()),
        _ => return None,
    };
    let reasoning_effort = match fields[1] {
        "-" => None,
        "low" => Some(orca_core::config::ReasoningEffort::Low),
        "high" => Some(orca_core::config::ReasoningEffort::High),
        "max" => Some(orca_core::config::ReasoningEffort::Max),
        _ => return None,
    };
    let approval_mode = match fields[2] {
        "-" => None,
        "suggest" => Some(ApprovalMode::Suggest),
        "auto-edit" => Some(ApprovalMode::AutoEdit),
        "full-auto" => Some(ApprovalMode::FullAuto),
        "plan" => Some(ApprovalMode::Plan),
        _ => return None,
    };
    (model.is_some() || reasoning_effort.is_some() || approval_mode.is_some()).then_some(
        SettingsIntent {
            model,
            reasoning_effort,
            approval_mode,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_reasoning_effort_round_trips_through_settings_intent() {
        let encoded = encode_settings_intent(
            Some("deepseek-v4-flash"),
            Some(orca_core::config::ReasoningEffort::Low),
            None,
        );

        let decoded = decode_settings_intent(&encoded).expect("decode low effort intent");

        assert_eq!(
            decoded.reasoning_effort,
            Some(orca_core::config::ReasoningEffort::Low)
        );
    }
}
