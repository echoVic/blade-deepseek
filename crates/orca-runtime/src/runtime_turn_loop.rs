use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::config::{ProviderKind, RunConfig};
use orca_core::event_schema::EventFactory;
use orca_core::event_sink::EventSink;
use orca_core::model::ModelSelection;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_provider::{ProviderConfig, context};

use crate::agent_child::ChildAgentExecutor;
use crate::background_turn::RuntimeTurnContinuation;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    AgentLoopResult, RuntimeTaskActor, RuntimeTurnInteractionState, RuntimeTurnLoopState,
    ThreadSteerHandle,
};
use crate::memory::MemoryBlock;
use crate::runtime_conversation_bootstrap::RuntimePreparedConversation;
use crate::runtime_turn_iteration::{
    RuntimeTurnIterationInput, RuntimeTurnIterationResult, RuntimeTurnIterationStep,
};
use crate::tool_invocation::AgentToolPolicyContext;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) struct RuntimeTurnLoopStep {
    iteration_step: RuntimeTurnIterationStep,
}

pub(crate) struct RuntimeTurnWorkflowContext<'background, 'ipc> {
    pub(crate) background_workflows: &'background mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'ipc WorkflowIpcContext>,
}

pub(crate) struct RuntimeTurnOutputContext<'events, 'sink, W: io::Write> {
    pub(crate) events: &'events mut EventFactory,
    pub(crate) sink: &'sink mut EventSink<W>,
}

pub(crate) struct RuntimeAgentTurnLoopInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) emit_deltas: bool,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) output: RuntimeTurnOutputContext<'a, 'a, W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) prompt: &'a str,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) continuation: Option<RuntimeTurnContinuation>,
    pub(crate) loop_state: RuntimeTurnLoopState<'a>,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) subagent_depth: u32,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) workflow: RuntimeTurnWorkflowContext<'a, 'a>,
    pub(crate) turn_interactions: RuntimeTurnInteractionState<'a>,
}

pub(crate) struct RuntimeTurnLoopInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider: ProviderKind,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) provider_config: &'a ProviderConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) emit_deltas: bool,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) output: RuntimeTurnOutputContext<'a, 'a, W>,
    pub(crate) prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) prompt: &'a str,
    pub(crate) model: &'a ModelSelection,
    pub(crate) subagent_type: &'a SubagentType,
    pub(crate) continuation: Option<RuntimeTurnContinuation>,
    pub(crate) loop_state: RuntimeTurnLoopState<'a>,
    pub(crate) steer_handle: Option<&'a ThreadSteerHandle>,
    pub(crate) max_budget_usd: Option<f64>,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) subagent_depth: u32,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) workflow: RuntimeTurnWorkflowContext<'a, 'a>,
    pub(crate) turn_interactions: RuntimeTurnInteractionState<'a>,
}

pub(crate) struct RuntimeTurnLoopExecutors<W: io::Write> {
    pub(crate) child_executor: ChildAgentExecutor<W>,
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    pub(crate) batch_child_executor: ChildAgentExecutor<io::Sink>,
}

