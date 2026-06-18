#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Model(Option<String>),
    Compact,
    Clear,
    Cost,
    ConfigShow,
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
        "config" if parts.next() == Some("show") => Some(SlashCommand::ConfigShow),
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

pub fn all_commands() -> &'static [(&'static str, &'static str)] {
    &[
        ("/help", "Show available commands"),
        ("/model", "Switch model: auto, flash, or pro"),
        ("/compact", "Compress conversation context"),
        ("/clear", "Clear message history"),
        ("/cost", "Show session cost"),
        ("/config show", "Show merged config"),
        ("/mode", "Switch approval mode"),
        ("/plan", "Toggle plan mode"),
        ("/remember", "Save a note to memory"),
        ("/history", "Browse session history"),
        ("/exit", "Exit Orca"),
    ]
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
    fn validates_supported_models() {
        assert!(validate_model("auto").is_ok());
        assert!(validate_model("deepseek-v4-flash").is_ok());
        assert!(validate_model("deepseek-v4-pro").is_ok());
        assert!(validate_model("deepseek-chat").is_ok());
        assert!(validate_model("deepseek-reasoner").is_ok());
        assert!(validate_model("bogus-model").is_err());
    }
}
