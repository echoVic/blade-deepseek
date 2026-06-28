use std::io;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::{OutputFormat, RunConfig};
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::provider_types::ProviderStep;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types;
use orca_mcp::McpRegistry;
use orca_provider::context;
use orca_provider::tool_schema::{
    deepseek_tools_schema_for_allowed_names_with_mcp_and_external,
    deepseek_tools_schema_for_type_with_mcp_and_external,
    deepseek_tools_schema_with_mcp_and_external,
};
use orca_provider::{self, ProviderConfig};
use orca_tools;

use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
use crate::agent_common;
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    AgentLoopContext, RuntimeCompactionStep, RuntimeProviderTurnStep, RuntimeSessionLifecycle,
    RuntimeSteerStep, RuntimeTaskActor, RuntimeTurnConfig, RuntimeTurnDeps, RuntimeTurnExecution,
    RuntimeTurnState, TurnPermissionOverlay,
};
use crate::memory::{self, MemoryBlock};
use crate::session::{
    AgentConversationContext, record_assistant_response_for_agent,
    record_initial_history_for_agent, record_plan_state_for_agent, record_tool_result_for_agent,
};
use crate::subagent_execution::{
    collect_subagent_batch, execute_subagent_batch, should_run_subagent_batch,
};
use crate::tasks::TaskRegistry;
use crate::thread_store;
use crate::tool_execution::{ToolExecutionActor, ToolExecutionContext};
use crate::tool_invocation::{
    AgentToolPolicyContext, execute_readonly_batch, reject_disallowed_child_tool,
};
use crate::workflow_execution::observe_background_workflows;

const DEFAULT_MAX_TURNS: u32 = 128;

#[derive(Clone, Debug)]
pub(crate) struct AgentLoopResult {
    pub(crate) status: RunStatus,
    pub(crate) final_message: Option<String>,
    pub(crate) error: Option<String>,
}

impl AgentLoopResult {
    fn success(final_message: Option<String>) -> Self {
        Self {
            status: RunStatus::Success,
            final_message,
            error: None,
        }
    }

    fn failure(status: RunStatus, error: impl Into<String>) -> Self {
        Self {
            status,
            final_message: None,
            error: Some(error.into()),
        }
    }
}

