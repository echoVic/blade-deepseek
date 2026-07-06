use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::{ProviderKind, RunConfig};
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_mcp::McpRegistry;
use orca_provider::{ProviderConfig, context};

use crate::agent_child::ChildAgentExecutor;
use crate::compaction::RuntimeCompactionStep;
use crate::cost::CostTracker;
use crate::extension::{ExtensionData, ExtensionRegistry, RuntimeExtensionStores};
use crate::hooks::{HookRunner, conversation_with_hook_context};
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    AgentLoopResult, RuntimePermissionRequestHandler, RuntimeTaskActor, RuntimeTurnStartError,
};
use crate::memory::{self, MemoryBlock};
use crate::runtime_conversation_bootstrap::RuntimePreparedConversation;
use crate::runtime_directive::conversation_with_runtime_system_messages;
use crate::runtime_steer::{RuntimeSteerInput, RuntimeSteerStep};
use crate::session::record_assistant_response_for_agent;
use crate::step_context::RuntimeStepContext;
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_invocation::{AgentToolPolicyContext, tool_requests_from_provider_steps};
use crate::tool_turn::{RuntimeToolTurnsContext, ToolTurnOutcome, run_tool_turns};
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) struct RuntimeProviderErrorResultStep;
pub(crate) struct RuntimeProviderErrorStep {
    reactive_compacted: bool,
}
pub(crate) struct RuntimeProviderTurnResultStep;
pub(crate) struct RuntimeProviderTurnResultResultStep;

pub(crate) struct RuntimeProviderTurnStep;
pub(crate) struct RuntimeProviderResponseStep;
pub(crate) struct RuntimeProviderResponseResultStep;
pub(crate) struct RuntimeTurnProviderCycleStep {
    provider_error_step: RuntimeProviderErrorStep,
}

pub(crate) struct RuntimeProviderCycleInput<'a, 'runtime, W: io::Write> {
    pub(crate) actor: &'a mut RuntimeTaskActor<'runtime>,
    pub(crate) provider: ProviderKind,
    pub(crate) turn_provider_config: &'a ProviderConfig,
    pub(crate) runtime_system_messages: &'a [String],
    pub(crate) cwd: &'a Path,
    pub(crate) context_config: &'a context::ContextConfig,
    pub(crate) base_provider_config: &'a ProviderConfig,
    pub(crate) emit_deltas: bool,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) max_budget_usd: Option<f64>,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut RuntimePreparedConversation<'runtime>,
    pub(crate) config: &'a RunConfig,
    pub(crate) tool_policy: AgentToolPolicyContext<'a>,
    pub(crate) subagent_depth: u32,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) extension_registry: &'a ExtensionRegistry,
    pub(crate) thread_extensions: &'a ExtensionData,
    pub(crate) turn_extensions: &'a ExtensionData,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    pub(crate) steer_handle: Option<&'a crate::lifecycle::ThreadSteerHandle>,
}

pub(crate) struct RuntimeProviderResponseInput<'a, W: io::Write> {
    pub(crate) step_context: RuntimeStepContext<'a>,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
}

pub(crate) struct RuntimeProviderTurnOutput {
    pub(crate) response: Option<ProviderResponse>,
    pub(crate) terminal_error: Option<RuntimeTurnStartError>,
}

pub(crate) enum RuntimeProviderErrorOutcome {
    ContinueAfterCompaction,
    Failed(String),
    NoError,
}

pub(crate) enum RuntimeProviderResponseOutcome {
    Continue,
    Success {
        final_message: Option<String>,
    },
    Return {
        status: RunStatus,
        error: Option<String>,
    },
}

pub(crate) enum RuntimeProviderErrorStepOutcome {
    ContinueAfterCompaction,
    Failed(RuntimeTurnStartError),
    NoError,
}

pub(crate) enum RuntimeProviderErrorResult {
    ContinueTurn,
    ContinueLoop,
    Return(AgentLoopResult),
}

pub(crate) enum RuntimeProviderTurnResultOutcome {
    Response(ProviderResponse),
    Failed(RuntimeTurnStartError),
}

