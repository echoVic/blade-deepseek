use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::context::ContextConfig;
use orca_provider::tool_schema::deepseek_tools_schema_for_type_with_mcp_and_external;

use crate::agent_common;
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::RuntimeSessionLifecycle;
use crate::memory::MemoryBlock;
use crate::workflow::ipc::WorkflowIpcContext;

#[derive(Clone, Debug)]
pub struct ChildAgentRequest {
    pub prompt: String,
    pub subagent_type: SubagentType,
    pub model: Option<String>,
    pub depth: u32,
    pub emit_deltas: bool,
    pub allowed_tools: Option<Vec<String>>,
    pub tool_policy_label: Option<String>,
    pub(crate) workflow_ipc: Option<WorkflowIpcContext>,
}

impl ChildAgentRequest {
    pub fn new(
        prompt: String,
        subagent_type: SubagentType,
        model: Option<String>,
        depth: u32,
        emit_deltas: bool,
    ) -> Self {
        Self {
            prompt,
            subagent_type,
            model,
            depth,
            emit_deltas,
            allowed_tools: None,
            tool_policy_label: None,
            workflow_ipc: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChildAgentResult {
    pub status: RunStatus,
    pub final_message: Option<String>,
    pub error: Option<String>,
}

pub struct ChildAgentLoopSetup {
    pub mcp_registry: McpRegistry,
    pub provider_config: ProviderConfig,
    pub context_config: ContextConfig,
    pub conversation: Conversation,
    pub policy: ApprovalPolicy,
}

pub(crate) type ChildAgentExecutor<W> = fn(
    &RunConfig,
    &ChildAgentRequest,
    &mut ChildAgentRuntime<'_, W>,
    &mut CostTracker,
) -> io::Result<ChildAgentResult>;

pub(crate) struct ChildAgentRuntime<'a, W: io::Write> {
    pub cwd: &'a Path,
    pub events: &'a mut EventFactory,
    pub sink: &'a mut EventSink<W>,
    pub instructions: &'a ProjectInstructions,
    pub memory: &'a MemoryBlock,
    pub mcp_registry: &'a McpRegistry,
    pub hooks: &'a HookRunner,
    pub cancel: &'a CancelToken,
    pub lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
    executor: ChildAgentExecutor<W>,
}

impl<'a, W: io::Write> ChildAgentRuntime<'a, W> {
    pub(crate) fn new(
        cwd: &'a Path,
        events: &'a mut EventFactory,
        sink: &'a mut EventSink<W>,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        hooks: &'a HookRunner,
        cancel: &'a CancelToken,
        lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
        executor: ChildAgentExecutor<W>,
    ) -> Self {
        Self {
            cwd,
            events,
            sink,
            instructions,
            memory,
            mcp_registry,
            hooks,
            cancel,
            lifecycle,
            executor,
        }
    }

    fn execute(
        &mut self,
        config: &RunConfig,
        request: &ChildAgentRequest,
        child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        (self.executor)(config, request, self, child_cost_tracker)
    }
}

pub(crate) fn run_child_agent<W: io::Write>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    runtime: &mut ChildAgentRuntime<'_, W>,
) -> (ChildAgentResult, CostTracker) {
    run_child_agent_with_executor(
        config,
        request,
        |child_config, request, child_cost_tracker| {
            runtime.execute(child_config, request, child_cost_tracker)
        },
    )
}

pub fn run_child_agent_with_executor<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    mut executor: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(&RunConfig, &ChildAgentRequest, &mut CostTracker) -> io::Result<ChildAgentResult>,
{
    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let mut child_cost_tracker = CostTracker::new(child_config.model.as_deref());
    let result =
        executor(&child_config, request, &mut child_cost_tracker).unwrap_or_else(|error| {
            ChildAgentResult {
                status: RunStatus::Failed,
                final_message: None,
                error: Some(error.to_string()),
            }
        });
    (result, child_cost_tracker)
}

pub fn prepare_child_agent_loop(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
) -> ChildAgentLoopSetup {
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(orca_core::model::FLASH_MODEL.to_string()),
        reasoning_effort: config.reasoning_effort,
        tools_override: Some(deepseek_tools_schema_for_type_with_mcp_and_external(
            &request.subagent_type,
            Some(&mcp_registry),
            &config.external_tools,
        )),
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
    };

