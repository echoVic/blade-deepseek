use orca_approval::ApprovalPolicy;
use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::{ActionKind, ApprovalDecision, ApprovalMode, ApprovalRequest};
use orca_core::config::{
    HistoryMode, OutputFormat, PermissionProfileNetworkAccess, ProviderKind, RunConfig, ThemeName,
    ToolConfig, WorkflowConfig,
};
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::hook_types::{HookConfig, HookEvent};
use orca_core::model::{ModelSelection, PRO_MODEL};
use orca_core::provider_types::{ProviderStep, Usage};
use orca_core::subagent_config::SubagentConfig;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::{TaskStatus, TaskType};
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_runtime::cost::CostTracker;
use orca_runtime::hooks::HookRunner;
use orca_runtime::lifecycle::{
    RuntimeApprovalDecision, RuntimeApprovalHandler, RuntimePermissionRequest,
    RuntimePermissionRequestHandler, RuntimePermissionResponse, RuntimeSessionLifecycle,
    RuntimeSpecialToolDispatch, RuntimeSubagentStatusLookup, RuntimeSubagentStatusRecord,
    RuntimeTaskActor, RuntimeTaskKind, RuntimeTaskStatus, RuntimeToolActorContext,
    RuntimeTurnRunner, RuntimeUserInputHandler, RuntimeUserInputRequest,
    RuntimeWorkflowDraftRequest, RuntimeWorkflowIpc, TurnPermissionOverlay,
};
use orca_runtime::protocol::{
    PermissionGrantScope, PermissionResponseDecision, RequestFileSystemPermissions,
    RequestNetworkPermissions, RequestPermissionProfile,
};
use orca_runtime::tasks::TaskRegistry;
use serde_json::Value;

#[test]
fn agent_loop_module_does_not_depend_on_controller() {
    let agent_loop = std::fs::read_to_string("crates/orca-runtime/src/agent_loop.rs")
        .expect("agent loop source");

    assert!(
        !agent_loop.contains("crate::controller"),
        "agent_loop must own child-agent execution instead of delegating back to controller"
    );
}

#[test]
fn controller_does_not_own_async_subagent_worker_entrypoint() {
    let controller = std::fs::read_to_string("crates/orca-runtime/src/controller.rs")
        .expect("controller source");

    assert!(
        !controller.contains("run_async_subagent_worker"),
        "async subagent worker entrypoint should live with subagent execution, not controller"
    );
}

#[test]
fn tool_execution_does_not_call_interactive_approval_resolution_directly() {
    let tool_execution = std::fs::read_to_string("crates/orca-runtime/src/tool_execution.rs")
        .expect("tool execution source");

    assert!(
        !tool_execution.contains("approval_resolution::resolve_interactive")
            && !tool_execution.contains("resolve_interactive("),
        "tool execution should route interactive approvals through the runtime actor context"
    );
}

#[test]
fn turn_iteration_step_uses_grouped_runtime_input() {
    let lifecycle =
        std::fs::read_to_string("crates/orca-runtime/src/lifecycle.rs").expect("lifecycle source");

    assert!(
        lifecycle.contains("struct RuntimeTurnIterationInput"),
        "turn iteration should have a grouped input boundary before more runtime-loop refactors"
    );

    let run_impl = lifecycle
        .split("impl RuntimeTurnIterationStep")
        .nth(1)
        .and_then(|text| text.split("impl RuntimeTurnLoopStep").next())
        .expect("runtime turn iteration impl block");
    assert!(
        run_impl.contains("input: RuntimeTurnIterationInput"),
        "turn iteration run should take grouped input instead of a long flat parameter list"
    );
    assert!(
        !run_impl.contains("provider: ProviderKind"),
        "provider should be carried by RuntimeTurnIterationInput"
    );
    assert!(
        !run_impl.contains("background_workflows: &mut Vec<BackgroundWorkflowRun>"),
        "mutable workflow state should be carried by RuntimeTurnIterationInput"
    );
}

#[test]
fn provider_cycle_step_uses_grouped_runtime_input() {
    let provider_turn = std::fs::read_to_string("crates/orca-runtime/src/provider_turn.rs")
        .expect("provider turn source");

    assert!(
        provider_turn.contains("struct RuntimeProviderCycleInput"),
        "provider cycle should have a grouped input boundary before deeper response/tool-turn refactors"
    );

    let run_impl = provider_turn
        .split("impl RuntimeTurnProviderCycleStep")
        .nth(1)
        .and_then(|text| text.split("fn cancelled_provider_turn").next())
        .expect("runtime turn provider cycle impl block");
    assert!(
        run_impl.contains("input: RuntimeProviderCycleInput"),
        "provider cycle run should take grouped input instead of a long flat parameter list"
    );
    assert!(
        !run_impl.contains("provider: ProviderKind"),
        "provider should be carried by RuntimeProviderCycleInput"
    );
    assert!(
        !run_impl.contains("background_workflows: &mut Vec<BackgroundWorkflowRun>"),
        "mutable workflow state should be carried by RuntimeProviderCycleInput"
    );
}

#[test]
fn provider_response_and_tool_turn_share_runtime_step_context() {
    let lib =
        std::fs::read_to_string("crates/orca-runtime/src/lib.rs").expect("runtime lib source");
    let step_context = std::fs::read_to_string("crates/orca-runtime/src/step_context.rs")
        .expect("runtime step context source");
    let provider_turn = std::fs::read_to_string("crates/orca-runtime/src/provider_turn.rs")
        .expect("provider turn source");
    let tool_turn =
        std::fs::read_to_string("crates/orca-runtime/src/tool_turn.rs").expect("tool turn source");

    assert!(
        lib.contains("mod step_context;"),
        "runtime crate should own a focused request-scoped step context module"
    );
    assert!(
        step_context.contains("pub(crate) struct RuntimeStepContext"),
        "runtime step context should group request-scoped runtime inputs"
    );

    let provider_response_impl = provider_turn
        .split("impl RuntimeProviderResponseStep")
        .nth(1)
        .and_then(|text| text.split("impl RuntimeProviderResponseResultStep").next())
        .expect("runtime provider response impl block");
    assert!(
        provider_response_impl.contains("step_context: RuntimeStepContext"),
        "provider response handling should consume RuntimeStepContext instead of repeating request state"
    );
    assert!(
        !provider_response_impl.contains("tool_policy: AgentToolPolicyContext"),
        "tool policy should be carried by RuntimeStepContext"
    );
    assert!(
        !provider_response_impl.contains("mcp_registry: &McpRegistry"),
        "MCP registry should be carried by RuntimeStepContext"
    );

    let run_tool_turns_signature = tool_turn
        .split("pub(crate) fn run_tool_turns")
        .nth(1)
        .and_then(|text| text.split(") -> io::Result<ToolTurnOutcome>").next())
        .expect("run_tool_turns signature");
    assert!(
        run_tool_turns_signature.contains("step_context: RuntimeStepContext"),
        "tool turns should share the provider response step context"
    );
    assert!(
        !run_tool_turns_signature.contains("tool_policy: AgentToolPolicyContext"),
        "tool policy should not be a separate run_tool_turns argument"
    );
    assert!(
        !run_tool_turns_signature.contains("hooks: &HookRunner"),
        "hooks should not be a separate run_tool_turns argument"
    );
}