pub(crate) enum RuntimeProviderTurnResultResult {
    Response(ProviderResponse),
    Return(AgentLoopResult),
}

pub(crate) enum RuntimeProviderResponseResult {
    Continue,
    Return(AgentLoopResult),
}

pub(crate) enum RuntimeTurnProviderCycleResult {
    ContinueLoop,
    ContinueTurn,
    Return(AgentLoopResult),
}

impl RuntimeProviderErrorResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold(
        &self,
        outcome: RuntimeProviderErrorStepOutcome,
    ) -> RuntimeProviderErrorResult {
        match outcome {
            RuntimeProviderErrorStepOutcome::NoError => RuntimeProviderErrorResult::ContinueTurn,
            RuntimeProviderErrorStepOutcome::ContinueAfterCompaction => {
                RuntimeProviderErrorResult::ContinueLoop
            }
            RuntimeProviderErrorStepOutcome::Failed(error) => RuntimeProviderErrorResult::Return(
                AgentLoopResult::failure(error.status, error.message),
            ),
        }
    }
}

impl RuntimeProviderErrorStep {
    pub(crate) fn new() -> Self {
        Self {
            reactive_compacted: false,
        }
    }

    pub(crate) fn handle<W: io::Write>(
        &mut self,
        response: &ProviderResponse,
        compaction: &mut RuntimeCompactionStep<'_, W>,
        conversation: &mut Conversation,
    ) -> io::Result<RuntimeProviderErrorStepOutcome> {
        match RuntimeProviderTurnStep::new().handle_provider_error(
            response,
            compaction,
            conversation,
            self.reactive_compacted,
        )? {
            RuntimeProviderErrorOutcome::ContinueAfterCompaction => {
                self.reactive_compacted = true;
                Ok(RuntimeProviderErrorStepOutcome::ContinueAfterCompaction)
            }
            RuntimeProviderErrorOutcome::Failed(message) => {
                self.reactive_compacted = false;
                Ok(RuntimeProviderErrorStepOutcome::Failed(
                    RuntimeTurnStartError {
                        status: RunStatus::Failed,
                        message,
                    },
                ))
            }
            RuntimeProviderErrorOutcome::NoError => {
                self.reactive_compacted = false;
                Ok(RuntimeProviderErrorStepOutcome::NoError)
            }
        }
    }

    #[cfg(test)]
    fn mark_reactive_compacted_for_test(&mut self) {
        self.reactive_compacted = true;
    }

    #[cfg(test)]
    fn reactive_compacted_for_test(&self) -> bool {
        self.reactive_compacted
    }
}

impl RuntimeProviderTurnResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold<W: io::Write>(
        &mut self,
        provider_turn: RuntimeProviderTurnOutput,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        emit_deltas: bool,
    ) -> io::Result<RuntimeProviderTurnResultOutcome> {
        match provider_response_or_terminal(provider_turn) {
            Ok(response) => Ok(RuntimeProviderTurnResultOutcome::Response(response)),
            Err(error) => {
                if emit_deltas && error.status != RunStatus::Cancelled {
                    sink.emit(&events.error(&error.message))?;
                }
                Ok(RuntimeProviderTurnResultOutcome::Failed(error))
            }
        }
    }
}

impl RuntimeProviderTurnResultResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold(
        &self,
        outcome: RuntimeProviderTurnResultOutcome,
    ) -> RuntimeProviderTurnResultResult {
        match outcome {
            RuntimeProviderTurnResultOutcome::Response(response) => {
                RuntimeProviderTurnResultResult::Response(response)
            }
            RuntimeProviderTurnResultOutcome::Failed(error) => {
                RuntimeProviderTurnResultResult::Return(AgentLoopResult::failure(
                    error.status,
                    error.message,
                ))
            }
        }
    }
}

