#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Model(Option<String>),
    Compact,
    Clear,
    Cost,
    History,
    Mode(Option<String>),
    Plan(Option<String>),
    Remember(String),
    Exit,
}

pub fn parse(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let command = parts.next()?;
    match command {
        "help" => Some(SlashCommand::Help),
        "model" => Some(SlashCommand::Model(
            parts.next().map(|name| name.to_string()),
        )),
        "compact" => Some(SlashCommand::Compact),
        "clear" => Some(SlashCommand::Clear),
        "cost" => Some(SlashCommand::Cost),
        "history" => Some(SlashCommand::History),
        "mode" => Some(SlashCommand::Mode(
            parts.next().map(|mode| mode.to_string()),
        )),
        "plan" => Some(SlashCommand::Plan(parts.next().map(str::to_string))),
        "remember" => {
            let note = parts.collect::<Vec<_>>().join(" ");
            if note.is_empty() {
                None
            } else {
                Some(SlashCommand::Remember(note))
            }
        }
        "exit" => Some(SlashCommand::Exit),
        _ => None,
    }
}

pub fn validate_model(model: &str) -> Result<(), String> {
    match model {
        "deepseek-v4-flash" | "deepseek-v4-pro" => Ok(()),
        other => Err(format!(
            "unsupported model '{other}'. Allowed models: deepseek-v4-flash, deepseek-v4-pro"
        )),
    }
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
    fn parses_remember_command() {
        assert_eq!(
            parse("/remember prefers rust"),
            Some(SlashCommand::Remember("prefers rust".to_string()))
        );
    }

    #[test]
    fn validates_only_v4_models() {
        assert!(validate_model("deepseek-v4-flash").is_ok());
        assert!(validate_model("deepseek-v4-pro").is_ok());
        assert!(validate_model("deepseek-reasoner").is_err());
    }
}