    let budget_model = config.model.as_option();
    let context_config =
        ContextConfig::for_model_with_runtime(budget_model.as_deref(), &config.model_runtime);
    let mut conversation = Conversation::new();
    conversation.add_system(agent_common::build_agent_system_prompt(
        cwd,
        request.depth,
        &request.subagent_type,
        Some(instructions),
        config.approval_mode,
        Some(memory),
    ));
    conversation.add_user(request.prompt.clone());

    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());

    ChildAgentLoopSetup {
        mcp_registry,
        provider_config,
        context_config,
        conversation,
        policy,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Message;
    use orca_core::event_schema::EventFactory;
    use orca_core::event_sink::EventSink;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::{AUTO_MODEL, FLASH_MODEL, ModelSelection};
    use orca_core::provider_types::Usage;
    use orca_core::subagent_config::SubagentConfig;
    use std::io::Cursor;

    use crate::hooks::HookRunner;
    use crate::instructions::ProjectInstructions;
    use crate::memory::MemoryBlock;

    fn config(model: Option<&str>) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(model.map(str::to_string)),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::<McpServerConfig>::new(),
            hooks: Vec::<HookConfig>::new(),
            external_tools: Vec::<ExternalToolConfig>::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
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

    fn runtime<'a>(
        sink: &'a mut EventSink<Cursor<Vec<u8>>>,
        events: &'a mut EventFactory,
        cancel: &'a CancelToken,
        executor: ChildAgentExecutor<Cursor<Vec<u8>>>,
    ) -> ChildAgentRuntime<'a, Cursor<Vec<u8>>> {
        let instructions = Box::leak(Box::new(ProjectInstructions::default()));
        let memory = Box::leak(Box::new(MemoryBlock::default()));
        let mcp_registry = Box::leak(Box::new(McpRegistry::default()));
        let hooks = Box::leak(Box::new(HookRunner::new(Vec::new())));
        let cwd = Box::leak(Box::new(std::env::temp_dir()));
        ChildAgentRuntime::new(
            cwd.as_path(),
            events,
            sink,
            instructions,
            memory,
            mcp_registry,
            hooks,
            cancel,
            None,
            executor,
        )
    }

    #[test]
    fn prepare_child_agent_loop_builds_provider_conversation_and_policy() {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let setup = prepare_child_agent_loop(
            &config(Some("deepseek-v4-pro")),
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );

        assert_eq!(
            setup.provider_config.model.as_deref(),
            Some(orca_core::model::FLASH_MODEL)
        );
        assert!(setup.provider_config.tools_override.is_some());
        assert!(setup.provider_config.mcp_registry.is_some());
        assert!(setup.context_config.max_tokens > 0);
        assert_eq!(setup.conversation.messages.len(), 2);
        assert!(matches!(
            setup.conversation.messages.first(),
            Some(Message::System { .. })
        ));
        assert!(matches!(
            setup.conversation.messages.get(1),
            Some(Message::User { content, .. }) if content == "inspect repo"
        ));
        assert!(format!("{:?}", setup.policy).contains("Suggest"));
    }

    #[test]
    fn run_child_agent_applies_subagent_model_override() {
        let request = ChildAgentRequest {
            prompt: "inspect repo".to_string(),
            subagent_type: SubagentType::General,
            model: Some(FLASH_MODEL.to_string()),
            depth: 1,
            emit_deltas: false,
            allowed_tools: None,
            tool_policy_label: None,
            workflow_ipc: None,
        };
        let cancel = CancelToken::new();
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut runtime = runtime(&mut sink, &mut events, &cancel, |child_config, _, _, _| {
            assert_eq!(child_config.model.as_deref(), Some(FLASH_MODEL));
            Ok(ChildAgentResult {
                status: RunStatus::Success,
                final_message: Some("ok".to_string()),
                error: None,
            })
        });

        let (result, _) = run_child_agent(&config(None), &request, &mut runtime);

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
            allowed_tools: None,
            tool_policy_label: None,
            workflow_ipc: None,
        };
        let cancel = CancelToken::new();
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut runtime = runtime(&mut sink, &mut events, &cancel, |child_config, _, _, _| {
            assert_eq!(child_config.model.as_deref(), Some("deepseek-v4-pro"));
            Ok(ChildAgentResult {
                status: RunStatus::Success,
                final_message: None,
                error: None,
            })
        });

        let _ = run_child_agent(&config(Some("deepseek-v4-pro")), &request, &mut runtime);
    }

    #[test]
    fn run_child_agent_preserves_cost_tracker_on_loop_error() {
        let request = ChildAgentRequest {
            prompt: "inspect repo".to_string(),
            subagent_type: SubagentType::General,
            model: None,
            depth: 1,
            emit_deltas: false,
            allowed_tools: None,
            tool_policy_label: None,
            workflow_ipc: None,
        };
        let cancel = CancelToken::new();
        let mut events = EventFactory::new("test-run".to_string());
        let mut sink = EventSink::new(Cursor::new(Vec::new()), OutputFormat::Jsonl);
        let mut runtime = runtime(&mut sink, &mut events, &cancel, |_, _, _, tracker| {
            tracker.add_usage(Usage {
                input_tokens: 7,
                output_tokens: 3,
                cache_tokens: 2,
            });
            Err(io::Error::other("child loop failed"))
        });

        let (result, tracker) = run_child_agent(&config(None), &request, &mut runtime);

        assert_eq!(result.status, RunStatus::Failed);
        assert_eq!(result.error.as_deref(), Some("child loop failed"));
        let tracker_debug = format!("{tracker:?}");
        assert!(tracker_debug.contains("input_tokens: 7"));
        assert!(tracker_debug.contains("output_tokens: 3"));
    }
}
