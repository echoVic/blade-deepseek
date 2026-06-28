use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventEnvelope, EventFactory, EventType, RunStatus};
use orca_core::hook_types::HookEvent;
use orca_core::model::ModelRouteContext;
use orca_core::provider_types::ProviderStep;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::BackgroundTaskSummary;
use orca_core::tool_types;
use orca_core::workflow_types::{WorkflowDraftActionOutput, WorkflowInput};
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::tool_schema::{
    deepseek_goal_tools_schema_with_mcp_and_external,
    deepseek_tools_schema_for_type_with_mcp_and_external,
    deepseek_tools_schema_with_mcp_and_external,
};
use orca_runtime::agent_common;
use orca_runtime::cost::CostTracker;
use orca_runtime::history;
use orca_runtime::hooks::{
    HookContext, HookRunner, conversation_with_hook_context, tool_request_with_hook_outcome,
};
use orca_runtime::instructions::ProjectInstructions;
use orca_runtime::lifecycle::{
    RuntimeApprovalDecision, RuntimeApprovalHandler, RuntimeSessionLifecycle, RuntimeTaskKind,
    RuntimeToolActorContext, RuntimeTurnRunner, RuntimeUserInputHandler, RuntimeUserInputRequest,
};
use orca_runtime::memory::{self, MemoryBlock};
use orca_runtime::session::InteractiveSession;
use orca_runtime::subagent::{self, SubagentMode};
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::tool_invocation::{approval_request_for_invocation, prepare_tool_invocation};
use orca_runtime::workflow::{WorkflowDraftStore, WorkflowLaunchRequest, WorkflowRunner};
use serde::Deserialize;

use crate::diff;
use crate::types::{TuiEvent, TuiTaskLifecycle, UserAction};

const DEFAULT_MAX_TURNS: u32 = 128;

#[derive(Clone, Debug)]
struct TuiAgentResult {
    status: String,
    final_message: Option<String>,
    error: Option<String>,
    cost_tracker: CostTracker,
}

struct TuiApprovalHandler<'a> {
    action_rx: &'a Receiver<UserAction>,
}

impl<'a> TuiApprovalHandler<'a> {
    fn new(action_rx: &'a Receiver<UserAction>) -> Self {
        Self { action_rx }
    }
}

impl RuntimeApprovalHandler for TuiApprovalHandler<'_> {
    fn resolve_interactive(
        &self,
        approval: &ApprovalRequest,
        _request: &tool_types::ToolRequest,
    ) -> std::io::Result<ApprovalResolution> {
        let allowed = loop {
            match self.action_rx.recv() {
                Ok(UserAction::Approve(value)) => break value,
                Ok(UserAction::Interrupt) | Ok(UserAction::Cancel) | Err(_) => break false,
                _ => continue,
            }
        };
        Ok(ApprovalResolution {
            id: approval.id.clone(),
            decision: if allowed {
                ApprovalDecision::Allow
            } else {
                ApprovalDecision::Deny
            },
            reason: if allowed {
                "user approved".to_string()
            } else {
                "user denied".to_string()
            },
        })
    }
}

struct TuiUserInputHandler<'a> {
    event_tx: &'a Sender<TuiEvent>,
    action_rx: &'a Receiver<UserAction>,
}

impl<'a> TuiUserInputHandler<'a> {
    fn new(event_tx: &'a Sender<TuiEvent>, action_rx: &'a Receiver<UserAction>) -> Self {
        Self {
            event_tx,
            action_rx,
        }
    }
}

impl RuntimeUserInputHandler for TuiUserInputHandler<'_> {
    fn request_user_input(
        &self,
        request: &RuntimeUserInputRequest,
    ) -> std::io::Result<Option<String>> {
        let _ = self.event_tx.send(TuiEvent::UserInputRequested {
            id: request.id.clone(),
            question: request.question.clone(),
            choices: request.choices.clone(),
        });

        loop {
            match self.action_rx.recv() {
                Ok(UserAction::RespondToUserInput(answer)) => return Ok(Some(answer)),
                Ok(UserAction::Interrupt) | Ok(UserAction::Cancel) | Err(_) => return Ok(None),
                _ => continue,
            }
        }
    }
}

fn tui_event_from_runtime_event(event: &EventEnvelope) -> Option<TuiEvent> {
    match event.event_type {
        EventType::AssistantReasoningDelta => Some(TuiEvent::ReasoningDelta(
            event.payload["text"].as_str()?.to_string(),
        )),
        EventType::AssistantMessageDelta => Some(TuiEvent::MessageDelta(
            event.payload["text"].as_str()?.to_string(),
        )),
        EventType::UsageUpdated => Some(TuiEvent::UsageUpdated(UsageTotals {
            input_tokens: event.payload["input_tokens"].as_u64()?,
            output_tokens: event.payload["output_tokens"].as_u64()?,
            cache_tokens: event.payload["cache_tokens"].as_u64().unwrap_or_default(),
            estimated_cost_usd: event.payload["estimated_cost_usd"].as_f64()?,
        })),
        EventType::ToolCallRequested => Some(TuiEvent::ToolRequested {
            id: event.payload["id"].as_str()?.to_string(),
            name: event.payload["name"].as_str()?.to_string(),
            target: event
                .payload
                .get("target")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        }),
        EventType::ToolCallCompleted => {
            let output = event
                .payload
                .get("output")
                .and_then(|value| value.as_str())
                .or_else(|| event.payload.get("error").and_then(|value| value.as_str()))
                .unwrap_or_default()
                .to_string();
            Some(TuiEvent::ToolCompleted {
                id: event.payload["id"].as_str()?.to_string(),
                name: event.payload["name"].as_str()?.to_string(),
                status: event.payload["status"].as_str()?.to_string(),
                output,
                diff: None,
                kind: event
                    .payload
                    .get("kind")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            })
        }
        EventType::PlanUpdated => Some(TuiEvent::PlanUpdated {
            explanation: serde_json::from_value(event.payload["explanation"].clone()).ok()?,
            plan: serde_json::from_value(event.payload["plan"].clone()).ok()?,
        }),
        EventType::ApprovalRequested => Some(TuiEvent::ApprovalNeeded {
            id: event.payload["id"].as_str()?.to_string(),
            tool: event
                .payload
                .get("tool")
                .and_then(|value| value.as_str())
                .or_else(|| event.payload["action"].as_str())?
                .to_string(),
            target: event
                .payload
                .get("target")
                .and_then(|value| value.as_str())
                .or_else(|| {
                    event
                        .payload
                        .get("description")
                        .and_then(|value| value.as_str())
                })
                .map(str::to_string),
            preview: event
                .payload
                .get("preview")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        }),
        EventType::SubagentStarted => Some(TuiEvent::SubagentStarted {
            id: event.payload["id"].as_str()?.to_string(),
            description: event.payload["description"].as_str()?.to_string(),
        }),
        EventType::SubagentCompleted => Some(TuiEvent::SubagentCompleted {
            id: event.payload["id"].as_str()?.to_string(),
            description: event.payload["description"].as_str()?.to_string(),
            status: match event.payload["status"].as_str()? {
                "success" => "completed",
                status => status,
            }
            .to_string(),
            output: event
                .payload
                .get("output")
                .and_then(|value| value.as_str())
                .map(str::to_string),
            error: event
                .payload
                .get("error")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        }),
        EventType::WorkflowResultAvailable | EventType::WorkflowFailed => {
            let status = event.payload["status"]
                .as_str()
                .unwrap_or(if event.event_type == EventType::WorkflowFailed {
                    "failed"
                } else {
                    "completed"
                })
                .to_string();
            let summary = event
                .payload
                .get("result")
                .and_then(|value| value.as_str())
                .or_else(|| event.payload.get("error").and_then(|value| value.as_str()))
                .unwrap_or_default()
                .to_string();
            let notification = WorkflowTerminalNotification {
                task_id: event.payload["taskId"].as_str()?.to_string(),
                run_id: event.payload["runId"].as_str()?.to_string(),
                tool_use_id: event
                    .payload
                    .get("toolUseId")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                status: status.clone(),
                summary: summary.clone(),
            };
            let workflow_name = event
                .payload
                .get("workflowName")
                .and_then(|value| value.as_str())
                .unwrap_or("workflow");
            Some(TuiEvent::WorkflowNotification {
                prompt: notification.to_prompt(),
                status,
                summary: format!("{workflow_name}: {summary}"),
            })
        }
        EventType::WorkflowTasksUpdated => Some(TuiEvent::WorkflowTasksUpdated {
            tasks: serde_json::from_value(event.payload["tasks"].clone()).ok()?,
        }),
        EventType::Error => Some(TuiEvent::Error(
            event.payload["message"].as_str()?.to_string(),
        )),
        EventType::SessionCompleted => Some(TuiEvent::SessionCompleted {
            status: event.payload["status"].as_str()?.to_string(),
        }),
        _ => None,
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

fn send_runtime_event_as_tui(event_tx: &Sender<TuiEvent>, event: EventEnvelope) {
    if let Some(event) = tui_event_from_runtime_event(&event) {
        let _ = event_tx.send(event);
    }
}

fn send_workflow_tasks_updated_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    tasks: &[BackgroundTaskSummary],
) {
    send_runtime_event_as_tui(event_tx, events.workflow_tasks_updated(tasks));
}

fn send_tool_requested_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    request: &tool_types::ToolRequest,
) {
    send_runtime_event_as_tui(event_tx, events.tool_call_requested(request));
}

