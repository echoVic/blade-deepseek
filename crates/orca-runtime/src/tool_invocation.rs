use std::collections::HashSet;
use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ActionKind, ApprovalRequest};
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::EventFactory;
use orca_core::event_schema::RunStatus;
use orca_core::event_sink::EventSink;
use orca_core::external_config::ExternalToolConfig;
use orca_core::hook_types::HookEvent;
use orca_core::provider_types::ProviderStep;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::{ToolName, ToolOutputTruncation, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::tool_schema::{
    deepseek_tools_schema_for_allowed_names_with_mcp_and_external,
    deepseek_tools_schema_for_type_with_mcp_and_external,
    deepseek_tools_schema_with_mcp_and_external,
};
use serde_json::Value;

use crate::agent_child::ChildAgentExecutor;
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookOutcome, HookRunner, tool_request_with_hook_outcome};
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{RuntimePermissionRequestHandler, TurnPermissionOverlay};
use crate::memory::MemoryBlock;
use crate::session::{record_plan_state_for_agent, record_tool_result_for_agent};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_execution::{ToolExecutionContext, execute_tool_with_approval};
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

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

pub(crate) enum ToolTurnOutcome {
    Continue,
    Return {
        status: RunStatus,
        error: Option<String>,
    },
}

pub(crate) struct ToolRequestCursor<'a> {
    requests: &'a [ToolRequest],
    index: usize,
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

impl<'a> ToolRequestCursor<'a> {
    pub(crate) fn new(requests: &'a [ToolRequest]) -> Self {
        Self { requests, index: 0 }
    }

    pub(crate) fn current(&self) -> Option<&'a ToolRequest> {
        self.requests.get(self.index)
    }

    pub(crate) fn position(&self) -> usize {
        self.index
    }

    pub(crate) fn advance_one(&mut self) {
        self.advance_to(self.index.saturating_add(1));
    }

    pub(crate) fn advance_to(&mut self, next_index: usize) {
        self.index = next_index.min(self.requests.len());
    }
}

impl ToolExecutionFailure {
    pub fn into_result(self) -> ToolResult {
        ToolResult::invalid_input(&self.request, self.message)
    }
}

impl ToolTurnOutcome {
    pub(crate) fn from_terminal(status: RunStatus, error: Option<String>) -> Self {
        Self::Return { status, error }
    }
}

pub(crate) fn terminal_tool_turn(status: RunStatus, error: Option<String>) -> ToolTurnOutcome {
    ToolTurnOutcome::from_terminal(status, error)
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

pub(crate) fn execute_readonly_batch(
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_requests: &[ToolRequest],
    emit_deltas: bool,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    output_truncation: ToolOutputTruncation,
) -> io::Result<Vec<ToolResult>> {
    let mut hook_failed: Vec<Option<ToolResult>> = vec![None; tool_requests.len()];
    let mut runnable = Vec::new();

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        let invocation =
            prepare_tool_invocation_with_external(tool_request, 0, u32::MAX, mcp_registry, &[]);
        if emit_deltas {
            sink.emit(&events.tool_call_requested(tool_request))?;
        }
        match hooks.run(
            HookEvent::PreToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) => {
                match apply_pre_tool_outcome_with_external(invocation, &outcome, mcp_registry, &[])
                {
                    Ok(invocation) => runnable.push((idx, invocation.effective)),
                    Err(error) => hook_failed[idx] = Some(error.into_result()),
                }
            }
            Err(error) => {
                hook_failed[idx] = Some(ToolResult::failed(
                    tool_request,
                    format!("pre_tool_use hook blocked tool: {error}"),
                    None,
                ));
            }
        }
    }

    let mut results = orca_tools::run_readonly_batch_parallel_with_policy(
        tool_requests,
        runnable,
        cwd,
        mcp_registry,
        output_truncation,
    );

    for (idx, failed) in hook_failed.into_iter().enumerate() {
        if let Some(result) = failed {
            results[idx] = result;
        }
    }

    for (tool_request, result) in tool_requests.iter().zip(results.iter()) {
        if emit_deltas {
            sink.emit(&events.tool_call_completed(result))?;
            if let Err(error) = hooks.run(
                HookEvent::PostToolUse,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: Some(tool_request),
                    tool_result: Some(result),
                    before_messages: None,
                    after_messages: None,
                    usage: None,
                },
            ) {
                sink.emit(&events.error(&format!("post_tool_use hook failed: {error}")))?;
            }
        }
    }

    Ok(results)
}