impl RuntimeProviderTurnStep {
    pub(crate) fn new() -> Self {
        Self
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn run<W: io::Write>(
        &mut self,
        actor: &mut RuntimeTaskActor<'_>,
        provider: ProviderKind,
        conversation: &mut Conversation,
        runtime_system_messages: &[String],
        provider_config: &ProviderConfig,
        cwd: &str,
        emit_deltas: bool,
        hooks: &HookRunner,
        cancel: &CancelToken,
        cost_tracker: &mut CostTracker,
        max_budget_usd: Option<f64>,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        mut history_writer: Option<&mut SessionWriter>,
        steer_handle: Option<&crate::lifecycle::ThreadSteerHandle>,
    ) -> io::Result<RuntimeProviderTurnOutput> {
        let pre_model_outcome = match actor.run_pre_model_hook_with_cancel(hooks, cwd, Some(cancel))
        {
            Ok(outcome) => outcome,
            Err(error) => return Ok(RuntimeProviderTurnOutput::terminal(error)),
        };
        if cancel.is_cancelled() {
            return cancelled_provider_turn(emit_deltas, events, sink);
        }

        RuntimeSteerStep::new().apply(RuntimeSteerInput {
            steer_handle,
            conversation,
            history_writer: history_writer.as_deref_mut(),
        })?;
        let model_conversation = conversation_with_hook_context(conversation, &pre_model_outcome);
        let model_conversation =
            conversation_with_runtime_system_messages(&model_conversation, runtime_system_messages);
        let response = actor.call_streaming_provider(
            provider,
            &model_conversation,
            provider_config,
            cancel,
            &mut |step| emit_provider_delta(step, emit_deltas, events, sink),
        );
        if cancel.is_cancelled() {
            return cancelled_provider_turn(emit_deltas, events, sink);
        }

        if let Some(warning) =
            actor.run_post_model_hook_with_cancel(hooks, cwd, response.usage.as_ref(), Some(cancel))
            && emit_deltas
        {
            sink.emit(&events.error(&warning))?;
        }
        if cancel.is_cancelled() {
            return cancelled_provider_turn(emit_deltas, events, sink);
        }

        if let Some(usage) = response.usage
            && !usage.is_empty()
        {
            match actor.record_usage(usage, cost_tracker, max_budget_usd) {
                Ok(totals) => {
                    if emit_deltas {
                        sink.emit(&events.usage_updated(totals))?;
                        if let Some(writer) = history_writer.as_deref_mut() {
                            writer.append_usage(totals)?;
                        }
                    }
                }
                Err(error) => return Ok(RuntimeProviderTurnOutput::terminal(error)),
            }
        }

        Ok(RuntimeProviderTurnOutput::response(response))
    }

    pub(crate) fn handle_provider_error<W: io::Write>(
        &mut self,
        response: &ProviderResponse,
        compaction: &mut RuntimeCompactionStep<'_, W>,
        conversation: &mut Conversation,
        reactive_compacted: bool,
    ) -> io::Result<RuntimeProviderErrorOutcome> {
        let provider_error = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(message) => Some(message.clone()),
            _ => None,
        });

        let Some(error) = provider_error else {
            return Ok(RuntimeProviderErrorOutcome::NoError);
        };

        if context::is_prompt_too_long_error(&error) && !reactive_compacted {
            compaction.compact_after_prompt_too_long(conversation)?;
            return Ok(RuntimeProviderErrorOutcome::ContinueAfterCompaction);
        }

        compaction.emit_error(&error)?;
        Ok(RuntimeProviderErrorOutcome::Failed(error))
    }
}

impl RuntimeProviderResponseStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn handle<W: io::Write>(
        &mut self,
        response: ProviderResponse,
        step_context: RuntimeStepContext<'_>,
        events: &mut EventFactory,
        sink: &mut EventSink<W>,
        conversation: &mut Conversation,
        mut history_writer: Option<&mut SessionWriter>,
        cost_tracker: &mut CostTracker,
        background_workflows: &mut Vec<BackgroundWorkflowRun>,
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> io::Result<RuntimeProviderResponseOutcome> {
        if response.tool_calls.is_empty() {
            let final_message = response.assistant_content.clone();
            record_assistant_response_for_agent(
                conversation,
                history_writer.as_deref_mut(),
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
                step_context.emit_deltas,
            )?;
            if step_context.emit_deltas && step_context.config.auto_memory {
                memory::extract_project_memory_after_final_response(
                    step_context.config,
                    step_context.cwd,
                    &conversation.messages,
                    events,
                    sink,
                )?;
            }
            return Ok(RuntimeProviderResponseOutcome::Success { final_message });
        }

        record_assistant_response_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
            step_context.emit_deltas,
        )?;

        let tool_requests = tool_requests_from_provider_steps(&response.steps);
        match run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            events,
            sink,
            conversation,
            history_writer: history_writer.as_deref_mut(),
            tool_requests: &tool_requests,
            cost_tracker,
            background_workflows,
            child_executor,
            workflow_child_executor,
            batch_child_executor,
        })? {
            ToolTurnOutcome::Continue => Ok(RuntimeProviderResponseOutcome::Continue),
            ToolTurnOutcome::Return { status, error } => {
                Ok(RuntimeProviderResponseOutcome::Return { status, error })
            }
        }
    }
}

impl RuntimeProviderResponseResultStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn fold(
        &self,
        outcome: RuntimeProviderResponseOutcome,
    ) -> RuntimeProviderResponseResult {
        match outcome {
            RuntimeProviderResponseOutcome::Continue => RuntimeProviderResponseResult::Continue,
            RuntimeProviderResponseOutcome::Success { final_message } => {
                RuntimeProviderResponseResult::Return(AgentLoopResult::success(final_message))
            }
            RuntimeProviderResponseOutcome::Return { status, error } => {
                RuntimeProviderResponseResult::Return(AgentLoopResult::terminal(status, error))
            }
        }
    }
}

impl RuntimeTurnProviderCycleStep {
    pub(crate) fn new() -> Self {
        Self {
            provider_error_step: RuntimeProviderErrorStep::new(),
        }
    }

    pub(crate) fn run<W: io::Write>(
        &mut self,
        input: RuntimeProviderCycleInput<'_, '_, W>,
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> io::Result<RuntimeTurnProviderCycleResult> {
        let cwd_display = input.cwd.display().to_string();
        let provider_turn = {
            let (conversation, history_writer) = input.conversation.parts_mut();
            RuntimeProviderTurnStep::new().run(
                input.actor,
                input.provider,
                conversation,
                input.runtime_system_messages,
                input.turn_provider_config,
                &cwd_display,
                input.emit_deltas,
                input.hooks,
                input.cancel,
                input.cost_tracker,
                input.max_budget_usd,
                input.events,
                input.sink,
                history_writer,
                input.steer_handle,
            )?
        };
        let response = match RuntimeProviderTurnResultResultStep::new().fold(
            RuntimeProviderTurnResultStep::new().fold(
                provider_turn,
                input.events,
                input.sink,
                input.emit_deltas,
            )?,
        ) {
            RuntimeProviderTurnResultResult::Response(response) => response,
            RuntimeProviderTurnResultResult::Return(result) => {
                return Ok(RuntimeTurnProviderCycleResult::Return(result));
            }
        };

        let provider_error_outcome = {
            let (conversation, history_writer) = input.conversation.parts_mut();
            self.provider_error_step.handle(
                &response,
                &mut RuntimeCompactionStep::new(
                    input.provider,
                    input.context_config,
                    input.base_provider_config,
                    input.cwd,
                    input.emit_deltas,
                    input.hooks,
                    input.events,
                    input.sink,
                    history_writer,
                ),
                conversation,
            )?
        };
        match RuntimeProviderErrorResultStep::new().fold(provider_error_outcome) {
            RuntimeProviderErrorResult::ContinueLoop => {
                return Ok(RuntimeTurnProviderCycleResult::ContinueLoop);
            }
            RuntimeProviderErrorResult::Return(result) => {
                return Ok(RuntimeTurnProviderCycleResult::Return(result));
            }
            RuntimeProviderErrorResult::ContinueTurn => {}
        }

        let (conversation, history_writer) = input.conversation.parts_mut();
        self.handle_response(
            response,
            RuntimeProviderResponseInput {
                step_context: RuntimeStepContext::new(
                    input.config,
                    input.cwd,
                    input.tool_policy,
                    input.subagent_depth,
                    input.emit_deltas,
                    input.policy,
                    input.instructions,
                    input.memory,
                    input.mcp_registry,
                    input.hooks,
                    input.cancel,
                    input.task_registry,
                    input.workflow_ipc,
                    input.permission_handler,
                )
                .with_extensions(
                    input.extension_registry,
                    RuntimeExtensionStores::new(input.thread_extensions, input.turn_extensions),
                ),
                events: input.events,
                sink: input.sink,
                conversation,
                history_writer,
                cost_tracker: input.cost_tracker,
                background_workflows: input.background_workflows,
            },
            child_executor,
            workflow_child_executor,
            batch_child_executor,
        )
    }

    pub(crate) fn handle_response<W: io::Write>(
        &mut self,
        response: ProviderResponse,
        input: RuntimeProviderResponseInput<'_, W>,
        child_executor: ChildAgentExecutor<W>,
        workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
        batch_child_executor: ChildAgentExecutor<io::Sink>,
    ) -> io::Result<RuntimeTurnProviderCycleResult> {
        let provider_response_outcome = RuntimeProviderResponseStep::new().handle(
            response,
            input.step_context,
            input.events,
            input.sink,
            input.conversation,
            input.history_writer,
            input.cost_tracker,
            input.background_workflows,
            child_executor,
            workflow_child_executor,
            batch_child_executor,
        )?;

        Ok(
            match RuntimeProviderResponseResultStep::new().fold(provider_response_outcome) {
                RuntimeProviderResponseResult::Continue => {
                    RuntimeTurnProviderCycleResult::ContinueTurn
                }
                RuntimeProviderResponseResult::Return(result) => {
                    RuntimeTurnProviderCycleResult::Return(result)
                }
            },
        )
    }
}

fn cancelled_provider_turn<W: io::Write>(
    emit_deltas: bool,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
) -> io::Result<RuntimeProviderTurnOutput> {
    if emit_deltas {
        sink.emit(&events.error("turn cancelled"))?;
    }
    Ok(RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
        status: RunStatus::Cancelled,
        message: "turn cancelled".to_string(),
    }))
}