fn send_tool_completed_for_tui(
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

fn send_subagent_started_for_tui(
    event_tx: &Sender<TuiEvent>,
    events: &mut EventFactory,
    id: &str,
    description: &str,
) {
    send_runtime_event_as_tui(event_tx, events.subagent_started(id, description));
}

fn send_subagent_completed_for_tui(
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

struct WorkflowNotificationPayload<'a> {
    task_id: &'a str,
    run_id: &'a str,
    tool_use_id: &'a str,
    workflow_name: &'a str,
    status: &'a str,
    summary: &'a str,
}

fn send_workflow_notification_for_tui(
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

pub struct TuiConversationSession {
    runtime: InteractiveSession,
    lifecycle: RuntimeSessionLifecycle,
}

impl TuiConversationSession {
    pub fn new_with_preloaded(
        config: &RunConfig,
        prompt_for_title: &str,
        preloaded: Option<history::SessionTranscript>,
    ) -> std::io::Result<Self> {
        let runtime = InteractiveSession::new_with_preloaded(config, prompt_for_title, preloaded)?;
        let run_id = runtime
            .session_id()
            .unwrap_or_else(|| "tui-session")
            .to_string();
        Ok(Self {
            runtime,
            lifecycle: RuntimeSessionLifecycle::new(run_id),
        })
    }

    pub fn runtime_session(&self) -> &InteractiveSession {
        &self.runtime
    }

    fn conversation(&self) -> &orca_core::conversation::Conversation {
        self.runtime.conversation()
    }

    fn conversation_mut(&mut self) -> &mut orca_core::conversation::Conversation {
        self.runtime.conversation_mut()
    }

    fn writer_mut(&mut self) -> Option<&mut orca_runtime::history::SessionWriter> {
        self.runtime.writer_mut()
    }

    fn instructions(&self) -> &ProjectInstructions {
        self.runtime.instructions()
    }

    fn cost_tracker_mut(&mut self) -> &mut CostTracker {
        self.runtime.cost_tracker_mut()
    }

    fn mcp_registry(&self) -> &McpRegistry {
        self.runtime.mcp_registry()
    }

    fn hooks(&self) -> &HookRunner {
        self.runtime.hooks()
    }

    fn memory(&self) -> &MemoryBlock {
        self.runtime.memory()
    }

    fn task_registry(&self) -> &TaskRegistry {
        self.runtime.task_registry()
    }

    fn append_message(&mut self, message: &orca_core::conversation::Message) {
        self.runtime.append_message(message);
    }

    fn complete(&mut self, status: &str) {
        self.runtime.complete(status);
    }

    pub fn session_id(&self) -> Option<&str> {
        self.runtime.session_id()
    }

    pub fn usage_totals(&self) -> UsageTotals {
        self.runtime.usage_totals()
    }

    pub fn has_active_workflows(&self) -> bool {
        self.runtime.has_active_workflows()
    }

    pub fn backtrack_last_user(&mut self) -> Option<String> {
        self.runtime.backtrack_last_user()
    }

    pub fn set_model(&mut self, model: Option<&str>) {
        self.runtime.set_model(model);
    }

    pub fn add_pinned_context(&mut self, content: String) {
        self.runtime.add_pinned_context(content);
    }

    pub fn replace_goal_context(&mut self, content: String) {
        self.runtime.replace_goal_context(content);
    }

    fn replace_skill_context(&mut self, content: Option<String>) {
        self.runtime.replace_skill_context(content);
    }

    pub fn compact(&mut self, config: &RunConfig, cwd: &Path) -> (usize, usize) {
        self.runtime.compact(config, cwd)
    }

    fn next_turn_lifecycle(&mut self) -> (u32, Option<TuiTaskLifecycle>) {
        if self.lifecycle.active_task().is_none() {
            self.lifecycle.start_task(RuntimeTaskKind::Agent);
        }
        let started = RuntimeTurnRunner::new(&mut self.lifecycle).advance_turn();
        let task = started.task().map(|task| TuiTaskLifecycle {
            id: task.id().to_string(),
            kind: lifecycle_kind_label(task.kind()).to_string(),
            status: lifecycle_status_label(task.status()).to_string(),
            turn: task.current_turn(),
        });
        (started.turn(), task)
    }
}

fn lifecycle_kind_label(kind: orca_runtime::lifecycle::RuntimeTaskKind) -> &'static str {
    match kind {
        orca_runtime::lifecycle::RuntimeTaskKind::Agent => "agent",
        orca_runtime::lifecycle::RuntimeTaskKind::Workflow => "workflow",
        orca_runtime::lifecycle::RuntimeTaskKind::Subagent => "subagent",
        orca_runtime::lifecycle::RuntimeTaskKind::Shell => "shell",
    }
}

fn lifecycle_status_label(status: orca_runtime::lifecycle::RuntimeTaskStatus) -> &'static str {
    match status {
        orca_runtime::lifecycle::RuntimeTaskStatus::Running => "running",
        orca_runtime::lifecycle::RuntimeTaskStatus::Succeeded => "succeeded",
        orca_runtime::lifecycle::RuntimeTaskStatus::Failed => "failed",
        orca_runtime::lifecycle::RuntimeTaskStatus::Cancelled => "cancelled",
        orca_runtime::lifecycle::RuntimeTaskStatus::ApprovalRequired => "approval_required",
        orca_runtime::lifecycle::RuntimeTaskStatus::BudgetExhausted => "budget_exhausted",
    }
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
            return "budget_exhausted".to_string();
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
                return "failed".to_string();
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
                return "budget_exhausted".to_string();
            }
        }

        if cancel.is_cancelled() {
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Cancelled,
            );
            session.complete("interrupted");
            return "interrupted".to_string();
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
            return "failed".to_string();
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
                    &cwd,
                    &session.conversation().messages,
                ) {
                    send_error_for_tui(
                        event_tx,
                        &mut runtime_events,
                        &format!("memory extraction failed: {error}"),
                    );
                }
            }
            send_session_completed_for_tui(
                event_tx,
                &mut runtime_events,
                orca_core::event_schema::RunStatus::Success,
            );
            session.complete("success");
            return "success".to_string();
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
                        return "approval_required".to_string();
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
                return status.to_string();
            }
            index += 1;
        }
    }
}

fn should_run_subagent_batch(
    config: &RunConfig,
    tool_request: &tool_types::ToolRequest,
    subagent_depth: u32,
) -> bool {
    tool_request.name == tool_types::ToolName::Subagent
        && subagent_depth < config.subagents.max_depth
        && config.subagents.max_parallel > 1
        && subagent::create_subagent_request(tool_request).mode == SubagentMode::Sync
}

