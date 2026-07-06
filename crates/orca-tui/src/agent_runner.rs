use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventEnvelope, EventFactory};
use orca_core::hook_types::HookEvent;
use orca_core::model::ModelRouteContext;
use orca_core::provider_types::ProviderStep;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::BackgroundTaskSummary;
use orca_core::tool_types;
use orca_core::workflow_types::WorkflowInput;
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::tool_schema::{
    deepseek_goal_tools_schema_with_mcp_and_external, deepseek_tools_schema_with_mcp_and_external,
};
use orca_runtime::agent_common;
use orca_runtime::hooks::{HookContext, conversation_with_hook_context};
use orca_runtime::memory;

use crate::agent_subagent_execution::{
    collect_subagent_batch, execute_subagent_batch_for_tui, should_run_subagent_batch,
};
use crate::agent_tool_execution::{execute_readonly_batch_for_tui, execute_tool_for_tui};
use crate::agent_workflow_execution::execute_workflow_for_tui;
use crate::bridge::TuiConversationSession;
use crate::runtime_event_projection::tui_event_from_runtime_event;
use crate::types::{TuiEvent, UserAction};

pub(crate) const DEFAULT_MAX_TURNS: u32 = 128;

pub(crate) type PendingWorkflowNotifications = Arc<Mutex<VecDeque<String>>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TuiAgentTurnResult {
    pub(crate) status: String,
    pub(crate) next_prompt: Option<String>,
}

impl TuiAgentTurnResult {
    fn new(status: impl Into<String>) -> Self {
        Self {
            status: status.into(),
            next_prompt: None,
        }
    }

    fn with_next_prompt(status: impl Into<String>, next_prompt: String) -> Self {
        Self {
            status: status.into(),
            next_prompt: Some(next_prompt),
        }
    }
}

fn send_error_for_tui(event_tx: &Sender<TuiEvent>, events: &mut EventFactory, message: &str) {
    send_runtime_event_as_tui(event_tx, events.error(message));
}

fn send_session_completed_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    status: orca_core::event_schema::RunStatus,
) {
    send_runtime_event_as_tui(event_tx, events.session_completed(status));
}

fn send_session_completed_status_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    status: &str,
) {
    let status = match status {
        "success" => orca_core::event_schema::RunStatus::Success,
        "failed" => orca_core::event_schema::RunStatus::Failed,
        "interrupted" | "cancelled" => orca_core::event_schema::RunStatus::Cancelled,
        "approval_required" => orca_core::event_schema::RunStatus::ApprovalRequired,
        "verification_failed" => orca_core::event_schema::RunStatus::VerificationFailed,
        "budget_exhausted" => orca_core::event_schema::RunStatus::BudgetExhausted,
        _ => orca_core::event_schema::RunStatus::Failed,
    };
    send_session_completed_for_tui(event_tx, events, status);
}

pub(crate) fn send_runtime_event_as_tui(event_tx: &Sender<TuiEvent>, event: EventEnvelope) {
    if let Some(event) = tui_event_from_runtime_event(&event) {
        let _ = event_tx.send(event);
    }
}

pub(crate) fn send_workflow_tasks_updated_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    tasks: &[BackgroundTaskSummary],
) {
    send_runtime_event_as_tui(event_tx, events.workflow_tasks_updated(tasks));
}

pub(crate) fn send_tool_requested_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    request: &tool_types::ToolRequest,
) {
    send_runtime_event_as_tui(event_tx, events.tool_call_requested(request));
}

pub(crate) fn send_tool_completed_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    result: &tool_types::ToolResult,
    diff: Option<String>,
) {
    if let Some(TuiEvent::ToolCompleted {
        id,
        name,
        status,
        output,
        kind,
        ..
    }) = tui_event_from_runtime_event(&events.tool_call_completed(result))
    {
        let _ = event_tx.send(TuiEvent::ToolCompleted {
            id,
            name,
            status,
            output,
            diff,
            kind,
        });
    }
}

pub(crate) fn send_subagent_started_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    id: &str,
    description: &str,
) {
    send_runtime_event_as_tui(event_tx, events.subagent_started(id, description));
}

pub(crate) fn send_subagent_completed_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    id: &str,
    description: &str,
    status: orca_core::event_schema::RunStatus,
    output: Option<&str>,
    error: Option<&str>,
) {
    send_runtime_event_as_tui(
        event_tx,
        events.subagent_completed(id, description, status, output, error),
    );
}

pub(crate) struct WorkflowNotificationPayload<'a> {
    pub(crate) task_id: &'a str,
    pub(crate) run_id: &'a str,
    pub(crate) tool_use_id: &'a str,
    pub(crate) workflow_name: &'a str,
    pub(crate) status: &'a str,
    pub(crate) summary: &'a str,
}

pub(crate) fn send_workflow_notification_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    payload: WorkflowNotificationPayload<'_>,
) {
    let event = if payload.status == "completed" {
        events.workflow_result_available(
            payload.task_id,
            payload.run_id,
            payload.workflow_name,
            Some(payload.tool_use_id),
            payload.status,
            payload.summary,
        )
    } else {
        events.workflow_failed(
            payload.task_id,
            payload.run_id,
            payload.workflow_name,
            Some(payload.tool_use_id),
            payload.summary,
        )
    };
    send_runtime_event_as_tui(event_tx, event);
}