#[test]
fn runtime_tool_router_owns_tool_invocation_dispatch_boundary() {
    let lib =
        std::fs::read_to_string("crates/orca-runtime/src/lib.rs").expect("runtime lib source");
    let tool_execution = std::fs::read_to_string("crates/orca-runtime/src/tool_execution.rs")
        .expect("tool execution source");
    let tool_router = std::fs::read_to_string("crates/orca-runtime/src/tool_router.rs")
        .expect("runtime tool router source");

    assert!(
        lib.contains("mod tool_router;"),
        "orca-runtime should register a focused runtime tool-router module"
    );
    assert!(
        tool_router.contains("pub(crate) struct RuntimeToolRouter"),
        "tool_router should own the runtime tool dispatch router"
    );
    assert!(
        tool_router.contains("pub(crate) struct RuntimeToolInvocationContext"),
        "tool_router should group dispatch-time tool invocation state"
    );
    assert!(
        tool_router.contains("RuntimeSpecialToolDispatch"),
        "tool_router should classify runtime special tool dispatch"
    );
    assert!(
        tool_router.contains("RuntimeNormalToolInvocation")
            && tool_router.contains("execute_normal_tool_invocation"),
        "tool_router should route normal tool execution through the invocation-object entrypoint"
    );
    assert!(
        tool_execution.contains("RuntimeToolRouter::new"),
        "tool execution actor should delegate dispatch through RuntimeToolRouter"
    );
    assert!(
        !tool_execution.contains("pub(crate) fn dispatch_tool"),
        "tool execution actor should not own the large runtime tool dispatch method"
    );
}

#[test]
fn runtime_normal_tool_executor_owns_normal_tool_execution_boundary() {
    let lib =
        std::fs::read_to_string("crates/orca-runtime/src/lib.rs").expect("runtime lib source");
    let lifecycle =
        std::fs::read_to_string("crates/orca-runtime/src/lifecycle.rs").expect("lifecycle source");
    let normal_tool = std::fs::read_to_string("crates/orca-runtime/src/runtime_normal_tool.rs")
        .expect("runtime normal tool source");

    assert!(
        lib.contains("mod runtime_normal_tool;"),
        "orca-runtime should register a focused normal-tool execution module"
    );
    assert!(
        normal_tool.contains("pub(crate) struct RuntimeNormalToolExecutor"),
        "runtime_normal_tool should own the normal tool executor boundary"
    );
    assert!(
        normal_tool.contains("pub(crate) struct RuntimeNormalToolExecutionContext"),
        "runtime_normal_tool should group normal tool execution state"
    );
    assert!(
        normal_tool.contains("pub(crate) struct RuntimeNormalToolInvocation"),
        "runtime_normal_tool should expose a smaller invocation object for lifecycle/router callers"
    );
    assert!(
        normal_tool.contains("pub(crate) trait RuntimeNormalToolFallbackExecutor"),
        "runtime_normal_tool should expose a focused fallback executor boundary"
    );
    assert!(
        normal_tool.contains("pub(crate) struct RuntimeNormalToolFallbackContext"),
        "runtime_normal_tool should group fallback executor state"
    );
    assert!(
        normal_tool.contains("DefaultRuntimeNormalToolFallbackExecutor"),
        "runtime_normal_tool should keep the orca-tools fallback behind a default executor"
    );
    assert!(
        normal_tool.contains("pub(crate) fn execute_runtime_normal_tool"),
        "runtime_normal_tool should expose one helper for normal tool invocations"
    );
    assert!(
        normal_tool.contains("execute_bash_with_shell_session"),
        "runtime_normal_tool should own the shell-session bash execution branch"
    );
    assert!(
        normal_tool.contains("execute_with_mcp_external_roots_policy_or_cancel"),
        "runtime_normal_tool should own the fallback to the orca-tools executor"
    );
    assert!(
        lifecycle.contains("execute_runtime_normal_tool"),
        "lifecycle should delegate normal tool execution through the runtime_normal_tool invocation helper"
    );
    assert!(
        lifecycle.contains("pub(crate) fn execute_normal_tool_invocation"),
        "lifecycle actors should expose one invocation-object entrypoint for normal tools"
    );
    assert!(
        tool_router_uses_normal_tool_invocation(),
        "tool_router should route normal tools through RuntimeNormalToolInvocation instead of the long roots/cancel method"
    );
    assert!(
        !lifecycle.contains("RuntimeNormalToolExecutor::new"),
        "lifecycle should not instantiate the normal tool executor directly"
    );
    assert!(
        !lifecycle.contains("execute_bash_with_shell_session("),
        "lifecycle should not directly own the shell-session bash execution branch"
    );
    assert!(
        !lifecycle.contains("orca_tools::execute_with_mcp_external_roots_policy_or_cancel"),
        "lifecycle should not directly invoke the normal tool fallback executor"
    );
}

fn tool_router_uses_normal_tool_invocation() -> bool {
    let tool_router = std::fs::read_to_string("crates/orca-runtime/src/tool_router.rs")
        .expect("runtime tool router source");
    tool_router.contains("RuntimeNormalToolInvocation")
        && tool_router.contains("execute_normal_tool_invocation")
        && !tool_router.contains("execute_normal_tool_with_roots_and_cancel")
}

#[test]
fn session_lifecycle_assigns_agent_task_and_monotonic_turns() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    let task = lifecycle.start_task(RuntimeTaskKind::Agent);

    assert_eq!(task.id(), "run-test:task-1");
    assert_eq!(task.kind(), RuntimeTaskKind::Agent);
    assert_eq!(task.status(), RuntimeTaskStatus::Running);

    let first = lifecycle.next_turn();
    let second = lifecycle.next_turn();

    assert_eq!(first.number(), 1);
    assert_eq!(second.number(), 2);
    assert_eq!(lifecycle.active_task().unwrap().current_turn(), 2);
}

#[test]
fn turn_started_event_carries_task_lifecycle_payload() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let turn = lifecycle.next_turn();
    let task = lifecycle.active_task().unwrap();
    let mut events = EventFactory::new(lifecycle.run_id().to_string());

    let event = turn.started_event(&mut events, Some("hello"), task);

    assert_eq!(event.payload["turn"], 1);
    assert_eq!(event.payload["prompt"], "hello");
    assert_eq!(event.payload["task"]["task_id"], "run-test:task-1");
    assert_eq!(event.payload["task"]["kind"], "agent");
    assert_eq!(event.payload["task"]["status"], "running");
}