fn collect_subagent_batch(
    config: &RunConfig,
    tool_requests: &[tool_types::ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + config.subagents.max_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end
        && tool_requests[end].name == tool_types::ToolName::Subagent
        && subagent::create_subagent_request(&tool_requests[end]).mode == SubagentMode::Sync
    {
        end += 1;
    }
    end
}

fn execute_readonly_batch_for_tui(
    cwd: &Path,
    tool_requests: &[tool_types::ToolRequest],
    event_tx: &Sender<TuiEvent>,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    output_truncation: tool_types::ToolOutputTruncation,
) -> Vec<tool_types::ToolResult> {
    let mut hook_failed: Vec<Option<tool_types::ToolResult>> = vec![None; tool_requests.len()];
    let mut runnable = Vec::new();
    let mut events = EventFactory::new("tui-readonly-batch".to_string());

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        send_tool_requested_for_tui(event_tx, &mut events, tool_request);
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
                runnable.push((idx, tool_request_with_hook_outcome(tool_request, &outcome)));
            }
            Err(error) => {
                hook_failed[idx] = Some(tool_types::ToolResult::failed(
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
        send_tool_completed_for_tui(event_tx, &mut events, result, None);
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
            let _ = event_tx.send(TuiEvent::Error(format!(
                "post_tool_use hook failed: {error}"
            )));
        }
    }

    results
}

#[allow(clippy::too_many_arguments)]
fn execute_subagent_batch_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_requests: &[tool_types::ToolRequest],
    event_tx: &Sender<TuiEvent>,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> Vec<(bool, tool_types::ToolResult, CostTracker)> {
    let mut handles = Vec::new();
    let mut results: Vec<Option<(bool, tool_types::ToolResult, CostTracker)>> =
        vec![None; tool_requests.len()];
    let mut events = EventFactory::new("tui-subagent-batch".to_string());

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        let request = subagent::create_subagent_request(tool_request);
        let description = request.description.clone();
        let subagent_type = request.subagent_type;
        send_subagent_started_for_tui(event_tx, &mut events, &tool_request.id, &description);

        if subagent_depth >= config.subagents.max_depth {
            let error = format!("subagent max depth {} reached", config.subagents.max_depth);
            send_subagent_completed_for_tui(
                event_tx,
                &mut events,
                &tool_request.id,
                &description,
                RunStatus::Failed,
                None,
                Some(&error),
            );
            results[idx] = Some((
                false,
                tool_types::ToolResult::failed(tool_request, error, None),
                CostTracker::new(None),
            ));
            continue;
        }

        let mut child_config = config.clone();
        child_config.model = child_config
            .model
            .with_subagent_override(request.model.clone());
        let child_cwd = cwd.to_path_buf();
        let child_prompt = request.prompt;
        let child_instructions = instructions.clone();
        let child_memory = memory.clone();
        let child_hooks = hooks.clone();
        let child_tool_request = tool_request.clone();
        handles.push((
            idx,
            description,
            thread::spawn(move || {
                let child = run_child_agent_for_tui_silent(
                    &child_config,
                    &child_cwd,
                    &child_prompt,
                    subagent_depth + 1,
                    &subagent_type,
                    &child_instructions,
                    &child_memory,
                    &child_hooks,
                );
                (child_tool_request, child)
            }),
        ));
    }

    for (idx, description, handle) in handles {
        let (tool_request, child) = match handle.join() {
            Ok(result) => result,
            Err(_) => {
                let tool_request = &tool_requests[idx];
                let result =
                    tool_types::ToolResult::failed(tool_request, "subagent thread panicked", None);
                send_subagent_completed_for_tui(
                    event_tx,
                    &mut events,
                    &tool_request.id,
                    &description,
                    RunStatus::Failed,
                    None,
                    result.error.as_deref(),
                );
                results[idx] = Some((false, result, CostTracker::new(None)));
                continue;
            }
        };

        let (should_stop, result, cost_tracker) =
            child_result_to_tui_tool_result(&tool_request, &description, child, event_tx);
        results[idx] = Some((should_stop, result, cost_tracker));
    }

    results
        .into_iter()
        .map(|result| result.expect("each subagent batch item has a result"))
        .collect()
}

/// Build a human-readable preview of what a tool call will do, parsed from its
/// raw JSON arguments. Returns `None` when there is nothing meaningful to show.
/// This is best-effort: the strings come straight from the pending request, so
/// the diff/command shown is exactly what would run.
fn build_approval_preview(request: &tool_types::ToolRequest) -> Option<String> {
    use orca_core::tool_types::ToolName;

    let raw = request.raw_arguments.as_deref()?;
    let args: serde_json::Value = serde_json::from_str(raw).ok()?;

    match &request.name {
        ToolName::Edit => {
            let path = args["path"].as_str().unwrap_or("(file)");
            let old_text = args["old_text"].as_str().unwrap_or_default();
            let new_text = args["new_text"].as_str().unwrap_or_default();
            let mut out = format!("@@ {path} @@\n");
            for line in old_text.lines() {
                out.push_str(&format!("- {line}\n"));
            }
            for line in new_text.lines() {
                out.push_str(&format!("+ {line}\n"));
            }
            Some(out.trim_end().to_string())
        }
        ToolName::WriteFile => {
            let path = args["path"].as_str().unwrap_or("(file)");
            let content = args["content"]
                .as_str()
                .or_else(|| args["contents"].as_str())
                .unwrap_or_default();
            let mut out = format!("@@ write {path} @@\n");
            for line in content.lines().take(40) {
                out.push_str(&format!("+ {line}\n"));
            }
            let total = content.lines().count();
            if total > 40 {
                out.push_str(&format!("+ … (+{} more lines)\n", total - 40));
            }
            Some(out.trim_end().to_string())
        }
        ToolName::Bash => {
            let command = args["command"].as_str().or_else(|| args.as_str())?;
            Some(format!("$ {command}"))
        }
        _ => None,
    }
}