pub fn launch_saved_workflow_for_tui(
    config: &RunConfig,
    session: &TuiConversationSession,
    name: &str,
    raw_args: Option<&str>,
    event_tx: &Sender<TuiEvent>,
) {
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let args = match raw_args.map(parse_saved_workflow_args).transpose() {
        Ok(args) => args,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error));
            return;
        }
    };
    let input = WorkflowInput {
        name: Some(name.to_string()),
        args,
        ..Default::default()
    };
    let raw_arguments = match serde_json::to_string(&input) {
        Ok(raw_arguments) => raw_arguments,
        Err(error) => {
            let _ = event_tx.send(TuiEvent::Error(error.to_string()));
            return;
        }
    };
    let request = tool_types::ToolRequest {
        id: format!("tui-workflow-{}", now_ms()),
        name: tool_types::ToolName::Workflow,
        action: orca_core::approval_types::ActionKind::Agent,
        target: Some(name.to_string()),
        raw_arguments: Some(raw_arguments),
    };
    let mut events = EventFactory::new(
        session
            .session_id()
            .unwrap_or("tui-workflow-session")
            .to_string(),
    );
    send_tool_requested_for_tui(event_tx, &mut events, &request);
    let result =
        execute_workflow_for_tui(config, &cwd, &request, event_tx, session.task_registry());
    send_tool_completed_for_tui(event_tx, &mut events, &result, None);
}

fn parse_saved_workflow_args(raw: &str) -> Result<serde_json::Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    if trimmed.starts_with('{') {
        let value: serde_json::Value =
            serde_json::from_str(trimmed).map_err(|error| error.to_string())?;
        if value.is_object() {
            return Ok(value);
        }
        return Err("workflow args JSON must be an object".to_string());
    }

    let mut object = serde_json::Map::new();
    for part in trimmed.split_whitespace() {
        let Some((key, value)) = part.split_once('=') else {
            return Err(format!("workflow arg `{part}` must use key=value"));
        };
        if key.trim().is_empty() {
            return Err("workflow arg key cannot be empty".to_string());
        }
        let parsed_value = serde_json::from_str(value)
            .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
        object.insert(key.to_string(), parsed_value);
    }
    Ok(serde_json::Value::Object(object))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

fn tui_tools_schema(
    mcp_registry: &McpRegistry,
    external_tools: &[orca_core::external_config::ExternalToolConfig],
    allow_goal_tools: bool,
) -> Vec<serde_json::Value> {
    if allow_goal_tools {
        deepseek_goal_tools_schema_with_mcp_and_external(Some(mcp_registry), external_tools)
    } else {
        deepseek_tools_schema_with_mcp_and_external(Some(mcp_registry), external_tools)
    }
}

pub fn run_agent_for_tui(
    config: &RunConfig,
    session: &mut TuiConversationSession,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    cancel: &CancelToken,
    allow_goal_tools: bool,
) -> String {
    run_agent_for_tui_with_notification_queue(
        config,
        session,
        prompt,
        event_tx,
        action_rx,
        cancel,
        allow_goal_tools,
        None,
    )
    .status
}