pub(crate) fn should_run_readonly_batch(
    max_read_parallel: usize,
    tool_request: &ToolRequest,
) -> bool {
    orca_tools::should_run_readonly_batch(max_read_parallel, tool_request)
}

pub(crate) fn collect_readonly_batch(
    max_read_parallel: usize,
    tool_requests: &[ToolRequest],
    start: usize,
) -> usize {
    orca_tools::collect_readonly_batch(max_read_parallel, tool_requests, start)
}

pub(crate) fn record_readonly_batch_results(
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    results: Vec<ToolResult>,
    emit_deltas: bool,
) -> io::Result<()> {
    for result in results {
        record_tool_result_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            &result,
            emit_deltas,
        )?;
    }
    Ok(())
}

pub(crate) fn record_normal_tool_result(
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    tool_request: &ToolRequest,
    result: &ToolResult,
    status: RunStatus,
    emit_deltas: bool,
) -> io::Result<ToolTurnOutcome> {
    record_plan_state_for_agent(
        conversation,
        history_writer.as_deref_mut(),
        tool_request,
        result,
    );
    record_tool_result_for_agent(
        conversation,
        history_writer.as_deref_mut(),
        result,
        emit_deltas,
    )?;

    if status == RunStatus::ApprovalRequired {
        return Ok(terminal_tool_turn(status, result.error.clone()));
    }
    if status == RunStatus::Failed && tool_request.name == ToolName::Subagent {
        return Ok(terminal_tool_turn(
            RunStatus::Failed,
            Some(result.error.clone().unwrap_or_default()),
        ));
    }

    Ok(ToolTurnOutcome::Continue)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_normal_tool_turn<W: io::Write>(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    conversation: &mut Conversation,
    history_writer: Option<&mut SessionWriter>,
    tool_request: &ToolRequest,
    subagent_depth: u32,
    emit_deltas: bool,
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
    workflow_ipc: Option<&WorkflowIpcContext>,
    permission_overlay: &mut TurnPermissionOverlay,
    permission_handler: Option<&(dyn RuntimePermissionRequestHandler + Send + Sync)>,
    child_executor: ChildAgentExecutor<W>,
    workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
) -> io::Result<ToolTurnOutcome> {
    let (status, result) = execute_tool_with_approval(
        config,
        events,
        sink,
        tool_request,
        ToolExecutionContext::new(cwd, subagent_depth, emit_deltas, policy)
            .with_services(instructions, memory, mcp_registry, hooks)
            .with_runtime(
                cost_tracker,
                cancel,
                task_registry,
                background_workflows,
                workflow_ipc,
            )
            .with_permission_overlay(permission_overlay)
            .with_permission_handler(permission_handler),
        child_executor,
        workflow_child_executor,
    )?;

    record_normal_tool_result(
        conversation,
        history_writer,
        tool_request,
        &result,
        status,
        emit_deltas,
    )
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
    use std::io;

    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::{Conversation, Message};
    use orca_core::event_schema::EventFactory;
    use orca_core::event_schema::RunStatus;
    use orca_core::event_sink::EventSink;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::mcp_types::McpTool;
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::ProviderStep;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::subagent_types::SubagentType;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
    use orca_mcp::McpRegistry;
    use serde_json::json;

    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::cost::CostTracker;
    use crate::hooks::HookOutcome;
    use crate::instructions::ProjectInstructions;
    use crate::lifecycle::TurnPermissionOverlay;
    use crate::memory::MemoryBlock;
    use crate::tasks::TaskRegistry;
    use crate::tool_execution::policy_for_tool_execution;

    use super::{
        AgentToolPolicyContext, ToolRequestCursor, ToolTurnOutcome, apply_pre_tool_outcome,
        approval_request_for_invocation, prepare_tool_invocation, provider_config_for_agent_loop,
        provider_tool_schema_override, record_normal_tool_result, record_readonly_batch_results,
        run_normal_tool_turn, terminal_tool_turn, tool_requests_from_provider_steps,
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
    fn tool_request_cursor_advances_over_single_and_batch_requests() {
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: None,
        };
        let third = ToolRequest {
            id: "tool-3".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let requests = vec![first, second, third];

        let mut cursor = ToolRequestCursor::new(&requests);

        assert_eq!(
            cursor.current().map(|request| request.id.as_str()),
            Some("tool-1")
        );
        cursor.advance_one();
        assert_eq!(cursor.position(), 1);
        assert_eq!(
            cursor.current().map(|request| request.id.as_str()),
            Some("tool-2")
        );
        cursor.advance_to(3);
        assert_eq!(cursor.position(), 3);
        assert!(cursor.current().is_none());
        cursor.advance_to(99);
        assert_eq!(cursor.position(), 3);
    }

    #[test]
    fn record_normal_tool_result_returns_approval_required_after_recording_tool_message() {
        let mut conversation = Conversation::new();
        let request = request(
            ToolName::RequestPermissions,
            ActionKind::Read,
            Some("read"),
            None,
        );
        let result = ToolResult::denied(&request, "needs approval");

        let outcome = record_normal_tool_result(
            &mut conversation,
            None,
            &request,
            &result,
            RunStatus::ApprovalRequired,
            false,
        )
        .expect("record approval result");

        match outcome {
            ToolTurnOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::ApprovalRequired);
                assert_eq!(error.as_deref(), Some("needs approval"));
            }
            ToolTurnOutcome::Continue => panic!("approval-required result must return"),
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
    }

    #[test]
    fn record_normal_tool_result_returns_subagent_failure_after_recording_tool_message() {
        let mut conversation = Conversation::new();
        let request = request(ToolName::Subagent, ActionKind::Agent, Some("audit"), None);
        let result = ToolResult::failed(&request, "child failed", None);

        let outcome = record_normal_tool_result(
            &mut conversation,
            None,
            &request,
            &result,
            RunStatus::Failed,
            false,
        )
        .expect("record subagent failure");

        match outcome {
            ToolTurnOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(error.as_deref(), Some("child failed"));
            }
            ToolTurnOutcome::Continue => panic!("failed subagent result must return"),
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
    }

    #[test]
    fn record_readonly_batch_results_records_each_tool_message_in_order() {
        let mut conversation = Conversation::new();
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: None,
        };
        let results = vec![
            ToolResult::completed(&first, "one".to_string(), false),
            ToolResult::completed(&second, "two".to_string(), false),
        ];

        record_readonly_batch_results(&mut conversation, None, results, false)
            .expect("record readonly batch results");

        assert_eq!(conversation.messages.len(), 2);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
        assert!(
            matches!(&conversation.messages[1], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-2")
        );
    }

    fn unused_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("read_file turn must not execute child agents")
    }

    #[test]
    fn run_normal_tool_turn_executes_and_records_tool_result() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").expect("write file");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        let mut events = EventFactory::new("normal-tool-turn".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let request = request(
            ToolName::ReadFile,
            ActionKind::Read,
            Some("tracked.txt"),
            Some(json!({ "path": "tracked.txt" }).to_string().as_str()),
        );
        let policy = policy_for_tool_execution(&config);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = crate::hooks::HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("normal-tool-turn".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = TurnPermissionOverlay::default();

        let outcome = run_normal_tool_turn(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &mut conversation,
            None,
            &request,
            0,
            false,
            &policy,
            &instructions,
            &memory,
            &registry,
            &hooks,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            &mut background_workflows,
            None,
            &mut permission_overlay,
            None,
            unused_child_executor,
            unused_child_executor,
        )
        .expect("run normal tool turn");

        assert!(matches!(outcome, ToolTurnOutcome::Continue));
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, content, .. }
                if tool_call_id == "tool-1" && content.contains("hello"))
        );
    }

    #[test]
    fn terminal_tool_turn_carries_status_and_optional_error() {
        match terminal_tool_turn(RunStatus::Failed, Some("tool failed".to_string())) {
            ToolTurnOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(error.as_deref(), Some("tool failed"));
            }
            ToolTurnOutcome::Continue => panic!("terminal tool turn must return"),
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