fn execute_tool_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    session_id: Option<&str>,
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    task_registry: Option<&TaskRegistry>,
    cancel: &CancelToken,
) -> (bool, tool_types::ToolResult, Option<CostTracker>) {
    let invocation = prepare_tool_invocation(tool_request, subagent_depth, mcp_registry, config);
    let mut events = EventFactory::new(
        session_id
            .map(str::to_string)
            .unwrap_or_else(|| "tui-tool-session".to_string()),
    );
    if let Some(approval) = approval_request_for_invocation(&invocation)
        && agent_common::requires_approval(approval.action)
    {
        let mut runtime_context =
            RuntimeToolActorContext::new(events.run_id().to_string(), DEFAULT_MAX_TURNS);
        let approval_decision =
            runtime_context.resolve_tool_approval(policy, Some(approval.clone()), tool_request);

        match approval_decision {
            RuntimeApprovalDecision::Allowed(_) => {}
            RuntimeApprovalDecision::Ask(approval) => {
                let mut approval = approval.clone();
                approval.preview = build_approval_preview(tool_request);
                send_runtime_event_as_tui(event_tx, events.approval_requested(&approval));

                let handler = TuiApprovalHandler::new(action_rx);
                let resolution = runtime_context
                    .resolve_interactive_tool_approval(&handler, &approval, tool_request)
                    .unwrap_or_else(|error| ApprovalResolution {
                        id: approval.id.clone(),
                        decision: ApprovalDecision::Deny,
                        reason: format!("interactive approval failed: {error}"),
                    });

                if resolution.decision == ApprovalDecision::Deny {
                    let result = tool_types::ToolResult::denied(tool_request, resolution.reason);
                    send_tool_requested_for_tui(event_tx, &mut events, tool_request);
                    send_tool_completed_for_tui(event_tx, &mut events, &result, None);
                    return (true, result, None);
                }
            }
            RuntimeApprovalDecision::Denied { result, .. } => {
                send_tool_requested_for_tui(event_tx, &mut events, tool_request);
                send_tool_completed_for_tui(event_tx, &mut events, &result, None);
                return (true, result, None);
            }
            RuntimeApprovalDecision::NotRequired => {}
        }
    }

    let mut rendered_diff = None;
    let (result, child_cost) = if tool_request.name == tool_types::ToolName::Subagent {
        let (r, c) = execute_subagent_for_tui(
            config,
            cwd,
            tool_request,
            event_tx,
            action_rx,
            subagent_depth,
            instructions,
            memory,
            hooks,
            task_registry,
        );
        (r, Some(c))
    } else {
        send_tool_requested_for_tui(event_tx, &mut events, tool_request);
        let pre_tool_outcome = match hooks.run(
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
            Ok(outcome) => outcome,
            Err(error) => {
                let result = tool_types::ToolResult::failed(
                    tool_request,
                    format!("pre_tool_use hook blocked tool: {error}"),
                    None,
                );
                send_tool_completed_for_tui(event_tx, &mut events, &result, None);
                return (true, result, None);
            }
        };
        let effective_tool_request =
            tool_request_with_hook_outcome(tool_request, &pre_tool_outcome);
        let execution_request = &effective_tool_request;
        let before = diff::capture_before(execution_request, cwd);
        let result = if execution_request.name == tool_types::ToolName::Bash {
            let mut on_output = |chunk: &str| {
                let _ = event_tx.send(TuiEvent::ToolOutputDelta {
                    id: execution_request.id.clone(),
                    chunk: chunk.to_string(),
                });
            };
            orca_tools::bash::execute_streaming_with_policy_or_cancel(
                execution_request,
                cwd,
                config.tools.output_truncation,
                std::time::Duration::from_secs(config.tools.shell_timeout_secs.max(1)),
                &mut on_output,
                || cancel.is_cancelled(),
            )
        } else if execution_request.name == tool_types::ToolName::RequestUserInput {
            execute_user_input_request_for_tui(execution_request, event_tx, action_rx)
        } else if execution_request.name == tool_types::ToolName::WorkflowDraft {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "workflow draft tools require a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            execute_workflow_draft_for_tui(config, cwd, execution_request, task_registry)
        } else if execution_request.name == tool_types::ToolName::WorkflowDraftAction {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "workflow draft action tools require a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            execute_workflow_draft_action_for_tui(
                config,
                cwd,
                execution_request,
                event_tx,
                task_registry,
            )
        } else if execution_request.name == tool_types::ToolName::Workflow {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "workflow tools require a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            execute_workflow_for_tui(config, cwd, execution_request, event_tx, task_registry)
        } else if execution_request.name == tool_types::ToolName::SubagentStatus {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "subagent_status requires a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            execute_subagent_status_for_tui(execution_request, task_registry)
        } else if matches!(
            execution_request.name,
            tool_types::ToolName::GetGoal
                | tool_types::ToolName::CreateGoal
                | tool_types::ToolName::UpdateGoal
        ) {
            let Some(session_id) = session_id.map(str::to_string) else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "goal tools require a persistent goal session",
                        None,
                    ),
                    None,
                );
            };
            let handler = Arc::new(
                move |operation: orca_tools::update_goal::GoalToolOperation| {
                    let mut store = orca_runtime::goals::GoalStore::load_default();
                    match operation {
                        orca_tools::update_goal::GoalToolOperation::Get => {
                            store.get(&session_id).map_err(|error| error.to_string())
                        }
                        orca_tools::update_goal::GoalToolOperation::Create {
                            objective,
                            token_budget,
                        } => match store.get(&session_id).map_err(|error| error.to_string())? {
                            Some(goal) if goal.status.should_continue() => Ok(None),
                            Some(goal) if !goal.status.is_terminal() => Ok(None),
                            _ => store
                                .replace(
                                    &session_id,
                                    &objective,
                                    orca_core::goal_types::ThreadGoalStatus::Active,
                                    token_budget,
                                )
                                .map(Some)
                                .map_err(|error| error.to_string()),
                        },
                        orca_tools::update_goal::GoalToolOperation::Update(update) => store
                            .update(&session_id, update)
                            .map_err(|error| error.to_string()),
                    }
                },
            );
            orca_tools::update_goal::with_goal_handler(handler, || {
                orca_tools::execute_with_mcp_external_and_policy(
                    execution_request,
                    cwd,
                    mcp_registry,
                    &config.external_tools,
                    config.tools.output_truncation,
                    config.tools.shell_timeout_secs,
                )
            })
        } else {
            orca_tools::execute_with_mcp_external_and_policy(
                execution_request,
                cwd,
                mcp_registry,
                &config.external_tools,
                config.tools.output_truncation,
                config.tools.shell_timeout_secs,
            )
        };
        if matches!(result.status, tool_types::ToolStatus::Completed) {
            rendered_diff = before.and_then(diff::render_after);
        }
        (result, None)
    };

    if tool_request.name != tool_types::ToolName::Subagent {
        send_tool_completed_for_tui(event_tx, &mut events, &result, rendered_diff);
        if tool_request.name == tool_types::ToolName::UpdatePlan
            && result.status == tool_types::ToolStatus::Completed
        {
            match orca_tools::update_plan::parse_args(tool_request) {
                Ok(update) => {
                    send_runtime_event_as_tui(event_tx, events.plan_updated(&update));
                }
                Err(error) => {
                    let _ = event_tx.send(TuiEvent::Error(format!(
                        "failed to render plan update: {error}"
                    )));
                }
            }
        }
        if let Err(error) = hooks.run(
            HookEvent::PostToolUse,
            HookContext {
                cwd: &cwd.display().to_string(),
                session_status: None,
                tool_request: Some(tool_request),
                tool_result: Some(&result),
                before_messages: None,
                after_messages: None,
                usage: None,
            },
        ) {
            let _ = event_tx.send(TuiEvent::Error(format!(
                "post_tool_use hook failed: {error}"
            )));
        }
    }

    let should_stop = should_stop_after_tui_tool_result(tool_request, &result);
    (should_stop, result, child_cost)
}

fn should_stop_after_tui_tool_result(
    tool_request: &tool_types::ToolRequest,
    result: &tool_types::ToolResult,
) -> bool {
    matches!(result.status, tool_types::ToolStatus::Denied)
        || (tool_request.name == tool_types::ToolName::RequestUserInput
            && result.status == tool_types::ToolStatus::Failed)
}

fn execute_workflow_draft_for_tui(
    config: &RunConfig,
    cwd: &Path,
    request: &tool_types::ToolRequest,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    if !config.workflows.enabled {
        return tool_types::ToolResult::failed(request, "workflows are disabled", None);
    }
    let input = match parse_workflow_draft_input(request) {
        Ok(input) => input,
        Err(error) => return tool_types::ToolResult::invalid_input(request, error.to_string()),
    };
    let session_dir = cwd
        .join(".orca")
        .join("workflow-sessions")
        .join(task_registry.session_id());
    let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
    let draft = match draft_store.create_from_script(
        task_registry.session_id(),
        cwd,
        &input.script,
        config.workflows.max_concurrent_agents,
    ) {
        Ok(draft) => draft,
        Err(error) => return tool_types::ToolResult::failed(request, error.to_string(), None),
    };
    match serde_json::to_string(&draft) {
        Ok(output) => tool_types::ToolResult::completed(request, output, false),
        Err(error) => tool_types::ToolResult::failed(request, error.to_string(), None),
    }
}