#[test]
fn turn_runner_advances_lifecycle_and_builds_started_event() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut events = EventFactory::new(lifecycle.run_id().to_string());
    let mut runner = RuntimeTurnRunner::new(&mut lifecycle);

    let started = runner.start_turn(&mut events, Some("hello"));

    assert_eq!(started.turn(), 1);
    assert_eq!(started.event.payload["turn"], 1);
    assert_eq!(started.event.payload["prompt"], "hello");
    assert_eq!(started.event.payload["task"]["kind"], "agent");
    assert_eq!(started.event.payload["task"]["status"], "running");
    assert_eq!(started.event.payload["task"]["turn"], 1);
}

#[test]
fn turn_runner_advances_lifecycle_without_emitting_event() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    lifecycle.start_task(RuntimeTaskKind::Subagent);
    let mut runner = RuntimeTurnRunner::new(&mut lifecycle);

    let advanced = runner.advance_turn();

    assert_eq!(advanced.turn(), 1);
    let task = advanced.task().expect("task snapshot");
    assert_eq!(task.kind(), RuntimeTaskKind::Subagent);
    assert_eq!(task.current_turn(), 1);
}

#[test]
fn finish_task_maps_run_status_to_lifecycle_status() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-test");
    lifecycle.start_task(RuntimeTaskKind::Agent);

    let task = lifecycle.finish_task(RunStatus::BudgetExhausted).unwrap();

    assert_eq!(task.status(), RuntimeTaskStatus::BudgetExhausted);
    assert_eq!(task.payload()["status"], "budget_exhausted");
}

#[test]
fn shell_task_snapshot_serializes_lifecycle_payload() {
    let task = orca_runtime::lifecycle::RuntimeTaskLifecycle::new_snapshot(
        "shell-call-1:task-1",
        RuntimeTaskKind::Shell,
        RuntimeTaskStatus::Succeeded,
        1,
    );

    assert_eq!(task.payload()["task_id"], "shell-call-1:task-1");
    assert_eq!(task.payload()["kind"], "shell");
    assert_eq!(task.payload()["status"], "succeeded");
    assert_eq!(task.payload()["turn"], 1);
}

#[test]
fn task_actor_starts_turns_and_enforces_turn_budget() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 1);
    let mut events = EventFactory::new("run-actor".to_string());

    let first = actor
        .start_turn(&mut events, Some("hello"), true)
        .expect("first turn");

    assert_eq!(first.turn(), 1);
    let event = first.event().expect("emitted event");
    assert_eq!(event.payload["turn"], 1);
    assert_eq!(event.payload["task"]["task_id"], "run-actor:task-1");
    assert_eq!(event.payload["task"]["kind"], "agent");

    let exhausted = actor
        .start_turn(&mut events, None, true)
        .expect_err("turn budget exhausted");
    assert_eq!(exhausted.status, RunStatus::BudgetExhausted);
    assert_eq!(exhausted.message, "max turns exhausted");
}

#[test]
fn task_actor_enforces_turn_budget_from_existing_lifecycle_state() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    lifecycle.next_turn();
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 1);
    let mut events = EventFactory::new("run-actor".to_string());

    let exhausted = actor
        .start_turn(&mut events, Some("second turn"), true)
        .expect_err("turn budget exhausted");

    assert_eq!(exhausted.status, RunStatus::BudgetExhausted);
    assert_eq!(exhausted.message, "max turns exhausted");
    assert_eq!(actor.active_task().expect("task").current_turn(), 1);
}

#[test]
fn task_actor_advances_turn_without_emitting_event() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let mut events = EventFactory::new("run-actor".to_string());

    let first = actor
        .start_turn(&mut events, Some("hello"), false)
        .expect("first turn");

    assert_eq!(first.turn(), 1);
    assert!(first.event().is_none());
    assert_eq!(actor.active_task().expect("task").payload()["turn"], 1);
}

#[test]
fn task_actor_routes_model_turn_and_updates_cost_model() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let mut cost_tracker = CostTracker::new(Some("deepseek-v4-flash"));
    let provider_config = ProviderConfig {
        api_key: None,
        base_url: None,
        model: None,
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        tools_override: None,
        mcp_registry: None,
        external_tools: Vec::new(),
    };

    let routed = actor.route_model_turn(
        &ModelSelection::from_unchecked(Some("auto".to_string())),
        &SubagentType::General,
        None,
        &provider_config,
        &mut cost_tracker,
    );

    assert_eq!(routed.decision.actual_model, PRO_MODEL);
    assert_eq!(routed.provider_config.model.as_deref(), Some(PRO_MODEL));
    let totals = cost_tracker.add_usage(Usage {
        input_tokens: 100,
        output_tokens: 50,
        cache_tokens: 0,
    });
    let expected_pro_cost = (100.0 * 0.435 + 50.0 * 0.87) / 1_000_000.0;
    assert!((totals.estimated_cost_usd - expected_pro_cost).abs() < 1e-12);
}

#[test]
fn task_actor_records_usage_and_reports_budget_exhaustion() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let mut cost_tracker = CostTracker::new(Some(PRO_MODEL));

    let exhausted = actor
        .record_usage(
            Usage {
                input_tokens: 1_000_000,
                output_tokens: 1_000_000,
                cache_tokens: 0,
            },
            &mut cost_tracker,
            Some(0.000001),
        )
        .expect_err("budget exhausted");

    assert_eq!(exhausted.status, RunStatus::BudgetExhausted);
    assert!(exhausted.message.contains("budget exhausted"));
    assert_eq!(cost_tracker.totals().total_tokens(), 2_000_000);
}

#[test]
fn task_actor_runs_pre_model_hook_and_returns_injected_context() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PreModelCall,
        command: "printf '%s' '{\"action\":\"inject\",\"context\":\"actor hook context\"}'"
            .to_string(),
        tool: None,
    }]);

    let outcome = actor
        .run_pre_model_hook(&hooks, "/tmp")
        .expect("pre model hook");

    assert_eq!(outcome.injected_context, vec!["actor hook context"]);
}

#[test]
fn task_actor_formats_post_model_hook_failure_as_warning() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PostModelCall,
        command: "exit 7".to_string(),
        tool: None,
    }]);

    let warning = actor
        .run_post_model_hook(
            &hooks,
            "/tmp",
            Some(&Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_tokens: 0,
            }),
        )
        .expect("post model warning");

    assert!(warning.contains("post_model_call hook failed"));
}

