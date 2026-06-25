use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::ToolRequest;

#[derive(Clone, Debug)]
pub struct SubagentRequest {
    pub description: String,
    pub prompt: String,
    pub subagent_type: SubagentType,
    pub model: Option<String>,
    pub mode: SubagentMode,
    pub isolation: SubagentIsolation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentMode {
    Sync,
    Async,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentIsolation {
    None,
    Worktree,
}

pub fn extract_subagent_field(tool_request: &ToolRequest, field: &str) -> Option<String> {
    let raw = tool_request.raw_arguments.as_ref()?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value[field].as_str().map(String::from)
}

pub fn create_subagent_request(tool_request: &ToolRequest) -> SubagentRequest {
    let description = extract_subagent_field(tool_request, "description")
        .or_else(|| tool_request.target.clone())
        .unwrap_or_else(|| "subagent".to_string());

    let prompt =
        extract_subagent_field(tool_request, "prompt").unwrap_or_else(|| description.clone());

    let subagent_type = extract_subagent_field(tool_request, "subagent_type")
        .map(|s| SubagentType::from_str(&s))
        .unwrap_or_default();
    let model = extract_subagent_field(tool_request, "model")
        .filter(|model| orca_core::model::validate_model(model).is_ok());
    let mode = match extract_subagent_field(tool_request, "mode").as_deref() {
        Some("async") => SubagentMode::Async,
        _ => SubagentMode::Sync,
    };
    let isolation = match extract_subagent_field(tool_request, "isolation").as_deref() {
        Some("worktree") => SubagentIsolation::Worktree,
        _ => SubagentIsolation::None,
    };

    SubagentRequest {
        description,
        prompt,
        subagent_type,
        model,
        mode,
        isolation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::ToolName;

    #[test]
    fn create_request_with_all_fields() {
        let req = ToolRequest {
            id: "t1".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("test task".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "review code",
                    "prompt": "review src/main.rs for bugs",
                    "subagent_type": "code_reviewer",
                    "model": "deepseek-v4-pro",
                    "isolation": "worktree"
                })
                .to_string(),
            ),
        };
        let result = create_subagent_request(&req);
        assert_eq!(result.description, "review code");
        assert_eq!(result.prompt, "review src/main.rs for bugs");
        assert_eq!(result.subagent_type, SubagentType::CodeReviewer);
        assert_eq!(result.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(result.mode, SubagentMode::Sync);
        assert_eq!(result.isolation, SubagentIsolation::Worktree);
    }

    #[test]
    fn create_request_parses_async_mode() {
        let req = ToolRequest {
            id: "t4".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("async task".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async task",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
        };
        let result = create_subagent_request(&req);
        assert_eq!(result.mode, SubagentMode::Async);
    }

    #[test]
    fn create_request_defaults_to_general() {
        let req = ToolRequest {
            id: "t2".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("analyze".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "analyze repo",
                    "prompt": "analyze the repository structure"
                })
                .to_string(),
            ),
        };
        let result = create_subagent_request(&req);
        assert_eq!(result.subagent_type, SubagentType::General);
    }

    #[test]
    fn create_request_falls_back_to_target() {
        let req = ToolRequest {
            id: "t3".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("fallback desc".to_string()),
            raw_arguments: Some("{}".to_string()),
        };
        let result = create_subagent_request(&req);
        assert_eq!(result.description, "fallback desc");
        assert_eq!(result.prompt, "fallback desc");
    }
}