fn execute_workflow_draft_action_for_tui(
    config: &RunConfig,
    cwd: &Path,
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    if !config.workflows.enabled {
        return tool_types::ToolResult::failed(request, "workflows are disabled", None);
    }
    let input = match parse_workflow_draft_action_input(request) {
        Ok(input) => input,
        Err(error) => return tool_types::ToolResult::invalid_input(request, error.to_string()),
    };
    let session_dir = cwd
        .join(".orca")
        .join("workflow-sessions")
        .join(task_registry.session_id());
    let draft_store = WorkflowDraftStore::new(session_dir.join("workflow-drafts"));
    let draft = match draft_store.load(&input.draft_id) {
        Ok(draft) => draft,
        Err(error) => return tool_types::ToolResult::failed(request, error.to_string(), None),
    };

    let output = match input.action.as_str() {
        "run" => {
            let runner = WorkflowRunner::new(config.clone(), task_registry.clone(), session_dir);
            let launch =
                match runner.launch_background(WorkflowLaunchRequest::from(WorkflowInput {
                    draft_id: Some(input.draft_id.clone()),
                    args: input.args.clone(),
                    ..Default::default()
                })) {
                    Ok(launch) => launch,
                    Err(error) => {
                        return tool_types::ToolResult::failed(request, error.to_string(), None);
                    }
                };
            let task_id = launch.task_id.clone();
            let run_id = launch.run_id.clone();
            let workflow_name = launch.workflow_name.clone();
            let tool_use_id = request.id.clone();
            let task_id_for_notification = task_id.clone();
            let run_id_for_notification = run_id.clone();
            let tool_use_id_for_notification = tool_use_id.clone();
            let workflow_name_for_notification = workflow_name.clone();
            let mut task_events = EventFactory::new(run_id.clone());
            send_workflow_tasks_updated_for_tui(event_tx, &mut task_events, &task_registry.list());
            let notify_tx = event_tx.clone();
            let notify_registry = task_registry.clone();
            thread::spawn(move || {
                let mut events = EventFactory::new(run_id_for_notification.clone());
                while !launch.is_finished() {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    send_workflow_tasks_updated_for_tui(
                        &notify_tx,
                        &mut events,
                        &notify_registry.list(),
                    );
                }
                let (task_id, status, summary) = match launch.join() {
                    Ok(Ok(result)) => (result.task_id, "completed".to_string(), result.status_line),
                    Ok(Err(error)) => (
                        task_id_for_notification.clone(),
                        "failed".to_string(),
                        error.to_string(),
                    ),
                    Err(_) => (
                        task_id_for_notification,
                        "failed".to_string(),
                        "workflow thread panicked".to_string(),
                    ),
                };
                send_workflow_tasks_updated_for_tui(
                    &notify_tx,
                    &mut events,
                    &notify_registry.list(),
                );
                send_workflow_notification_for_tui(
                    &notify_tx,
                    &mut events,
                    WorkflowNotificationPayload {
                        task_id: &task_id,
                        run_id: &run_id_for_notification,
                        tool_use_id: &tool_use_id_for_notification,
                        workflow_name: &workflow_name_for_notification,
                        status: &status,
                        summary: &summary,
                    },
                );
            });
            WorkflowDraftActionOutput {
                status: "async_launched".to_string(),
                action: "run".to_string(),
                draft_id: input.draft_id.clone(),
                workflow_name,
                saved_path: None,
                task_id: Some(task_id),
                run_id: Some(run_id),
                script_path: Some(draft.script_path),
            }
        }
        "save" => {
            let workflow_dir = match input.scope.as_deref().unwrap_or("project") {
                "project" => cwd.join(".orca").join("workflows"),
                "user" => std::env::var_os("HOME")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| cwd.to_path_buf())
                    .join(".orca")
                    .join("workflows"),
                other => {
                    return tool_types::ToolResult::invalid_input(
                        request,
                        format!("unsupported workflow draft save scope: {other}"),
                    );
                }
            };
            let saved_path = match draft_store.save_reusable(
                &input.draft_id,
                &workflow_dir,
                input.save_as.as_deref(),
            ) {
                Ok(path) => path,
                Err(error) => {
                    return tool_types::ToolResult::failed(request, error.to_string(), None);
                }
            };
            WorkflowDraftActionOutput {
                status: "saved".to_string(),
                action: "save".to_string(),
                draft_id: input.draft_id.clone(),
                workflow_name: draft.name,
                saved_path: Some(saved_path.display().to_string()),
                task_id: None,
                run_id: None,
                script_path: Some(draft.script_path),
            }
        }
        "edit" => {
            let Some(script) = input.script.as_deref() else {
                return tool_types::ToolResult::invalid_input(
                    request,
                    "workflow draft action edit requires script",
                );
            };
            let edited = match draft_store.edit_script(
                &input.draft_id,
                script,
                config.workflows.max_concurrent_agents,
            ) {
                Ok(edited) => edited,
                Err(error) => {
                    return tool_types::ToolResult::failed(request, error.to_string(), None);
                }
            };
            WorkflowDraftActionOutput {
                status: "edited".to_string(),
                action: "edit".to_string(),
                draft_id: input.draft_id.clone(),
                workflow_name: edited.name,
                saved_path: None,
                task_id: None,
                run_id: None,
                script_path: Some(edited.script_path),
            }
        }
        "cancel" => {
            if let Err(error) = draft_store.cancel(&input.draft_id) {
                return tool_types::ToolResult::failed(request, error.to_string(), None);
            }
            WorkflowDraftActionOutput {
                status: "cancelled".to_string(),
                action: "cancel".to_string(),
                draft_id: input.draft_id,
                workflow_name: draft.name,
                saved_path: None,
                task_id: None,
                run_id: None,
                script_path: None,
            }
        }
        other => {
            return tool_types::ToolResult::invalid_input(
                request,
                format!("unsupported workflow draft action: {other}"),
            );
        }
    };

    match serde_json::to_string(&output) {
        Ok(output) => tool_types::ToolResult::completed(request, output, false),
        Err(error) => tool_types::ToolResult::failed(request, error.to_string(), None),
    }
}

fn execute_workflow_for_tui(
    config: &RunConfig,
    cwd: &Path,
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    if !config.workflows.enabled {
        return tool_types::ToolResult::failed(request, "workflows are disabled", None);
    }

    let input = match parse_workflow_input(request) {
        Ok(input) => input,
        Err(error) => return tool_types::ToolResult::invalid_input(request, error.to_string()),
    };
    let session_dir = cwd
        .join(".orca")
        .join("workflow-sessions")
        .join(task_registry.session_id());
    let runner = WorkflowRunner::new(config.clone(), task_registry.clone(), session_dir);
    let launch = match runner.launch_background(WorkflowLaunchRequest::from(input)) {
        Ok(launch) => launch,
        Err(error) => return tool_types::ToolResult::failed(request, error.to_string(), None),
    };

    let task_id = launch.task_id.clone();
    let run_id = launch.run_id.clone();
    let workflow_name = launch.workflow_name.clone();
    let tool_use_id = request.id.clone();
    let output = match serde_json::to_string(&launch.output) {
        Ok(output) => output,
        Err(error) => return tool_types::ToolResult::failed(request, error.to_string(), None),
    };
    let mut task_events = EventFactory::new(run_id.clone());
    send_workflow_tasks_updated_for_tui(event_tx, &mut task_events, &task_registry.list());

    let notify_tx = event_tx.clone();
    let notify_registry = task_registry.clone();
    thread::spawn(move || {
        let mut events = EventFactory::new(run_id.clone());
        while !launch.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(300));
            send_workflow_tasks_updated_for_tui(&notify_tx, &mut events, &notify_registry.list());
        }
        let (task_id, status, summary) = match launch.join() {
            Ok(Ok(result)) => (result.task_id, "completed".to_string(), result.status_line),
            Ok(Err(error)) => (task_id, "failed".to_string(), error.to_string()),
            Err(_) => (
                task_id,
                "failed".to_string(),
                "workflow thread panicked".to_string(),
            ),
        };
        send_workflow_tasks_updated_for_tui(&notify_tx, &mut events, &notify_registry.list());
        send_workflow_notification_for_tui(
            &notify_tx,
            &mut events,
            WorkflowNotificationPayload {
                task_id: &task_id,
                run_id: &run_id,
                tool_use_id: &tool_use_id,
                workflow_name: &workflow_name,
                status: &status,
                summary: &summary,
            },
        );
    });

    tool_types::ToolResult::completed(request, output, false)
}

struct WorkflowTerminalNotification {
    task_id: String,
    run_id: String,
    tool_use_id: String,
    status: String,
    summary: String,
}

impl WorkflowTerminalNotification {
    fn to_prompt(&self) -> String {
        format!(
            "<task-notification>\n<task-id>{}</task-id>\n<tool-use-id>{}</tool-use-id>\n<run-id>{}</run-id>\n<status>{}</status>\n<summary>{}</summary>\n</task-notification>\n\nA background workflow finished. Use this result to continue the current task.",
            xml_escape(&self.task_id),
            xml_escape(&self.tool_use_id),
            xml_escape(&self.run_id),
            xml_escape(&self.status),
            xml_escape(&self.summary)
        )
    }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn parse_workflow_input(request: &tool_types::ToolRequest) -> std::io::Result<WorkflowInput> {
    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowDraftInput {
    script: String,
}

fn parse_workflow_draft_input(
    request: &tool_types::ToolRequest,
) -> std::io::Result<WorkflowDraftInput> {
    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowDraftActionInput {
    draft_id: String,
    action: String,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    save_as: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    args: Option<serde_json::Value>,
}

fn parse_workflow_draft_action_input(
    request: &tool_types::ToolRequest,
) -> std::io::Result<WorkflowDraftActionInput> {
    let raw_arguments = request.raw_arguments.as_deref().unwrap_or("{}");
    serde_json::from_str(raw_arguments)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
}

fn execute_user_input_request_for_tui(
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
) -> tool_types::ToolResult {
    let handler = TuiUserInputHandler::new(event_tx, action_rx);
    let mut runtime_context = RuntimeToolActorContext::new("tui-user-input", DEFAULT_MAX_TURNS);
    match runtime_context.execute_user_input_tool(request, &handler) {
        Ok(result) => result,
        Err(error) => tool_types::ToolResult::failed(request, error.to_string(), None),
    }
}

#[cfg(test)]
fn canonical_action_for_tool(
    tool_request: &tool_types::ToolRequest,
    mcp_registry: &McpRegistry,
    external_tools: &[orca_core::external_config::ExternalToolConfig],
) -> orca_core::approval_types::ActionKind {
    orca_tools::canonical_action_kind_with_mcp_and_external(
        tool_request,
        Some(mcp_registry),
        external_tools,
    )
}

fn execute_subagent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    task_registry: Option<&TaskRegistry>,
) -> (tool_types::ToolResult, CostTracker) {
    let request = subagent::create_subagent_request(tool_request);
    let description = request.description.clone();
    let subagent_type = request.subagent_type.clone();
    let mut events = EventFactory::new("tui-subagent".to_string());

    send_subagent_started_for_tui(event_tx, &mut events, &tool_request.id, &description);

    if subagent_depth >= config.subagents.max_depth {
        let error = format!("subagent max depth {} reached", config.subagents.max_depth);
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            &description,
            RunStatus::Failed,
            None,
            Some(&error),
        );
        return (
            tool_types::ToolResult::failed(tool_request, error, None),
            CostTracker::new(None),
        );
    }

    if request.mode == SubagentMode::Async {
        let Some(task_registry) = task_registry else {
            return (
                tool_types::ToolResult::failed(
                    tool_request,
                    "async subagents require a main TUI session",
                    None,
                ),
                CostTracker::new(None),
            );
        };
        let result = launch_async_subagent_for_tui(
            config,
            cwd,
            tool_request,
            request,
            event_tx,
            subagent_depth,
            instructions,
            memory,
            hooks,
            task_registry,
        );
        return (result, CostTracker::new(None));
    }

    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let child = run_child_agent_for_tui(
        &child_config,
        cwd,
        &request.prompt,
        event_tx,
        action_rx,
        subagent_depth + 1,
        &subagent_type,
        instructions,
        memory,
        hooks,
    );

    if child.status == "success" {
        let output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            &description,
            RunStatus::Success,
            Some(&output),
            None,
        );
        (
            tool_types::ToolResult::completed(
                tool_request,
                format!("Subagent status: success\n\n{output}"),
                false,
            ),
            child.cost_tracker,
        )
    } else {
        let error = child
            .error
            .unwrap_or_else(|| format!("subagent ended with status {}", child.status));
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            &description,
            RunStatus::Failed,
            child.final_message.as_deref(),
            Some(&error),
        );
        (
            tool_types::ToolResult::failed(tool_request, error, None),
            child.cost_tracker,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn launch_async_subagent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    request: subagent::SubagentRequest,
    event_tx: &Sender<TuiEvent>,
    subagent_depth: u32,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    let agent_type = serde_json::to_value(&request.subagent_type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string));
    let task = task_registry.create_subagent(request.description.clone(), agent_type);
    let agent_id = task.id.clone();
    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let child_cwd = cwd.to_path_buf();
    let child_prompt = request.prompt;
    let child_type = request.subagent_type;
    let child_instructions = instructions.clone();
    let child_memory = memory.clone();
    let child_hooks = hooks.clone();
    let child_registry = task_registry.clone();
    let child_event_tx = event_tx.clone();
    let thread_agent_id = agent_id.clone();