pub(crate) fn run_agent_loop(
    config: &RunConfig,
    loop_context: AgentLoopContext<'_>,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    conversation_context: AgentConversationContext<'_>,
    tool_policy: AgentToolPolicyContext<'_>,
) -> io::Result<AgentLoopResult> {
    let AgentConversationContext {
        resumed,
        history_writer,
        conversation,
    } = conversation_context;
    let AgentLoopContext {
        turn_config:
            RuntimeTurnConfig {
                cwd,
                prompt,
                subagent_depth,
                emit_deltas,
                subagent_type,
            },
        turn_deps,
        turn_state,
        turn_execution,
        steer_handle,
        permission_handler,
    } = loop_context;
    let RuntimeTurnDeps {
        instructions,
        memory,
        mcp_registry,
        hooks,
    } = turn_deps.expect("agent loop turn deps");
    let RuntimeTurnState {
        cost_tracker,
        cancel,
        task_registry,
    } = turn_state.expect("agent loop turn state");
    let RuntimeTurnExecution {
        background_workflows,
        workflow_ipc,
        lifecycle,
    } = turn_execution.expect("agent loop turn execution");
    let max_turns = DEFAULT_MAX_TURNS;
    let budget_model = config.model.as_option();
    let ctx_config = context::ContextConfig::for_model_with_runtime(
        budget_model.as_deref(),
        &config.model_runtime,
    );
    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    let tools_override = if subagent_depth > 0 {
        if let Some(allowed_tools) = tool_policy.allowed_tools() {
            Some(
                deepseek_tools_schema_for_allowed_names_with_mcp_and_external(
                    allowed_tools,
                    Some(mcp_registry),
                    &config.external_tools,
                ),
            )
        } else {
            Some(deepseek_tools_schema_for_type_with_mcp_and_external(
                subagent_type,
                Some(mcp_registry),
                &config.external_tools,
            ))
        }
    } else {
        Some(deepseek_tools_schema_with_mcp_and_external(
            Some(mcp_registry),
            &config.external_tools,
        ))
    };
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.as_option(),
        tools_override,
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
    };

    let system_prompt = agent_common::build_agent_system_prompt(
        cwd,
        subagent_depth,
        subagent_type,
        Some(instructions),
        config.approval_mode,
        Some(memory),
    );
    let mut owned_conversation;
    let conversation = if let Some(conversation) = conversation {
        conversation
    } else {
        owned_conversation = if let Some(resumed) = resumed {
            let mut conv = thread_store::resume_conversation(resumed, system_prompt);
            conv.strip_legacy_pinned_volatile();
            conv.strip_legacy_summary_messages();
            conv
        } else {
            let mut conversation = Conversation::new();
            conversation.add_system(system_prompt);
            conversation
        };
        owned_conversation.replace_skill_context(agent_common::explicit_skill_context(cwd, prompt));
        owned_conversation.add_user(prompt.to_string());
        &mut owned_conversation
    };

    let mut history_writer = history_writer;
    record_initial_history_for_agent(
        conversation,
        history_writer.as_deref_mut(),
        resumed.is_some(),
        emit_deltas,
    )?;

    let mut legacy_lifecycle = RuntimeSessionLifecycle::new(events.run_id().to_string());
    let lifecycle = lifecycle.unwrap_or(&mut legacy_lifecycle);
    let mut actor = RuntimeTaskActor::new(lifecycle, max_turns);
    let mut reactive_compacted = false;

    loop {
        RuntimeCompactionStep::new(
            config.provider,
            &ctx_config,
            &provider_config,
            cwd,
            emit_deltas,
            hooks,
            events,
            sink,
            history_writer.as_deref_mut(),
        )
        .compact_if_needed(conversation)?;

        let turn_prompt = if actor
            .active_task()
            .map(|task| task.current_turn())
            .unwrap_or(0)
            == 0
        {
            Some(prompt)
        } else {
            None
        };
        let started_turn = match actor.start_turn(events, turn_prompt, emit_deltas) {
            Ok(started_turn) => started_turn,
            Err(error) => {
                if emit_deltas {
                    sink.emit(&events.error(&error.message))?;
                }
                return Ok(AgentLoopResult::failure(error.status, error.message));
            }
        };
        if let Some(event) = started_turn.into_event() {
            sink.emit(&event)?;
        }

        let routed_model = actor.route_model_turn(
            &config.model,
            subagent_type,
            None,
            &provider_config,
            cost_tracker,
        );
        if emit_deltas {
            sink.emit(&events.model_routed(&routed_model.decision))?;
        }
        let turn_provider_config = routed_model.provider_config;

        RuntimeSteerStep::new().apply(steer_handle, conversation, history_writer.as_deref_mut())?;

        let cwd_display = cwd.display().to_string();
        let provider_turn = RuntimeProviderTurnStep::new().run(
            &mut actor,
            config.provider,
            conversation,
            &turn_provider_config,
            &cwd_display,
            emit_deltas,
            hooks,
            cancel,
            cost_tracker,
            config.max_budget_usd,
            events,
            sink,
            history_writer.as_deref_mut(),
        )?;
        let response = match provider_turn.response {
            Some(response) => response,
            None => {
                let error = provider_turn
                    .terminal_error
                    .expect("provider turn terminal");
                if emit_deltas && error.status != RunStatus::Cancelled {
                    sink.emit(&events.error(&error.message))?;
                }
                return Ok(AgentLoopResult::failure(error.status, error.message));
            }
        };

        let provider_error = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(message) => Some(message.clone()),
            _ => None,
        });

        if let Some(error) = provider_error {
            if context::is_prompt_too_long_error(&error) && !reactive_compacted {
                RuntimeCompactionStep::new(
                    config.provider,
                    &ctx_config,
                    &provider_config,
                    cwd,
                    emit_deltas,
                    hooks,
                    events,
                    sink,
                    history_writer.as_deref_mut(),
                )
                .compact_after_prompt_too_long(conversation)?;
                reactive_compacted = true;
                continue;
            }
            if emit_deltas {
                sink.emit(&events.error(&error))?;
            }
            return Ok(AgentLoopResult::failure(RunStatus::Failed, error));
        }

        reactive_compacted = false;

        for step in &response.steps {
            match step {
                ProviderStep::ReplayState(replay) => {
                    if emit_deltas {
                        sink.emit(&events.provider_replay_updated(replay))?;
                    }
                }
                _ => {}
            }
        }

        if response.tool_calls.is_empty() {
            let final_message = response.assistant_content.clone();
            record_assistant_response_for_agent(
                conversation,
                history_writer.as_deref_mut(),
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
                emit_deltas,
            )?;
            if emit_deltas && config.auto_memory {
                let provider_config = ProviderConfig {
                    api_key: config.api_key.clone(),
                    base_url: config.base_url.clone(),
                    model: Some(orca_core::model::auxiliary_model().to_string()),
                    tools_override: Some(Vec::new()),
                    mcp_registry: None,
                    external_tools: Vec::new(),
                };
                if let Err(error) = memory::extract_project_memory(
                    config.provider,
                    &provider_config,
                    cwd,
                    &conversation.messages,
                ) {
                    sink.emit(&events.error(&format!("memory extraction failed: {error}")))?;
                }
            }
            return Ok(AgentLoopResult::success(final_message));
        }

        record_assistant_response_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
            emit_deltas,
        )?;

        let tool_requests: Vec<tool_types::ToolRequest> = response
            .steps
            .iter()
            .filter_map(|step| match step {
                ProviderStep::ToolCall(tool_request) => Some(tool_request.clone()),
                _ => None,
            })
            .collect();
        let mut index = 0;
        let mut permission_overlay = TurnPermissionOverlay::default();
        while index < tool_requests.len() {
            if let Some(result) = reject_disallowed_child_tool(
                &tool_requests[index],
                tool_policy,
                mcp_registry,
                &config.external_tools,
            ) {
                if emit_deltas {
                    sink.emit(&events.tool_call_requested(&tool_requests[index]))?;
                    sink.emit(&events.tool_call_completed(&result))?;
                }
                return Ok(AgentLoopResult::failure(
                    RunStatus::Failed,
                    result.error.clone().unwrap_or_default(),
                ));
            }

            if should_run_subagent_batch(config, &tool_requests[index], subagent_depth) {
                let batch_end = collect_subagent_batch(config, &tool_requests, index);
                let results = execute_subagent_batch(
                    config,
                    cwd,
                    events,
                    sink,
                    &tool_requests[index..batch_end],
                    subagent_depth,
                    emit_deltas,
                    instructions,
                    memory,
                    mcp_registry,
                    hooks,
                    cost_tracker,
                    cancel,
                    workflow_ipc,
                    execute_child_agent_loop,
                )?;

                for (status, result) in results {
                    record_tool_result_for_agent(
                        conversation,
                        history_writer.as_deref_mut(),
                        &result,
                        emit_deltas,
                    )?;

                    if status == RunStatus::ApprovalRequired {
                        return Ok(AgentLoopResult {
                            status,
                            final_message: None,
                            error: result.error.clone(),
                        });
                    }
                    if status == RunStatus::Failed {
                        return Ok(AgentLoopResult::failure(
                            RunStatus::Failed,
                            result.error.clone().unwrap_or_default(),
                        ));
                    }
                }
                index = batch_end;
                continue;
            }

            if orca_tools::should_run_readonly_batch(
                config.tools.max_read_parallel,
                &tool_requests[index],
            ) {
                let batch_end = orca_tools::collect_readonly_batch(
                    config.tools.max_read_parallel,
                    &tool_requests,
                    index,
                );
                let results = execute_readonly_batch(
                    cwd,
                    events,
                    sink,
                    &tool_requests[index..batch_end],
                    emit_deltas,
                    mcp_registry,
                    hooks,
                    config.tools.output_truncation,
                )?;

                for result in results {
                    record_tool_result_for_agent(
                        conversation,
                        history_writer.as_deref_mut(),
                        &result,
                        emit_deltas,
                    )?;
                }
                index = batch_end;
                continue;
            }

            let tool_request = &tool_requests[index];
            let (status, result) = execute_tool_with_approval(
                config,
                events,
                sink,
                tool_request,
                ToolExecutionContext::new(cwd, subagent_depth, emit_deltas, &policy)
                    .with_services(instructions, memory, mcp_registry, hooks)
                    .with_runtime(
                        cost_tracker,
                        cancel,
                        task_registry,
                        background_workflows,
                        workflow_ipc,
                    )
                    .with_permission_overlay(&mut permission_overlay)
                    .with_permission_handler(permission_handler),
            )?;

            record_plan_state_for_agent(
                conversation,
                history_writer.as_deref_mut(),
                tool_request,
                &result,
            );

            record_tool_result_for_agent(
                conversation,
                history_writer.as_deref_mut(),
                &result,
                emit_deltas,
            )?;

            if status == RunStatus::ApprovalRequired {
                return Ok(AgentLoopResult {
                    status,
                    final_message: None,
                    error: result.error.clone(),
                });
            }
            if status == RunStatus::Failed && tool_request.name == tool_types::ToolName::Subagent {
                return Ok(AgentLoopResult::failure(
                    RunStatus::Failed,
                    result.error.clone().unwrap_or_default(),
                ));
            }
            index += 1;
        }
    }
}

