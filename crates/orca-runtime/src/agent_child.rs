use std::io;

use orca_core::config::RunConfig;
use orca_core::event_schema::RunStatus;
use orca_core::subagent_types::SubagentType;

use crate::cost::CostTracker;

#[derive(Clone, Debug)]
pub struct ChildAgentRequest {
    pub prompt: String,
    pub subagent_type: SubagentType,
    pub model: Option<String>,
    pub depth: u32,
    pub emit_deltas: bool,
}

#[derive(Clone, Debug)]
pub struct ChildAgentResult {
    pub status: RunStatus,
    pub final_message: Option<String>,
    pub error: Option<String>,
}

pub fn run_child_agent<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    run: F,
) -> io::Result<(ChildAgentResult, CostTracker)>
where
    F: FnOnce(&RunConfig, &ChildAgentRequest, &mut CostTracker) -> io::Result<ChildAgentResult>,
{
    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let mut child_cost_tracker = CostTracker::new(child_config.model.as_deref());
    let result = run(&child_config, request, &mut child_cost_tracker)?;
    Ok((result, child_cost_tracker))
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{HistoryMode, OutputFormat, RunConfig};
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_config::McpServerConfig;
    use orca_core::model::{AUTO_MODEL, FLASH_MODEL, ModelSelection};
    use orca_core::permission_types::PermissionRules;
    use orca_core::provider_types::ProviderKind;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::theme::ThemeName;
    use orca_core::tool_config::ToolConfig;
    use orca_core::workflow_config::WorkflowConfig;

    fn config(model: Option<&str>) -> RunConfig {
        RunConfig {
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(model.map(str::to_string)),
            api_key: None,
            base_url: None,
            mcp_servers: Vec::<McpServerConfig>::new(),
            hooks: Vec::<HookConfig>::new(),
            external_tools: Vec::<ExternalToolConfig>::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            permission_rules: PermissionRules::default(),
            max_budget_usd: None,
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::default(),
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn run_child_agent_applies_subagent_model_override() {
        let request = ChildAgentRequest {
            prompt: "inspect repo".to_string(),
            subagent_type: SubagentType::General,
            model: Some(FLASH_MODEL.to_string()),
            depth: 1,
            emit_deltas: false,
        };

        let (result, _) = run_child_agent(&config(None), &request, |child_config, _, _| {
            assert_eq!(child_config.model.as_deref(), Some(FLASH_MODEL));
            Ok(ChildAgentResult {
                status: RunStatus::Success,
                final_message: Some("ok".to_string()),
                error: None,
            })
        })
        .expect("run child agent");

        assert_eq!(result.status, RunStatus::Success);
    }

    #[test]
    fn run_child_agent_ignores_auto_override() {
        let request = ChildAgentRequest {
            prompt: "inspect repo".to_string(),
            subagent_type: SubagentType::General,
            model: Some(AUTO_MODEL.to_string()),
            depth: 1,
            emit_deltas: false,
        };

        run_child_agent(
            &config(Some("deepseek-v4-pro")),
            &request,
            |child_config, _, _| {
                assert_eq!(child_config.model.as_deref(), Some("deepseek-v4-pro"));
                Ok(ChildAgentResult {
                    status: RunStatus::Success,
                    final_message: None,
                    error: None,
                })
            },
        )
        .expect("run child agent");
    }
}
