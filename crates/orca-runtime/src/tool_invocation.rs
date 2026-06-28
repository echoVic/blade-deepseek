use orca_core::approval_types::{ActionKind, ApprovalRequest};
use orca_core::config::RunConfig;
use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

use crate::hooks::{HookOutcome, tool_request_with_hook_outcome};

#[derive(Clone, Debug)]
pub struct ToolInvocation {
    pub requested: ToolRequest,
    pub effective: ToolRequest,
    pub action: Option<ActionKind>,
}

#[derive(Clone, Debug)]
pub struct ToolExecutionFailure {
    pub request: ToolRequest,
    pub message: String,
}

impl ToolExecutionFailure {
    pub fn into_result(self) -> ToolResult {
        ToolResult::invalid_input(&self.request, self.message)
    }
}

pub fn prepare_tool_invocation(
    tool_request: &ToolRequest,
    subagent_depth: u32,
    mcp_registry: &McpRegistry,
    config: &RunConfig,
) -> ToolInvocation {
    let action = if tool_request.name == ToolName::Subagent
        && subagent_depth >= config.subagents.max_depth
    {
        None
    } else {
        Some(orca_tools::canonical_action_kind_with_mcp_and_external(
            tool_request,
            Some(mcp_registry),
            &config.external_tools,
        ))
    };

    ToolInvocation {
        requested: tool_request.clone(),
        effective: tool_request.clone(),
        action,
    }
}

