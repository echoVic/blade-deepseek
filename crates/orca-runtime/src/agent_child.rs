use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::ToolRequest;
use orca_mcp::McpRegistry;

use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::RuntimeSessionLifecycle;
use crate::memory::MemoryBlock;
use crate::workflow::ipc::WorkflowIpcContext;

pub use crate::child_agent_loop_setup::{
    ChildAgentLoopSetup, ChildAgentTurnBudget, DEFAULT_CHILD_AGENT_MAX_TURNS,
    advance_child_agent_turn, advance_child_agent_turn_with_limit, prepare_child_agent_loop,
};
pub use crate::child_agent_provider_turn::{
    ChildAgentProviderErrorDecision, ChildAgentProviderTurn,
    compact_child_agent_conversation_if_needed, handle_child_agent_provider_error,
    route_child_agent_model, run_child_agent_provider_turn,
};
pub use crate::child_agent_response_folding::{
    ChildAgentProviderResponseFold, ChildAgentToolContext, ChildAgentToolExecution,
    ChildAgentToolResultFold, child_agent_tool_requests, fold_child_agent_provider_response,
    fold_child_agent_tool_result,
};

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

pub fn run_child_agent_loop_with_tool_executor<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    child_cost_tracker: &mut CostTracker,
    mut execute_tool: F,
) -> io::Result<ChildAgentResult>
where
    F: FnMut(&ChildAgentToolContext<'_>, &CancelToken, &ToolRequest) -> ChildAgentToolExecution,
{
    let mut setup = prepare_child_agent_loop(config, request, cwd, instructions, memory);
    loop {
        match advance_child_agent_turn(&mut setup) {
            ChildAgentTurnBudget::Continue => {}
            ChildAgentTurnBudget::Stop(result) => return Ok(result),
        }

        compact_child_agent_conversation_if_needed(config, &mut setup, cwd, hooks)?;

        let child_cancel = CancelToken::new();
        let turn_provider_config =
            route_child_agent_model(config, request, &setup, child_cost_tracker);

        let response = match run_child_agent_provider_turn(
            config,
            &setup,
            cwd,
            hooks,
            &turn_provider_config,
            &child_cancel,
        ) {
            ChildAgentProviderTurn::Response(response) => response,
            ChildAgentProviderTurn::Fail(result) => return Ok(result),
        };

        match handle_child_agent_provider_error(config, &mut setup, cwd, hooks, &response)? {
            Some(ChildAgentProviderErrorDecision::RetryAfterCompaction) => continue,
            Some(ChildAgentProviderErrorDecision::Fail(result)) => return Ok(result),
            None => {}
        }

        match fold_child_agent_provider_response(&mut setup, &response, child_cost_tracker) {
            ChildAgentProviderResponseFold::Complete(result) => return Ok(result),
            ChildAgentProviderResponseFold::ContinueToTools => {}
        }

        for tool_request in child_agent_tool_requests(&response) {
            let tool_context = ChildAgentToolContext {
                policy: &setup.policy,
                mcp_registry: &setup.mcp_registry,
            };
            let tool_execution = execute_tool(&tool_context, &child_cancel, tool_request);
            match fold_child_agent_tool_result(
                &mut setup,
                tool_request,
                tool_execution.should_stop,
                tool_execution.result,
                tool_execution.child_cost,
                child_cost_tracker,
            ) {
                ChildAgentToolResultFold::Continue => {}
                ChildAgentToolResultFold::Stop(result) => return Ok(result),
            }
        }
    }
}

pub fn run_child_agent_with_tool_executor<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    mut execute_tool: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(
        &RunConfig,
        &ChildAgentRequest,
        &ChildAgentToolContext<'_>,
        &CancelToken,
        &ToolRequest,
    ) -> ChildAgentToolExecution,
{
    run_child_agent_with_executor(config, request, |config, request, child_cost_tracker| {
        run_child_agent_loop_with_tool_executor(
            config,
            request,
            cwd,
            instructions,
            memory,
            hooks,
            child_cost_tracker,
            |tool_context, child_cancel, tool_request| {
                execute_tool(config, request, tool_context, child_cancel, tool_request)
            },
        )
    })
}

pub fn run_child_agent_prompt_with_tool_executor<F>(
    config: &RunConfig,
    prompt: String,
    subagent_type: &SubagentType,
    subagent_model: Option<String>,
    subagent_depth: u32,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    execute_tool: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(
        &RunConfig,
        &ChildAgentRequest,
        &ChildAgentToolContext<'_>,
        &CancelToken,
        &ToolRequest,
    ) -> ChildAgentToolExecution,
{
    let request = ChildAgentRequest::new(
        prompt,
        subagent_type.clone(),
        subagent_model,
        subagent_depth,
        false,
    );
    run_child_agent_with_tool_executor(
        config,
        &request,
        cwd,
        instructions,
        memory,
        hooks,
        execute_tool,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::cancel::CancelToken;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::{Message, RawToolCall};
    use orca_core::event_schema::EventFactory;
    use orca_core::event_sink::EventSink;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::{HookConfig, HookEvent};
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::{AUTO_MODEL, FLASH_MODEL, ModelSelection};
    use orca_core::provider_types::{ProviderResponse, ProviderStep, Usage};
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
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

    fn child_loop_setup(runtime_config: &RunConfig) -> ChildAgentLoopSetup {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        prepare_child_agent_loop(
            runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
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
        assert_eq!(setup.turn, 0);
        assert!(!setup.reactive_compacted);
    }

    #[test]
    fn advance_child_agent_turn_stops_after_runtime_owned_limit() {
        let runtime_config = config(None);
        let mut setup = child_loop_setup(&runtime_config);

        assert!(matches!(
            advance_child_agent_turn_with_limit(&mut setup, 1),
            ChildAgentTurnBudget::Continue
        ));
        assert_eq!(setup.turn, 1);

        match advance_child_agent_turn_with_limit(&mut setup, 1) {
            ChildAgentTurnBudget::Stop(result) => {
                assert_eq!(result.status, RunStatus::BudgetExhausted);
                assert_eq!(result.error.as_deref(), Some("max turns exhausted"));
            }
            ChildAgentTurnBudget::Continue => panic!("turn beyond limit should stop"),
        }
        assert_eq!(setup.turn, 2);
    }

    #[test]
    fn advance_child_agent_turn_uses_default_child_limit() {
        let runtime_config = config(None);
        let mut setup = child_loop_setup(&runtime_config);

        assert!(matches!(
            advance_child_agent_turn(&mut setup),
            ChildAgentTurnBudget::Continue
        ));

        assert_eq!(setup.turn, 1);
    }

    #[test]
    fn route_child_agent_model_updates_provider_config_and_cost_model() {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let setup = prepare_child_agent_loop(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );
        let mut tracker = CostTracker::new(None);

        let provider_config =
            route_child_agent_model(&runtime_config, &request, &setup, &mut tracker);
        let totals = tracker.add_usage(Usage {
            input_tokens: 1_000,
            output_tokens: 1_000,
            cache_tokens: 0,
        });

        assert_eq!(
            provider_config.model.as_deref(),
            Some(orca_core::model::PRO_MODEL)
        );
        let expected_pro_cost = (1_000.0 * 0.435 + 1_000.0 * 0.87) / 1_000_000.0;
        assert!((totals.estimated_cost_usd - expected_pro_cost).abs() < 1e-12);
    }

    #[test]
    fn run_child_agent_provider_turn_applies_model_hooks_around_provider_call() {
        let request = ChildAgentRequest::new(
            "mock_system_echo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let setup = prepare_child_agent_loop(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );
        let provider_config = route_child_agent_model(
            &runtime_config,
            &request,
            &setup,
            &mut CostTracker::new(None),
        );
        let hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreModelCall,
            command: "printf runtime-hook-context".to_string(),
            tool: None,
        }]);
        let cancel = CancelToken::new();

        let turn = run_child_agent_provider_turn(
            &runtime_config,
            &setup,
            std::env::temp_dir().as_path(),
            &hooks,
            &provider_config,
            &cancel,
        );

        let ChildAgentProviderTurn::Response(response) = turn else {
            panic!("expected provider response")
        };
        assert!(
            response
                .assistant_content
                .as_deref()
                .unwrap_or_default()
                .contains("runtime-hook-context")
        );
    }

    #[test]
    fn run_child_agent_provider_turn_returns_child_failure_for_model_hook_errors() {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let setup = prepare_child_agent_loop(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );
        let provider_config = route_child_agent_model(
            &runtime_config,
            &request,
            &setup,
            &mut CostTracker::new(None),
        );
        let cancel = CancelToken::new();
        let pre_hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PreModelCall,
            command: "printf pre-failed >&2; exit 7".to_string(),
            tool: None,
        }]);

        let pre_turn = run_child_agent_provider_turn(
            &runtime_config,
            &setup,
            std::env::temp_dir().as_path(),
            &pre_hooks,
            &provider_config,
            &cancel,
        );

        match pre_turn {
            ChildAgentProviderTurn::Fail(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert!(
                    result
                        .error
                        .as_deref()
                        .unwrap_or_default()
                        .contains("pre_model_call hook failed")
                );
            }
            ChildAgentProviderTurn::Response(_) => panic!("pre hook failure should fail the child"),
        }

        let post_hooks = HookRunner::new(vec![HookConfig {
            event: HookEvent::PostModelCall,
            command: "printf post-failed >&2; exit 8".to_string(),
            tool: None,
        }]);

        let post_turn = run_child_agent_provider_turn(
            &runtime_config,
            &setup,
            std::env::temp_dir().as_path(),
            &post_hooks,
            &provider_config,
            &cancel,
        );

        match post_turn {
            ChildAgentProviderTurn::Fail(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert!(
                    result
                        .error
                        .as_deref()
                        .unwrap_or_default()
                        .contains("post_model_call hook failed")
                );
            }
            ChildAgentProviderTurn::Response(_) => {
                panic!("post hook failure should fail the child")
            }
        }
    }

    #[test]
    fn compact_child_agent_conversation_uses_runtime_compaction_step() {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mut runtime_config = config(None);
        runtime_config.model_runtime.context_window = Some(128);
        runtime_config.model_runtime.auto_compact_token_limit = Some(64);
        let mut setup = prepare_child_agent_loop(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );
        for index in 0..20 {
            setup.conversation.add_user(format!(
                "child message {index}: {}",
                "important context ".repeat(20)
            ));
            setup.conversation.add_assistant(
                Some(format!(
                    "child answer {index}: {}",
                    "detailed response ".repeat(20)
                )),
                None,
                vec![],
            );
        }
        let before_messages = setup.conversation.messages.len();

        let compacted = compact_child_agent_conversation_if_needed(
            &runtime_config,
            &mut setup,
            std::env::temp_dir().as_path(),
            &HookRunner::default(),
        )
        .expect("child compaction should not fail");

        assert!(compacted);
        assert!(setup.conversation.messages.len() < before_messages);
    }

    #[test]
    fn handle_child_agent_provider_error_retries_prompt_too_long_once() {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mut runtime_config = config(None);
        runtime_config.model_runtime.context_window = Some(128);
        runtime_config.model_runtime.auto_compact_token_limit = Some(64);
        let mut setup = prepare_child_agent_loop(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );
        for index in 0..20 {
            setup.conversation.add_user(format!(
                "child message {index}: {}",
                "important context ".repeat(20)
            ));
            setup.conversation.add_assistant(
                Some(format!(
                    "child answer {index}: {}",
                    "detailed response ".repeat(20)
                )),
                None,
                vec![],
            );
        }
        let before_messages = setup.conversation.messages.len();
        let response = ProviderResponse {
            steps: vec![ProviderStep::Error("prompt_too_long".to_string())],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: vec![],
            usage: None,
        };

        let decision = handle_child_agent_provider_error(
            &runtime_config,
            &mut setup,
            std::env::temp_dir().as_path(),
            &HookRunner::default(),
            &response,
        )
        .expect("provider-error handling should not fail")
        .expect("prompt-too-long should produce a decision");

        assert!(matches!(
            decision,
            ChildAgentProviderErrorDecision::RetryAfterCompaction
        ));
        assert!(setup.reactive_compacted);
        assert!(setup.conversation.messages.len() < before_messages);

        let decision = handle_child_agent_provider_error(
            &runtime_config,
            &mut setup,
            std::env::temp_dir().as_path(),
            &HookRunner::default(),
            &response,
        )
        .expect("provider-error handling should not fail")
        .expect("repeated prompt-too-long should fail");

        match decision {
            ChildAgentProviderErrorDecision::Fail(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert_eq!(result.error.as_deref(), Some("prompt_too_long"));
            }
            ChildAgentProviderErrorDecision::RetryAfterCompaction => {
                panic!("repeated prompt-too-long should not retry")
            }
        }
    }

    #[test]
    fn fold_child_agent_provider_response_records_usage_and_terminal_assistant() {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let mut setup = prepare_child_agent_loop(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("done".to_string())],
            assistant_content: Some("done".to_string()),
            assistant_reasoning: Some("reasoned".to_string()),
            tool_calls: vec![],
            usage: Some(Usage {
                input_tokens: 120,
                output_tokens: 30,
                cache_tokens: 10,
            }),
        };
        let mut tracker = CostTracker::new(Some(orca_core::model::PRO_MODEL));

        let fold = fold_child_agent_provider_response(&mut setup, &response, &mut tracker);

        match fold {
            ChildAgentProviderResponseFold::Complete(result) => {
                assert_eq!(result.status, RunStatus::Success);
                assert_eq!(result.final_message.as_deref(), Some("done"));
            }
            ChildAgentProviderResponseFold::ContinueToTools => {
                panic!("terminal response should complete child run")
            }
        }
        assert!(tracker.totals().total_tokens() > 0);
        assert!(matches!(
            setup.conversation.messages.last(),
            Some(Message::Assistant {
                content: Some(content),
                reasoning_content: Some(reasoning),
                tool_calls,
                ..
            }) if content == "done" && reasoning == "reasoned" && tool_calls.is_empty()
        ));
    }

    #[test]
    fn fold_child_agent_provider_response_records_assistant_before_tools() {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let mut setup = prepare_child_agent_loop(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );
        let response = ProviderResponse {
            steps: vec![],
            assistant_content: Some("I need a tool".to_string()),
            assistant_reasoning: None,
            tool_calls: vec![RawToolCall {
                id: "tool-1".to_string(),
                function_name: "bash".to_string(),
                arguments: "{\"command\":\"echo hi\"}".to_string(),
            }],
            usage: None,
        };
        let mut tracker = CostTracker::new(None);

        let fold = fold_child_agent_provider_response(&mut setup, &response, &mut tracker);

        assert!(matches!(
            fold,
            ChildAgentProviderResponseFold::ContinueToTools
        ));
        assert!(matches!(
            setup.conversation.messages.last(),
            Some(Message::Assistant {
                content: Some(content),
                tool_calls,
                ..
            }) if content == "I need a tool" && tool_calls.len() == 1
        ));
    }

    #[test]
    fn child_agent_tool_requests_extracts_only_provider_tool_calls() {
        let first = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo one".to_string()),
            raw_arguments: None,
        };
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("Cargo.toml".to_string()),
            raw_arguments: None,
        };
        let response = ProviderResponse {
            steps: vec![
                ProviderStep::MessageDelta("before".to_string()),
                ProviderStep::ToolCall(first.clone()),
                ProviderStep::Error("ignored here".to_string()),
                ProviderStep::ToolCall(second.clone()),
            ],
            assistant_content: Some("tool please".to_string()),
            assistant_reasoning: None,
            tool_calls: vec![],
            usage: None,
        };

        let requests = child_agent_tool_requests(&response);

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].id, first.id);
        assert_eq!(requests[1].id, second.id);
    }

    #[test]
    fn run_child_agent_loop_with_tool_executor_runs_tools_until_provider_completes() {
        let request = ChildAgentRequest::new(
            "bash echo child".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let mut tracker = CostTracker::new(None);
        let mut tool_count = 0;

        let result = run_child_agent_loop_with_tool_executor(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
            &HookRunner::default(),
            &mut tracker,
            |_setup, _cancel, tool_request| {
                tool_count += 1;
                assert_eq!(tool_request.name, ToolName::Bash);
                assert_eq!(tool_request.target.as_deref(), Some("echo child"));
                ChildAgentToolExecution {
                    should_stop: false,
                    result: ToolResult::completed(
                        tool_request,
                        "child tool ran".to_string(),
                        false,
                    ),
                    child_cost: None,
                }
            },
        )
        .expect("child loop runner should complete");

        assert_eq!(result.status, RunStatus::Success);
        assert_eq!(
            result.final_message.as_deref(),
            Some("Mock completed after tool execution.")
        );
        assert_eq!(tool_count, 1);
    }

    #[test]
    fn run_child_agent_with_tool_executor_applies_override_and_runs_loop() {
        let request = ChildAgentRequest::new(
            "bash echo child".to_string(),
            SubagentType::General,
            Some(FLASH_MODEL.to_string()),
            3,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let mut saw_child_config = false;
        let mut tool_count = 0;

        let (result, _tracker) = run_child_agent_with_tool_executor(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
            &HookRunner::default(),
            |child_config, child_request, _tool_context, _cancel, tool_request| {
                saw_child_config = true;
                tool_count += 1;
                assert_eq!(child_config.model.as_deref(), Some(FLASH_MODEL));
                assert_eq!(child_request.depth, 3);
                assert_eq!(tool_request.name, ToolName::Bash);
                ChildAgentToolExecution {
                    should_stop: false,
                    result: ToolResult::completed(
                        tool_request,
                        "child tool ran".to_string(),
                        false,
                    ),
                    child_cost: None,
                }
            },
        );

        assert_eq!(result.status, RunStatus::Success);
        assert!(saw_child_config);
        assert_eq!(tool_count, 1);
    }

    #[test]
    fn run_child_agent_prompt_with_tool_executor_builds_runtime_request() {
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let mut saw_request = false;

        let (result, _tracker) = run_child_agent_prompt_with_tool_executor(
            &runtime_config,
            "bash echo child".to_string(),
            &SubagentType::General,
            Some(FLASH_MODEL.to_string()),
            4,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
            &HookRunner::default(),
            |child_config, child_request, _tool_context, _cancel, tool_request| {
                saw_request = true;
                assert_eq!(child_config.model.as_deref(), Some(FLASH_MODEL));
                assert_eq!(child_request.prompt.as_str(), "bash echo child");
                assert!(matches!(
                    &child_request.subagent_type,
                    SubagentType::General
                ));
                assert_eq!(child_request.depth, 4);
                assert_eq!(tool_request.name, ToolName::Bash);
                ChildAgentToolExecution {
                    should_stop: false,
                    result: ToolResult::completed(
                        tool_request,
                        "child tool ran".to_string(),
                        false,
                    ),
                    child_cost: None,
                }
            },
        );

        assert_eq!(result.status, RunStatus::Success);
        assert!(saw_request);
    }

    #[test]
    fn fold_child_agent_tool_result_merges_cost_and_records_model_context() {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let mut setup = prepare_child_agent_loop(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );
        let tool_request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let result = ToolResult::completed(&tool_request, "hello from tool".to_string(), false);
        let mut nested_cost = CostTracker::new(Some(orca_core::model::PRO_MODEL));
        nested_cost.add_usage(Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_tokens: 0,
        });
        let mut tracker = CostTracker::new(None);

        let fold = fold_child_agent_tool_result(
            &mut setup,
            &tool_request,
            false,
            result,
            Some(nested_cost),
            &mut tracker,
        );

        assert!(matches!(fold, ChildAgentToolResultFold::Continue));
        assert!(tracker.totals().total_tokens() > 0);
        assert!(matches!(
            setup.conversation.messages.last(),
            Some(Message::Tool {
                tool_call_id,
                content,
                ..
            }) if tool_call_id == "tool-1" && content.contains("hello from tool")
        ));
    }

    #[test]
    fn fold_child_agent_tool_result_turns_stop_into_failed_child_result() {
        let request = ChildAgentRequest::new(
            "inspect repo".to_string(),
            SubagentType::General,
            None,
            2,
            false,
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let runtime_config = config(None);
        let mut setup = prepare_child_agent_loop(
            &runtime_config,
            &request,
            std::env::temp_dir().as_path(),
            &instructions,
            &memory,
        );
        let tool_request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("exit 1".to_string()),
            raw_arguments: None,
        };
        let result = ToolResult::failed(&tool_request, "tool failed", Some(1));
        let mut tracker = CostTracker::new(None);

        let fold = fold_child_agent_tool_result(
            &mut setup,
            &tool_request,
            true,
            result,
            None,
            &mut tracker,
        );

        match fold {
            ChildAgentToolResultFold::Stop(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert_eq!(result.error.as_deref(), Some("tool failed"));
            }
            ChildAgentToolResultFold::Continue => panic!("should_stop should stop child execution"),
        }
        assert!(matches!(
            setup.conversation.messages.last(),
            Some(Message::Tool {
                tool_call_id,
                content,
                ..
            }) if tool_call_id == "tool-1" && content.contains("tool failed")
        ));
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