#[test]
fn task_actor_calls_streaming_provider_and_forwards_model_deltas() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let provider_config = ProviderConfig {
        api_key: None,
        base_url: None,
        model: None,
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        tools_override: None,
        mcp_registry: None,
        external_tools: Vec::new(),
    };
    let mut conversation = Conversation::new();
    conversation.add_user("mock_usage".to_string());
    let cancel = orca_core::cancel::CancelToken::new();
    let mut streamed = Vec::new();

    let response = actor.call_streaming_provider(
        ProviderKind::Mock,
        &conversation,
        &provider_config,
        &cancel,
        &mut |step| match step {
            ProviderStep::ReasoningDelta(text) | ProviderStep::MessageDelta(text) => {
                streamed.push(text.clone())
            }
            _ => {}
        },
    );

    assert_eq!(
        streamed,
        vec![
            "Mock runtime is preserving the DeepSeek reasoning channel.".to_string(),
            "Mock runtime completed with usage accounting.".to_string(),
        ]
    );
    assert_eq!(
        response.assistant_content.as_deref(),
        Some("Mock runtime completed with usage accounting.")
    );
    let usage = response.usage.expect("usage");
    assert_eq!(
        usage.input_tokens + usage.output_tokens + usage.cache_tokens,
        160
    );
}

#[test]
fn task_actor_builds_shell_tool_events_with_task_lifecycle() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let mut events = EventFactory::new("run-actor".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };

    let requested = actor.tool_call_requested_event(&mut events, &request);

    assert_eq!(requested.payload["task"]["task_id"], "shell-tool-1:task-1");
    assert_eq!(requested.payload["task"]["kind"], "shell");
    assert_eq!(requested.payload["task"]["status"], "running");
    assert_eq!(requested.payload["task"]["turn"], 1);

    let result = ToolResult::completed(&request, "hi".to_string(), false);
    let completed = actor.tool_call_completed_event(&mut events, &request, &result);

    assert_eq!(completed.payload["task"]["task_id"], "shell-tool-1:task-1");
    assert_eq!(completed.payload["task"]["kind"], "shell");
    assert_eq!(completed.payload["task"]["status"], "succeeded");
    assert_eq!(completed.payload["task"]["turn"], 1);
}

#[test]
fn task_actor_runs_pre_tool_hook_and_formats_blocked_result() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PreToolUse,
        command: "printf '%s' '{\"action\":\"deny\",\"reason\":\"blocked by actor\"}'".to_string(),
        tool: None,
    }]);

    let blocked = actor
        .run_pre_tool_hook(&hooks, "/tmp", &request)
        .expect_err("blocked result");

    assert_eq!(blocked.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        blocked.error.as_deref(),
        Some("pre_tool_use hook blocked tool: blocked by actor")
    );
}

#[test]
fn task_actor_formats_post_tool_hook_failure_as_warning() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let result = ToolResult::completed(&request, "hi".to_string(), false);
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PostToolUse,
        command: "exit 9".to_string(),
        tool: None,
    }]);

    let warning = actor
        .run_post_tool_hook(&hooks, "/tmp", &request, &result)
        .expect("post tool warning");

    assert!(warning.contains("post_tool_use hook failed"));
}

#[test]
fn task_actor_resolves_required_tool_approval_as_allowed() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let approval = orca_core::approval_types::ApprovalRequest {
        id: "approval-tool-1".to_string(),
        action: ActionKind::Shell,
        description: "bash requested shell".to_string(),
        tool: Some("bash".to_string()),
        target: Some("printf hi".to_string()),
        preview: None,
    };

    let decision = actor.resolve_tool_approval(
        &ApprovalPolicy::new(ApprovalMode::FullAuto),
        Some(approval),
        &request,
    );

    match decision {
        RuntimeApprovalDecision::Allowed(resolution) => {
            assert_eq!(
                resolution.decision,
                orca_core::approval_types::ApprovalDecision::Allow
            );
            assert_eq!(resolution.reason, "full-auto permits shell");
        }
        other => panic!("expected allowed approval decision, got {other:?}"),
    }
}

#[test]
fn task_actor_resolves_denied_tool_approval_with_denied_result() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let approval = orca_core::approval_types::ApprovalRequest {
        id: "approval-tool-1".to_string(),
        action: ActionKind::Shell,
        description: "bash requested shell".to_string(),
        tool: Some("bash".to_string()),
        target: Some("printf hi".to_string()),
        preview: None,
    };

    let decision = actor.resolve_tool_approval(
        &ApprovalPolicy::new(ApprovalMode::Plan),
        Some(approval),
        &request,
    );

    match decision {
        RuntimeApprovalDecision::Denied { resolution, result } => {
            assert_eq!(
                resolution.decision,
                orca_core::approval_types::ApprovalDecision::Deny
            );
            assert_eq!(resolution.reason, "plan denies shell");
            assert_eq!(result.status, orca_core::tool_types::ToolStatus::Denied);
            assert_eq!(result.error.as_deref(), Some("plan denies shell"));
        }
        other => panic!("expected denied approval decision, got {other:?}"),
    }
}

#[test]
fn task_actor_routes_interactive_approval_through_handler() {
    struct DenyHandler;

    impl RuntimeApprovalHandler for DenyHandler {
        fn resolve_interactive(
            &self,
            approval: &ApprovalRequest,
            _request: &ToolRequest,
        ) -> std::io::Result<orca_core::approval_types::ApprovalResolution> {
            Ok(orca_core::approval_types::ApprovalResolution {
                id: approval.id.clone(),
                decision: ApprovalDecision::Deny,
                reason: "handler denied".to_string(),
            })
        }
    }

    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf hi".to_string()),
        raw_arguments: None,
    };
    let approval = ApprovalRequest {
        id: "approval-tool-1".to_string(),
        action: ActionKind::Shell,
        description: "bash requested shell".to_string(),
        tool: Some("bash".to_string()),
        target: Some("printf hi".to_string()),
        preview: None,
    };

    let resolution = actor
        .resolve_interactive_tool_approval(&DenyHandler, &approval, &request)
        .expect("interactive approval resolution");

    assert_eq!(resolution.id, "approval-tool-1");
    assert_eq!(resolution.decision, ApprovalDecision::Deny);
    assert_eq!(resolution.reason, "handler denied");
}