    thread::spawn(move || {
        let mut events = EventFactory::new(thread_agent_id.clone());
        let _ = child_registry.mark_running(&thread_agent_id);
        let child = run_child_agent_for_tui_silent(
            &child_config,
            &child_cwd,
            &child_prompt,
            subagent_depth + 1,
            &child_type,
            &child_instructions,
            &child_memory,
            &child_hooks,
        );
        let usage = usage_totals_if_non_empty(child.cost_tracker.totals());
        if child.status == "success" {
            let output = child
                .final_message
                .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
            let _ = child_registry.complete_with_usage(&thread_agent_id, output, usage);
        } else {
            let error = child
                .error
                .or(child.final_message)
                .unwrap_or_else(|| format!("subagent ended with status {}", child.status));
            let _ = child_registry.fail_with_usage(&thread_agent_id, error, usage);
        }
        send_workflow_tasks_updated_for_tui(&child_event_tx, &mut events, &child_registry.list());
    });

    let mut events = EventFactory::new(agent_id.clone());
    send_workflow_tasks_updated_for_tui(event_tx, &mut events, &task_registry.list());
    tool_types::ToolResult::completed(
        tool_request,
        serde_json::json!({
            "status": "async_launched",
            "agent_id": agent_id,
            "description": request.description,
        })
        .to_string(),
        false,
    )
}

fn execute_subagent_status_for_tui(
    tool_request: &tool_types::ToolRequest,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    let agent_id = subagent::extract_subagent_field(tool_request, "agent_id")
        .or_else(|| tool_request.target.clone());
    let Some(agent_id) = agent_id else {
        return tool_types::ToolResult::invalid_input(tool_request, "missing agent_id");
    };
    let Some(record) = task_registry.get(&agent_id) else {
        return tool_types::ToolResult::failed(
            tool_request,
            format!("subagent '{agent_id}' not found"),
            None,
        );
    };
    if record.task_type != orca_core::task_types::TaskType::Subagent {
        return tool_types::ToolResult::failed(
            tool_request,
            format!("task '{agent_id}' is not a subagent"),
            None,
        );
    }
    tool_types::ToolResult::completed(
        tool_request,
        serde_json::json!({
            "agent_id": agent_id,
            "status": record.status,
            "description": record.description,
            "agent_type": record.agent_type,
            "created_at_ms": record.created_at_ms,
            "started_at_ms": record.started_at_ms,
            "completed_at_ms": record.completed_at_ms,
            "output": record.result,
            "error": record.error,
            "usage": record.usage.map(usage_totals_json),
        })
        .to_string(),
        false,
    )
}

fn usage_totals_if_non_empty(usage: UsageTotals) -> Option<UsageTotals> {
    if usage.total_tokens() == 0 && usage.cache_tokens == 0 && usage.estimated_cost_usd == 0.0 {
        None
    } else {
        Some(usage)
    }
}

fn usage_totals_json(usage: UsageTotals) -> serde_json::Value {
    serde_json::json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_tokens": usage.cache_tokens,
        "total_tokens": usage.total_tokens(),
        "estimated_cost_usd": usage.estimated_cost_usd,
    })
}