pub fn prepare_tool_invocation_with_external(
    tool_request: &ToolRequest,
    subagent_depth: u32,
    max_subagent_depth: u32,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> ToolInvocation {
    let action = if tool_request.name == ToolName::Subagent && subagent_depth >= max_subagent_depth
    {
        None
    } else {
        Some(orca_tools::canonical_action_kind_with_mcp_and_external(
            tool_request,
            Some(mcp_registry),
            external_tools,
        ))
    };

    ToolInvocation {
        requested: tool_request.clone(),
        effective: tool_request.clone(),
        action,
    }
}

pub fn validate_tool_invocation(
    invocation: &ToolInvocation,
    mcp_registry: &McpRegistry,
    config: &RunConfig,
) -> Result<(), ToolExecutionFailure> {
    orca_tools::validate_with_mcp_and_external(
        &invocation.effective,
        Some(mcp_registry),
        &config.external_tools,
    )
    .map_err(|error| ToolExecutionFailure {
        request: invocation.effective.clone(),
        message: format!("tool arguments failed schema validation: {error}"),
    })
}

pub fn validate_tool_invocation_with_external(
    invocation: &ToolInvocation,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Result<(), ToolExecutionFailure> {
    orca_tools::validate_with_mcp_and_external(
        &invocation.effective,
        Some(mcp_registry),
        external_tools,
    )
    .map_err(|error| ToolExecutionFailure {
        request: invocation.effective.clone(),
        message: format!("tool arguments failed schema validation: {error}"),
    })
}

pub fn apply_pre_tool_outcome(
    invocation: ToolInvocation,
    outcome: &HookOutcome,
    mcp_registry: &McpRegistry,
    config: &RunConfig,
) -> Result<ToolInvocation, ToolExecutionFailure> {
    let effective = tool_request_with_hook_outcome(&invocation.effective, outcome);
    let updated = ToolInvocation {
        effective,
        ..invocation
    };
    validate_tool_invocation(&updated, mcp_registry, config)?;
    Ok(updated)
}

pub fn apply_pre_tool_outcome_with_external(
    invocation: ToolInvocation,
    outcome: &HookOutcome,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Result<ToolInvocation, ToolExecutionFailure> {
    let effective = tool_request_with_hook_outcome(&invocation.effective, outcome);
    let updated = ToolInvocation {
        effective,
        ..invocation
    };
    validate_tool_invocation_with_external(&updated, mcp_registry, external_tools)?;
    Ok(updated)
}

pub fn approval_request_for_invocation(invocation: &ToolInvocation) -> Option<ApprovalRequest> {
    let action = invocation.action?;
    Some(ApprovalRequest {
        id: format!("approval-{}", invocation.requested.id),
        action,
        description: format!(
            "{} requested {}",
            invocation.requested.name.as_str(),
            action.as_str()
        ),
        tool: Some(invocation.requested.name.as_str().to_string()),
        target: invocation.requested.target.clone(),
        preview: None,
    })
}

#[cfg(test)]
mod tests {
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::mcp_types::McpTool;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest};
    use orca_mcp::McpRegistry;
    use serde_json::json;

    use crate::hooks::HookOutcome;

    use super::{
        apply_pre_tool_outcome, approval_request_for_invocation, prepare_tool_invocation,
        validate_tool_invocation,
    };

    fn config_with_external(external_tools: Vec<ExternalToolConfig>) -> RunConfig {
        RunConfig {
            prompt: "test".to_string(),
            app_version: "test".to_string(),
            cwd: Some(std::env::current_dir().expect("cwd")),
            provider: ProviderKind::Mock,
            model: ModelSelection::from_unchecked(Some("mock".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            api_key: None,
            base_url: None,
            approval_mode: ApprovalMode::Suggest,
            output_format: OutputFormat::Jsonl,
            verifier: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            theme: ThemeName::Dark,
            mcp_servers: Vec::new(),
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            hooks: Vec::new(),
            workflows: WorkflowConfig::default(),
            subagents: SubagentConfig {
                max_depth: 1,
                ..SubagentConfig::default()
            },
            tools: ToolConfig::default(),
            external_tools,
            max_budget_usd: None,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn request(
        name: ToolName,
        action: ActionKind,
        target: Option<&str>,
        raw: Option<&str>,
    ) -> ToolRequest {
        ToolRequest {
            id: "tool-1".to_string(),
            name,
            action,
            target: target.map(str::to_string),
            raw_arguments: raw.map(str::to_string),
        }
    }

    #[test]
    fn invocation_uses_registry_action_instead_of_caller_supplied_action() {
        let config = config_with_external(Vec::new());
        let registry = McpRegistry::default();
        let request = request(ToolName::Bash, ActionKind::Read, Some("echo hi"), None);

        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);

        assert_eq!(invocation.action, Some(ActionKind::Shell));
        let approval = approval_request_for_invocation(&invocation).expect("approval");
        assert_eq!(approval.action, ActionKind::Shell);
        assert_eq!(approval.id, "approval-tool-1");
        assert_eq!(approval.description, "bash requested shell");
        assert_eq!(approval.tool, Some("bash".to_string()));
        assert_eq!(approval.target, Some("echo hi".to_string()));
        assert_eq!(approval.preview, None);
    }

    #[test]
    fn invocation_uses_external_tool_action_kind() {
        let config = config_with_external(vec![ExternalToolConfig {
            name: "deploy".to_string(),
            description: "deploy".to_string(),
            action_kind: ActionKind::Shell,
            command: "echo deploy".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "env": { "type": "string" }
                },
                "required": ["env"],
                "additionalProperties": false
            }),
        }]);
        let registry = McpRegistry::default();
        let request = request(
            ToolName::plain("deploy"),
            ActionKind::Read,
            Some("prod"),
            Some(r#"{"env":"prod"}"#),
        );

        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);

        assert_eq!(invocation.action, Some(ActionKind::Shell));
    }

    #[test]
    fn invocation_uses_mcp_tool_action_kind() {
        let config = config_with_external(Vec::new());
        let registry = McpRegistry::from_tools_for_test(vec![McpTool {
            server: "local".to_string(),
            name: "write".to_string(),
            schema_name: "mcp__local__write".to_string(),
            description: Some("write via mcp".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }]);
        let request = request(
            ToolName::Mcp("mcp__local__write".to_string()),
            ActionKind::Read,
            None,
            Some("{}"),
        );

        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);

        assert_eq!(invocation.action, Some(ActionKind::Write));
    }

    #[test]
    fn subagent_max_depth_blocks_approval_request() {
        let config = config_with_external(Vec::new());
        let registry = McpRegistry::default();
        let request = request(ToolName::Subagent, ActionKind::Agent, Some("audit"), None);

        let invocation = prepare_tool_invocation(&request, 1, &registry, &config);

        assert_eq!(invocation.action, None);
        assert!(approval_request_for_invocation(&invocation).is_none());
    }

    #[test]
    fn invalid_external_arguments_report_shared_validation_error() {
        let config = config_with_external(vec![ExternalToolConfig {
            name: "deploy".to_string(),
            description: "deploy".to_string(),
            action_kind: ActionKind::Shell,
            command: "echo deploy".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "env": { "type": "string" }
                },
                "required": ["env"],
                "additionalProperties": false
            }),
        }]);
        let registry = McpRegistry::default();
        let request = request(
            ToolName::plain("deploy"),
            ActionKind::Read,
            Some("prod"),
            Some(r#"{"unexpected":"prod"}"#),
        );
        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);

        let failure =
            validate_tool_invocation(&invocation, &registry, &config).expect_err("invalid args");

        assert!(
            failure
                .message
                .contains("tool arguments failed schema validation")
        );
        assert!(
            failure
                .message
                .contains("missing required property \"env\"")
        );
    }

    #[test]
    fn hook_modified_target_keeps_shared_validation_path() {
        let config = config_with_external(Vec::new());
        let registry = McpRegistry::default();
        let request = request(
            ToolName::Bash,
            ActionKind::Shell,
            Some("echo before"),
            Some(r#"{"command":"echo before"}"#),
        );
        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);
        let outcome = HookOutcome {
            modified_target: Some("echo after".to_string()),
            injected_context: Vec::new(),
        };

        let invocation = apply_pre_tool_outcome(invocation, &outcome, &registry, &config)
            .expect("hook-modified request remains valid");

        assert_eq!(invocation.effective.target.as_deref(), Some("echo after"));
        assert_eq!(invocation.action, Some(ActionKind::Shell));
    }
}