pub(crate) fn execute_child_agent_loop<W: io::Write>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    runtime: &mut ChildAgentRuntime<'_, W>,
    child_cost_tracker: &mut CostTracker,
) -> io::Result<ChildAgentResult> {
    let task_registry = TaskRegistry::new_for_cwd(runtime.events.run_id().to_string(), runtime.cwd);
    let mut background_workflows = Vec::new();
    let child = run_agent_loop(
        config,
        AgentLoopContext::new(
            runtime.cwd,
            &request.prompt,
            request.depth,
            request.emit_deltas,
            &request.subagent_type,
        )
        .with_services(
            runtime.instructions,
            runtime.memory,
            runtime.mcp_registry,
            runtime.hooks,
        )
        .with_runtime(child_cost_tracker, runtime.cancel, &task_registry)
        .with_execution(
            &mut background_workflows,
            request.workflow_ipc.as_ref(),
            runtime.lifecycle.as_deref_mut(),
        ),
        runtime.events,
        runtime.sink,
        AgentConversationContext::new(),
        AgentToolPolicyContext::new(
            request.allowed_tools.as_deref(),
            request.tool_policy_label.as_deref(),
        ),
    )?;
    observe_background_workflows(
        config.output_format == OutputFormat::Jsonl,
        runtime.events,
        runtime.sink,
        &mut background_workflows,
    )?;
    Ok(ChildAgentResult {
        status: child.status,
        final_message: child.final_message,
        error: child.error,
    })
}