fn child_result_to_tui_tool_result(
    tool_request: &tool_types::ToolRequest,
    description: &str,
    child: TuiAgentResult,
    event_tx: &Sender<TuiEvent>,
) -> (bool, tool_types::ToolResult, CostTracker) {
    let cost_tracker = child.cost_tracker.clone();
    let mut events = EventFactory::new("tui-subagent-child".to_string());
    if child.status == "success" {
        let output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            description,
            RunStatus::Success,
            Some(&output),
            None,
        );
        (
            false,
            tool_types::ToolResult::completed(
                tool_request,
                format!("Subagent status: success\n\n{output}"),
                false,
            ),
            cost_tracker,
        )
    } else {
        let error = child
            .error
            .unwrap_or_else(|| format!("subagent ended with status {}", child.status));
        send_subagent_completed_for_tui(
            event_tx,
            &mut events,
            &tool_request.id,
            description,
            RunStatus::Failed,
            child.final_message.as_deref(),
            Some(&error),
        );
        (
            false,
            tool_types::ToolResult::failed(tool_request, error, None),
            cost_tracker,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn run_child_agent_for_tui(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> TuiAgentResult {
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(orca_core::model::FLASH_MODEL.to_string()),
        tools_override: Some(deepseek_tools_schema_for_type_with_mcp_and_external(
            subagent_type,
            Some(&mcp_registry),
            &config.external_tools,
        )),
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
    };

    let budget_model = config.model.as_option();
    let ctx_config = orca_provider::context::ContextConfig::for_model_with_runtime(
        budget_model.as_deref(),
        &config.model_runtime,
    );
    let mut conversation = Conversation::new();
    conversation.add_system(agent_common::build_agent_system_prompt(
        cwd,
        subagent_depth,
        subagent_type,
        Some(instructions),
        config.approval_mode,
        Some(memory),
    ));
    conversation.add_user(prompt.to_string());

    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());
    let mut child_cost_tracker = CostTracker::new(None);
    let mut turn: u32 = 0;
    let mut reactive_compacted = false;
    loop {
        turn += 1;
        if turn > DEFAULT_MAX_TURNS {
            return TuiAgentResult {
                status: "budget_exhausted".to_string(),
                final_message: None,
                error: Some("max turns exhausted".to_string()),
                cost_tracker: child_cost_tracker,
            };
        }

        if orca_provider::context::needs_compaction_wire(
            &conversation,
            &ctx_config,
            &provider_config,
        ) {
            let before_messages = conversation.messages.len();
            if let Ok(outcome) = hooks.run(
                HookEvent::OnBudgetWarning,
                HookContext {
                    cwd: &cwd.display().to_string(),
                    session_status: None,
                    tool_request: None,
                    tool_result: None,
                    before_messages: Some(before_messages),
                    after_messages: None,
                    usage: None,
                },
            ) {
                if !outcome.injected_context.is_empty() {
                    conversation = conversation_with_hook_context(&conversation, &outcome);
                }
            }
            conversation = orca_provider::context::compact(&conversation, &ctx_config);
        }

        let child_cancel = CancelToken::new();
        let route_decision = config.model.route(ModelRouteContext {
            subagent_type,
            subagent_model: None,
        });
        child_cost_tracker.set_model(Some(&route_decision.actual_model));
        let mut turn_provider_config = provider_config.clone();
        turn_provider_config.model = Some(route_decision.actual_model.clone());

        let pre_model_outcome = match hooks.run(
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
                return TuiAgentResult {
                    status: "failed".to_string(),
                    final_message: None,
                    error: Some(format!("pre_model_call hook failed: {error}")),
                    cost_tracker: child_cost_tracker,
                };
            }
        };
        let model_conversation = conversation_with_hook_context(&conversation, &pre_model_outcome);

        let response = orca_provider::call_streaming(
            config.provider,
            &model_conversation,
            &turn_provider_config,
            &child_cancel,
            &mut |_| {},
        );

        if let Err(error) = hooks.run(
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
            return TuiAgentResult {
                status: "failed".to_string(),
                final_message: None,
                error: Some(format!("post_model_call hook failed: {error}")),
                cost_tracker: child_cost_tracker,
            };
        }

        if let Some(error) = response.steps.iter().find_map(|step| match step {
            ProviderStep::Error(message) => Some(message.clone()),
            _ => None,
        }) {
            if orca_provider::context::is_prompt_too_long_error(&error) && !reactive_compacted {
                conversation = orca_provider::context::compact(&conversation, &ctx_config);
                reactive_compacted = true;
                continue;
            }
            return TuiAgentResult {
                status: "failed".to_string(),
                final_message: None,
                error: Some(error),
                cost_tracker: child_cost_tracker,
            };
        }

        reactive_compacted = false;

        if let Some(usage) = response.usage
            && !usage.is_empty()
        {
            child_cost_tracker.add_usage(usage);
        }

        if response.tool_calls.is_empty() {
            conversation.add_assistant(
                response.assistant_content.clone(),
                response.assistant_reasoning,
                vec![],
            );
            return TuiAgentResult {
                status: "success".to_string(),
                final_message: response.assistant_content,
                error: None,
                cost_tracker: child_cost_tracker,
            };
        }

        conversation.add_assistant(
            response.assistant_content,
            response.assistant_reasoning,
            response.tool_calls.clone(),
        );

        for step in &response.steps {
            if let ProviderStep::ToolCall(tool_request) = step {
                let (should_stop, result, child_cost) = execute_tool_for_tui(
                    config,
                    cwd,
                    tool_request,
                    event_tx,
                    action_rx,
                    subagent_depth,
                    None,
                    &policy,
                    instructions,
                    memory,
                    &mcp_registry,
                    hooks,
                    None,
                    &child_cancel,
                );

                if let Some(c) = child_cost {
                    child_cost_tracker.merge(&c);
                }

                let result_content = agent_common::format_tool_result_for_model(&result);
                conversation.add_tool_result(tool_request.id.clone(), result_content);

                if should_stop {
                    return TuiAgentResult {
                        status: "failed".to_string(),
                        final_message: None,
                        error: result.error,
                        cost_tracker: child_cost_tracker,
                    };
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_child_agent_for_tui_silent(
    config: &RunConfig,
    cwd: &Path,
    prompt: &str,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    hooks: &HookRunner,
) -> TuiAgentResult {
    let (event_tx, _event_rx) = std::sync::mpsc::channel();
    let (action_tx, action_rx) = std::sync::mpsc::channel();
    drop(action_tx);
    run_child_agent_for_tui(
        config,
        cwd,
        prompt,
        &event_tx,
        &action_rx,
        subagent_depth,
        subagent_type,
        instructions,
        memory,
        hooks,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};
    use orca_core::cost_types::UsageTotals;
    use orca_core::event_schema::EventFactory;
    use orca_core::event_schema::RunStatus;
    use orca_core::model::ModelSelection;

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
    fn runtime_tool_requested_event_maps_to_tui_tool_requested() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let request = tool_types::ToolRequest {
            id: "tool-call-1".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "echo hi" }).to_string()),
        };

        let tui_event =
            tui_event_from_runtime_event(&events.tool_call_requested(&request)).expect("tui event");

        match tui_event {
            TuiEvent::ToolRequested { id, name, target } => {
                assert_eq!(id, "tool-call-1");
                assert_eq!(name, "bash");
                assert_eq!(target, Some("echo hi".to_string()));
            }
            other => panic!("expected tool requested event, got {other:?}"),
        }
    }

    #[test]
    fn runtime_failed_tool_completed_event_maps_error_to_tui_output() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let request = tool_types::ToolRequest {
            id: "tool-call-2".to_string(),
            name: tool_types::ToolName::External("deploy_preview".to_string()),
            action: orca_core::approval_types::ActionKind::Agent,
            target: Some("preview".to_string()),
            raw_arguments: None,
        };
        let result = tool_types::ToolResult::failed(&request, "preview failed", Some(42));

        let tui_event =
            tui_event_from_runtime_event(&events.tool_call_completed(&result)).expect("tui event");

        match tui_event {
            TuiEvent::ToolCompleted {
                id,
                name,
                status,
                output,
                diff,
                kind,
            } => {
                assert_eq!(id, "tool-call-2");
                assert_eq!(name, "deploy_preview");
                assert_eq!(status, "failed");
                assert_eq!(output, "preview failed");
                assert_eq!(diff, None);
                assert_eq!(kind, Some("runtime_error".to_string()));
            }
            other => panic!("expected tool completed event, got {other:?}"),
        }
    }

    #[test]
    fn runtime_assistant_delta_events_map_to_tui_streaming_events() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let reasoning = tui_event_from_runtime_event(&events.assistant_reasoning_delta("thinking"))
            .expect("reasoning event");
        let message = tui_event_from_runtime_event(&events.assistant_message_delta("hello"))
            .expect("message event");

        assert!(matches!(reasoning, TuiEvent::ReasoningDelta(text) if text == "thinking"));
        assert!(matches!(message, TuiEvent::MessageDelta(text) if text == "hello"));
    }

    #[test]
    fn runtime_usage_error_and_completion_events_map_to_tui_events() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let usage = tui_event_from_runtime_event(&events.usage_updated(UsageTotals {
            input_tokens: 10,
            output_tokens: 5,
            cache_tokens: 2,
            estimated_cost_usd: 0.001,
        }))
        .expect("usage event");
        let error = tui_event_from_runtime_event(&events.error("boom")).expect("error event");
        let completed =
            tui_event_from_runtime_event(&events.session_completed(RunStatus::BudgetExhausted))
                .expect("completion event");

        match usage {
            TuiEvent::UsageUpdated(totals) => {
                assert_eq!(totals.input_tokens, 10);
                assert_eq!(totals.output_tokens, 5);
                assert_eq!(totals.cache_tokens, 2);
                assert_eq!(totals.estimated_cost_usd, 0.001);
            }
            other => panic!("expected usage event, got {other:?}"),
        }
        assert!(matches!(error, TuiEvent::Error(message) if message == "boom"));
        assert!(
            matches!(completed, TuiEvent::SessionCompleted { status } if status == "budget_exhausted")
        );
    }

    #[test]
    fn runtime_plan_approval_and_subagent_events_map_to_tui_events() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let plan_update = orca_core::plan_types::UpdatePlanArgs {
            explanation: Some("next steps".to_string()),
            plan: vec![orca_core::plan_types::PlanItem {
                step: "wire adapter".to_string(),
                status: orca_core::plan_types::PlanStatus::InProgress,
            }],
        };
        let approval = orca_core::approval_types::ApprovalRequest {
            id: "approval-1".to_string(),
            action: orca_core::approval_types::ActionKind::Shell,
            description: "run cargo test".to_string(),
            tool: Some("bash".to_string()),
            target: Some("cargo test".to_string()),
            preview: Some("$ cargo test".to_string()),
        };

        let plan =
            tui_event_from_runtime_event(&events.plan_updated(&plan_update)).expect("plan event");
        let approval =
            tui_event_from_runtime_event(&events.approval_requested(&approval)).expect("approval");
        let subagent_started =
            tui_event_from_runtime_event(&events.subagent_started("agent-1", "review code"))
                .expect("subagent started");
        let subagent_completed = tui_event_from_runtime_event(&events.subagent_completed(
            "agent-1",
            "review code",
            RunStatus::Success,
            Some("looks good"),
            None,
        ))
        .expect("subagent completed");

        match plan {
            TuiEvent::PlanUpdated { explanation, plan } => {
                assert_eq!(explanation, Some("next steps".to_string()));
                assert_eq!(plan.len(), 1);
                assert_eq!(plan[0].step, "wire adapter");
                assert_eq!(
                    plan[0].status,
                    orca_core::plan_types::PlanStatus::InProgress
                );
            }
            other => panic!("expected plan event, got {other:?}"),
        }
        assert!(
            matches!(approval, TuiEvent::ApprovalNeeded { id, tool, target, preview }
                if id == "approval-1"
                    && tool == "bash"
                    && target == Some("cargo test".to_string())
                    && preview == Some("$ cargo test".to_string()))
        );
        assert!(
            matches!(subagent_started, TuiEvent::SubagentStarted { id, description }
                if id == "agent-1" && description == "review code")
        );
        assert!(
            matches!(subagent_completed, TuiEvent::SubagentCompleted { id, description, status, output, error }
                if id == "agent-1"
                    && description == "review code"
                    && status == "completed"
                    && output == Some("looks good".to_string())
                    && error.is_none())
        );
    }

    #[test]
    fn runtime_workflow_result_event_maps_to_tui_notification() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());

        let notification = tui_event_from_runtime_event(&events.workflow_result_available(
            "task-1",
            "workflow-run-1",
            "mock-workflow",
            Some("workflow-tool-1"),
            "completed",
            "all phases passed",
        ))
        .expect("workflow notification");

        match notification {
            TuiEvent::WorkflowNotification {
                prompt,
                status,
                summary,
            } => {
                assert_eq!(status, "completed");
                assert_eq!(summary, "mock-workflow: all phases passed");
                assert!(prompt.contains("<task-id>task-1</task-id>"));
                assert!(prompt.contains("<tool-use-id>workflow-tool-1</tool-use-id>"));
                assert!(prompt.contains("<run-id>workflow-run-1</run-id>"));
                assert!(prompt.contains("<status>completed</status>"));
                assert!(prompt.contains("<summary>all phases passed</summary>"));
            }
            other => panic!("expected workflow notification, got {other:?}"),
        }
    }

    #[test]
    fn runtime_workflow_tasks_event_maps_to_tui_tasks_updated() {
        let mut events = EventFactory::new("tui-runtime-adapter".to_string());
        let task = orca_core::task_types::BackgroundTaskSummary {
            id: "task-1".to_string(),
            task_type: orca_core::task_types::TaskType::Workflow,
            status: orca_core::task_types::TaskStatus::Running,
            description: "demo workflow".to_string(),
            created_at_ms: 10,
            started_at_ms: Some(20),
            completed_at_ms: None,
            command: None,
            agent_type: None,
            server: None,
            tool: Some("workflow".to_string()),
            name: Some("demo".to_string()),
            workflow_run_id: Some("workflow-run-1".to_string()),
            phase_count: Some(2),
            workflow_progress: Some(orca_core::task_types::WorkflowTaskProgress {
                total_agents: 3,
                running_agents: 1,
                completed_agents: 2,
                failed_agents: 0,
                completed_phases: 1,
                running_phases: 1,
                failed_phases: 0,
            }),
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: Some("workflow.md".to_string()),
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
        };

        let tui_event = tui_event_from_runtime_event(&events.workflow_tasks_updated(&[task]))
            .expect("workflow tasks updated event");

        match tui_event {
            TuiEvent::WorkflowTasksUpdated { tasks } => {
                assert_eq!(tasks.len(), 1);
                assert_eq!(tasks[0].id, "task-1");
                assert_eq!(tasks[0].workflow_run_id, Some("workflow-run-1".to_string()));
                assert_eq!(
                    tasks[0]
                        .workflow_progress
                        .as_ref()
                        .map(|progress| progress.completed_agents),
                    Some(2)
                );
            }
            other => panic!("expected workflow tasks updated event, got {other:?}"),
        }
    }

    #[test]
    fn tui_session_exposes_runtime_owned_workflow_state() {
        let config = config();
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
    fn tui_workflow_tool_launches_runtime_instead_of_placeholder_executor() {
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
    fn tui_approval_handler_resolves_approve_action_through_runtime_context() {
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::Approve(true))
            .expect("send approval");
        let handler = TuiApprovalHandler::new(&action_rx);
        let mut context = orca_runtime::lifecycle::RuntimeToolActorContext::new("tui-approval", 2);
        let approval = orca_core::approval_types::ApprovalRequest {
            id: "approval-1".to_string(),
            action: orca_core::approval_types::ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        };
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "echo hi" }).to_string()),
        };

        let resolution = context
            .resolve_interactive_tool_approval(&handler, &approval, &request)
            .expect("approval resolution");

        assert_eq!(resolution.id, "approval-1");
        assert_eq!(
            resolution.decision,
            orca_core::approval_types::ApprovalDecision::Allow
        );
        assert_eq!(resolution.reason, "user approved");
    }

    #[test]
    fn tui_approval_handler_maps_cancel_to_runtime_denial() {
        let (action_tx, action_rx) = mpsc::channel();
        action_tx.send(UserAction::Cancel).expect("send cancel");
        let handler = TuiApprovalHandler::new(&action_rx);
        let mut context = orca_runtime::lifecycle::RuntimeToolActorContext::new("tui-approval", 2);
        let approval = orca_core::approval_types::ApprovalRequest {
            id: "approval-1".to_string(),
            action: orca_core::approval_types::ActionKind::Shell,
            description: "bash requested shell".to_string(),
            tool: Some("bash".to_string()),
            target: Some("echo hi".to_string()),
            preview: Some("$ echo hi".to_string()),
        };
        let request = tool_types::ToolRequest {
            id: "bash".to_string(),
            name: tool_types::ToolName::Bash,
            action: orca_core::approval_types::ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: Some(serde_json::json!({ "command": "echo hi" }).to_string()),
        };

        let resolution = context
            .resolve_interactive_tool_approval(&handler, &approval, &request)
            .expect("approval resolution");

        assert_eq!(resolution.id, "approval-1");
        assert_eq!(
            resolution.decision,
            orca_core::approval_types::ApprovalDecision::Deny
        );
        assert_eq!(resolution.reason, "user denied");
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
    fn tui_user_input_handler_routes_answer_through_runtime_context() {
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx
            .send(UserAction::RespondToUserInput("yes".to_string()))
            .expect("send answer");
        let handler = TuiUserInputHandler::new(&event_tx, &action_rx);
        let mut context = RuntimeToolActorContext::new("tui-user-input", 2);
        let request = tool_types::ToolRequest {
            id: "ask".to_string(),
            name: tool_types::ToolName::RequestUserInput,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "question": "Continue?",
                    "choices": ["yes", "no"]
                })
                .to_string(),
            ),
        };

        let result = context
            .execute_user_input_tool(&request, &handler)
            .expect("user input result");
        let events: Vec<TuiEvent> = event_rx.try_iter().collect();

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("yes"));
        assert!(events.iter().any(|event| {
            matches!(
                event,
                TuiEvent::UserInputRequested { id, question, choices }
                if id == "ask"
                    && question == "Continue?"
                    && choices == &vec!["yes".to_string(), "no".to_string()]
            )
        }));
    }

    #[test]
    fn tui_user_input_handler_maps_cancel_to_runtime_failure() {
        let (event_tx, _event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        action_tx.send(UserAction::Cancel).expect("send cancel");
        let handler = TuiUserInputHandler::new(&event_tx, &action_rx);
        let mut context = RuntimeToolActorContext::new("tui-user-input", 2);
        let request = tool_types::ToolRequest {
            id: "ask".to_string(),
            name: tool_types::ToolName::RequestUserInput,
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "question": "Continue?" }).to_string()),
        };

        let result = context
            .execute_user_input_tool(&request, &handler)
            .expect("user input result");

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert_eq!(
            result.error.as_deref(),
            Some("user input request cancelled")
        );
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

        let child = run_child_agent_for_tui_silent(
            &config,
            config.cwd.as_deref().unwrap_or_else(|| Path::new(".")),
            "bad_plan_then_fix",
            1,
            &SubagentType::General,
            &instructions,
            &memory,
            &hooks,
        );

        assert_eq!(child.status, "success");
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
        );

        assert_eq!(results.len(), 2);
        assert!(!results[0].0, "child failure should not stop parent batch");
        assert_eq!(results[0].1.status, tool_types::ToolStatus::Failed);
        assert!(!results[1].0);
        assert_eq!(results[1].1.status, tool_types::ToolStatus::Completed);
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
}