fn emit_provider_delta<W: io::Write>(
    step: &ProviderStep,
    emit_deltas: bool,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
) {
    if !emit_deltas {
        return;
    }
    match step {
        ProviderStep::ReasoningDelta(text) => {
            let _ = sink.emit(&events.assistant_reasoning_delta(text));
        }
        ProviderStep::MessageDelta(text) => {
            let _ = sink.emit(&events.assistant_message_delta(text));
        }
        ProviderStep::ToolCallProgress(progress) => {
            let _ = sink.emit(&events.tool_call_progress(progress));
        }
        ProviderStep::ReplayState(replay) => {
            let _ = sink.emit(&events.provider_replay_updated(replay));
        }
        _ => {}
    }
}

impl RuntimeProviderTurnOutput {
    fn response(response: ProviderResponse) -> Self {
        Self {
            response: Some(response),
            terminal_error: None,
        }
    }

    fn terminal(error: RuntimeTurnStartError) -> Self {
        Self {
            response: None,
            terminal_error: Some(error),
        }
    }
}

pub(crate) fn provider_response_or_terminal(
    provider_turn: RuntimeProviderTurnOutput,
) -> Result<ProviderResponse, RuntimeTurnStartError> {
    match provider_turn.response {
        Some(response) => Ok(response),
        None => Err(provider_turn
            .terminal_error
            .expect("provider turn terminal")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::Message;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::hook_types::HookConfig;
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;

    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
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
            permission_rules: PermissionRules::default(),
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

    fn unused_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("final provider response must not execute child agents")
    }

    #[test]
    fn provider_turn_error_handler_emits_failure_event_for_non_compaction_errors() {
        let response = ProviderResponse {
            steps: vec![ProviderStep::Error(
                "DeepSeek provider error: quota".to_string(),
            )],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config =
            context::ContextConfig::for_model_with_runtime(Some("deepseek-chat"), &runtime);
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("provider-error-test".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let cwd = Path::new(".");
        let mut compaction = RuntimeCompactionStep::new(
            ProviderKind::DeepSeek,
            &context_config,
            &provider_config,
            cwd,
            true,
            &hooks,
            &mut events,
            &mut sink,
            None,
        );
        let mut conversation = Conversation::new();

        let outcome = RuntimeProviderTurnStep::new()
            .handle_provider_error(&response, &mut compaction, &mut conversation, false)
            .expect("provider error handling succeeds");

        match outcome {
            RuntimeProviderErrorOutcome::Failed(error) => {
                assert_eq!(error, "DeepSeek provider error: quota");
            }
            _ => panic!("expected non-compaction provider error to fail"),
        }
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"error\""));
        assert!(output.contains("DeepSeek provider error: quota"));
    }

    #[test]
    fn provider_delta_emits_tool_call_progress_event() {
        let mut events = EventFactory::new("provider-progress-test".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let progress = orca_core::provider_types::ToolCallProgress {
            id: "call_1".to_string(),
            function_name: Some("write_file".to_string()),
            arguments_bytes: 12_345,
        };

        emit_provider_delta(
            &ProviderStep::ToolCallProgress(progress),
            true,
            &mut events,
            &mut sink,
        );

        drop(sink);
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"tool.call.progress\""));
        assert!(output.contains("\"id\":\"call_1\""));
        assert!(output.contains("\"name\":\"write_file\""));
        assert!(output.contains("\"arguments_bytes\":12345"));
    }

    #[test]
    fn provider_turn_injects_runtime_system_messages_without_mutating_conversation() {
        let mut lifecycle = crate::lifecycle::RuntimeSessionLifecycle::new(
            "provider-runtime-system-directive".to_string(),
        );
        let mut actor = RuntimeTaskActor::new(&mut lifecycle, 3);
        let mut conversation = Conversation::new();
        conversation.add_system("base system".to_string());
        conversation.add_user("mock_system_echo".to_string());
        let runtime_system_messages = vec!["runtime directive context".to_string()];
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: Some(orca_core::model::PRO_MODEL.to_string()),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let hooks = HookRunner::default();
        let cancel = CancelToken::new();
        let mut cost_tracker = CostTracker::new(None);
        let mut events = EventFactory::new("provider-runtime-system-directive".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);

        let result = RuntimeProviderTurnStep::new()
            .run(
                &mut actor,
                ProviderKind::Mock,
                &mut conversation,
                runtime_system_messages.as_slice(),
                &provider_config,
                ".",
                true,
                &hooks,
                &cancel,
                &mut cost_tracker,
                None,
                &mut events,
                &mut sink,
                None,
                None,
            )
            .expect("provider turn");

        let response = result.response.expect("provider response");
        assert!(
            response
                .assistant_content
                .as_deref()
                .unwrap_or_default()
                .contains("runtime directive context")
        );
        assert!(conversation.messages.iter().all(|message| {
            !message
                .content_str()
                .unwrap_or_default()
                .contains("runtime directive context")
        }));
    }

    #[test]
    fn provider_error_step_returns_failure_and_resets_reactive_state() {
        let response = ProviderResponse {
            steps: vec![ProviderStep::Error(
                "DeepSeek provider error: quota".to_string(),
            )],
            assistant_content: None,
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let runtime = ModelRuntimeConfig::default();
        let context_config =
            context::ContextConfig::for_model_with_runtime(Some("deepseek-chat"), &runtime);
        let provider_config = ProviderConfig {
            api_key: None,
            base_url: None,
            model: None,
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            tools_override: Some(Vec::new()),
            mcp_registry: None,
            external_tools: Vec::new(),
        };
        let hooks = HookRunner::default();
        let mut events = EventFactory::new("provider-error-step".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let cwd = Path::new(".");
        let mut compaction = RuntimeCompactionStep::new(
            ProviderKind::DeepSeek,
            &context_config,
            &provider_config,
            cwd,
            true,
            &hooks,
            &mut events,
            &mut sink,
            None,
        );
        let mut conversation = Conversation::new();
        let mut step = RuntimeProviderErrorStep::new();
        step.mark_reactive_compacted_for_test();

        let outcome = step
            .handle(&response, &mut compaction, &mut conversation)
            .expect("provider error step succeeds");

        match outcome {
            RuntimeProviderErrorStepOutcome::Failed(error) => {
                assert_eq!(error.status, RunStatus::Failed);
                assert_eq!(error.message, "DeepSeek provider error: quota");
            }
            _ => panic!("expected provider error step failure"),
        }
        assert!(!step.reactive_compacted_for_test());
        let output = String::from_utf8(output).expect("jsonl is utf8");
        assert!(output.contains("\"type\":\"error\""));
        assert!(output.contains("DeepSeek provider error: quota"));
    }

    #[test]
    fn provider_error_result_step_folds_continue_loop_and_failure() {
        let no_error =
            RuntimeProviderErrorResultStep::new().fold(RuntimeProviderErrorStepOutcome::NoError);
        assert!(matches!(no_error, RuntimeProviderErrorResult::ContinueTurn));

        let retry = RuntimeProviderErrorResultStep::new()
            .fold(RuntimeProviderErrorStepOutcome::ContinueAfterCompaction);
        assert!(matches!(retry, RuntimeProviderErrorResult::ContinueLoop));

        let failed = RuntimeProviderErrorResultStep::new().fold(
            RuntimeProviderErrorStepOutcome::Failed(RuntimeTurnStartError {
                status: RunStatus::Failed,
                message: "provider failed".to_string(),
            }),
        );
        match failed {
            RuntimeProviderErrorResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert_eq!(result.final_message, None);
                assert_eq!(result.error.as_deref(), Some("provider failed"));
            }
            RuntimeProviderErrorResult::ContinueTurn | RuntimeProviderErrorResult::ContinueLoop => {
                panic!("provider error failure should return loop result")
            }
        }
    }

    #[test]
    fn provider_response_step_records_final_assistant_message() {
        let config = config();
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("done".to_string())],
            assistant_content: Some("done".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let cwd = tempfile::tempdir().expect("cwd");
        let mut events = EventFactory::new("provider-response-final".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("provider-response-final".to_string());
        let mut background_workflows = Vec::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
        );

        let outcome = RuntimeProviderResponseStep::new()
            .handle(
                response,
                step_context,
                &mut events,
                &mut sink,
                &mut conversation,
                None,
                &mut cost_tracker,
                &mut background_workflows,
                unused_child_executor::<Vec<u8>>,
                unused_child_executor::<crate::workflow::runner::SharedEventBuffer>,
                unused_child_executor::<io::Sink>,
            )
            .expect("handle provider response");

        match outcome {
            RuntimeProviderResponseOutcome::Success { final_message } => {
                assert_eq!(final_message.as_deref(), Some("done"));
            }
            _ => panic!("final response should complete the agent loop"),
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Assistant { content, tool_calls, .. }
                if content.as_deref() == Some("done") && tool_calls.is_empty())
        );
    }

    #[test]
    fn provider_cycle_step_handles_final_response() {
        let config = config();
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("done".to_string())],
            assistant_content: Some("done".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let cwd = tempfile::tempdir().expect("cwd");
        let mut events = EventFactory::new("provider-cycle-final".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("provider-cycle-final".to_string());
        let mut background_workflows = Vec::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
        );

        let result = RuntimeTurnProviderCycleStep::new()
            .handle_response(
                response,
                RuntimeProviderResponseInput {
                    step_context,
                    events: &mut events,
                    sink: &mut sink,
                    conversation: &mut conversation,
                    history_writer: None,
                    cost_tracker: &mut cost_tracker,
                    background_workflows: &mut background_workflows,
                },
                unused_child_executor::<Vec<u8>>,
                unused_child_executor::<crate::workflow::runner::SharedEventBuffer>,
                unused_child_executor::<io::Sink>,
            )
            .expect("handle provider cycle response");

        match result {
            RuntimeTurnProviderCycleResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Success);
                assert_eq!(result.final_message.as_deref(), Some("done"));
                assert_eq!(result.error, None);
            }
            RuntimeTurnProviderCycleResult::ContinueLoop
            | RuntimeTurnProviderCycleResult::ContinueTurn => {
                panic!("final response should return agent-loop result")
            }
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Assistant { content, tool_calls, .. }
                if content.as_deref() == Some("done") && tool_calls.is_empty())
        );
    }

    #[test]
    fn provider_response_or_terminal_returns_terminal_error() {
        let output = RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
            status: RunStatus::Cancelled,
            message: "turn cancelled".to_string(),
        });

        let error = match provider_response_or_terminal(output) {
            Ok(_) => panic!("expected terminal error"),
            Err(error) => error,
        };

        assert_eq!(error.status, RunStatus::Cancelled);
        assert_eq!(error.message, "turn cancelled");
    }

    #[test]
    fn provider_turn_result_step_suppresses_cancelled_error_event() {
        let output = RuntimeProviderTurnOutput::terminal(RuntimeTurnStartError {
            status: RunStatus::Cancelled,
            message: "turn cancelled".to_string(),
        });
        let mut events = EventFactory::new("provider-turn-result".to_string());
        let mut emitted = Vec::new();
        let mut sink = EventSink::new(&mut emitted, OutputFormat::Jsonl);

        let outcome = RuntimeProviderTurnResultStep::new()
            .fold(output, &mut events, &mut sink, true)
            .expect("fold provider turn result");

        match outcome {
            RuntimeProviderTurnResultOutcome::Failed(error) => {
                assert_eq!(error.status, RunStatus::Cancelled);
                assert_eq!(error.message, "turn cancelled");
            }
            _ => panic!("expected cancelled provider turn to fail"),
        }
        drop(sink);
        assert!(emitted.is_empty());
    }

    #[test]
    fn provider_turn_result_result_step_folds_response_and_failure() {
        let response = ProviderResponse {
            steps: vec![ProviderStep::MessageDelta("hello".to_string())],
            assistant_content: Some("hello".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        };
        let success = RuntimeProviderTurnResultResultStep::new()
            .fold(RuntimeProviderTurnResultOutcome::Response(response));
        match success {
            RuntimeProviderTurnResultResult::Response(response) => {
                assert_eq!(response.assistant_content.as_deref(), Some("hello"));
            }
            RuntimeProviderTurnResultResult::Return(_) => {
                panic!("provider response should continue the turn")
            }
        }

        let failed = RuntimeProviderTurnResultResultStep::new().fold(
            RuntimeProviderTurnResultOutcome::Failed(RuntimeTurnStartError {
                status: RunStatus::Failed,
                message: "provider failed".to_string(),
            }),
        );
        match failed {
            RuntimeProviderTurnResultResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Failed);
                assert_eq!(result.final_message, None);
                assert_eq!(result.error.as_deref(), Some("provider failed"));
            }
            RuntimeProviderTurnResultResult::Response(_) => {
                panic!("provider failure should return loop result")
            }
        }
    }

    #[test]
    fn provider_response_result_step_folds_success_return_and_continue() {
        let success = RuntimeProviderResponseResultStep::new().fold(
            RuntimeProviderResponseOutcome::Success {
                final_message: Some("done".to_string()),
            },
        );
        match success {
            RuntimeProviderResponseResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Success);
                assert_eq!(result.final_message.as_deref(), Some("done"));
                assert_eq!(result.error, None);
            }
            RuntimeProviderResponseResult::Continue => panic!("success should return loop result"),
        }

        let terminal =
            RuntimeProviderResponseResultStep::new().fold(RuntimeProviderResponseOutcome::Return {
                status: RunStatus::Cancelled,
                error: Some("cancelled".to_string()),
            });
        match terminal {
            RuntimeProviderResponseResult::Return(result) => {
                assert_eq!(result.status, RunStatus::Cancelled);
                assert_eq!(result.final_message, None);
                assert_eq!(result.error.as_deref(), Some("cancelled"));
            }
            RuntimeProviderResponseResult::Continue => panic!("terminal outcome should return"),
        }

        let continuing =
            RuntimeProviderResponseResultStep::new().fold(RuntimeProviderResponseOutcome::Continue);
        assert!(matches!(
            continuing,
            RuntimeProviderResponseResult::Continue
        ));
    }
}