#[test]
fn tool_actor_context_routes_request_user_input_through_handler() {
    struct AnswerHandler;

    impl RuntimeUserInputHandler for AnswerHandler {
        fn request_user_input(
            &self,
            request: &RuntimeUserInputRequest,
        ) -> std::io::Result<Option<String>> {
            assert_eq!(request.id, "ask");
            assert_eq!(request.question, "Continue?");
            assert_eq!(request.choices, vec!["yes".to_string(), "no".to_string()]);
            Ok(Some("yes".to_string()))
        }
    }

    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let request = ToolRequest {
        id: "ask".to_string(),
        name: ToolName::RequestUserInput,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(r#"{"question":"Continue?","choices":["yes","no"]}"#.to_string()),
    };

    let result = context
        .execute_user_input_tool(&request, &AnswerHandler)
        .expect("user input result");

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(result.output.as_deref(), Some("yes"));
}

#[test]
fn tool_actor_context_cancelled_user_input_returns_failed_result() {
    struct CancelHandler;

    impl RuntimeUserInputHandler for CancelHandler {
        fn request_user_input(
            &self,
            request: &RuntimeUserInputRequest,
        ) -> std::io::Result<Option<String>> {
            assert_eq!(request.id, "ask");
            Ok(None)
        }
    }

    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let request = ToolRequest {
        id: "ask".to_string(),
        name: ToolName::RequestUserInput,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(r#"{"question":"Continue?"}"#.to_string()),
    };

    let result = context
        .execute_user_input_tool(&request, &CancelHandler)
        .expect("user input result");

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("user input request cancelled")
    );
}

#[test]
fn tool_actor_context_grants_request_permissions_write_roots_for_current_turn() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let extra = tempfile::tempdir().expect("extra");
    let request = ToolRequest {
        id: "grant".to_string(),
        name: ToolName::RequestPermissions,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(
            serde_json::json!({
                "reason": "write generated files",
                "permissions": {
                    "fileSystem": {
                        "read": null,
                        "write": [extra.path()]
                    },
                    "network": null
                }
            })
            .to_string(),
        ),
    };

    let result = context.execute_request_permissions_tool(&request);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(
        context.granted_additional_working_directories(),
        vec![extra.path().to_path_buf()]
    );
    let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
    assert_eq!(
        output["granted"]["fileSystem"]["write"][0],
        extra.path().display().to_string()
    );
}

#[test]
fn tool_actor_context_grants_request_permissions_entry_write_roots() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let extra = tempfile::tempdir().expect("extra");
    let request = ToolRequest {
        id: "grant".to_string(),
        name: ToolName::RequestPermissions,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(
            serde_json::json!({
                "reason": "write generated files",
                "permissions": {
                    "fileSystem": {
                        "read": null,
                        "write": null,
                        "entries": [
                            {
                                "path": extra.path(),
                                "access": "write"
                            }
                        ]
                    },
                    "network": null
                }
            })
            .to_string(),
        ),
    };

    let result = context.execute_request_permissions_tool(&request);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(
        context.granted_additional_working_directories(),
        vec![extra.path().to_path_buf()]
    );
    let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
    assert_eq!(
        output["granted"]["fileSystem"]["write"][0],
        extra.path().display().to_string()
    );
}

#[test]
fn tool_actor_context_reports_request_permissions_network_domain_grants() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let request = ToolRequest {
        id: "grant-network".to_string(),
        name: ToolName::RequestPermissions,
        action: ActionKind::Network,
        target: Some("api.example.com".to_string()),
        raw_arguments: Some(
            serde_json::json!({
                "reason": "fetch release metadata",
                "permissions": {
                    "fileSystem": null,
                    "network": {
                        "enabled": true,
                        "domains": {
                            "api.example.com": "allow"
                        }
                    }
                }
            })
            .to_string(),
        ),
    };

    let result = context.execute_request_permissions_tool(&request);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
    assert_eq!(output["granted"]["network"]["enabled"], true);
    assert_eq!(
        output["granted"]["network"]["domains"]["api.example.com"],
        "allow"
    );
}

#[test]
fn turn_permission_overlay_requests_and_merges_network_grants() {
    struct AllowNetwork;

    impl RuntimePermissionRequestHandler for AllowNetwork {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            assert_eq!(request.id, "net-tool");
            assert_eq!(
                request.reason.as_deref(),
                Some("tool attempted network access")
            );
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: request.permissions.clone(),
                strict_auto_review: true,
            })
        }
    }

    let mut overlay = TurnPermissionOverlay::default();
    let response = overlay
        .request_and_merge(
            &AllowNetwork,
            RuntimePermissionRequest {
                id: "net-tool".to_string(),
                reason: Some("tool attempted network access".to_string()),
                permissions: RequestPermissionProfile {
                    file_system: None,
                    network: Some(RequestNetworkPermissions {
                        enabled: None,
                        domains: std::collections::HashMap::from([(
                            "api.example.com".to_string(),
                            PermissionProfileNetworkAccess::Allow,
                        )]),
                    }),
                },
            },
        )
        .expect("permission request");

    assert_eq!(response.decision, PermissionResponseDecision::Allow);
    assert_eq!(
        overlay.network_domain_permissions().get("api.example.com"),
        Some(&PermissionProfileNetworkAccess::Allow)
    );
    assert!(overlay.strict_auto_review());
}

#[test]
fn turn_permission_overlay_requests_and_merges_file_system_write_grants() {
    struct AllowFileSystem {
        root: std::path::PathBuf,
    }

    impl RuntimePermissionRequestHandler for AllowFileSystem {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![self.root.clone()]),
                        entries: request
                            .permissions
                            .file_system
                            .as_ref()
                            .and_then(|file_system| file_system.entries.clone()),
                    }),
                    network: None,
                },
                strict_auto_review: false,
            })
        }
    }

    let root = tempfile::tempdir().expect("write root");
    let mut overlay = TurnPermissionOverlay::default();
    overlay
        .request_and_merge(
            &AllowFileSystem {
                root: root.path().to_path_buf(),
            },
            RuntimePermissionRequest {
                id: "fs-tool".to_string(),
                reason: Some("tool needs write access".to_string()),
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![root.path().to_path_buf()]),
                        entries: None,
                    }),
                    network: None,
                },
            },
        )
        .expect("permission request");

    assert_eq!(
        overlay.additional_working_directories(),
        &[root.path().to_path_buf()]
    );
}

#[test]
fn turn_permission_overlay_does_not_merge_denied_responses() {
    struct DenyNetwork;

    impl RuntimePermissionRequestHandler for DenyNetwork {
        fn request_permissions(
            &self,
            request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Deny,
                scope: PermissionGrantScope::Turn,
                permissions: request.permissions.clone(),
                strict_auto_review: true,
            })
        }
    }

    let mut overlay = TurnPermissionOverlay::default();
    let response = overlay
        .request_and_merge(
            &DenyNetwork,
            RuntimePermissionRequest {
                id: "denied-network".to_string(),
                reason: Some("blocked network".to_string()),
                permissions: RequestPermissionProfile {
                    file_system: None,
                    network: Some(RequestNetworkPermissions {
                        enabled: None,
                        domains: std::collections::HashMap::from([(
                            "api.example.com".to_string(),
                            PermissionProfileNetworkAccess::Allow,
                        )]),
                    }),
                },
            },
        )
        .expect("permission request");

    assert_eq!(response.decision, PermissionResponseDecision::Deny);
    assert!(overlay.network_domain_permissions().is_empty());
    assert!(!overlay.strict_auto_review());
}