fn execute_tool_with_approval(
    config: &RunConfig,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_request: &tool_types::ToolRequest,
    context: ToolExecutionContext<'_>,
) -> io::Result<(RunStatus, tool_types::ToolResult)> {
    let mut actor = ToolExecutionActor::new(events.run_id().to_string(), DEFAULT_MAX_TURNS);
    actor.execute(
        config,
        events,
        sink,
        tool_request,
        context,
        execute_child_agent_loop,
        execute_child_agent_loop,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::RuntimeTaskKind;
    use std::path::PathBuf;

    #[test]
    fn runtime_turn_config_snapshots_agent_loop_entry_values() {
        let cwd = PathBuf::from("/tmp/orca-runtime-turn-config");
        let subagent_type = SubagentType::General;

        let config = RuntimeTurnConfig::new(&cwd, "inspect repo", 2, false, &subagent_type);

        assert_eq!(config.cwd(), cwd.as_path());
        assert_eq!(config.prompt(), "inspect repo");
        assert_eq!(config.subagent_depth(), 2);
        assert!(!config.emit_deltas());
        assert_eq!(config.subagent_type(), &SubagentType::General);
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_config() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-config");
        let subagent_type = SubagentType::General;

        let context = AgentLoopContext::new(&cwd, "inspect repo", 1, true, &subagent_type);

        let config = context.turn_config();
        assert_eq!(config.cwd(), cwd.as_path());
        assert_eq!(config.prompt(), "inspect repo");
        assert_eq!(config.subagent_depth(), 1);
        assert!(config.emit_deltas());
        assert_eq!(config.subagent_type(), &SubagentType::General);
    }

    #[test]
    fn agent_loop_context_carries_readonly_services() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-services");
        let subagent_type = SubagentType::General;
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_services(&instructions, &memory, &registry, &hooks);

        assert!(std::ptr::eq(context.instructions(), &instructions));
        assert!(std::ptr::eq(context.memory(), &memory));
        assert!(std::ptr::eq(context.mcp_registry(), &registry));
        assert!(std::ptr::eq(context.hooks(), &hooks));
    }

    #[test]
    fn runtime_turn_deps_group_agent_loop_services() {
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let deps = RuntimeTurnDeps::new(&instructions, &memory, &registry, &hooks);

        assert!(std::ptr::eq(deps.instructions(), &instructions));
        assert!(std::ptr::eq(deps.memory(), &memory));
        assert!(std::ptr::eq(deps.mcp_registry(), &registry));
        assert!(std::ptr::eq(deps.hooks(), &hooks));
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_deps() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-deps");
        let subagent_type = SubagentType::General;
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_services(&instructions, &memory, &registry, &hooks);

        let deps = context.turn_deps();
        assert!(std::ptr::eq(deps.instructions(), &instructions));
        assert!(std::ptr::eq(deps.memory(), &memory));
        assert!(std::ptr::eq(deps.mcp_registry(), &registry));
        assert!(std::ptr::eq(deps.hooks(), &hooks));
    }

    #[test]
    fn agent_loop_context_carries_runtime_refs() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-runtime");
        let subagent_type = SubagentType::General;
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-runtime".to_string());

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_runtime(&mut cost_tracker, &cancel, &task_registry);

        assert_eq!(context.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(context.cancel(), &cancel));
        assert!(std::ptr::eq(context.task_registry(), &task_registry));
    }

    #[test]
    fn runtime_turn_state_groups_mutable_runtime_refs() {
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-state".to_string());

        let state = RuntimeTurnState::new(&mut cost_tracker, &cancel, &task_registry);

        assert_eq!(state.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(state.cancel(), &cancel));
        assert!(std::ptr::eq(state.task_registry(), &task_registry));
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_state() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-state");
        let subagent_type = SubagentType::General;
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("agent-loop-state-context".to_string());

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_runtime(&mut cost_tracker, &cancel, &task_registry);

        let state = context.turn_state();
        assert_eq!(state.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(state.cancel(), &cancel));
        assert!(std::ptr::eq(state.task_registry(), &task_registry));
    }

    #[test]
    fn agent_loop_context_carries_execution_refs() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-execution");
        let subagent_type = SubagentType::General;
        let mut background_workflows = Vec::new();
        let mut lifecycle = RuntimeSessionLifecycle::new("agent-loop-execution");
        lifecycle.start_task(RuntimeTaskKind::Agent);

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_execution(&mut background_workflows, None, Some(&mut lifecycle));

        assert_eq!(context.background_workflow_count(), 0);
        assert!(context.workflow_ipc().is_none());
        assert_eq!(
            context.lifecycle().unwrap().run_id(),
            "agent-loop-execution"
        );
    }

    #[test]
    fn runtime_turn_execution_groups_lifecycle_refs() {
        let mut background_workflows = Vec::new();
        let mut lifecycle = RuntimeSessionLifecycle::new("agent-loop-execution-group");
        lifecycle.start_task(RuntimeTaskKind::Agent);

        let execution =
            RuntimeTurnExecution::new(&mut background_workflows, None, Some(&mut lifecycle));

        assert_eq!(execution.background_workflow_count(), 0);
        assert!(execution.workflow_ipc().is_none());
        assert_eq!(
            execution.lifecycle().unwrap().run_id(),
            "agent-loop-execution-group"
        );
    }

    #[test]
    fn agent_loop_context_exposes_runtime_turn_execution() {
        let cwd = PathBuf::from("/tmp/orca-agent-loop-execution-context");
        let subagent_type = SubagentType::General;
        let mut background_workflows = Vec::new();
        let mut lifecycle = RuntimeSessionLifecycle::new("agent-loop-execution-context");
        lifecycle.start_task(RuntimeTaskKind::Agent);

        let context = AgentLoopContext::new(&cwd, "inspect repo", 0, true, &subagent_type)
            .with_execution(&mut background_workflows, None, Some(&mut lifecycle));

        let execution = context.turn_execution();
        assert_eq!(execution.background_workflow_count(), 0);
        assert!(execution.workflow_ipc().is_none());
        assert_eq!(
            execution.lifecycle().unwrap().run_id(),
            "agent-loop-execution-context"
        );
    }
}