pub(crate) fn run_agent_for_tui_with_notification_queue(
    config: &RunConfig,
    session: &mut TuiConversationSession,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    cancel: &CancelToken,
    allow_goal_tools: bool,
    pending_workflow_notifications: Option<&PendingWorkflowNotifications>,
) -> TuiAgentTurnResult {
    let cwd = config
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

    let tools_override = tui_tools_schema(
        session.mcp_registry(),
        &config.external_tools,
        allow_goal_tools,
    );
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(orca_core::model::FLASH_MODEL.to_string()),
        reasoning_effort: config.reasoning_effort,
        tools_override: Some(tools_override),
        mcp_registry: Some(session.mcp_registry().clone()),
        external_tools: config.external_tools.clone(),
    };

    let budget_model = config.model.as_option();
    let ctx_config = orca_provider::context::ContextConfig::for_model_with_runtime(
        budget_model.as_deref(),
        &config.model_runtime,
    );
    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    let mut permission_overlay = orca_runtime::lifecycle::TurnPermissionOverlay::default();
    session.replace_skill_context(agent_common::explicit_skill_context(&cwd, prompt));
    session.conversation_mut().add_user(prompt.to_string());
    if let Some(message) = session.conversation().messages.last().cloned() {
        session.append_message(&message);
    }

    let mut turn: u32 = 0;
    let mut reactive_compacted = false;
    let mut runtime_events = EventFactory::new(
        session
            .session_id()
            .unwrap_or("tui-agent-session")
            .to_string(),
    );

    loop {
        turn += 1;

        if turn > DEFAULT_MAX_TURNS {
            send_error_for_tui(event_tx, &mut runtime_events, "max turns exhausted");
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::BudgetExhausted,
            );
            session.complete("budget_exhausted");
            return TuiAgentTurnResult::new("budget_exhausted");
        }

        if orca_provider::context::needs_compaction_wire(
            session.conversation(),
            &ctx_config,
            &provider_config,
        ) {
            session.compact(config, &cwd);
        }

        let _ = event_tx.send(TuiEvent::ContextUpdated {
            used_tokens: orca_provider::context::conversation_tokens(session.conversation()),
            limit_tokens: ctx_config.effective_limit(),
        });

        let (turn, task) = session.next_turn_lifecycle();
        let _ = event_tx.send(TuiEvent::TurnStarted { turn, task });

        let route_decision = config.model.route(ModelRouteContext {
            subagent_type: &SubagentType::General,
            subagent_model: None,
        });
        session
            .cost_tracker_mut()
            .set_model(Some(&route_decision.actual_model));
        let mut turn_provider_config = provider_config.clone();
        turn_provider_config.model = Some(route_decision.actual_model.clone());

        let pre_model_outcome = match session.hooks().run(
            HookEvent::PreModelCall,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                send_error_for_tui(
                    event_tx,
                    &mut runtime_events,
                    &format!("pre_model_call hook failed: {error}"),
                );
                send_session_completed_for_tui(
                    event_tx,
                    &mut runtime_events,
                    orca_core::event_schema::RunStatus::Failed,
                );
                session.complete("failed");
                return TuiAgentTurnResult::new("failed");
            }
        };
        let model_conversation =
            conversation_with_hook_context(session.conversation(), &pre_model_outcome);

        let tx = event_tx.clone();
        let mut emitted_message_delta = false;
        let mut stream_events = EventFactory::new(runtime_events.run_id().to_string());
        let response = orca_provider::call_streaming(
            config.provider,
            &model_conversation,
            &turn_provider_config,
            cancel,
            &mut |step| match step {
                ProviderStep::ReasoningDelta(text) => {
                    send_runtime_event_as_tui(&tx, stream_events.assistant_reasoning_delta(text));
                }
                ProviderStep::MessageDelta(text) => {
                    emitted_message_delta = true;
                    send_runtime_event_as_tui(&tx, stream_events.assistant_message_delta(text));
                }
                ProviderStep::ToolCallProgress(progress) => {
                    send_runtime_event_as_tui(&tx, stream_events.tool_call_progress(progress));
                }
                _ => {}
            },
        );

        if let Err(error) = session.hooks().run(
            HookEvent::PostModelCall,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: None,
                tool_result: None,
                before_messages: None,
                after_messages: None,
                usage: response.usage.as_ref(),
            },
        ) {
            send_error_for_tui(
                event_tx,
                &mut runtime_events,
                &format!("post_model_call hook failed: {error}"),
            );
        }

        if let Some(usage) = response.usage
            && !usage.is_empty()
        {
            let totals = session.cost_tracker_mut().add_usage(usage);
            send_runtime_event_as_tui(event_tx, runtime_events.usage_updated(totals));
            if let Some(writer) = session.writer_mut() {
                let _ = writer.append_usage(totals);
            }
            if let Some(max_budget) = config.max_budget_usd
                && totals.estimated_cost_usd > max_budget
            {
                send_error_for_tui(
                    event_tx,
                    &mut runtime_events,
                    &format!(
                        "budget exhausted: estimated cost ${:.6} exceeded limit ${:.6}",
                        totals.estimated_cost_usd, max_budget
                    ),
                );
                send_session_completed_for_tui(
                    event_tx,
                    &mut runtime_events,
                    orca_core::event_schema::RunStatus::BudgetExhausted,
                );
                session.complete("budget_exhausted");
                return TuiAgentTurnResult::new("budget_exhausted");
            }
        }

        if cancel.is_cancelled() {
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Cancelled,
            );
            session.complete("interrupted");
            return TuiAgentTurnResult::new("interrupted");
        }

        if let Some(error) = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(message) => Some(message.clone()),
            _ => None,
        }) {
            if orca_provider::context::is_prompt_too_long_error(&error) && !reactive_compacted {
                let before_messages = session.conversation().messages.len();
                let compaction = orca_provider::context::compact_with_summary(
                    config.provider,
                    session.conversation(),
                    &ctx_config,
                    &provider_config,
                );
                *session.conversation_mut() = compaction.conversation;
                let after_messages = session.conversation().messages.len();
                let summary_state = session.conversation().summary.clone();
                if let Some(writer) = session.writer_mut() {
                    let _ = writer.append_compaction(before_messages, after_messages);
                    if let orca_provider::context::CompactionKind::RemoteSummary(summary) =
                        compaction.kind
                    {
                        let _ = writer.append_summary_state(
                            before_messages,
                            after_messages,
                            summary,
                            &summary_state,
                        );
                    }
                }
                reactive_compacted = true;
                continue;
            }
            send_error_for_tui(event_tx, &mut runtime_events, &error);
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Failed,
            );
            session.complete("failed");
            return TuiAgentTurnResult::new("failed");
        }

        reactive_compacted = false;

        if response.tool_calls.is_empty() {
            if !emitted_message_delta
                && let Some(content) = response.assistant_content.as_deref()
                && !content.is_empty()
            {
                send_runtime_event_as_tui(
                    event_tx,
                    runtime_events.assistant_message_delta(content),
                );
            }
            session.conversation_mut().add_assistant(
                response.assistant_content,
                response.assistant_reasoning,
                vec![],
            );
            if let Some(message) = session.conversation().messages.last().cloned() {
                session.append_message(&message);
            }
            if config.auto_memory {
                let provider_kind = config.provider;
                let provider_config = ProviderConfig {
                    api_key: config.api_key.clone(),
                    base_url: config.base_url.clone(),
                    model: Some(orca_core::model::auxiliary_model().to_string()),
                    reasoning_effort: config.reasoning_effort,
                    tools_override: Some(Vec::new()),
                    mcp_registry: None,
                    external_tools: Vec::new(),
                };
                let memory_cwd = cwd.clone();
                let messages = session.conversation().messages.clone();
                let memory_tx = event_tx.clone();
                let run_id = runtime_events.run_id().to_string();
                thread::spawn(move || {
                    if let Err(error) = memory::extract_project_memory(
                        provider_kind,
                        &provider_config,
                        &memory_cwd,
                        &messages,
                    ) {
                        let mut events = EventFactory::new(run_id);
                        send_error_for_tui(
                            &memory_tx,
                            &mut events,
                            &format!("memory extraction failed: {error}"),
                        );
                    }
                });
            }
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Success,
            );
            session.complete("success");
            return TuiAgentTurnResult::new("success");
        }

        session.conversation_mut().add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );
        if let Some(message) = session.conversation().messages.last().cloned() {
            session.append_message(&message);
        }

        let tool_requests: Vec<tool_types::ToolRequest> = response
            .steps
            .iter()
            .filter_map(|step| match step {
                ProviderStep::ToolCall(tool_request) => Some(tool_request.clone()),
                _ => None,
            })
            .collect();
        let mut index = 0;
        while index < tool_requests.len() {
            if should_run_subagent_batch(config, &tool_requests[index], 0) {
                let batch_end = collect_subagent_batch(config, &tool_requests, index);
                let results = execute_subagent_batch_for_tui(
                    config,
                    &cwd,
                    &tool_requests[index..batch_end],
                    event_tx,
                    0,
                    session.instructions(),
                    session.memory(),
                    session.hooks(),
                    Some(session.task_registry()),
                );
                for (should_stop, result, child_cost) in results {
                    session.cost_tracker_mut().merge(&child_cost);
                    let result_content = agent_common::format_tool_result_for_model(&result);
                    session
                        .conversation_mut()
                        .add_tool_result(result.id.clone(), result_content);
                    if let Some(message) = session.conversation().messages.last().cloned() {
                        session.append_message(&message);
                    }
                    if should_stop {
                        send_session_completed_for_tui(
                            event_tx,
                            &mut runtime_events,
                            orca_core::event_schema::RunStatus::ApprovalRequired,
                        );
                        session.complete("approval_required");
                        return TuiAgentTurnResult::new("approval_required");
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
                let results = execute_readonly_batch_for_tui(
                    &cwd,
                    &tool_requests[index..batch_end],
                    event_tx,
                    session.mcp_registry(),
                    session.hooks(),
                    config.tools.output_truncation,
                );
                for result in results {
                    let result_content = agent_common::format_tool_result_for_model(&result);
                    session
                        .conversation_mut()
                        .add_tool_result(result.id.clone(), result_content);
                    if let Some(message) = session.conversation().messages.last().cloned() {
                        session.append_message(&message);
                    }
                }
                index = batch_end;
                continue;
            }

            let tool_request = &tool_requests[index];
            let (should_stop, result, child_cost) = execute_tool_for_tui(
                config,
                &cwd,
                tool_request,
                event_tx,
                action_rx,
                0,
                session.session_id(),
                &policy,
                session.instructions(),
                session.memory(),
                session.mcp_registry(),
                session.hooks(),
                Some(session.task_registry()),
                &mut permission_overlay,
                cancel,
            );

            if let Some(c) = child_cost {
                session.cost_tracker_mut().merge(&c);
            }

            if tool_request.name == tool_types::ToolName::UpdatePlan
                && result.status == tool_types::ToolStatus::Completed
            {
                if let Ok(update) = orca_tools::update_plan::parse_args(tool_request) {
                    session.conversation_mut().replace_plan_state(
                        orca_tools::update_plan::format_context_message(&update),
                    );
                    if let Some(writer) = session.writer_mut() {
                        let _ = writer.append_plan_state(update.explanation, update.plan);
                    }
                }
            }

            let result_content = agent_common::format_tool_result_for_model(&result);
            session
                .conversation_mut()
                .add_tool_result(tool_request.id.clone(), result_content);
            if let Some(message) = session.conversation().messages.last().cloned() {
                session.append_message(&message);
            }

            if should_stop {
                let status = if matches!(result.status, tool_types::ToolStatus::Denied) {
                    "approval_required"
                } else {
                    "failed"
                };
                send_session_completed_status_for_tui(event_tx, &mut runtime_events, status);
                session.complete(status);
                return TuiAgentTurnResult::new(status);
            }
            index += 1;
        }
        if let Some(next_prompt) =
            take_pending_workflow_notification(pending_workflow_notifications)
        {
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Success,
            );
            session.complete("success");
            return TuiAgentTurnResult::with_next_prompt("success", next_prompt);
        }
    }
}