#[test]
fn tool_actor_context_includes_strict_auto_review_in_permission_output() {
    struct StrictHandler {
        root: std::path::PathBuf,
    }

    impl RuntimePermissionRequestHandler for StrictHandler {
        fn request_permissions(
            &self,
            _request: &RuntimePermissionRequest,
        ) -> std::io::Result<RuntimePermissionResponse> {
            Ok(RuntimePermissionResponse {
                decision: PermissionResponseDecision::Allow,
                scope: PermissionGrantScope::Turn,
                permissions: RequestPermissionProfile {
                    file_system: Some(RequestFileSystemPermissions {
                        read: None,
                        write: Some(vec![self.root.clone()]),
                        entries: None,
                    }),
                    network: None,
                },
                strict_auto_review: true,
            })
        }
    }

    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let extra = tempfile::tempdir().expect("extra");
    let request = ToolRequest {
        id: "grant".to_string(),
        name: ToolName::RequestPermissions,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(
            serde_json::json!({
                "reason": "write generated files",
                "permissions": {
                    "fileSystem": {
                        "read": null,
                        "write": [extra.path()]
                    },
                    "network": null
                }
            })
            .to_string(),
        ),
    };

    let result = context.execute_request_permissions_tool_with_handler(
        &request,
        &StrictHandler {
            root: extra.path().to_path_buf(),
        },
    );

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
    assert_eq!(output["strictAutoReview"], true);
}

#[test]
fn task_actor_executes_normal_tool_with_runtime_policy() {
    let mut lifecycle = RuntimeSessionLifecycle::new("run-actor");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle, 2);
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf actor-tool".to_string()),
        raw_arguments: Some(serde_json::json!({ "command": "printf actor-tool" }).to_string()),
    };

    let result = actor.execute_normal_tool(
        &request,
        std::env::current_dir().expect("cwd").as_path(),
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        ToolConfig::default().shell_timeout_secs,
        None,
    );

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(result.output.as_deref(), Some("actor-tool"));
    assert_eq!(result.exit_code, Some(0));
}

#[test]
fn tool_actor_context_allows_bash_writes_to_additional_working_directories() {
    if !sandbox_seatbelt_available() {
        return;
    }

    let parent =
        tempfile::tempdir_in(std::env::current_dir().expect("cwd")).expect("sandbox parent");
    let workspace = parent.path().join("workspace");
    let extra = parent.path().join("extra");
    let outside = parent.path().join("outside");
    std::fs::create_dir(&workspace).expect("workspace dir");
    std::fs::create_dir(&extra).expect("extra dir");
    std::fs::create_dir(&outside).expect("outside dir");
    let extra_file = extra.join("allowed.txt");
    let outside_file = outside.join("blocked.txt");
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let task_registry = TaskRegistry::new("run-tools".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some(format!(
            "printf allowed > {} && printf blocked > {}",
            extra_file.display(),
            outside_file.display()
        )),
        raw_arguments: None,
    };

    let result = context.execute_normal_tool_with_roots_and_cancel(
        None,
        &request,
        &workspace,
        std::slice::from_ref(&extra),
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        5,
        Some(&task_registry),
        None,
        None,
    );

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(std::fs::read_to_string(extra_file).unwrap(), "allowed");
    assert!(!outside_file.exists());
}

#[test]
fn tool_actor_context_reuses_one_runtime_task_for_approval_hooks_and_execution() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let task_registry = orca_runtime::tasks::TaskRegistry::new("run-tools".to_string());
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf actor-context".to_string()),
        raw_arguments: Some(serde_json::json!({ "command": "printf actor-context" }).to_string()),
    };
    let approval = orca_core::approval_types::ApprovalRequest {
        id: "approval-tool-1".to_string(),
        action: ActionKind::Shell,
        description: "bash requested shell".to_string(),
        tool: Some("bash".to_string()),
        target: Some("printf actor-context".to_string()),
        preview: None,
    };
    let hooks = HookRunner::new(vec![HookConfig {
        event: HookEvent::PreToolUse,
        command: "printf '%s' '{\"action\":\"allow\"}'".to_string(),
        tool: None,
    }]);

    let approval_decision = context.resolve_tool_approval(
        &ApprovalPolicy::new(ApprovalMode::FullAuto),
        Some(approval),
        &request,
    );
    assert!(matches!(
        approval_decision,
        RuntimeApprovalDecision::Allowed(_)
    ));

    let pre_tool_outcome = context
        .run_pre_tool_hook(&hooks, "/tmp", &request)
        .expect("pre tool hook");
    assert!(pre_tool_outcome.modified_target.is_none());
    assert!(pre_tool_outcome.injected_context.is_empty());

    let result = context.execute_normal_tool(
        &request,
        std::env::current_dir().expect("cwd").as_path(),
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        ToolConfig::default().shell_timeout_secs,
        Some(&task_registry),
    );
    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    assert_eq!(result.output.as_deref(), Some("actor-context"));
    let shell_tasks = task_registry.list();
    assert_eq!(shell_tasks.len(), 1);
    assert_eq!(shell_tasks[0].task_type, TaskType::Shell);
    assert_eq!(shell_tasks[0].status, TaskStatus::Completed);
    assert_eq!(
        shell_tasks[0].command.as_deref(),
        Some("printf actor-context")
    );

    assert!(
        context
            .run_post_tool_hook(&HookRunner::new(Vec::new()), "/tmp", &request, &result)
            .is_none()
    );

    let task = context.active_task().expect("active task");
    assert_eq!(task.id(), "run-tools:task-1");
    assert_eq!(task.kind(), RuntimeTaskKind::Agent);
    assert_eq!(task.status(), RuntimeTaskStatus::Running);
}

#[test]
fn tool_actor_context_cancels_shell_session_tool_wait() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let task_registry = orca_runtime::tasks::TaskRegistry::new("run-tools".to_string());
    let cancel = orca_core::cancel::CancelToken::new();
    cancel.cancel();
    let request = ToolRequest {
        id: "tool-1".to_string(),
        name: ToolName::Bash,
        action: ActionKind::Shell,
        target: Some("printf before; sleep 5; printf after".to_string()),
        raw_arguments: Some(
            serde_json::json!({ "command": "printf before; sleep 5; printf after" }).to_string(),
        ),
    };
    let start = std::time::Instant::now();

    let result = context.execute_normal_tool_with_cancel(
        &request,
        std::env::current_dir().expect("cwd").as_path(),
        &McpRegistry::default(),
        &[],
        ToolConfig::default().output_truncation,
        30,
        Some(&task_registry),
        Some(&cancel),
    );

    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "cancelled shell-session tool should not wait for the shell timeout"
    );
    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("shell command cancelled"),
        "unexpected error: {:?}",
        result.error
    );
    assert!(
        task_registry
            .list()
            .iter()
            .any(|task| task.task_type == TaskType::Shell && task.status == TaskStatus::Stopped),
        "cancelled shell execution should stop its shell task record"
    );
}

