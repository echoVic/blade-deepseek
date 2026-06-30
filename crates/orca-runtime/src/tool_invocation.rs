use std::collections::HashSet;

use orca_core::approval_types::{ActionKind, ApprovalRequest};
use orca_core::config::RunConfig;
use orca_core::external_config::ExternalToolConfig;
use orca_core::provider_types::ProviderStep;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::tool_schema::{
    deepseek_tools_schema_for_allowed_names_with_mcp_and_external,
    deepseek_tools_schema_for_type_with_mcp_and_external,
    deepseek_tools_schema_with_mcp_and_external,
};
use serde_json::Value;

use crate::hooks::{HookOutcome, tool_request_with_hook_outcome};

#[derive(Clone, Debug)]
pub struct ToolInvocation {
    pub requested: ToolRequest,
    pub effective: ToolRequest,
    pub action: Option<ActionKind>,
}

#[derive(Clone, Copy)]
pub(crate) struct AgentToolPolicyContext<'a> {
    allowed_tools: Option<&'a [String]>,
    label: Option<&'a str>,
}

#[derive(Clone, Debug)]
pub struct ToolExecutionFailure {
    pub request: ToolRequest,
    pub message: String,
}

impl<'a> AgentToolPolicyContext<'a> {
    pub(crate) fn new(allowed_tools: Option<&'a [String]>, label: Option<&'a str>) -> Self {
        Self {
            allowed_tools,
            label,
        }
    }

    pub(crate) fn unrestricted() -> Self {
        Self::new(None, None)
    }

    pub(crate) fn allowed_tools(&self) -> Option<&'a [String]> {
        self.allowed_tools
    }

    pub(crate) fn label(&self) -> Option<&'a str> {
        self.label
    }
}

impl ToolExecutionFailure {
    pub fn into_result(self) -> ToolResult {
        ToolResult::invalid_input(&self.request, self.message)
    }
}

pub(crate) fn provider_tool_schema_override(
    subagent_depth: u32,
    subagent_type: &SubagentType,
    tool_policy: AgentToolPolicyContext<'_>,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Option<Vec<Value>> {
    if subagent_depth > 0 {
        if let Some(allowed_tools) = tool_policy.allowed_tools() {
            Some(
                deepseek_tools_schema_for_allowed_names_with_mcp_and_external(
                    allowed_tools,
                    Some(mcp_registry),
                    external_tools,
                ),
            )
        } else {
            Some(deepseek_tools_schema_for_type_with_mcp_and_external(
                subagent_type,
                Some(mcp_registry),
                external_tools,
            ))
        }
    } else {
        Some(deepseek_tools_schema_with_mcp_and_external(
            Some(mcp_registry),
            external_tools,
        ))
    }
}

pub(crate) fn provider_config_for_agent_loop(
    config: &RunConfig,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    tool_policy: AgentToolPolicyContext<'_>,
    mcp_registry: &McpRegistry,
) -> ProviderConfig {
    ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.as_option(),
        tools_override: provider_tool_schema_override(
            subagent_depth,
            subagent_type,
            tool_policy,
            mcp_registry,
            &config.external_tools,
        ),
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
    }
}

