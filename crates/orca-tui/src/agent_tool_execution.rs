use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use orca_approval::ApprovalPolicy;
use orca_core::approval_types::{ActionKind, ApprovalDecision, ApprovalRequest};
use orca_core::cancel::CancelToken;
use orca_core::config::{PermissionProfileNetworkAccess, RunConfig};
use orca_core::event_schema::EventFactory;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types;
use orca_mcp::McpRegistry;
use orca_runtime::cost::CostTracker;
use orca_runtime::hooks::{HookContext, HookRunner, tool_request_with_hook_outcome};
use orca_runtime::instructions::ProjectInstructions;
use orca_runtime::lifecycle::{
    RuntimePermissionRequest, RuntimeToolActorContext, TurnPermissionOverlay,
};
use orca_runtime::memory::MemoryBlock;
use orca_runtime::protocol::PermissionResponseDecision;
use orca_runtime::runtime_pending_interaction::RuntimePendingInteractionStore;
use orca_runtime::runtime_permission::{
    RuntimePermissionEvaluation, RuntimePermissionOrigin, RuntimePermissionPolicy,
};
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::tool_invocation::{
    apply_pre_tool_outcome, prepare_tool_invocation, validate_tool_invocation,
};

use crate::agent_runner::{
    DEFAULT_MAX_TURNS, send_runtime_event_as_tui, send_task_status_updated_for_tui,
    send_tool_completed_for_tui, send_tool_requested_for_tui, task_summary_for_tui,
};
use crate::agent_subagent_execution::{execute_subagent_for_tui, execute_subagent_status_for_tui};
use crate::agent_workflow_execution::{
    execute_workflow_draft_action_for_tui, execute_workflow_draft_for_tui, execute_workflow_for_tui,
};
use crate::diff;
use crate::runtime_interaction_adapter::{
    AutoAllowPermissionRequests, TuiMcpElicitationHandler, TuiPermissionRequestHandler,
    TuiToolApprovalOutcome, TuiUserInputHandler, resolve_tui_tool_approval,
};
use crate::types::{TuiEvent, UserAction};