impl<'a, 'runtime, W: io::Write> RuntimeTurnLoopInput<'a, 'runtime, W> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        actor: &'a mut RuntimeTaskActor<'runtime>,
        provider: ProviderKind,
        context_config: &'a context::ContextConfig,
        provider_config: &'a ProviderConfig,
        cwd: &'a Path,
        emit_deltas: bool,
        hooks: &'a HookRunner,
        output: RuntimeTurnOutputContext<'a, 'a, W>,
        prepared_conversation: &'a mut RuntimePreparedConversation<'runtime>,
        prompt: &'a str,
        model: &'a ModelSelection,
        subagent_type: &'a SubagentType,
        continuation: Option<RuntimeTurnContinuation>,
        loop_state: RuntimeTurnLoopState<'a>,
        steer_handle: Option<&'a ThreadSteerHandle>,
        max_budget_usd: Option<f64>,
        config: &'a RunConfig,
        tool_policy: AgentToolPolicyContext<'a>,
        subagent_depth: u32,
        policy: &'a ApprovalPolicy,
        instructions: &'a ProjectInstructions,
        memory: &'a MemoryBlock,
        mcp_registry: &'a McpRegistry,
        workflow: RuntimeTurnWorkflowContext<'a, 'a>,
        turn_interactions: RuntimeTurnInteractionState<'a>,
    ) -> Self {
        Self {
            actor,
            provider,
            context_config,
            provider_config,
            cwd,
            emit_deltas,
            hooks,
            output,
            prepared_conversation,
            prompt,
            model,
            subagent_type,
            continuation,
            loop_state,
            steer_handle,
            max_budget_usd,
            config,
            tool_policy,
            subagent_depth,
            policy,
            instructions,
            memory,
            mcp_registry,
            workflow,
            turn_interactions,
        }
    }

    pub(crate) fn iteration_input<'iter>(
        &'iter mut self,
    ) -> RuntimeTurnIterationInput<'iter, 'runtime, W> {
        let loop_state = self.loop_state.iteration_state(self.tool_policy);
        RuntimeTurnIterationInput {
            actor: &mut *self.actor,
            provider: self.provider,
            context_config: self.context_config,
            provider_config: self.provider_config,
            runtime_system_messages: loop_state.runtime_system_messages,
            cwd: self.cwd,
            emit_deltas: self.emit_deltas,
            hooks: self.hooks,
            output: RuntimeTurnOutputContext::new(&mut *self.output.events, &mut *self.output.sink),
            prepared_conversation: &mut *self.prepared_conversation,
            prompt: self.prompt,
            model: self.model,
            subagent_type: self.subagent_type,
            continuation: self.continuation.take(),
            model_override: loop_state.model_override,
            cost_tracker: loop_state.cost_tracker,
            steer_handle: self.steer_handle,
            cancel: loop_state.cancel,
            max_budget_usd: self.max_budget_usd,
            config: self.config,
            tool_policy: loop_state.tool_policy,
            subagent_depth: self.subagent_depth,
            policy: self.policy,
            instructions: self.instructions,
            memory: self.memory,
            mcp_registry: self.mcp_registry,
            task_registry: loop_state.task_registry,
            extensions: loop_state.extensions,
            workflow: RuntimeTurnWorkflowContext::new(
                &mut *self.workflow.background_workflows,
                self.workflow.workflow_ipc,
            ),
            turn_interactions: self.turn_interactions,
        }
    }
}

impl<'background, 'ipc> RuntimeTurnWorkflowContext<'background, 'ipc> {
    pub(crate) fn new(
        background_workflows: &'background mut Vec<BackgroundWorkflowRun>,
        workflow_ipc: Option<&'ipc WorkflowIpcContext>,
    ) -> Self {
        Self {
            background_workflows,
            workflow_ipc,
        }
    }
}

impl<'events, 'sink, W: io::Write> RuntimeTurnOutputContext<'events, 'sink, W> {
    pub(crate) fn new(events: &'events mut EventFactory, sink: &'sink mut EventSink<W>) -> Self {
        Self { events, sink }
    }
}

