#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    Model(Option<String>),
    Compact,
    Cost,
    ConfigShow,
    History,
    Mode(Option<String>),
    Plan(Option<String>),
    Goal(GoalSlashCommand),
    WorkflowList,
    Remember(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GoalSlashCommand {
    Show,
    Set(String),
    Edit(String),
    Clear,
    Pause,
    Resume,
}

pub fn parse(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let command = parts.next()?;
    match command {
        "model" => Some(SlashCommand::Model(
            parts.next().map(|name| name.to_string()),
        )),
        "compact" => Some(SlashCommand::Compact),
        "cost" => Some(SlashCommand::Cost),
        "config" if parts.next() == Some("show") => Some(SlashCommand::ConfigShow),
        "history" => Some(SlashCommand::History),
        "mode" => Some(SlashCommand::Mode(
            parts.next().map(|mode| mode.to_string()),
        )),
        "plan" => Some(SlashCommand::Plan(parts.next().map(str::to_string))),
        "goal" => parse_goal(parts.collect::<Vec<_>>().join(" ")).map(SlashCommand::Goal),
        "workflows" => Some(SlashCommand::WorkflowList),
        "remember" => {
            let note = parts.collect::<Vec<_>>().join(" ");
            if note.is_empty() {
                None
            } else {
                Some(SlashCommand::Remember(note))
            }
        }
        _ => None,
    }
}

pub fn all_commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("/model", "Switch model: auto, flash, or pro"),
        ("/compact", "Compress conversation context"),
        ("/cost", "Show session cost"),
        ("/config show", "Show merged config"),
        ("/mode", "Switch approval mode"),
        ("/plan", "Toggle plan mode"),
        ("/goal", "Manage a persistent goal"),
        ("/workflows", "Show workflow tasks"),
        ("/remember", "Save a note to memory"),
        ("/history", "Browse session history"),
    ]
}

fn parse_goal(args: String) -> Option<GoalSlashCommand> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Some(GoalSlashCommand::Show);
    }
    match trimmed {
        "clear" => Some(GoalSlashCommand::Clear),
        "pause" => Some(GoalSlashCommand::Pause),
        "resume" => Some(GoalSlashCommand::Resume),
        "edit" => None,
        _ => {
            if let Some(rest) = trimmed.strip_prefix("edit ") {
                let objective = rest.trim();
                if objective.is_empty() {
                    None
                } else {
                    Some(GoalSlashCommand::Edit(objective.to_string()))
                }
            } else {
                Some(GoalSlashCommand::Set(trimmed.to_string()))
            }
        }
    }
}

pub fn available_models() -> &'static [&'static str] {
    orca_core::model::allowed_models()
}

pub fn validate_model(model: &str) -> Result<(), String> {
    orca_core::model::validate_model(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_command() {
        assert_eq!(
            parse("/model deepseek-v4-pro"),
            Some(SlashCommand::Model(Some("deepseek-v4-pro".to_string())))
        );
        assert_eq!(parse("/model"), Some(SlashCommand::Model(None)));
    }

    #[test]
    fn parses_plan_commands() {
        assert_eq!(parse("/plan"), Some(SlashCommand::Plan(None)));
        assert_eq!(
            parse("/plan off"),
            Some(SlashCommand::Plan(Some("off".to_string())))
        );
    }

    #[test]
    fn parses_goal_commands() {
        assert_eq!(
            parse("/goal"),
            Some(SlashCommand::Goal(GoalSlashCommand::Show))
        );
        assert_eq!(
            parse("/goal ship it"),
            Some(SlashCommand::Goal(GoalSlashCommand::Set(
                "ship it".to_string()
            )))
        );
        assert_eq!(
            parse("/goal edit better goal"),
            Some(SlashCommand::Goal(GoalSlashCommand::Edit(
                "better goal".to_string()
            )))
        );
        assert_eq!(
            parse("/goal clear"),
            Some(SlashCommand::Goal(GoalSlashCommand::Clear))
        );
        assert_eq!(
            parse("/goal pause"),
            Some(SlashCommand::Goal(GoalSlashCommand::Pause))
        );
        assert_eq!(
            parse("/goal resume"),
            Some(SlashCommand::Goal(GoalSlashCommand::Resume))
        );
        assert_eq!(parse("/goal edit"), None);
    }

    #[test]
    fn parses_workflows_command() {
        assert_eq!(parse("/workflows"), Some(SlashCommand::WorkflowList));
    }

    #[test]
    fn parses_config_show_command() {
        assert_eq!(parse("/config show"), Some(SlashCommand::ConfigShow));
    }

    #[test]
    fn parses_remember_command() {
        assert_eq!(
            parse("/remember prefers rust"),
            Some(SlashCommand::Remember("prefers rust".to_string()))
        );
    }

    #[test]
    fn removed_terminal_aliases_are_not_slash_commands() {
        assert_eq!(parse("/help"), None);
        assert_eq!(parse("/clear"), None);
        assert_eq!(parse("/exit"), None);

        let command_names = all_commands()
            .iter()
            .map(|(command, _)| *command)
            .collect::<Vec<_>>();
        assert!(!command_names.contains(&"/help"));
        assert!(!command_names.contains(&"/clear"));
        assert!(!command_names.contains(&"/exit"));
    }

    #[test]
    fn validates_supported_models() {
        assert_eq!(
            available_models(),
            &["auto", "deepseek-v4-flash", "deepseek-v4-pro"]
        );
        assert!(validate_model("auto").is_ok());
        assert!(validate_model("deepseek-v4-flash").is_ok());
        assert!(validate_model("deepseek-v4-pro").is_ok());
        assert!(validate_model("deepseek-chat").is_err());
        assert!(validate_model("deepseek-reasoner").is_err());
        assert!(validate_model("bogus-model").is_err());
    }
}