fn take_pending_workflow_notification(
    pending_workflow_notifications: Option<&PendingWorkflowNotifications>,
) -> Option<String> {
    pending_workflow_notifications.and_then(|queue| queue.lock().ok()?.pop_front())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_subagent_execution::{
        collect_subagent_batch, execute_subagent_batch_for_tui, execute_subagent_for_tui,
        execute_subagent_status_for_tui, run_child_agent_for_tui_silent, should_run_subagent_batch,
    };
    use crate::agent_tool_execution::{canonical_action_for_tool, execute_tool_for_tui};
    use orca_runtime::hooks::HookRunner;
    use orca_runtime::instructions::ProjectInstructions;
    use orca_runtime::memory::MemoryBlock;
    use orca_runtime::tasks::TaskRegistry;
    use std::collections::VecDeque;
    use std::path::Path;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};
    use orca_core::event_schema::RunStatus;
    use orca_core::model::ModelSelection;
    use orca_runtime::workflow::host::WorkflowHost;

    fn config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: std::env::current_dir().ok(),
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
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            subagents: Default::default(),
            tools: Default::default(),
            workflows: Default::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn full_auto_config() -> RunConfig {
        RunConfig {
            approval_mode: ApprovalMode::FullAuto,
            ..config()
        }
    }

    #[test]
    fn saved_workflow_args_parse_key_value_and_json_objects() {
        let value = parse_saved_workflow_args("target=src maxAgents=8 dryRun=true").unwrap();
        assert_eq!(value["target"], "src");
        assert_eq!(value["maxAgents"], 8);
        assert_eq!(value["dryRun"], true);

        let value = parse_saved_workflow_args(r#"{"target":"crates","maxAgents":4}"#).unwrap();
        assert_eq!(value["target"], "crates");
        assert_eq!(value["maxAgents"], 4);
    }

    #[test]
    fn tui_session_owns_runtime_thread_boundary() {
        let source = include_str!("bridge.rs");
        let session_start = source
            .find("pub struct TuiConversationSession")
            .expect("TuiConversationSession source");
        let session_source = &source[session_start..];
        let session_end = session_source
            .find("impl TuiConversationSession")
            .expect("TuiConversationSession impl");
        let session_struct = &session_source[..session_end];

        assert!(
            session_struct.contains("runtime: RuntimeThread"),
            "TUI session must own RuntimeThread instead of rebuilding runtime state locally"
        );
        assert!(
            !session_struct.contains("RuntimeSessionLifecycle"),
            "TUI session lifecycle must be owned through RuntimeThread"
        );
        assert!(
            !session_struct.contains("InteractiveSession"),
            "TUI session must not own InteractiveSession outside RuntimeThread"
        );
    }

    #[test]
    fn tui_session_reuses_conversation_across_submits() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "first", None).expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "first prompt",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );
        run_agent_for_tui(
            &config,
            &mut session,
            "mock_history_echo",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let echoed = events.iter().find_map(|event| match event {
            TuiEvent::MessageDelta(text) if text.contains("Mock history users") => {
                Some(text.as_str())
            }
            _ => None,
        });
        assert!(
            echoed
                .unwrap_or_default()
                .contains("first prompt | mock_history_echo")
        );
    }

    #[test]
    fn tui_displays_final_assistant_content_without_stream_delta() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "silent", None).expect("session");

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "mock_silent_final",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        assert_eq!(status, "success");
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::MessageDelta(text) if text.contains("Mock silent final response.")
            )
        }));
    }

    #[test]
    fn tui_turn_started_events_include_agent_task_lifecycle() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "task lifecycle", None)
                .expect("session");

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "mock_silent_final",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(status, "success");
        let turn = event_rx
            .try_iter()
            .find_map(|event| match event {
                TuiEvent::TurnStarted { turn, task } => task.map(|task| (turn, task)),
                _ => None,
            })
            .expect("turn started with task lifecycle");
        assert_eq!(turn.0, 1);
        assert_eq!(turn.1.kind, "agent");
        assert_eq!(turn.1.status, "running");
        assert_eq!(turn.1.turn, 1);
    }

    #[test]
    fn tui_tool_schema_exposes_goal_tool_only_for_goal_turns() {
        let config = config();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "first", None).expect("session");
        session.replace_goal_context("goal instructions".to_string());

        let base_names = tui_tools_schema(session.mcp_registry(), &config.external_tools, false)
            .into_iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        let goal_names = tui_tools_schema(session.mcp_registry(), &config.external_tools, true)
            .into_iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();

        assert!(!base_names.contains(&"update_goal".to_string()));
        assert!(goal_names.contains(&"update_goal".to_string()));
    }

    #[test]
    fn tui_session_exposes_runtime_owned_workflow_state() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = config();
        config.cwd = Some(temp.path().to_path_buf());
        let session = TuiConversationSession::new_with_preloaded(&config, "workflow state", None)
            .expect("session");

        assert!(!session.runtime_session().has_active_workflows());
        let handle = session.runtime_session().task_registry().create_workflow(
            "run-1".to_string(),
            "demo".to_string(),
            "demo workflow".to_string(),
            1,
        );
        session
            .runtime_session()
            .task_registry()
            .mark_running(&handle.id)
            .expect("running");

        assert!(session.has_active_workflows());
    }

    #[test]
    fn tui_task_list_uses_runtime_task_registry() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = full_auto_config();
        config.cwd = Some(temp.path().to_path_buf());
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session = TuiConversationSession::new_with_preloaded(&config, "task_list", None)
            .expect("session");
        let task = session.runtime_session().task_registry().create_workflow(
            "workflow-run-1".to_string(),
            "mock-workflow".to_string(),
            "demo workflow".to_string(),
            1,
        );
        session
            .runtime_session()
            .task_registry()
            .mark_running(&task.id)
            .expect("mark workflow running");

        run_agent_for_tui(
            &config,
            &mut session,
            "task_list",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let task_list = events
            .iter()
            .find_map(|event| match event {
                TuiEvent::ToolCompleted {
                    name,
                    status,
                    output,
                    ..
                } if name == "task_list" => Some((status.as_str(), output.as_str())),
                _ => None,
            })
            .expect("task_list tool completion");

        assert_eq!(
            task_list.0, "completed",
            "expected completed task_list, got {}",
            task_list.1
        );
        assert!(
            task_list.1.contains("demo workflow"),
            "expected runtime task output, got {}",
            task_list.1
        );
        assert!(
            !task_list
                .1
                .contains("task_list tool must be executed by the runtime"),
            "TUI must not route task_list through the placeholder executor"
        );
    }

    #[test]
    fn failed_workflow_notification_is_returned_after_tool_batch_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = full_auto_config();
        config.cwd = Some(temp.path().to_path_buf());
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let pending_notifications = Arc::new(Mutex::new(VecDeque::from([String::from(
            "<task-notification><status>failed</status></task-notification>",
        )])));
        let mut session = TuiConversationSession::new_with_preloaded(&config, "task_list", None)
            .expect("session");

        let result = run_agent_for_tui_with_notification_queue(
            &config,
            &mut session,
            "task_list",
            &event_tx,
            &action_rx,
            &cancel,
            false,
            Some(&pending_notifications),
        );

        assert_eq!(result.status, "success");
        assert_eq!(
            result.next_prompt.as_deref(),
            Some("<task-notification><status>failed</status></task-notification>")
        );
        assert!(pending_notifications.lock().unwrap().is_empty());
        assert!(event_rx.try_iter().any(|event| {
            matches!(event, TuiEvent::SessionCompleted { status } if status == "success")
        }));
    }

    #[test]
    fn empty_failed_workflow_notification_queue_does_not_inject_after_tool_batch() {
        let temp = tempfile::tempdir().unwrap();
        let mut config = full_auto_config();
        config.cwd = Some(temp.path().to_path_buf());
        let (event_tx, _event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let pending_notifications = Arc::new(Mutex::new(VecDeque::new()));
        let mut session = TuiConversationSession::new_with_preloaded(&config, "task_list", None)
            .expect("session");

        let result = run_agent_for_tui_with_notification_queue(
            &config,
            &mut session,
            "task_list",
            &event_tx,
            &action_rx,
            &cancel,
            false,
            Some(&pending_notifications),
        );

        assert_eq!(result.status, "success");
        assert!(result.next_prompt.is_none());
    }

    #[test]
    fn tui_workflow_tool_launches_runtime_instead_of_placeholder_executor() {
        if !WorkflowHost::node_available() {
            return;
        }

        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "workflow inline", None)
                .expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "workflow inline",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let mut events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && !workflow_runtime_events_complete(&events) {
            if let Ok(event) = event_rx.recv_timeout(Duration::from_millis(50)) {
                events.push(event);
            }
        }
        let workflow = events
            .iter()
            .find_map(|event| match event {
                TuiEvent::ToolCompleted {
                    name,
                    status,
                    output,
                    ..
                } if name == "Workflow" => Some((status.as_str(), output.as_str())),
                _ => None,
            })
            .expect("workflow tool completion");

        assert_eq!(workflow.0, "completed");
        assert!(
            workflow.1.contains("\"status\":\"async_launched\""),
            "expected async workflow launch output, got {}",
            workflow.1
        );
        assert!(
            !workflow
                .1
                .contains("Workflow must be executed by the runtime controller"),
            "TUI must not route Workflow through the placeholder executor"
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::WorkflowTasksUpdated { tasks }
                if tasks.iter().any(|task| task.workflow_run_id.is_some())
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::WorkflowTasksUpdated { tasks }
                if tasks.iter().any(|task| {
                    task.workflow_progress
                        .map(|progress| {
                            progress.total_agents > 0
                                && progress.completed_agents + progress.failed_agents > 0
                        })
                        .unwrap_or(false)
                })
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::WorkflowNotification { prompt, status, summary }
                if prompt.contains("<task-notification>")
                    && prompt.contains("<status>completed</status>")
                    && *status == "completed"
                    && summary.contains("mock-workflow")
            )
        }));
    }

    fn workflow_runtime_events_complete(events: &[TuiEvent]) -> bool {
        let has_notification = events
            .iter()
            .any(|event| matches!(event, TuiEvent::WorkflowNotification { .. }));
        let has_terminal_progress = events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::WorkflowTasksUpdated { tasks }
                if tasks.iter().any(|task| {
                    task.workflow_progress
                        .map(|progress| {
                            progress.total_agents > 0
                                && progress.completed_agents + progress.failed_agents > 0
                        })
                        .unwrap_or(false)
                })
            )
        });
        has_notification && has_terminal_progress
    }

    #[test]
    fn tui_workflow_draft_tool_uses_runtime_draft_store() {
        let mut config = full_auto_config();
        config.output_format = OutputFormat::Jsonl;
        let temp = tempfile::tempdir().unwrap();
        config.cwd = Some(temp.path().to_path_buf());
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "workflow draft", None)
                .expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "workflow draft",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let draft_tool = events.iter().find_map(|event| match event {
            TuiEvent::ToolCompleted {
                name,
                status,
                output,
                ..
            } if name == "WorkflowDraft" => Some((status.as_str(), output.as_str())),
            _ => None,
        });
        let (status, output) = draft_tool.expect("workflow draft tool completed");
        assert_eq!(status, "completed");
        assert!(output.contains("\"draftId\""));
        assert!(
            !output.contains("WorkflowDraft must be executed by the runtime controller"),
            "TUI must not route WorkflowDraft through the placeholder executor"
        );
    }

    #[test]
    fn tui_streaming_bash_observes_turn_cancel() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let turn_cancel = cancel.clone();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "bash", None).expect("session");

        let handle = std::thread::spawn(move || {
            run_agent_for_tui(
                &config,
                &mut session,
                "bash printf 'before\\n'; sleep 5; printf after",
                &event_tx,
                &action_rx,
                &turn_cancel,
                false,
            )
        });

        let start = Instant::now();
        loop {
            match event_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("TUI event before timeout")
            {
                TuiEvent::ToolOutputDelta { chunk, .. } if chunk.contains("before") => {
                    cancel.cancel();
                    break;
                }
                TuiEvent::SessionCompleted { status } => {
                    panic!("session completed before streaming output: {status}");
                }
                _ => {}
            }
        }

        let status = handle.join().expect("turn thread joined");
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "cancelled TUI streaming bash should not wait for shell timeout"
        );
        assert_eq!(status, "interrupted");
    }

    #[test]
    fn tui_approval_action_rejects_caller_supplied_read_for_shell() {
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Read,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let registry = McpRegistry::default();

        assert_eq!(
            canonical_action_for_tool(&request, &registry, &[]),
            orca_core::approval_types::ActionKind::Shell
        );
    }

    #[test]
    fn tui_tool_approval_uses_runtime_handler_before_execution() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::Approve(true))
            .expect("send approval");
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("printf approved".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "printf approved" }).to_string()),
        };

        let (should_stop, result, _) = execute_tool_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &request,
            &event_tx,
            &action_rx,
            0,
            Some("approval-session"),
            &ApprovalPolicy::new(config.approval_mode),
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &HookRunner::default(),
            None,
            &mut orca_runtime::lifecycle::TurnPermissionOverlay::default(),
            &CancelToken::new(),
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        assert!(!should_stop);
        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("approved"));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::ApprovalNeeded { tool, target, preview, .. }
                if tool == "bash"
                    && target == &Some("printf approved".to_string())
                    && preview == &Some("$ printf approved".to_string())
            )
        }));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::ToolCompleted { name, status, output, .. }
                if name == "bash" && status == "completed" && output == "approved"
            )
        }));
    }

    #[test]
    fn tui_tool_approval_cancel_returns_denied_result() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx.send(UserAction::Cancel).expect("send cancel");
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("printf denied".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "printf denied" }).to_string()),
        };

        let (should_stop, result, _) = execute_tool_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &request,
            &event_tx,
            &action_rx,
            0,
            Some("approval-session"),
            &ApprovalPolicy::new(config.approval_mode),
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &HookRunner::default(),
            None,
            &mut orca_runtime::lifecycle::TurnPermissionOverlay::default(),
            &CancelToken::new(),
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        assert!(should_stop);
        assert_eq!(result.status, tool_types::ToolStatus::Denied);
        assert_eq!(result.error.as_deref(), Some("user denied"));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::ToolCompleted { name, status, output, .. }
                if name == "bash" && status == "denied" && output == "user denied"
            )
        }));
    }

    #[test]
    fn tui_session_backtracks_last_user_before_next_submit() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "first", None).expect("session");

        run_agent_for_tui(
            &config,
            &mut session,
            "first prompt",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );
        run_agent_for_tui(
            &config,
            &mut session,
            "second prompt",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        assert_eq!(
            session.backtrack_last_user(),
            Some("second prompt".to_string())
        );

        run_agent_for_tui(
            &config,
            &mut session,
            "mock_history_echo",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        let events: Vec<TuiEvent> = event_rx.try_iter().collect();
        let echoed = events.iter().rev().find_map(|event| match event {
            TuiEvent::MessageDelta(text) if text.contains("Mock history users") => {
                Some(text.as_str())
            }
            _ => None,
        });
        let echoed = echoed.unwrap_or_default();
        assert!(echoed.contains("first prompt | mock_history_echo"));
        assert!(!echoed.contains("second prompt"));
    }

    #[test]
    fn tui_request_user_input_waits_for_answer_and_continues() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "ask", None).expect("session");

        let responder = std::thread::spawn(move || {
            loop {
                match event_rx.recv().expect("event") {
                    TuiEvent::UserInputRequested { question, .. } => {
                        assert_eq!(question, "Continue?");
                        action_tx
                            .send(UserAction::RespondToUserInput("yes".to_string()))
                            .expect("send answer");
                        break;
                    }
                    TuiEvent::SessionCompleted { status } => {
                        panic!("completed before user input request: {status}");
                    }
                    _ => {}
                }
            }
        });

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "ask Continue?",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        responder.join().expect("responder joined");
        assert_eq!(status, "success");
    }

    #[test]
    fn tui_request_user_input_cancel_stops_turn() {
        let config = config();
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let cancel = CancelToken::new();
        let mut session =
            TuiConversationSession::new_with_preloaded(&config, "ask", None).expect("session");

        let responder = std::thread::spawn(move || {
            loop {
                match event_rx.recv().expect("event") {
                    TuiEvent::UserInputRequested { .. } => {
                        action_tx.send(UserAction::Cancel).expect("send cancel");
                        break;
                    }
                    TuiEvent::SessionCompleted { status } => {
                        panic!("completed before user input request: {status}");
                    }
                    _ => {}
                }
            }
        });

        let status = run_agent_for_tui(
            &config,
            &mut session,
            "ask Continue?",
            &event_tx,
            &action_rx,
            &cancel,
            false,
        );

        responder.join().expect("responder joined");
        assert_eq!(status, "failed");
    }

    #[test]
    fn tui_child_agent_recovers_from_invalid_tool_arguments() {
        let config = full_auto_config();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();

        let (child, _child_cost_tracker) = run_child_agent_for_tui_silent(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            "bad_plan_then_fix",
            None,
            1,
            &SubagentType::General,
            &instructions,
            &memory,
            &hooks,
            None,
        );

        assert_eq!(child.status, RunStatus::Success);
        assert!(
            child
                .final_message
                .as_deref()
                .unwrap_or_default()
                .contains("Mock completed after fixing malformed tool arguments")
        );
    }

    #[test]
    fn tui_subagent_batch_records_child_failure_without_stopping_batch() {
        let config = full_auto_config();
        let (event_tx, _event_rx) = mpsc::channel();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let failing = tool_types::ToolRequest {
            id: "subagent-failing".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("failing child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "failing child",
                    "prompt": "mock_fail"
                })
                .to_string(),
            ),
        };
        let succeeding = tool_types::ToolRequest {
            id: "subagent-succeeding".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("succeeding child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "succeeding child",
                    "prompt": "simple audit"
                })
                .to_string(),
            ),
        };

        let results = execute_subagent_batch_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &[failing, succeeding],
            &event_tx,
            0,
            &instructions,
            &memory,
            &hooks,
            None,
        );

        assert_eq!(results.len(), 2);
        assert!(!results[0].0, "child failure should not stop parent batch");
        assert_eq!(results[0].1.status, tool_types::ToolStatus::Failed);
        assert!(!results[1].0);
        assert_eq!(results[1].1.status, tool_types::ToolStatus::Completed);
    }

    #[test]
    fn tui_subagent_batch_emits_child_activity_progress() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::channel();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let request = tool_types::ToolRequest {
            id: "subagent-progress".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("child progress".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "child progress",
                    "prompt": "bash echo child"
                })
                .to_string(),
            ),
        };

        let results = execute_subagent_batch_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &[request],
            &event_tx,
            0,
            &instructions,
            &memory,
            &hooks,
            None,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].1.status, tool_types::ToolStatus::Completed);
        let events = event_rx.try_iter().collect::<Vec<_>>();
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::SubagentProgress { id, activity, turn, .. }
                if id == "subagent-progress"
                    && activity.contains("bash")
                    && *turn == Some(1)
            )
        }));
    }

    #[test]
    fn tui_sync_subagent_batch_updates_task_registry_activity() {
        let config = full_auto_config();
        let (event_tx, event_rx) = mpsc::channel();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let registry = TaskRegistry::new("session-sync-progress".to_string());
        let request = tool_types::ToolRequest {
            id: "subagent-sync-progress".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("sync progress child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "sync progress child",
                    "prompt": "bash echo child"
                })
                .to_string(),
            ),
        };

        let results = execute_subagent_batch_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &[request],
            &event_tx,
            0,
            &instructions,
            &memory,
            &hooks,
            Some(&registry),
        );

        assert_eq!(results[0].1.status, tool_types::ToolStatus::Completed);
        let tasks = registry.list();
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].task_type,
            orca_core::task_types::TaskType::Subagent
        );
        assert!(
            tasks[0]
                .subagent_current_activity
                .as_deref()
                .unwrap_or_default()
                .contains("bash")
        );
        assert_eq!(
            tasks[0].status,
            orca_core::task_types::TaskStatus::Completed
        );
        assert!(event_rx.try_iter().any(|event| {
            matches!(event, TuiEvent::WorkflowTasksUpdated { tasks }
                if tasks.iter().any(|task| task.description == "sync progress child"))
        }));
    }

    #[test]
    fn tui_async_subagent_skips_sync_batch_path() {
        let config = full_auto_config();
        let request = tool_types::ToolRequest {
            id: "subagent-async".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("async child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async child",
                    "prompt": "simple audit",
                    "mode": "async"
                })
                .to_string(),
            ),
        };
        let requests = vec![request, {
            tool_types::ToolRequest {
                id: "subagent-sync".to_string(),
                name: tool_types::ToolName::Subagent,
                action: orca_core::approval_types::ActionKind::Agent,
                target: Some("sync child".to_string()),
                raw_arguments: Some(
                    serde_json::json!({
                        "description": "sync child",
                        "prompt": "simple audit"
                    })
                    .to_string(),
                ),
            }
        }];

        assert!(!should_run_subagent_batch(&config, &requests[0], 0));
        assert_eq!(collect_subagent_batch(&config, &requests, 0), 0);
        assert!(should_run_subagent_batch(&config, &requests[1], 0));
        assert_eq!(collect_subagent_batch(&config, &requests, 1), 2);
    }

    #[test]
    fn tui_async_subagent_launches_task_and_status_returns_result() {
        let config = full_auto_config();
        let (event_tx, _event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let registry = TaskRegistry::new("session-async".to_string());
        let request = tool_types::ToolRequest {
            id: "subagent-async".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("async child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async child",
                    "prompt": "mock_usage",
                    "mode": "async"
                })
                .to_string(),
            ),
        };

        let (result, _cost) = execute_subagent_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &request,
            &event_tx,
            &action_rx,
            0,
            &instructions,
            &memory,
            &hooks,
            Some(&registry),
        );

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        let launched: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("launch output")).unwrap();
        assert_eq!(launched["status"], "async_launched");
        let agent_id = launched["agent_id"].as_str().expect("agent id");

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if registry
                .get(agent_id)
                .map(|record| record.status == orca_core::task_types::TaskStatus::Completed)
                .unwrap_or(false)
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let status_request = tool_types::ToolRequest {
            id: "subagent-status".to_string(),
            name: tool_types::ToolName::SubagentStatus,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "agent_id": agent_id }).to_string()),
        };
        let status = execute_subagent_status_for_tui(&status_request, &registry);
        assert_eq!(status.status, tool_types::ToolStatus::Completed);
        let payload: serde_json::Value =
            serde_json::from_str(status.output.as_deref().expect("status output")).unwrap();
        assert_eq!(payload["status"], "completed");
        assert!(payload["created_at_ms"].as_i64().unwrap() > 0);
        assert!(payload["started_at_ms"].as_i64().unwrap() > 0);
        assert!(payload["completed_at_ms"].as_i64().unwrap() > 0);
        assert!(
            payload["output"]
                .as_str()
                .unwrap()
                .contains("Mock runtime completed")
        );
        assert_eq!(payload["usage"]["input_tokens"], 120);
        assert_eq!(payload["usage"]["output_tokens"], 30);
        assert_eq!(payload["usage"]["cache_tokens"], 10);
        assert_eq!(payload["usage"]["total_tokens"], 150);
        assert!(payload["usage"]["estimated_cost_usd"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn tui_async_subagent_records_live_activity_for_status() {
        let config = full_auto_config();
        let (event_tx, _event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let registry = TaskRegistry::new("session-async-progress".to_string());
        let request = tool_types::ToolRequest {
            id: "subagent-async-progress".to_string(),
            name: tool_types::ToolName::Subagent,
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("async progress child".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async progress child",
                    "prompt": "bash echo child",
                    "mode": "async"
                })
                .to_string(),
            ),
        };

        let (result, _cost) = execute_subagent_for_tui(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            &request,
            &event_tx,
            &action_rx,
            0,
            &instructions,
            &memory,
            &hooks,
            Some(&registry),
        );
        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        let launched: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("launch output")).unwrap();
        let agent_id = launched["agent_id"].as_str().expect("agent id");

        // Wait for the child to finish so the asserted registry state is
        // final rather than a transient mid-run snapshot. The specificity
        // rule keeps the tool activity ("bash: ...") in place through the
        // trailing turn-started/streaming events, and the mock provider
        // always runs exactly two turns (tool call, then final message).
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let completed = registry.get(agent_id).is_some_and(|record| {
                record.status == orca_core::task_types::TaskStatus::Completed
            });
            if completed {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        let status_request = tool_types::ToolRequest {
            id: "subagent-status".to_string(),
            name: tool_types::ToolName::SubagentStatus,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "agent_id": agent_id }).to_string()),
        };
        let status = execute_subagent_status_for_tui(&status_request, &registry);
        let payload: serde_json::Value =
            serde_json::from_str(status.output.as_deref().expect("status output")).unwrap();
        assert_eq!(payload["status"], "completed");
        assert!(
            payload["current_activity"]
                .as_str()
                .unwrap_or_default()
                .contains("bash"),
            "expected bash activity in status payload: {payload:?}; record: {:?}",
            registry.get(agent_id)
        );
        assert_eq!(payload["turn"], 2);
        assert!(payload["last_activity_at_ms"].as_i64().unwrap() > 0);
    }
}