impl<W: io::Write> RuntimeTurnLoopExecutors<W> {
    pub(crate) fn new(
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> Self {
        Self {
            child_executor,
            workflow_child_executor,
            batch_child_executor,
        }
    }
}

pub(crate) fn run_agent_turn_loop<W: io::Write>(
    step: &mut RuntimeTurnLoopStep,
    input: RuntimeAgentTurnLoopInput<'_, '_, W>,
    executors: RuntimeTurnLoopExecutors<W>,
) -> io::Result<AgentLoopResult> {
    step.run(input.into_turn_loop_input(), executors)
}

impl RuntimeTurnLoopStep {
    pub(crate) fn new() -> Self {
        Self {
            iteration_step: RuntimeTurnIterationStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        mut input: RuntimeTurnLoopInput<'_, '_, W>,
        executors: RuntimeTurnLoopExecutors<W>,
    ) -> io::Result<AgentLoopResult> {
        loop {
            match self.iteration_step.run(
                input.iteration_input(),
                executors.child_executor,
                executors.workflow_child_executor,
                executors.batch_child_executor,
            )? {
                RuntimeTurnIterationResult::ContinueLoop => {
                    continue;
                }
                RuntimeTurnIterationResult::Return(result) => return Ok(result),
            }
        }
    }
}

impl<'a, 'runtime, W: io::Write> RuntimeAgentTurnLoopInput<'a, 'runtime, W> {
    fn into_turn_loop_input(self) -> RuntimeTurnLoopInput<'a, 'runtime, W> {
        RuntimeTurnLoopInput::new(
            self.actor,
            self.config.provider,
            self.context_config,
            self.provider_config,
            self.cwd,
            self.emit_deltas,
            self.hooks,
            self.output,
            self.prepared_conversation,
            self.prompt,
            &self.config.model,
            self.subagent_type,
            self.continuation,
            self.loop_state,
            self.steer_handle,
            self.config.max_budget_usd,
            self.config,
            self.tool_policy,
            self.subagent_depth,
            self.policy,
            self.instructions,
            self.memory,
            self.mcp_registry,
            self.workflow,
            self.turn_interactions,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Conversation;
    use orca_core::event_sink::EventSink;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::{ProviderResponse, ProviderStep};
    use orca_core::subagent_config::SubagentConfig;
    use orca_mcp::McpRegistry;

    use crate::cost::CostTracker;
    use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTurnState};
    use crate::runtime_conversation_bootstrap::RuntimeConversationBootstrapStep;
    use crate::session::AgentConversationContext;
    use crate::tasks::TaskRegistry;
    use crate::tool_execution::policy_for_tool_execution;

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            max_budget_usd: None,
            mcp_servers: Vec::<McpServerConfig>::new(),
            external_tools: Vec::<ExternalToolConfig>::new(),
            hooks: Vec::<HookConfig>::new(),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn turn_loop_input_passes_continuation_to_first_iteration_only() {
        let config = config();
        let cwd = tempfile::tempdir().expect("cwd");
        let context_config = context::ContextConfig::for_model_with_runtime(
            Some("deepseek-chat"),
            &config.model_runtime,
        );
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: Some(orca_core::model::PRO_MODEL.to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let policy = policy_for_tool_execution(&config);
        let subagent_type = SubagentType::General;
        let cancel = orca_core::cancel::CancelToken::new();
        let task_registry = TaskRegistry::new("turn-loop-continuation".to_string());
        let mut cost_tracker = CostTracker::new(None);
        let loop_state =
            RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry).into_loop_state();
        let mut lifecycle = RuntimeSessionLifecycle::new("turn-loop-continuation");
        let mut actor = RuntimeTaskActor::new(&mut lifecycle, 3);
        let mut events = EventFactory::new("turn-loop-continuation".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let mut prepared_conversation = RuntimeConversationBootstrapStep::new()
            .prepare(
                AgentConversationContext::new().with_conversation(Some(&mut conversation)),
                cwd.path(),
                "continue",
                0,
                &subagent_type,
                &instructions,
                config.approval_mode,
                &memory,
                true,
            )
            .expect("prepare conversation");
        let mut background_workflows = Vec::new();
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("continued".to_string())],
            assistant_content: Some("continued".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let continuation = crate::background_turn::RuntimeTurnContinuation {
            response,
            preapproved_tool_call_id: Some("tool-1".to_string()),
        };
        let mut input = RuntimeTurnLoopInput {
            actor: &mut actor,
            provider: ProviderKind::DeepSeek,
            continuation: Some(continuation),
            context_config: &context_config,
            provider_config: &provider_config,
            cwd: cwd.path(),
            emit_deltas: true,
            hooks: &hooks,
            output: RuntimeTurnOutputContext::new(&mut events, &mut sink),
            prepared_conversation: &mut prepared_conversation,
            prompt: "continue",
            model: &config.model,
            subagent_type: &subagent_type,
            loop_state,
            steer_handle: None,
            max_budget_usd: None,
            config: &config,
            tool_policy: AgentToolPolicyContext::unrestricted(),
            subagent_depth: 0,
            policy: &policy,
            instructions: &instructions,
            memory: &memory,
            mcp_registry: &mcp_registry,
            workflow: RuntimeTurnWorkflowContext::new(&mut background_workflows, None),
            turn_interactions: RuntimeTurnInteractionState::new(),
        };

        {
            let first_iteration = input.iteration_input();
            assert_eq!(
                first_iteration
                    .continuation
                    .as_ref()
                    .and_then(|continuation| continuation.response.assistant_content.as_deref()),
                Some("continued")
            );
            assert_eq!(
                first_iteration
                    .continuation
                    .as_ref()
                    .and_then(|continuation| continuation.preapproved_tool_call_id()),
                Some("tool-1")
            );
        }
        {
            let second_iteration = input.iteration_input();
            assert!(second_iteration.continuation.is_none());
        }
    }
}