pub(crate) fn execute_readonly_batch_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_requests: &[tool_types::ToolRequest],
    event_tx: &Sender<TuiEvent>,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    output_truncation: tool_types::ToolOutputTruncation,
) -> Vec<tool_types::ToolResult> {
    let mut early_results: Vec<Option<tool_types::ToolResult>> = vec![None; tool_requests.len()];
    let mut runnable = Vec::new();
    let mut events = EventFactory::new("tui-readonly-batch".to_string());
    let mut schema_invalid = vec![false; tool_requests.len()];

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        send_tool_requested_for_tui(event_tx, &mut events, tool_request);
        let invocation = prepare_tool_invocation(tool_request, 0, mcp_registry, config);
        if tool_request.raw_arguments.is_some()
            && let Err(error) = validate_tool_invocation(&invocation, mcp_registry, config)
        {
            early_results[idx] = Some(error.into_result());
            schema_invalid[idx] = true;
            continue;
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
                let effective_request = if tool_request.raw_arguments.is_some() {
                    match apply_pre_tool_outcome(invocation, &outcome, mcp_registry, config) {
                        Ok(invocation) => invocation.effective,
                        Err(error) => {
                            early_results[idx] = Some(error.into_result());
                            schema_invalid[idx] = true;
                            continue;
                        }
                    }
                } else {
                    tool_request_with_hook_outcome(tool_request, &outcome)
                };
                runnable.push((idx, effective_request));
            }
            Err(error) => {
                early_results[idx] = Some(tool_types::ToolResult::failed(
                    tool_request,
                    format!("pre_tool_use hook blocked tool: {error}"),
                    None,
                ));
            }
        }
    }

    let runnable_requests = runnable
        .iter()
        .map(|(_, request)| request.clone())
        .collect::<Vec<_>>();
    let dense_runnable = runnable_requests.iter().cloned().enumerate().collect();
    let runnable_results = orca_tools::run_readonly_batch_parallel_with_policy(
        &runnable_requests,
        dense_runnable,
        cwd,
        mcp_registry,
        output_truncation,
    );

    let mut results = early_results;
    for ((original_idx, _), result) in runnable.into_iter().zip(runnable_results) {
        results[original_idx] = Some(result);
    }
    let results = results
        .into_iter()
        .map(|result| result.expect("each read-only batch item has a result"))
        .collect::<Vec<_>>();

    for result in &results {
        send_tool_completed_for_tui(event_tx, &mut events, result, None);
    }

    for ((tool_request, result), schema_invalid) in
        tool_requests.iter().zip(results.iter()).zip(schema_invalid)
    {
        if schema_invalid {
            continue;
        }
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

fn task_stop_target_id(request: &tool_types::ToolRequest) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(request.raw_arguments.as_deref()?).ok()?;
    value
        .get("task_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| value.get("shell_id").and_then(serde_json::Value::as_str))
        .map(str::trim)
        .filter(|task_id| !task_id.is_empty())
        .map(str::to_string)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_tool_for_tui(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<RuntimePendingInteractionStore>,
    subagent_depth: u32,
    session_id: Option<&str>,
    thread_extensions: Option<Arc<orca_runtime::extension::ExtensionData>>,
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    task_registry: Option<&TaskRegistry>,
    permission_overlay: &mut TurnPermissionOverlay,
    cancel: &CancelToken,
) -> (bool, tool_types::ToolResult, Option<CostTracker>) {
    execute_tool_for_tui_inner(
        config,
        cwd,
        tool_request,
        event_tx,
        action_rx,
        pending_actions,
        pending_interactions,
        subagent_depth,
        session_id,
        thread_extensions,
        policy,
        instructions,
        memory,
        mcp_registry,
        hooks,
        task_registry,
        permission_overlay,
        cancel,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_tool_for_tui_inner(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<RuntimePendingInteractionStore>,
    subagent_depth: u32,
    session_id: Option<&str>,
    thread_extensions: Option<Arc<orca_runtime::extension::ExtensionData>>,
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    task_registry: Option<&TaskRegistry>,
    permission_overlay: &mut TurnPermissionOverlay,
    cancel: &CancelToken,
) -> (bool, tool_types::ToolResult, Option<CostTracker>) {
    let invocation = prepare_tool_invocation(tool_request, subagent_depth, mcp_registry, config);
    let mut events = EventFactory::new(
        session_id
            .map(str::to_string)
            .unwrap_or_else(|| "tui-tool-session".to_string()),
    );
    if tool_request.raw_arguments.is_some()
        && let Err(error) = validate_tool_invocation(&invocation, mcp_registry, config)
    {
        send_tool_requested_for_tui(event_tx, &mut events, tool_request);
        let result = error.into_result();
        send_tool_completed_for_tui(event_tx, &mut events, &result, None);
        return (false, result, None);
    }
    let mut runtime_context =
        RuntimeToolActorContext::new(events.run_id().to_string(), DEFAULT_MAX_TURNS);
    // request_permissions is itself an approval prompt (the permission
    // handler asks the user); gating it behind the generic tool approval
    // would double-prompt for the same decision.
    if tool_request.name != tool_types::ToolName::RequestPermissions
        && let TuiToolApprovalOutcome::Denied(result) = resolve_tui_tool_approval(
            &invocation,
            tool_request,
            policy,
            &mut runtime_context,
            event_tx,
            action_rx,
            pending_actions,
            pending_interactions.as_ref(),
        )
    {
        send_tool_requested_for_tui(event_tx, &mut events, tool_request);
        send_tool_completed_for_tui(event_tx, &mut events, &result, None);
        return (true, result, None);
    }

    let mut rendered_diff = None;
    let (result, child_cost) = if tool_request.name == tool_types::ToolName::Subagent {
        let (r, c) = execute_subagent_for_tui(
            config,
            cwd,
            tool_request,
            event_tx,
            action_rx,
            pending_actions,
            pending_interactions.clone(),
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
        let effective_tool_request = if tool_request.raw_arguments.is_some() {
            match apply_pre_tool_outcome(invocation, &pre_tool_outcome, mcp_registry, config) {
                Ok(invocation) => invocation.effective,
                Err(error) => {
                    let result = error.into_result();
                    send_tool_completed_for_tui(event_tx, &mut events, &result, None);
                    return (false, result, None);
                }
            }
        } else {
            tool_request_with_hook_outcome(tool_request, &pre_tool_outcome)
        };
        let execution_request = &effective_tool_request;
        let before = diff::capture_before(execution_request, cwd);
        // Match the runtime path (tool_router.rs): configured extra working
        // directories plus roots granted during this turn.
        let additional_roots = config
            .additional_working_directories
            .iter()
            .map(|directory| directory.path.clone())
            .chain(
                permission_overlay
                    .additional_working_directories()
                    .iter()
                    .cloned(),
            )
            .collect::<Vec<_>>();
        let result = if execution_request.name == tool_types::ToolName::Bash {
            let mut on_output = |chunk: &str| {
                let _ = event_tx.send(TuiEvent::ToolOutputDelta {
                    id: execution_request.id.clone(),
                    chunk: chunk.to_string(),
                });
            };
            match orca_runtime::server::bash_sandbox_for_cwd(config, cwd) {
                Err(error) => tool_types::ToolResult::failed(execution_request, error, None),
                Ok(mut sandbox) => {
                    apply_overlay_network_permissions(&mut sandbox, permission_overlay);
                    execute_tui_bash_with_escalations(
                        TuiBashRunContext {
                            config,
                            request: execution_request,
                            cwd,
                            additional_roots: &additional_roots,
                            sandbox,
                            task_registry,
                            cancel,
                        },
                        policy,
                        permission_overlay,
                        event_tx,
                        action_rx,
                        pending_actions,
                        pending_interactions.as_ref(),
                        &mut on_output,
                    )
                }
            }
        } else if execution_request.name == tool_types::ToolName::RequestPermissions {
            let handler = with_pending_interactions(
                TuiPermissionRequestHandler::new(event_tx, action_rx, pending_actions),
                pending_interactions.as_ref(),
            );
            let result = runtime_context.execute_request_permissions_tool_with_policy(
                execution_request,
                config.approval_mode,
                Some(&handler),
            );
            permission_overlay.merge(runtime_context.permission_overlay());
            result
        } else if execution_request.name == tool_types::ToolName::RequestUserInput {
            execute_user_input_request_for_tui(
                execution_request,
                event_tx,
                action_rx,
                pending_actions,
                pending_interactions.as_ref(),
            )
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
        } else if execution_request.name == tool_types::ToolName::TaskList {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "task_list requires a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            let mut runtime_context =
                RuntimeToolActorContext::new("tui-task-list", DEFAULT_MAX_TURNS);
            runtime_context.execute_task_list_tool(execution_request, task_registry)
        } else if execution_request.name == tool_types::ToolName::TaskStop {
            let Some(task_registry) = task_registry else {
                return (
                    true,
                    tool_types::ToolResult::failed(
                        execution_request,
                        "task_stop requires a main TUI session",
                        None,
                    ),
                    None,
                );
            };
            let target_task_id = task_stop_target_id(execution_request);
            let mut runtime_context =
                RuntimeToolActorContext::new("tui-task-stop", DEFAULT_MAX_TURNS);
            let result = runtime_context.execute_task_stop_tool(execution_request, task_registry);
            if result.status == tool_types::ToolStatus::Completed
                && let Some(task_id) = target_task_id
                && let Some(task) = task_summary_for_tui(task_registry, &task_id)
            {
                let mut events = EventFactory::new(
                    session_id
                        .map(str::to_string)
                        .unwrap_or_else(|| "tui-task-stop".to_string()),
                );
                send_task_status_updated_for_tui(event_tx, &mut events, &task);
            }
            result
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
            let goal_thread_extensions = thread_extensions.clone();
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
                        orca_tools::update_goal::GoalToolOperation::Update(update) => {
                            let Some(thread_extensions) = goal_thread_extensions.as_deref() else {
                                return Err(
                                    "terminal update_goal status requires live runtime thread state"
                                        .to_string(),
                                );
                            };
                            orca_runtime::goals::validate_goal_terminal_update_against_extensions(
                                &update,
                                thread_extensions,
                            )?;
                            store
                                .update(&session_id, update)
                                .map_err(|error| error.to_string())
                        }
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
        } else if matches!(execution_request.name, tool_types::ToolName::Mcp(_)) {
            let handler = match pending_interactions.as_ref() {
                Some(store) => TuiMcpElicitationHandler::new(event_tx, action_rx, pending_actions)
                    .with_pending_interactions(store.clone()),
                None => TuiMcpElicitationHandler::new(event_tx, action_rx, pending_actions),
            };
            orca_tools::execute_with_mcp_external_roots_policy_or_cancel_and_elicitation(
                execution_request,
                cwd,
                &additional_roots,
                mcp_registry,
                &config.external_tools,
                config.tools.output_truncation,
                config.tools.shell_timeout_secs,
                Some(&handler),
                || cancel.is_cancelled(),
            )
        } else {
            orca_tools::execute_with_mcp_external_roots_policy_or_cancel(
                execution_request,
                cwd,
                &additional_roots,
                mcp_registry,
                &config.external_tools,
                config.tools.output_truncation,
                config.tools.shell_timeout_secs,
                || cancel.is_cancelled(),
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

/// Everything needed to (re-)run one bash invocation in the TUI with the
/// profile-derived sandbox.
struct TuiBashRunContext<'a> {
    config: &'a RunConfig,
    request: &'a tool_types::ToolRequest,
    cwd: &'a Path,
    additional_roots: &'a [PathBuf],
    sandbox: orca_runtime::server::CommandExecSandbox,
    task_registry: Option<&'a TaskRegistry>,
    cancel: &'a CancelToken,
}

fn apply_overlay_network_permissions(
    sandbox: &mut orca_runtime::server::CommandExecSandbox,
    permission_overlay: &TurnPermissionOverlay,
) {
    for (domain, access) in permission_overlay.network_domain_permissions() {
        match access {
            PermissionProfileNetworkAccess::Deny => {
                sandbox
                    .network_policy_domains
                    .insert(domain.clone(), *access);
            }
            PermissionProfileNetworkAccess::Allow => {
                sandbox
                    .network_policy_domains
                    .entry(domain.clone())
                    .or_insert(*access);
            }
        }
    }
}

/// Resolve a permission escalation through the approval policy: explicit
/// deny rules refuse it, allow rules and full-auto grant it silently, and
/// everything else prompts the user through the TUI approval channel.
/// Granted permissions merge into the turn overlay.
fn resolve_tui_permission_escalation(
    policy: &ApprovalPolicy,
    permission_overlay: &mut TurnPermissionOverlay,
    approval: &ApprovalRequest,
    permission_request: RuntimePermissionRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<&RuntimePendingInteractionStore>,
) -> std::io::Result<bool> {
    let resolution = policy.resolve_for_tool(
        approval,
        approval.tool.as_deref().unwrap_or_default(),
        approval.target.as_deref(),
    );
    let response = match resolution.decision {
        ApprovalDecision::Deny => return Ok(false),
        ApprovalDecision::Allow => permission_overlay
            .request_and_merge(&AutoAllowPermissionRequests, permission_request)?,
        ApprovalDecision::Ask => {
            let handler = TuiPermissionRequestHandler::new(event_tx, action_rx, pending_actions)
                .with_display(
                    approval.tool.clone().unwrap_or_default(),
                    approval.target.clone(),
                    approval.preview.clone(),
                );
            let handler = with_pending_interactions(handler, pending_interactions);
            permission_overlay.request_and_merge(&handler, permission_request)?
        }
    };
    Ok(response.decision == PermissionResponseDecision::Allow)
}

fn execute_tui_bash_with_escalations(
    mut context: TuiBashRunContext<'_>,
    policy: &ApprovalPolicy,
    permission_overlay: &mut TurnPermissionOverlay,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<&RuntimePendingInteractionStore>,
    on_output: &mut dyn FnMut(&str),
) -> tool_types::ToolResult {
    let (result, network_block) = run_tui_bash(&context, context.additional_roots, on_output);

    // Network escalation (mirrors runtime_bash): a domain the proxy blocked
    // can be granted for the rest of the turn.
    if let Some(block) = network_block {
        let permission_request = match RuntimePermissionPolicy::network_block_evaluation(
            &format!("approval-{}-network", context.request.id),
            RuntimePermissionOrigin::Bash,
            &block,
        ) {
            RuntimePermissionEvaluation::Request(decision) => decision.into_request(),
            RuntimePermissionEvaluation::Deny { reason, .. } => {
                return tool_types::ToolResult::denied(context.request, reason);
            }
        };
        let approval = ApprovalRequest {
            id: format!("approval-{}-network", context.request.id),
            action: ActionKind::Network,
            description: format!("bash requested network access to {}", block.host),
            tool: Some("bash".to_string()),
            target: context.request.target.clone(),
            preview: Some(format!(
                "$ {}\n\nbash attempted network access to {} ({})\n\nApprove to re-run this command with that domain allowed",
                context.request.target.as_deref().unwrap_or_default(),
                block.host,
                block.error
            )),
        };
        let allowed = match resolve_tui_permission_escalation(
            policy,
            permission_overlay,
            &approval,
            permission_request,
            event_tx,
            action_rx,
            pending_actions,
            pending_interactions,
        ) {
            Ok(allowed) => allowed,
            Err(error) => {
                return tool_types::ToolResult::failed(context.request, error.to_string(), None);
            }
        };
        if !allowed {
            return tool_types::ToolResult::denied(
                context.request,
                "permission request denied".to_string(),
            );
        }
        apply_overlay_network_permissions(&mut context.sandbox, permission_overlay);
        return run_tui_bash(&context, context.additional_roots, on_output).0;
    }

    let base_roots = context.additional_roots;
    escalate_sandbox_denied_bash_for_tui(
        result,
        context.request,
        context.cwd,
        policy,
        permission_overlay,
        event_tx,
        action_rx,
        pending_actions,
        pending_interactions,
        context.cancel,
        &mut |retry| match retry {
            TuiBashRetry::WriteRoots(granted) => {
                let mut roots = base_roots.to_vec();
                for root in granted {
                    if !roots.contains(&root) {
                        roots.push(root);
                    }
                }
                run_tui_bash(&context, &roots, on_output).0
            }
            TuiBashRetry::Unsandboxed => {
                let mut unsandboxed = TuiBashRunContext {
                    config: context.config,
                    request: context.request,
                    cwd: context.cwd,
                    additional_roots: context.additional_roots,
                    sandbox: context.sandbox.clone(),
                    task_registry: context.task_registry,
                    cancel: context.cancel,
                };
                unsandboxed.sandbox.mode =
                    orca_runtime::shell_session::ShellSandboxMode::DangerFullAccess;
                unsandboxed.sandbox.additional_readable_roots.clear();
                unsandboxed.sandbox.additional_writable_roots.clear();
                unsandboxed.sandbox.denied_writable_roots.clear();
                unsandboxed.sandbox.allowed_unix_socket_roots.clear();
                unsandboxed.sandbox.network_policy_domains.clear();
                run_tui_bash(&unsandboxed, context.additional_roots, on_output).0
            }
        },
    )
}

/// Run one sandboxed bash invocation: build the command from the sandbox
/// mode (mirroring shell_session's mapping), enforce network domain policy
/// through a local proxy, and register the run in the task registry so
/// `task_list`/`task_stop` can see and cancel it.
fn run_tui_bash(
    context: &TuiBashRunContext<'_>,
    extra_roots: &[PathBuf],
    on_output: &mut dyn FnMut(&str),
) -> (
    tool_types::ToolResult,
    Option<orca_runtime::network_proxy::RuntimeNetworkBlockReport>,
) {
    use orca_runtime::network_proxy::{RuntimeNetworkPolicy, RuntimeNetworkProxy};
    use orca_runtime::shell_session::ShellSandboxMode;

    let request = context.request;
    let Some(command_text) = request
        .target
        .as_deref()
        .filter(|target| !target.is_empty())
    else {
        return (
            tool_types::ToolResult::failed(request, "bash command is required", None),
            None,
        );
    };

    let mut writable_roots = extra_roots.to_vec();
    for root in &context.sandbox.additional_writable_roots {
        if !writable_roots.contains(root) {
            writable_roots.push(root.clone());
        }
    }

    let mut env: Vec<(String, Option<String>)> = Vec::new();
    let mut block_receiver = None;
    let _network_proxy = if context.sandbox.network_policy_domains.is_empty() {
        None
    } else {
        let (sender, receiver) = std::sync::mpsc::channel();
        block_receiver = Some(receiver);
        match RuntimeNetworkProxy::start_with_block_reporter(
            RuntimeNetworkPolicy::new(context.sandbox.network_policy_domains.clone()),
            Some(sender),
        ) {
            Ok(proxy) => {
                for key in [
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                ] {
                    env.push((key.to_string(), Some(proxy.proxy_url().to_string())));
                }
                for key in ["NO_PROXY", "no_proxy"] {
                    env.push((key.to_string(), None));
                }
                Some(proxy)
            }
            Err(error) => {
                return (
                    tool_types::ToolResult::failed(
                        request,
                        format!("failed to start network proxy: {error}"),
                        None,
                    ),
                    None,
                );
            }
        }
    };

    let mut command = match context.sandbox.mode {
        ShellSandboxMode::WorkspaceWrite {
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        } => orca_tools::sandbox::workspace_write_bash_command(
            orca_tools::sandbox::WorkspaceWriteSandboxCommandContext {
                command: command_text,
                cwd: context.cwd,
                readable_roots: &context.sandbox.additional_readable_roots,
                additional_roots: &writable_roots,
                denied_roots: &context.sandbox.denied_writable_roots,
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
                allowed_unix_socket_roots: &context.sandbox.allowed_unix_socket_roots,
            },
        ),
        ShellSandboxMode::ReadOnly {
            network_access,
            allow_global_read,
        } => orca_tools::sandbox::read_only_bash_command(
            orca_tools::sandbox::ReadOnlySandboxCommandContext {
                command: command_text,
                cwd: context.cwd,
                readable_roots: &context.sandbox.additional_readable_roots,
                additional_roots: &writable_roots,
                denied_roots: &context.sandbox.denied_writable_roots,
                network_access,
                allow_global_read,
                allowed_unix_socket_roots: &context.sandbox.allowed_unix_socket_roots,
            },
        ),
        ShellSandboxMode::DangerFullAccess => {
            orca_tools::sandbox::plain_bash_command(command_text, context.cwd)
        }
    };
    for (key, value) in &env {
        match value {
            Some(value) => {
                command.env(key, value);
            }
            None => {
                command.env_remove(key);
            }
        }
    }

    let task_id = context.task_registry.map(|registry| {
        let task = registry.create_shell(command_text.to_string(), command_text.to_string());
        let _ = registry.mark_running(&task.id);
        task.id
    });

    let cancel = context.cancel;
    let registry = context.task_registry;
    let registry_cancelled = || {
        task_id
            .as_deref()
            .zip(registry)
            .is_some_and(|(id, registry)| registry.is_cancelled(id))
    };
    let result = orca_tools::bash::execute_streaming_command_or_cancel(
        request,
        command,
        context.config.tools.output_truncation,
        std::time::Duration::from_secs(context.config.tools.shell_timeout_secs.max(1)),
        on_output,
        || cancel.is_cancelled() || registry_cancelled(),
    );
    if let (Some(registry), Some(task_id)) = (registry, task_id.as_deref()) {
        match result.status {
            tool_types::ToolStatus::Completed => {
                let _ = registry.complete(task_id, result.output.clone().unwrap_or_default());
            }
            _ => {
                let _ = registry.fail(
                    task_id,
                    result
                        .error
                        .clone()
                        .unwrap_or_else(|| "shell command failed".to_string()),
                );
            }
        }
    }
    let network_block = block_receiver.and_then(|receiver| receiver.try_iter().next());
    (result, network_block)
}

/// When a sandboxed bash command fails because the sandbox denied a write to
/// protected workspace metadata (`.git`, `.agents`, `.codex`) or a path
/// outside the workspace, escalate through the approval policy — prompting
/// the user in suggest/auto-edit mode — and re-run the command with the
/// denied write root granted. Approved roots merge into the turn permission
/// overlay so later commands in the same turn don't re-prompt. Anything else
/// passes through unchanged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn escalate_sandbox_denied_bash_for_tui(
    result: tool_types::ToolResult,
    request: &tool_types::ToolRequest,
    cwd: &Path,
    policy: &ApprovalPolicy,
    permission_overlay: &mut TurnPermissionOverlay,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<&RuntimePendingInteractionStore>,
    cancel: &CancelToken,
    retry: &mut dyn FnMut(TuiBashRetry) -> tool_types::ToolResult,
) -> tool_types::ToolResult {
    use orca_runtime::sandbox_denial::{
        diagnose_sandbox_denial, should_request_filesystem_permission_with_denied_roots,
    };

    if result.status != tool_types::ToolStatus::Failed || cancel.is_cancelled() {
        return result;
    }
    let Some(diagnostic) =
        diagnose_sandbox_denial(cwd, "", result.error.as_deref().unwrap_or_default())
    else {
        return result;
    };
    if !should_request_filesystem_permission_with_denied_roots(cwd, &diagnostic, &[])
        && diagnostic.suggested_write_root.is_some()
    {
        return with_sandbox_diagnostic(result, &diagnostic.message);
    }
    if let Some(write_root) = diagnostic.suggested_write_root.clone() {
        let approval = ApprovalRequest {
            id: format!("approval-{}-sandbox", request.id),
            action: ActionKind::Shell,
            description: format!(
                "bash requested sandbox write access to {}",
                write_root.display()
            ),
            tool: Some("bash".to_string()),
            target: request.target.clone(),
            preview: Some(format!(
                "$ {}\n\n{}\n\nApprove to re-run this command with write access to {}",
                request.target.as_deref().unwrap_or_default(),
                diagnostic.message,
                write_root.display()
            )),
        };
        let permission_request = RuntimePermissionPolicy::sandbox_denial_decision(
            &approval.id,
            RuntimePermissionOrigin::Bash,
            &diagnostic,
        )
        .into_request();
        let allowed = match resolve_tui_permission_escalation(
            policy,
            permission_overlay,
            &approval,
            permission_request,
            event_tx,
            action_rx,
            pending_actions,
            pending_interactions,
        ) {
            Ok(allowed) => allowed,
            Err(error) => {
                return tool_types::ToolResult::failed(request, error.to_string(), None);
            }
        };
        if !allowed {
            return with_sandbox_diagnostic(
                result,
                &format!(
                    "{}; write access to {} was not granted",
                    diagnostic.message,
                    write_root.display()
                ),
            );
        }

        let granted = permission_overlay.additional_working_directories().to_vec();
        let retry_result = retry(TuiBashRetry::WriteRoots(granted));
        if retry_result.status == tool_types::ToolStatus::Failed
            && let Some(retry_diagnostic) =
                diagnose_sandbox_denial(cwd, "", retry_result.error.as_deref().unwrap_or_default())
        {
            return with_sandbox_diagnostic(retry_result, &retry_diagnostic.message);
        }
        return retry_result;
    }

    let approval = ApprovalRequest {
        id: format!("approval-{}-unsandboxed", request.id),
        action: ActionKind::Shell,
        description: "bash requested to re-run without the filesystem sandbox".to_string(),
        tool: Some("bash".to_string()),
        target: request.target.clone(),
        preview: Some(format!(
            "$ {}\n\n{}\n\nApprove to re-run this command without the filesystem sandbox",
            request.target.as_deref().unwrap_or_default(),
            diagnostic.message
        )),
    };
    let permission_request = RuntimePermissionPolicy::sandbox_denial_decision(
        &approval.id,
        RuntimePermissionOrigin::Bash,
        &diagnostic,
    )
    .into_request();
    let allowed = match resolve_tui_permission_escalation(
        policy,
        permission_overlay,
        &approval,
        permission_request,
        event_tx,
        action_rx,
        pending_actions,
        pending_interactions,
    ) {
        Ok(allowed) => allowed,
        Err(error) => {
            return tool_types::ToolResult::failed(request, error.to_string(), None);
        }
    };
    if !allowed {
        return with_sandbox_diagnostic(
            result,
            &format!(
                "{}; unsandboxed shell access was not granted",
                diagnostic.message
            ),
        );
    }

    let retry_result = retry(TuiBashRetry::Unsandboxed);
    if retry_result.status == tool_types::ToolStatus::Failed
        && let Some(retry_diagnostic) =
            diagnose_sandbox_denial(cwd, "", retry_result.error.as_deref().unwrap_or_default())
    {
        return with_sandbox_diagnostic(retry_result, &retry_diagnostic.message);
    }
    retry_result
}

pub(crate) enum TuiBashRetry {
    WriteRoots(Vec<PathBuf>),
    Unsandboxed,
}

fn with_sandbox_diagnostic(
    mut result: tool_types::ToolResult,
    message: &str,
) -> tool_types::ToolResult {
    match result.error.as_mut() {
        Some(error) if !error.trim_end().is_empty() => {
            error.push_str(&format!("\n\nSandbox diagnostic: {message}"));
        }
        _ => result.error = Some(message.to_string()),
    }
    result
}

fn execute_user_input_request_for_tui(
    request: &tool_types::ToolRequest,
    event_tx: &Sender<TuiEvent>,
    action_rx: &Receiver<UserAction>,
    pending_actions: &RefCell<VecDeque<UserAction>>,
    pending_interactions: Option<&RuntimePendingInteractionStore>,
) -> tool_types::ToolResult {
    let handler = match pending_interactions {
        Some(store) => TuiUserInputHandler::new(event_tx, action_rx, pending_actions)
            .with_pending_interactions(store.clone()),
        None => TuiUserInputHandler::new(event_tx, action_rx, pending_actions),
    };
    let mut runtime_context = RuntimeToolActorContext::new("tui-user-input", DEFAULT_MAX_TURNS);
    match runtime_context.execute_user_input_tool(request, &handler) {
        Ok(result) => result,
        Err(error) => tool_types::ToolResult::failed(request, error.to_string(), None),
    }
}

fn with_pending_interactions<'a>(
    handler: TuiPermissionRequestHandler<'a>,
    pending_interactions: Option<&RuntimePendingInteractionStore>,
) -> TuiPermissionRequestHandler<'a> {
    match pending_interactions {
        Some(store) => handler.with_pending_interactions(store.clone()),
        None => handler,
    }
}

#[cfg(test)]
pub(crate) fn canonical_action_for_tool(
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

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};
    use orca_core::model::ModelSelection;
    use orca_core::tool_types::{ToolName, ToolStatus};
    use tempfile::TempDir;

    use super::*;

    fn sandbox_test_parent(prefix: &str) -> TempDir {
        #[cfg(target_os = "macos")]
        {
            let home = PathBuf::from(
                std::env::var_os("HOME").expect("HOME is required for macOS Seatbelt tests"),
            )
            .canonicalize()
            .expect("canonical macOS HOME");
            for root in [
                Some(PathBuf::from("/tmp")),
                std::env::var_os("TMPDIR").map(PathBuf::from),
            ]
            .into_iter()
            .flatten()
            {
                let root = root.canonicalize().unwrap_or(root);
                assert!(
                    !home.starts_with(&root),
                    "macOS Seatbelt fixtures require HOME outside temporary allow root {}",
                    root.display()
                );
            }
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(home)
                .expect("sandbox parent outside temporary allow roots")
        }
        #[cfg(not(target_os = "macos"))]
        {
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir()
                .expect("sandbox parent")
        }
    }

    fn config(approval_mode: ApprovalMode) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: std::env::current_dir().ok(),
            output_format: OutputFormat::Text,
            approval_mode,
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

    fn bash_request(command: &str) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            id: "bash-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some(command.to_string()),
            raw_arguments: None,
        }
    }

    fn git_denied_result(
        request: &tool_types::ToolRequest,
        workspace: &Path,
    ) -> tool_types::ToolResult {
        tool_types::ToolResult::failed(
            request,
            format!(
                "fatal: Unable to create '{}': Operation not permitted",
                workspace.join(".git/index.lock").display()
            ),
            Some(128),
        )
    }

    struct EscalationHarness {
        workspace: TempDir,
        event_tx: Sender<TuiEvent>,
        event_rx: mpsc::Receiver<TuiEvent>,
        action_tx: Sender<UserAction>,
        action_rx: Receiver<UserAction>,
        pending_actions: RefCell<VecDeque<UserAction>>,
    }

    impl EscalationHarness {
        fn new() -> Self {
            let workspace = TempDir::new_in(std::env::current_dir().unwrap()).unwrap();
            std::fs::create_dir(workspace.path().join(".git")).unwrap();
            let (event_tx, event_rx) = mpsc::channel();
            let (action_tx, action_rx) = mpsc::channel();
            Self {
                workspace,
                event_tx,
                event_rx,
                action_tx,
                action_rx,
                pending_actions: RefCell::new(VecDeque::new()),
            }
        }

        fn run(
            &self,
            approval_mode: ApprovalMode,
            request: &tool_types::ToolRequest,
            result: tool_types::ToolResult,
        ) -> tool_types::ToolResult {
            self.run_with_overlay(
                approval_mode,
                request,
                result,
                &mut TurnPermissionOverlay::default(),
            )
        }

        fn run_with_overlay(
            &self,
            approval_mode: ApprovalMode,
            request: &tool_types::ToolRequest,
            result: tool_types::ToolResult,
            permission_overlay: &mut TurnPermissionOverlay,
        ) -> tool_types::ToolResult {
            let config = config(approval_mode);
            let policy = ApprovalPolicy::new(approval_mode);
            let cancel = CancelToken::new();
            let cwd = self.workspace.path().to_path_buf();
            escalate_sandbox_denied_bash_for_tui(
                result,
                request,
                &cwd,
                &policy,
                permission_overlay,
                &self.event_tx,
                &self.action_rx,
                &self.pending_actions,
                None,
                &cancel,
                &mut |retry| match retry {
                    TuiBashRetry::WriteRoots(granted) => {
                        orca_tools::bash::execute_streaming_with_policy_roots_or_cancel(
                            request,
                            &cwd,
                            &granted,
                            config.tools.output_truncation,
                            std::time::Duration::from_secs(5),
                            &mut |_chunk| {},
                            || false,
                        )
                    }
                    TuiBashRetry::Unsandboxed => {
                        orca_tools::bash::execute_streaming_command_or_cancel(
                            request,
                            orca_tools::sandbox::plain_bash_command(
                                request.target.as_deref().unwrap_or_default(),
                                &cwd,
                            ),
                            config.tools.output_truncation,
                            std::time::Duration::from_secs(5),
                            &mut |_chunk| {},
                            || false,
                        )
                    }
                },
            )
        }

        fn approval_events(&self) -> Vec<TuiEvent> {
            self.event_rx
                .try_iter()
                .filter(|event| {
                    matches!(
                        event,
                        TuiEvent::ApprovalNeeded { .. } | TuiEvent::PermissionApprovalNeeded { .. }
                    )
                })
                .collect()
        }
    }

    #[test]
    fn sandbox_denied_git_write_escalates_and_retries_after_approval() {
        let harness = EscalationHarness::new();
        let marker = harness.workspace.path().join(".git/escalation-marker");
        let request = bash_request(&format!("printf granted > {}", marker.display()));
        let denied = git_denied_result(&request, harness.workspace.path());
        harness
            .action_tx
            .send(UserAction::Approve {
                id: "approval-bash-1-sandbox".to_string(),
                approved: true,
            })
            .unwrap();

        let result = harness.run(ApprovalMode::Suggest, &request, denied);

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "granted");
        let approvals = harness.approval_events();
        assert_eq!(approvals.len(), 1);
        assert!(approvals.iter().any(|event| matches!(
            event,
            TuiEvent::PermissionApprovalNeeded {
                tool,
                preview,
                permission_kind:
                    orca_runtime::runtime_permission::RuntimePermissionRequestKind::FilesystemWrite,
                ..
            }
            if tool == "bash"
                && preview.as_deref().unwrap_or_default().contains(".git")
        )));
    }

    #[test]
    fn sandbox_denied_git_write_keeps_failure_with_diagnostic_when_user_denies() {
        let harness = EscalationHarness::new();
        let marker = harness.workspace.path().join(".git/escalation-marker");
        let request = bash_request(&format!("printf granted > {}", marker.display()));
        let denied = git_denied_result(&request, harness.workspace.path());
        harness
            .action_tx
            .send(UserAction::Approve {
                id: "approval-bash-1-sandbox".to_string(),
                approved: false,
            })
            .unwrap();

        let result = harness.run(ApprovalMode::Suggest, &request, denied);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            !marker.exists(),
            "denied escalation must not re-run the command"
        );
        let error = result.error.unwrap_or_default();
        assert!(error.contains("Operation not permitted"), "{error}");
        assert!(error.contains("Sandbox diagnostic"), "{error}");
        assert!(error.contains("not granted"), "{error}");
        assert_eq!(harness.approval_events().len(), 1);
    }

    #[test]
    fn full_auto_retries_sandbox_denied_git_write_without_prompting() {
        let harness = EscalationHarness::new();
        let marker = harness.workspace.path().join(".git/escalation-marker");
        let request = bash_request(&format!("printf granted > {}", marker.display()));
        let denied = git_denied_result(&request, harness.workspace.path());

        let result = harness.run(ApprovalMode::FullAuto, &request, denied);

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "granted");
        assert!(harness.approval_events().is_empty());
    }

    #[test]
    fn approved_escalation_persists_write_root_in_turn_overlay() {
        let harness = EscalationHarness::new();
        let marker = harness.workspace.path().join(".git/escalation-marker");
        let request = bash_request(&format!("printf granted > {}", marker.display()));
        let denied = git_denied_result(&request, harness.workspace.path());
        harness
            .action_tx
            .send(UserAction::Approve {
                id: "approval-bash-1-sandbox".to_string(),
                approved: true,
            })
            .unwrap();
        let mut overlay = TurnPermissionOverlay::default();

        let result =
            harness.run_with_overlay(ApprovalMode::Suggest, &request, denied, &mut overlay);

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        let git_root = harness.workspace.path().join(".git");
        assert!(
            overlay.additional_working_directories().contains(&git_root),
            "approved write root must persist for the rest of the turn: {:?}",
            overlay.additional_working_directories()
        );
    }

    #[test]
    fn execute_tool_for_tui_rejects_malformed_subagent_before_task_creation() {
        let workspace = TempDir::new().unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let (_action_tx, action_rx) = mpsc::channel();
        let pending_actions = RefCell::new(VecDeque::new());
        let config = config(ApprovalMode::FullAuto);
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let registry = TaskRegistry::new("session-malformed-single-subagent".to_string());
        let request = tool_types::ToolRequest {
            id: "subagent-malformed-single".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Agent,
            target: None,
            raw_arguments: Some("{\"description\":\"broken".to_string()),
        };
        let mut overlay = TurnPermissionOverlay::default();

        let (should_stop, result, child_cost) = execute_tool_for_tui(
            &config,
            workspace.path(),
            &request,
            &event_tx,
            &action_rx,
            &pending_actions,
            None,
            0,
            None,
            None,
            &policy,
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &HookRunner::default(),
            Some(&registry),
            &mut overlay,
            &CancelToken::new(),
        );

        assert!(!should_stop);
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("arguments are not valid JSON")
        );
        assert!(child_cost.is_none());
        assert!(registry.list().is_empty());
    }

    #[test]
    fn execute_readonly_batch_rejects_malformed_arguments_before_hooks() {
        let workspace = TempDir::new().unwrap();
        let marker = workspace.path().join("hook-ran");
        let hooks = HookRunner::new(vec![orca_core::hook_types::HookConfig {
            event: HookEvent::PreToolUse,
            command: format!("printf ran > {}", marker.display()),
            tool: Some("read_file".to_string()),
        }]);
        let config = config(ApprovalMode::FullAuto);
        let (event_tx, _event_rx) = mpsc::channel();
        let request = tool_types::ToolRequest {
            id: "read-malformed".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{\"path\":\"broken".to_string()),
        };

        let results = execute_readonly_batch_for_tui(
            &config,
            workspace.path(),
            &[request],
            &event_tx,
            &McpRegistry::default(),
            &hooks,
            config.tools.output_truncation,
        );

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].status, ToolStatus::Failed);
        assert!(
            results[0]
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("arguments are not valid JSON")
        );
        assert!(!marker.exists(), "schema-invalid tools must not run hooks");
    }

    #[test]
    fn tui_bash_prompts_and_retries_pathless_sandbox_denial_unsandboxed() {
        if !seatbelt_available_for_tests() {
            return;
        }

        let harness = EscalationHarness::new();
        let outside = sandbox_test_parent("tui-escalation-outside-");
        let marker = outside.path().join("credential-helper-output");
        let request = bash_request(&format!(
            "touch {} 2>/dev/null || {{ printf %s\\\\n \"fatal: could not read Username for 'https://github.com': Operation not permitted\" >&2; exit 128; }}",
            marker.display()
        ));
        harness
            .action_tx
            .send(UserAction::Approve {
                id: "approval-bash-1".to_string(),
                approved: true,
            })
            .unwrap();
        harness
            .action_tx
            .send(UserAction::Approve {
                id: "approval-bash-1-unsandboxed".to_string(),
                approved: true,
            })
            .unwrap();
        let config = config(ApprovalMode::Suggest);
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let mut overlay = TurnPermissionOverlay::default();

        let (_, result, _) = execute_tool_for_tui(
            &config,
            harness.workspace.path(),
            &request,
            &harness.event_tx,
            &harness.action_rx,
            &harness.pending_actions,
            None,
            0,
            None,
            None,
            &policy,
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &orca_runtime::hooks::HookRunner::default(),
            None,
            &mut overlay,
            &CancelToken::new(),
        );

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert!(marker.exists());
        let approvals = harness.approval_events();
        assert!(
            approvals.iter().any(|event| matches!(
                event,
                TuiEvent::PermissionApprovalNeeded {
                    preview,
                    permission_kind:
                        orca_runtime::runtime_permission::RuntimePermissionRequestKind::UnsandboxedShellRetry,
                    ..
                }
                    if preview
                        .as_deref()
                        .is_some_and(|preview| preview.contains("without the filesystem sandbox"))
            )),
            "expected a dedicated unsandboxed retry approval: {approvals:?}"
        );
    }

    #[test]
    fn request_permissions_tool_prompts_and_merges_grant_into_overlay() {
        let harness = EscalationHarness::new();
        let write_root = harness.workspace.path().join(".git");
        let request = tool_types::ToolRequest {
            id: "perm-1".to_string(),
            name: ToolName::RequestPermissions,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "reason": "need to stage the merge",
                    "permissions": {
                        "fileSystem": { "write": [write_root.display().to_string()] }
                    }
                })
                .to_string(),
            ),
        };
        harness
            .action_tx
            .send(UserAction::Approve {
                id: "perm-1".to_string(),
                approved: true,
            })
            .unwrap();
        let config = config(ApprovalMode::Suggest);
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let mut overlay = TurnPermissionOverlay::default();

        let (should_stop, result, _) = execute_tool_for_tui(
            &config,
            harness.workspace.path(),
            &request,
            &harness.event_tx,
            &harness.action_rx,
            &harness.pending_actions,
            None,
            0,
            None,
            None,
            &policy,
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &orca_runtime::hooks::HookRunner::default(),
            None,
            &mut overlay,
            &CancelToken::new(),
        );

        assert!(!should_stop);
        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert!(
            overlay
                .additional_working_directories()
                .contains(&write_root),
            "granted root must merge into the turn overlay: {:?}",
            overlay.additional_working_directories()
        );
        assert_eq!(harness.approval_events().len(), 1);
    }

    #[test]
    fn request_permissions_tool_denied_by_user_grants_nothing() {
        let harness = EscalationHarness::new();
        let request = tool_types::ToolRequest {
            id: "perm-1".to_string(),
            name: ToolName::RequestPermissions,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                serde_json::json!({
                    "reason": "need broad access",
                    "permissions": {
                        "fileSystem": { "write": ["/"] }
                    }
                })
                .to_string(),
            ),
        };
        harness
            .action_tx
            .send(UserAction::Approve {
                id: "perm-1".to_string(),
                approved: false,
            })
            .unwrap();
        let config = config(ApprovalMode::Suggest);
        let policy = ApprovalPolicy::new(ApprovalMode::Suggest);
        let mut overlay = TurnPermissionOverlay::default();

        let (_, result, _) = execute_tool_for_tui(
            &config,
            harness.workspace.path(),
            &request,
            &harness.event_tx,
            &harness.action_rx,
            &harness.pending_actions,
            None,
            0,
            None,
            None,
            &policy,
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &orca_runtime::hooks::HookRunner::default(),
            None,
            &mut overlay,
            &CancelToken::new(),
        );

        assert_ne!(result.status, ToolStatus::Completed);
        assert!(overlay.additional_working_directories().is_empty());
    }

    #[test]
    fn execute_tool_for_tui_tracks_runtime_pending_user_input_until_answered() {
        let config = config(ApprovalMode::Suggest);
        let cwd = config.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let store = RuntimePendingInteractionStore::default();
        let request = tool_types::ToolRequest {
            id: "ask-1".to_string(),
            name: ToolName::RequestUserInput,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "question": "Continue?" }).to_string()),
        };
        let worker_store = store.clone();

        let handle = std::thread::spawn(move || {
            let pending_actions = RefCell::new(VecDeque::new());
            let mut overlay = TurnPermissionOverlay::default();
            execute_tool_for_tui(
                &config,
                &cwd,
                &request,
                &event_tx,
                &action_rx,
                &pending_actions,
                Some(worker_store),
                0,
                None,
                None,
                &ApprovalPolicy::new(ApprovalMode::Suggest),
                &ProjectInstructions::default(),
                &MemoryBlock::default(),
                &McpRegistry::default(),
                &orca_runtime::hooks::HookRunner::default(),
                None,
                &mut overlay,
                &CancelToken::new(),
            )
        });

        let prompt = loop {
            let event = event_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("user input prompt");
            if matches!(event, TuiEvent::UserInputRequested { .. }) {
                break event;
            }
        };
        assert!(matches!(
            prompt,
            TuiEvent::UserInputRequested { id, .. } if id == "ask-1"
        ));
        assert_eq!(
            store.get("ask-1").map(|record| record.kind),
            Some(
                orca_runtime::runtime_pending_interaction::RuntimePendingInteractionKind::UserInput
            )
        );

        action_tx
            .send(UserAction::RespondToUserInput {
                id: "ask-1".to_string(),
                answer: "yes".to_string(),
            })
            .expect("send answer");
        let (should_stop, result, _) = handle.join().expect("executor thread");

        assert!(!should_stop);
        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("yes"));
        assert!(store.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn execute_tool_for_tui_routes_mcp_elicitation_through_pending_interactions() {
        let workspace = TempDir::new().unwrap();
        let server = workspace.path().join("elicitation_mcp_server.sh");
        std::fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"elicits","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"authorize","description":"needs user input","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":"prompt-1","method":"elicitation/create","params":{"message":"Authorize GitHub","url":"https://github.com/login/device","elicitationId":"device-flow"}}\n'
      IFS= read -r response
      case "$response" in
        *'"id":"prompt-1"'*'"action":"accept"'*'"code":"1234"'*)
          printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"authorized"}],"isError":false}}\n'
          ;;
        *)
          printf '{"jsonrpc":"2.0","id":3,"error":{"code":-32000,"message":"missing elicitation response"}}\n'
          ;;
      esac
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }
        let mcp_registry =
            orca_mcp::initialize_registry(&[orca_core::mcp_types::McpServerConfig {
                name: "elicits".to_string(),
                transport: orca_core::mcp_types::McpTransportKind::Stdio,
                command: Some("/bin/sh".to_string()),
                args: vec![server.to_string_lossy().into_owned()],
                url: None,
                env: Default::default(),
                headers: Default::default(),
                disabled: false,
                startup_timeout_ms: Some(15_000),
                tool_timeout_ms: Some(1000),
            }]);
        assert!(
            mcp_registry.errors().is_empty(),
            "{:?}",
            mcp_registry.errors()
        );
        let config = config(ApprovalMode::FullAuto);
        let request = tool_types::ToolRequest {
            id: "mcp-1".to_string(),
            name: ToolName::Mcp("mcp__elicits__authorize".to_string()),
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({}).to_string()),
        };
        let (event_tx, event_rx) = mpsc::channel();
        let (action_tx, action_rx) = mpsc::channel();
        let store = RuntimePendingInteractionStore::default();
        let worker_store = store.clone();

        let handle = std::thread::spawn(move || {
            let pending_actions = RefCell::new(VecDeque::new());
            let mut overlay = TurnPermissionOverlay::default();
            execute_tool_for_tui(
                &config,
                workspace.path(),
                &request,
                &event_tx,
                &action_rx,
                &pending_actions,
                Some(worker_store),
                0,
                None,
                None,
                &ApprovalPolicy::new(ApprovalMode::FullAuto),
                &ProjectInstructions::default(),
                &MemoryBlock::default(),
                &mcp_registry,
                &orca_runtime::hooks::HookRunner::default(),
                None,
                &mut overlay,
                &CancelToken::new(),
            )
        });

        let prompt = loop {
            let event = event_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("mcp elicitation prompt");
            if matches!(event, TuiEvent::McpElicitationRequested { .. }) {
                break event;
            }
        };
        let request_id = match prompt {
            TuiEvent::McpElicitationRequested {
                id,
                server_name,
                message,
                url,
                ..
            } => {
                assert_eq!(server_name, "elicits");
                assert_eq!(message, "Authorize GitHub");
                assert_eq!(url.as_deref(), Some("https://github.com/login/device"));
                id
            }
            other => panic!("unexpected event: {other:?}"),
        };
        assert_eq!(
            store.get(&request_id).map(|record| record.kind),
            Some(
                orca_runtime::runtime_pending_interaction::RuntimePendingInteractionKind::McpElicitation
            )
        );
        action_tx
            .send(UserAction::RespondToMcpElicitation {
                id: request_id,
                accepted: true,
                content_json: Some(serde_json::json!({"code":"1234"}).to_string()),
            })
            .expect("send mcp elicitation response");

        let (should_stop, result, _) = handle.join().expect("executor thread");

        assert!(!should_stop);
        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        assert_eq!(result.output.as_deref(), Some("authorized"));
        assert!(store.is_empty());
    }

    fn seatbelt_available_for_tests() -> bool {
        std::process::Command::new("sandbox-exec")
            .arg("-p")
            .arg("(version 1) (allow default)")
            .arg("true")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn read_only_permission_profile_blocks_workspace_writes_in_tui_bash() {
        if !seatbelt_available_for_tests() {
            return;
        }
        let harness = EscalationHarness::new();
        let target = harness.workspace.path().join("blocked.txt");
        let request = bash_request(&format!("printf x > {}", target.display()));
        let mut config = config(ApprovalMode::FullAuto);
        config.active_permission_profile = Some(orca_core::config::ActivePermissionProfile {
            id: "read-only".to_string(),
            extends: None,
        });
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let mut overlay = TurnPermissionOverlay::default();

        let (_, result, _) = execute_tool_for_tui(
            &config,
            harness.workspace.path(),
            &request,
            &harness.event_tx,
            &harness.action_rx,
            &harness.pending_actions,
            None,
            0,
            None,
            None,
            &policy,
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &orca_runtime::hooks::HookRunner::default(),
            None,
            &mut overlay,
            &CancelToken::new(),
        );

        assert_ne!(
            result.status,
            ToolStatus::Completed,
            "read-only profile must deny workspace writes in the TUI bash path"
        );
        assert!(!target.exists());
    }

    #[test]
    fn tui_bash_registers_shell_task_for_task_list() {
        use orca_core::task_types::{TaskStatus, TaskType};

        let harness = EscalationHarness::new();
        let request = bash_request("printf ok");
        let config = config(ApprovalMode::FullAuto);
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let registry = TaskRegistry::new("tui-bash-tasks".to_string());
        let mut overlay = TurnPermissionOverlay::default();

        let (_, result, _) = execute_tool_for_tui(
            &config,
            harness.workspace.path(),
            &request,
            &harness.event_tx,
            &harness.action_rx,
            &harness.pending_actions,
            None,
            0,
            None,
            None,
            &policy,
            &ProjectInstructions::default(),
            &MemoryBlock::default(),
            &McpRegistry::default(),
            &orca_runtime::hooks::HookRunner::default(),
            Some(&registry),
            &mut overlay,
            &CancelToken::new(),
        );

        assert_eq!(result.status, ToolStatus::Completed, "{:?}", result.error);
        let tasks = registry.list();
        assert!(
            tasks.iter().any(|task| task.task_type == TaskType::Shell
                && task.status == TaskStatus::Completed
                && task.command.as_deref() == Some("printf ok")),
            "bash run must be visible to task_list: {tasks:?}"
        );
    }

    #[test]
    fn non_sandbox_failures_pass_through_untouched() {
        let harness = EscalationHarness::new();
        let request = bash_request("gitx status");
        let failed =
            tool_types::ToolResult::failed(&request, "sh: gitx: command not found", Some(127));

        let result = harness.run(ApprovalMode::Suggest, &request, failed.clone());

        assert_eq!(result.status, ToolStatus::Failed);
        assert_eq!(result.error, failed.error);
        assert!(harness.approval_events().is_empty());
    }

    #[test]
    fn workspace_internal_denial_gets_diagnostic_but_no_prompt() {
        let harness = EscalationHarness::new();
        let blocked = harness.workspace.path().join("blocked.txt");
        let request = bash_request(&format!("printf x > {}", blocked.display()));
        let failed = tool_types::ToolResult::failed(
            &request,
            format!("sh: {}: Operation not permitted", blocked.display()),
            Some(1),
        );

        let result = harness.run(ApprovalMode::Suggest, &request, failed);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Sandbox diagnostic"),
            "{:?}",
            result.error
        );
        assert!(harness.approval_events().is_empty());
    }
}