#[test]
fn tool_actor_context_task_stop_cancels_running_shell_task_wait() {
    let task_registry = TaskRegistry::new("run-tools".to_string());
    let shell_registry = task_registry.clone();
    let handle = std::thread::spawn(move || {
        let mut context = RuntimeToolActorContext::new("run-tools", 2);
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("printf before; sleep 5; printf after".to_string()),
            raw_arguments: Some(
                serde_json::json!({ "command": "printf before; sleep 5; printf after" })
                    .to_string(),
            ),
        };

        context.execute_normal_tool_with_cancel(
            &request,
            std::env::current_dir().expect("cwd").as_path(),
            &McpRegistry::default(),
            &[],
            ToolConfig::default().output_truncation,
            30,
            Some(&shell_registry),
            None,
        )
    });
    let started = std::time::Instant::now();
    let task_id =
        loop {
            if let Some(task) = task_registry.list().into_iter().find(|task| {
                task.task_type == TaskType::Shell && task.status == TaskStatus::Running
            }) {
                break task.id;
            }
            assert!(
                started.elapsed() < std::time::Duration::from_secs(2),
                "shell task did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
    let mut stop_context = RuntimeToolActorContext::new("run-tools", 2);
    let stop_request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task_id)),
    };
    let stop_started = std::time::Instant::now();

    let stop_result = stop_context.execute_task_stop_tool(&stop_request, &task_registry);
    let result = handle.join().expect("shell thread result");

    assert_eq!(
        stop_result.status,
        orca_core::tool_types::ToolStatus::Completed
    );
    assert!(
        stop_started.elapsed() < std::time::Duration::from_secs(2),
        "task_stop should cancel the running shell wait promptly"
    );
    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert!(
        task_registry
            .list()
            .iter()
            .any(|task| task.id == task_id && task.status == TaskStatus::Stopped),
        "task_stop should stop the shell task record"
    );
}

#[test]
fn tool_actor_context_classifies_runtime_special_tool_dispatch() {
    let context = RuntimeToolActorContext::new("run-tools", 2);

    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::WorkflowDraft)),
        RuntimeSpecialToolDispatch::WorkflowDraft
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::WorkflowDraftAction)),
        RuntimeSpecialToolDispatch::WorkflowDraftAction
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::Workflow)),
        RuntimeSpecialToolDispatch::Workflow
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::Subagent)),
        RuntimeSpecialToolDispatch::Subagent
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::SubagentStatus)),
        RuntimeSpecialToolDispatch::SubagentStatus
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::TaskList)),
        RuntimeSpecialToolDispatch::TaskList
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::TaskStop)),
        RuntimeSpecialToolDispatch::TaskStop
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::RequestPermissions)),
        RuntimeSpecialToolDispatch::RequestPermissions
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::WorkflowReadMessages)),
        RuntimeSpecialToolDispatch::WorkflowIpc
    );
    assert_eq!(
        context.classify_dispatch(&tool_request(ToolName::Bash)),
        RuntimeSpecialToolDispatch::Normal
    );
}

#[test]
fn tool_actor_context_executes_workflow_ipc_guardrail_without_child_context() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let request = ToolRequest {
        id: "mailbox".to_string(),
        name: ToolName::WorkflowReadMessages,
        action: ActionKind::Agent,
        target: Some("findings".to_string()),
        raw_arguments: Some(serde_json::json!({ "channel": "findings" }).to_string()),
    };

    let result = context.execute_workflow_ipc_tool(&request, None);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert!(
        result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("only available inside workflow child agents")
    );
}

#[test]
fn tool_actor_context_executes_workflow_ipc_against_runtime_trait() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let ipc = FakeWorkflowIpc;
    let request = ToolRequest {
        id: "mailbox".to_string(),
        name: ToolName::WorkflowSendMessage,
        action: ActionKind::Agent,
        target: Some("findings".to_string()),
        raw_arguments: Some(
            serde_json::json!({
                "channel": "findings",
                "from": "worker-a",
                "message": { "status": "ready" }
            })
            .to_string(),
        ),
    };

    let result = context.execute_workflow_ipc_tool(&request, Some(&ipc));

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["channel"], "findings");
    assert_eq!(output["from"], "worker-a");
    assert_eq!(output["message"]["status"], "ready");
}

#[test]
fn tool_actor_context_executes_subagent_status_against_runtime_lookup() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let request = ToolRequest {
        id: "status".to_string(),
        name: ToolName::SubagentStatus,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some(serde_json::json!({ "agent_id": "agent-1" }).to_string()),
    };
    let lookup = FakeSubagentStatusLookup;

    let result = context.execute_subagent_status_tool(&request, &lookup);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["agent_id"], "agent-1");
    assert_eq!(output["status"], "completed");
    assert_eq!(output["description"], "inspect auth");
    assert_eq!(output["agent_type"], "general");
    assert_eq!(output["output"], "finished async audit");
    assert_eq!(output["error"], Value::Null);
}

#[test]
fn tool_actor_context_lists_tasks_with_package3_shape() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let registry = TaskRegistry::new("session-1".to_string());
    let task = registry.create_shell("Run server".to_string(), "npm run dev".to_string());
    registry.mark_running(&task.id).unwrap();
    let request = ToolRequest {
        id: "tasks".to_string(),
        name: ToolName::TaskList,
        action: ActionKind::Read,
        target: None,
        raw_arguments: Some("{}".to_string()),
    };

    let result = context.execute_task_list_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["tasks"][0]["id"], task.id);
    assert_eq!(output["tasks"][0]["subject"], "Run server");
    assert_eq!(output["tasks"][0]["status"], "running");
    assert_eq!(output["tasks"][0]["task_type"], "shell");
    assert_eq!(output["tasks"][0]["command"], "npm run dev");
    assert_eq!(output["tasks"][0]["blockedBy"], serde_json::json!([]));
}

#[test]
fn tool_actor_context_stops_running_task_by_task_id() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let registry = TaskRegistry::new("session-1".to_string());
    let task = registry.create_shell("Run server".to_string(), "npm run dev".to_string());
    registry.mark_running(&task.id).unwrap();
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task.id)),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["task_id"], task.id);
    assert_eq!(output["task_type"], "shell");
    assert_eq!(output["command"], "npm run dev");
    assert_eq!(output["message"], "Task stop requested");
    assert_eq!(
        registry.get(&task.id).expect("task record").status,
        TaskStatus::Stopping
    );
}

#[test]
fn tool_actor_context_stops_running_task_by_deprecated_shell_id_alias() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let registry = TaskRegistry::new("session-1".to_string());
    let task = registry.create_shell("Run server".to_string(), "npm run dev".to_string());
    registry.mark_running(&task.id).unwrap();
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(format!(r#"{{"shell_id":"{}"}}"#, task.id)),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["task_id"], task.id);
    assert_eq!(
        registry.get(&task.id).expect("task record").status,
        TaskStatus::Stopping
    );
}

#[test]
fn tool_actor_context_rejects_task_stop_without_id() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let registry = TaskRegistry::new("session-1".to_string());
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some("{}".to_string()),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("missing required field: task_id")
    );
}