pub(crate) fn tool_requests_from_provider_steps(steps: &[ProviderStep]) -> Vec<ToolRequest> {
    steps
        .iter()
        .filter_map(|step| match step {
            ProviderStep::ToolCall(tool_request) => Some(tool_request.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn reject_disallowed_child_tool(
    tool_request: &ToolRequest,
    policy: AgentToolPolicyContext<'_>,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Option<ToolResult> {
    child_tool_policy_failure(
        tool_request,
        policy.allowed_tools(),
        policy.label(),
        mcp_registry,
        external_tools,
    )
}

fn child_tool_policy_failure(
    tool_request: &ToolRequest,
    allowed_tools: Option<&[String]>,
    policy_label: Option<&str>,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Option<ToolResult> {
    let allowed_tools = allowed_tools?;
    let registry = orca_tools::registry::tool_registry_with_mcp_and_external(
        Some(mcp_registry),
        external_tools,
    );
    let allowed_canonical_names = allowed_tools
        .iter()
        .filter_map(|tool| {
            registry
                .resolve(tool)
                .map(|resolved| resolved.tool.name().to_string())
        })
        .collect::<HashSet<_>>();
    let requested_name = tool_request.name.as_str();
    let requested_canonical_name = registry
        .resolve(requested_name)
        .map(|resolved| resolved.tool.name().to_string())
        .unwrap_or_else(|| requested_name.to_string());

    if allowed_canonical_names.contains(&requested_canonical_name) {
        return None;
    }

    let label = policy_label.unwrap_or("child agent tool policy");
    Some(ToolResult::invalid_input(
        tool_request,
        format!("{label} disallows tool '{requested_name}'"),
    ))
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
    use orca_core::provider_types::ProviderStep;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::subagent_types::SubagentType;
    use orca_core::tool_types::{ToolName, ToolRequest};
    use orca_mcp::McpRegistry;
    use serde_json::json;

    use crate::hooks::HookOutcome;

    use super::{
        AgentToolPolicyContext, apply_pre_tool_outcome, approval_request_for_invocation,
        prepare_tool_invocation, provider_config_for_agent_loop, provider_tool_schema_override,
        tool_requests_from_provider_steps, validate_tool_invocation,
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

    fn schema_names(tools: &[serde_json::Value]) -> Vec<&str> {
        tools
            .iter()
            .filter_map(|tool| tool["function"]["name"].as_str())
            .collect()
    }

    #[test]
    fn provider_tool_schema_override_exposes_root_agent_tools() {
        let registry = McpRegistry::default();
        let tools = provider_tool_schema_override(
            0,
            &SubagentType::General,
            AgentToolPolicyContext::unrestricted(),
            &registry,
            &[],
        )
        .expect("root tool schema");
        let names = schema_names(&tools);

        assert!(names.contains(&"subagent"));
        assert!(names.contains(&"bash"));
    }

    #[test]
    fn provider_tool_schema_override_limits_child_agent_to_allowed_tools() {
        let registry = McpRegistry::default();
        let allowed = vec!["read_file".to_string()];
        let tools = provider_tool_schema_override(
            1,
            &SubagentType::General,
            AgentToolPolicyContext::new(Some(&allowed), Some("test child")),
            &registry,
            &[],
        )
        .expect("child allowed tool schema");
        let names = schema_names(&tools);

        assert!(names.contains(&"read_file"));
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"subagent"));
    }

    #[test]
    fn provider_tool_schema_override_uses_child_subagent_type_policy() {
        let registry = McpRegistry::default();
        let tools = provider_tool_schema_override(
            1,
            &SubagentType::CodeReviewer,
            AgentToolPolicyContext::unrestricted(),
            &registry,
            &[],
        )
        .expect("child typed tool schema");
        let names = schema_names(&tools);

        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"grep"));
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"subagent"));
    }

    #[test]
    fn provider_config_for_agent_loop_builds_schema_limited_provider_config() {
        let registry = McpRegistry::default();
        let allowed = vec!["read_file".to_string()];
        let mut config = config_with_external(Vec::new());
        config.api_key = Some("test-key".to_string());
        config.base_url = Some("https://provider.test".to_string());

        let provider_config = provider_config_for_agent_loop(
            &config,
            1,
            &SubagentType::General,
            AgentToolPolicyContext::new(Some(&allowed), Some("test child")),
            &registry,
        );

        assert_eq!(provider_config.api_key.as_deref(), Some("test-key"));
        assert_eq!(
            provider_config.base_url.as_deref(),
            Some("https://provider.test")
        );
        assert_eq!(provider_config.model.as_deref(), Some("mock"));
        assert!(provider_config.mcp_registry.is_some());
        assert_eq!(provider_config.external_tools.len(), 0);

        let tools = provider_config.tools_override.expect("tool override");
        let names = schema_names(&tools);
        assert!(names.contains(&"read_file"));
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"subagent"));
    }

    #[test]
    fn tool_requests_from_provider_steps_extracts_tool_calls_in_order() {
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let steps = vec![
            ProviderStep::MessageDelta("hello".to_string()),
            ProviderStep::ToolCall(first.clone()),
            ProviderStep::ReasoningDelta("thinking".to_string()),
            ProviderStep::ToolCall(second.clone()),
            ProviderStep::Error("ignored".to_string()),
        ];

        let requests = tool_requests_from_provider_steps(&steps);

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].id, first.id);
        assert_eq!(requests[1].id, second.id);
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