#[test]
fn tool_actor_context_rejects_unknown_task_stop() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let registry = TaskRegistry::new("session-1".to_string());
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(r#"{"task_id":"missing-task"}"#.to_string()),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("task 'missing-task' not found")
    );
}

#[test]
fn tool_actor_context_rejects_terminal_task_stop() {
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let registry = TaskRegistry::new("session-1".to_string());
    let task = registry.create_shell("Run server".to_string(), "npm run dev".to_string());
    registry.complete(&task.id, "done".to_string()).unwrap();
    let request = ToolRequest {
        id: "stop".to_string(),
        name: ToolName::TaskStop,
        action: ActionKind::Write,
        target: None,
        raw_arguments: Some(format!(r#"{{"task_id":"{}"}}"#, task.id)),
    };

    let result = context.execute_task_stop_tool(&request, &registry);

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Failed);
    assert_eq!(
        result.error.as_deref(),
        Some("task is already completed and cannot be stopped")
    );
}

#[test]
fn tool_actor_context_executes_workflow_draft_preview() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut context = RuntimeToolActorContext::new("run-tools", 2);
    let request = ToolRequest {
        id: "draft".to_string(),
        name: ToolName::WorkflowDraft,
        action: ActionKind::Write,
        target: Some("preview workflow".to_string()),
        raw_arguments: Some(
            serde_json::json!({
                "script": workflow_script()
            })
            .to_string(),
        ),
    };

    let result = context
        .execute_workflow_draft_tool(
            &request,
            RuntimeWorkflowDraftRequest {
                workflows_enabled: true,
                cwd: temp.path(),
                session_id: "session-1",
                max_concurrent_agents: 3,
            },
        )
        .expect("workflow draft result");

    assert_eq!(result.status, orca_core::tool_types::ToolStatus::Completed);
    let output: Value = serde_json::from_str(result.output.as_deref().expect("output")).unwrap();
    assert_eq!(output["sessionId"], "session-1");
    assert_eq!(output["cwd"], temp.path().display().to_string());
    assert_eq!(output["name"], "runtime-draft");
    assert_eq!(output["description"], "Runtime draft");
    assert_eq!(output["phases"], serde_json::json!(["main"]));
    assert_eq!(output["estimatedAgentCount"], 1);
    assert_eq!(output["maxConfiguredConcurrentAgents"], 3);
    assert_eq!(output["sourceMutationRisk"], "read_only_likely");
    assert!(
        output["scriptPath"]
            .as_str()
            .expect("script path")
            .ends_with("script.js")
    );
}

#[test]
fn controller_turn_started_events_include_agent_task_lifecycle() {
    let mut output = Vec::new();
    let mut config = test_run_config();
    config.provider = ProviderKind::Mock;
    config.output_format = OutputFormat::Jsonl;
    config.history_mode = HistoryMode::Disabled;
    config.approval_mode = ApprovalMode::FullAuto;
    config.prompt = "reply once".to_string();

    let exit = orca_runtime::controller::run_to_writer(config, &mut output);

    assert_eq!(exit, 0);
    let events = String::from_utf8(output)
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("json event"))
        .collect::<Vec<_>>();
    let session_started = events
        .iter()
        .find(|event| event["type"] == "session.started")
        .expect("session.started event");
    let turn_started = events
        .iter()
        .find(|event| event["type"] == "turn.started")
        .expect("turn.started event");
    let session_completed = events
        .iter()
        .find(|event| event["type"] == "session.completed")
        .expect("session.completed event");

    assert_eq!(turn_started["payload"]["task"]["kind"], "agent");
    assert_eq!(turn_started["payload"]["task"]["status"], "running");
    assert_eq!(turn_started["payload"]["task"]["turn"], 1);
    assert_eq!(turn_started["runId"], session_started["runId"]);
    assert_eq!(session_completed["runId"], session_started["runId"]);
}

fn workflow_script() -> &'static str {
    "export const meta = { name: 'runtime-draft', description: 'Runtime draft', phases: ['main'] };\nconst result = await phase('main', async () => agent('inspect repo'));\nexport default result;"
}

struct FakeSubagentStatusLookup;

impl RuntimeSubagentStatusLookup for FakeSubagentStatusLookup {
    fn subagent_status_record(&self, agent_id: &str) -> Option<RuntimeSubagentStatusRecord> {
        if agent_id != "agent-1" {
            return None;
        }
        Some(RuntimeSubagentStatusRecord {
            id: agent_id.to_string(),
            status: "completed".to_string(),
            description: "inspect auth".to_string(),
            agent_type: Some("general".to_string()),
            created_at_ms: 1,
            started_at_ms: Some(2),
            completed_at_ms: Some(3),
            output: Some("finished async audit".to_string()),
            error: None,
            usage: None,
        })
    }
}

struct FakeWorkflowIpc;

impl RuntimeWorkflowIpc for FakeWorkflowIpc {
    fn send_message(
        &self,
        channel: &str,
        from: Option<&str>,
        message: Value,
    ) -> Result<Value, String> {
        Ok(serde_json::json!({
            "channel": channel,
            "from": from.unwrap_or("default"),
            "message": message,
        }))
    }

    fn read_messages(&self, channel: &str) -> Result<Value, String> {
        Ok(serde_json::json!([{ "channel": channel }]))
    }

    fn clear_messages(&self, channel: &str) -> Result<Value, String> {
        Ok(serde_json::json!({ "cleared": channel }))
    }

    fn create_task_list(&self, name: &str, items: Vec<Value>) -> Result<Value, String> {
        Ok(serde_json::json!({ "name": name, "items": items }))
    }

    fn claim_task(&self, name: &str, by: Option<&str>) -> Result<Value, String> {
        Ok(serde_json::json!({ "name": name, "by": by }))
    }

    fn complete_task(
        &self,
        name: &str,
        task_id: &str,
        result: Value,
        by: Option<&str>,
    ) -> Result<Value, String> {
        Ok(serde_json::json!({
            "name": name,
            "task_id": task_id,
            "result": result,
            "by": by,
        }))
    }

    fn list_tasks(&self, name: &str) -> Result<Value, String> {
        Ok(serde_json::json!({ "name": name, "tasks": [] }))
    }
}

fn tool_request(name: ToolName) -> ToolRequest {
    ToolRequest {
        id: "tool-1".to_string(),
        name,
        action: ActionKind::Read,
        target: None,
        raw_arguments: None,
    }
}

fn sandbox_seatbelt_available() -> bool {
    std::process::Command::new("sandbox-exec")
        .arg("-p")
        .arg("(version 1) (allow default)")
        .arg("true")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn test_run_config() -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: Some(std::env::current_dir().expect("cwd")),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::FullAuto,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::from_unchecked(Some("auto".to_string())),
        model_runtime: Default::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
        api_key: None,
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
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
        theme: ThemeName::Dark,
        vim_mode: false,
        update_check: false,
        desktop_notifications: false,
        auto_memory: false,
    }
}
