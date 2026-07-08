mod agent_runner;
mod agent_subagent_execution;
mod agent_tool_execution;
mod agent_workflow_execution;
pub mod app;
mod background_approval;
mod background_tasks;
pub mod bridge;
pub mod commands;
pub mod diff;
mod runtime_event_projection;
mod runtime_interaction_adapter;
pub mod shortcuts;
mod submitted_turn;
pub mod theme;
pub mod types;
pub mod ui;
pub mod vim;

pub use app::run_tui;

#[cfg(test)]
mod tests {
    #[test]
    fn runtime_event_projection_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let projection =
            std::fs::read_to_string(format!("{manifest_dir}/src/runtime_event_projection.rs"))
                .expect("runtime event projection module should exist");
        assert!(
            projection.contains("pub(crate) fn tui_event_from_runtime_event"),
            "runtime_event_projection should export the TUI runtime event adapter"
        );

        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            !bridge.contains("fn tui_event_from_runtime_event"),
            "bridge should call the shared TUI runtime event adapter instead of owning it"
        );
    }

    #[test]
    fn runtime_interaction_adapters_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let adapter =
            std::fs::read_to_string(format!("{manifest_dir}/src/runtime_interaction_adapter.rs"))
                .expect("runtime interaction adapter module should exist");
        assert!(
            adapter.contains("pub(crate) struct TuiApprovalHandler"),
            "runtime_interaction_adapter should own the TUI approval handler"
        );
        assert!(
            adapter.contains("pub(crate) struct TuiUserInputHandler"),
            "runtime_interaction_adapter should own the TUI user-input handler"
        );
        assert!(
            adapter.contains("pub(crate) fn resolve_tui_tool_approval"),
            "runtime_interaction_adapter should own the TUI tool approval gate"
        );
        assert!(
            adapter.contains("RuntimePendingInteractionRecord"),
            "runtime_interaction_adapter should project runtime-owned pending interaction records"
        );
        assert!(
            adapter.contains("fn approval_event_from_pending_interaction"),
            "runtime_interaction_adapter should map runtime pending approval records into TUI events"
        );
        assert!(
            adapter.contains("fn user_input_event_from_pending_interaction"),
            "runtime_interaction_adapter should map runtime pending user-input records into TUI events"
        );

        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            !bridge.contains("struct TuiApprovalHandler"),
            "bridge should use the TUI approval adapter instead of owning it"
        );
        assert!(
            !bridge.contains("struct TuiUserInputHandler"),
            "bridge should use the TUI user-input adapter instead of owning it"
        );
        assert!(
            !bridge.contains("approval_request_for_invocation"),
            "bridge should delegate TUI approval request construction to the interaction adapter"
        );
        assert!(
            !bridge.contains("resolve_interactive_tool_approval"),
            "bridge should delegate interactive approval waits to the interaction adapter"
        );
    }

    #[test]
    fn tui_agent_runner_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner module should exist");
        assert!(
            runner.contains("pub fn run_agent_for_tui"),
            "agent_runner should own the TUI agent turn loop entrypoint"
        );

        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            !bridge.contains("pub fn run_agent_for_tui"),
            "bridge should not own the TUI agent turn loop"
        );
    }

    #[test]
    fn tui_submitted_turn_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let submitted_turn =
            std::fs::read_to_string(format!("{manifest_dir}/src/submitted_turn.rs"))
                .expect("submitted_turn module should exist");
        assert!(
            submitted_turn.contains("pub(crate) struct SubmittedTurn"),
            "submitted_turn should own the submitted-turn boundary"
        );
        assert!(
            submitted_turn.contains("enum SubmittedTurnKind"),
            "submitted_turn should keep prompt/source state inside SubmittedTurnKind"
        );
        assert!(
            submitted_turn.contains("struct SubmittedTurnPresentation"),
            "submitted_turn should own TUI submitted-turn presentation metadata"
        );
        assert!(
            !submitted_turn.contains("pub(crate) struct SubmittedTurnPresentation"),
            "SubmittedTurnPresentation should stay private behind SubmittedTurn accessors"
        );
        assert!(
            submitted_turn.contains("pub(crate) fn task_label(&self) -> Option<&str>")
                && submitted_turn.contains("pub(crate) fn is_backtrack_target(&self) -> bool"),
            "submitted_turn should expose presentation policy through SubmittedTurn methods"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nstruct SubmittedTurn {"),
            "app should use the submitted_turn module instead of defining the boundary inline"
        );
        assert!(
            !app.contains("\nenum SubmittedTurnKind {"),
            "app should not own submitted-turn prompt/source variants"
        );
    }

    #[test]
    fn tui_background_approval_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let background_approval =
            std::fs::read_to_string(format!("{manifest_dir}/src/background_approval.rs"))
                .expect("background_approval module should exist");
        assert!(
            background_approval
                .contains("pub(crate) fn submit_background_approval_response_for_tui"),
            "background_approval should own TUI background approval response submission"
        );
        assert!(
            background_approval.contains("TuiBackgroundTurnContinuationRequest"),
            "background_approval should return the typed background continuation request"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn submit_background_approval_response_for_tui("),
            "app should use the background_approval module instead of defining approval submission inline"
        );
    }

    #[test]
    fn tui_background_task_actions_are_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let background_tasks =
            std::fs::read_to_string(format!("{manifest_dir}/src/background_tasks.rs"))
                .expect("background_tasks module should exist");
        assert!(
            background_tasks.contains("pub(crate) fn stop_task_for_tui"),
            "background_tasks should own TUI task stop execution"
        );
        assert!(
            background_tasks.contains("pub(crate) fn foreground_task_for_tui"),
            "background_tasks should own TUI task foreground execution"
        );
        assert!(
            background_tasks
                .contains("pub(crate) fn notify_recovered_background_approvals_for_tui"),
            "background_tasks should own recovered background approval notifications"
        );

        let app = std::fs::read_to_string(format!("{manifest_dir}/src/app.rs"))
            .expect("app source should be readable");
        assert!(
            !app.contains("\nfn stop_task_for_tui("),
            "app should use the background_tasks module instead of defining stop execution inline"
        );
        assert!(
            !app.contains("\nfn foreground_task_for_tui("),
            "app should use the background_tasks module instead of defining foreground execution inline"
        );
        assert!(
            !app.contains("\nfn notify_recovered_background_approvals_for_tui("),
            "app should use the background_tasks module instead of defining recovery notifications inline"
        );
    }

    #[test]
    fn approved_background_continuation_is_owned_by_runtime() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");
        assert!(
            runner.contains("take_approved_background_turn_continuation"),
            "agent_runner should ask runtime for approved background turn continuations"
        );
        assert!(
            !runner.contains(".take_approved_pending_provider_response("),
            "agent_runner should not directly consume pending provider responses from TaskRegistry"
        );
        assert!(
            !runner.contains("fn provider_response_first_tool_call_id"),
            "agent_runner should not derive preapproved tool call ids; runtime owns that boundary"
        );
        assert!(
            runner.contains("into_runtime_turn_continuation"),
            "agent_runner should convert approved background continuations into runtime turn continuations"
        );
        assert!(
            runner.contains("with_continuation"),
            "agent_runner should resume approved background turns through a runtime ThreadTurnRequest continuation"
        );
        assert!(
            !runner.contains("execute_preapproved_tool_for_tui"),
            "approved background continuation should not use a renderer-owned preapproved tool loop"
        );
    }

    #[test]
    fn tui_main_session_task_status_uses_runtime_task_status_event() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");

        assert!(
            runner.contains("fn send_task_status_updated_for_tui"),
            "agent_runner should expose a single-task runtime status event helper"
        );
        assert!(
            runner.contains("events.task_status_updated(task)"),
            "TUI task status helper must emit task.status.updated runtime events"
        );
        assert!(
            runner.contains("send_task_status_updated_for_tui(event_tx, events, &task);"),
            "main session task start should announce the concrete task status event"
        );
        assert!(
            runner.contains(
                "send_task_status_updated_for_tui(event_tx, events, &backgrounded_task);"
            ),
            "backgrounding a main session should announce the concrete task status event"
        );
        assert!(
            runner.contains("send_task_status_updated_for_tui(event_tx, events, &finished_task);"),
            "main session completion should announce the concrete task status event"
        );
        assert!(
            runner.contains(
                "send_task_status_updated_for_tui(&event_tx, &mut events, &updated_task);"
            ),
            "background provider completion should announce the concrete task status event"
        );
        assert!(
            runner.contains(
                "send_task_status_updated_for_tui(event_tx, &mut runtime_events, &continued_task);"
            ),
            "approved background turn continuation should announce the concrete task status event"
        );
    }

    #[test]
    fn tui_agent_tool_execution_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let execution =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_tool_execution.rs"))
                .expect("TUI agent tool execution module should exist");
        assert!(
            execution.contains("pub(crate) fn execute_tool_for_tui"),
            "agent_tool_execution should own the TUI tool execution entrypoint"
        );

        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");
        assert!(
            !runner.contains("fn execute_tool_for_tui"),
            "agent_runner should not own TUI tool execution helpers"
        );
    }

    #[test]
    fn tui_goal_updates_use_runtime_thread_extension_guard() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let bridge = std::fs::read_to_string(format!("{manifest_dir}/src/bridge.rs"))
            .expect("bridge source should be readable");
        assert!(
            bridge.contains("thread_extensions"),
            "TUI session should expose RuntimeThread thread extension state"
        );

        let execution =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_tool_execution.rs"))
                .expect("TUI agent tool execution source should be readable");
        assert!(
            execution.contains("validate_goal_terminal_update_against_extensions"),
            "TUI goal update handler must guard terminal updates with live runtime extension state"
        );

        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");
        assert!(
            runner.contains("record_tui_goal_tool_finish"),
            "TUI agent runner must record completed tools into live runtime thread extension state"
        );
        assert!(
            runner.contains("RuntimeTurnReducer"),
            "TUI completed-tool recording should route through the runtime turn reducer"
        );
        assert!(
            !runner.contains("goals::record_goal_tool_finish"),
            "TUI should not write goal progress directly; runtime turn reducer owns tool finish state"
        );
    }

    #[test]
    fn tui_agent_workflow_execution_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workflow =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_workflow_execution.rs"))
                .expect("TUI agent workflow execution module should exist");
        assert!(
            workflow.contains("pub(crate) fn execute_workflow_for_tui"),
            "agent_workflow_execution should own the TUI workflow execution entrypoint"
        );

        let execution =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_tool_execution.rs"))
                .expect("TUI agent tool execution source should be readable");
        assert!(
            !execution.contains("fn execute_workflow_for_tui"),
            "agent_tool_execution should not own TUI workflow helpers"
        );
    }

    #[test]
    fn tui_workflow_task_status_uses_runtime_task_status_event() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workflow =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_workflow_execution.rs"))
                .expect("TUI agent workflow execution module should exist");

        assert!(
            workflow.contains("fn send_workflow_task_status_for_tui"),
            "workflow execution should centralize single-task status announcements"
        );
        assert!(
            workflow.contains("send_task_status_updated_for_tui"),
            "workflow task status should announce concrete task status runtime events"
        );
        assert!(
            workflow.contains("task_summary_for_tui"),
            "workflow task status should load the concrete task summary before notifying"
        );
        assert!(
            workflow.contains("send_task_status_updated_for_tui(event_tx, events, &task);"),
            "workflow task status helper must emit task.status.updated runtime events"
        );
        assert!(
            workflow.contains("send_workflow_tasks_updated_for_tui"),
            "workflow progress refreshes should keep the aggregate workflow task-list event"
        );
    }

    #[test]
    fn tui_agent_subagent_execution_is_owned_by_dedicated_module() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("pub(crate) fn execute_subagent_batch_for_tui"),
            "agent_subagent_execution should own the TUI subagent batch entrypoint"
        );
        assert!(
            subagent.contains("pub(crate) fn execute_subagent_for_tui"),
            "agent_subagent_execution should own the TUI subagent execution entrypoint"
        );
        assert!(
            subagent.contains("pub(crate) fn execute_subagent_status_for_tui"),
            "agent_subagent_execution should own the TUI subagent status entrypoint"
        );

        let execution =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_tool_execution.rs"))
                .expect("TUI agent tool execution source should be readable");
        assert!(
            !execution.contains("fn execute_subagent_for_tui"),
            "agent_tool_execution should not own TUI subagent helpers"
        );
        assert!(
            !execution.contains("fn execute_subagent_batch_for_tui"),
            "agent_tool_execution should not own TUI subagent batch helpers"
        );
    }

    #[test]
    fn tui_subagent_task_status_uses_runtime_task_status_event() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");

        assert!(
            subagent.contains("send_task_status_updated_for_tui"),
            "subagent task status should announce concrete task status runtime events"
        );
        assert!(
            subagent.contains("task_summary_for_tui"),
            "subagent task status should load the concrete task summary before notifying"
        );
        assert!(
            !subagent.contains("send_workflow_tasks_updated_for_tui"),
            "subagent task status should not borrow the workflow task-list event"
        );
    }

    #[test]
    fn tui_subagent_results_use_runtime_child_agent_result() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let runner = std::fs::read_to_string(format!("{manifest_dir}/src/agent_runner.rs"))
            .expect("TUI agent runner source should be readable");
        assert!(
            !runner.contains("struct TuiAgentResult"),
            "agent_runner should not own the child-agent result type"
        );

        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("ChildAgentResult"),
            "agent_subagent_execution should use the runtime child-agent result type"
        );
    }

    #[test]
    fn tui_subagent_child_runs_delegate_model_override_to_runtime() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("run_child_agent_prompt_with_tool_executor"),
            "agent_subagent_execution should delegate child-agent model/cost setup to runtime"
        );
        assert!(
            !subagent.contains("run_child_agent_with_executor"),
            "agent_subagent_execution should not call the low-level child-agent executor wrapper"
        );
        assert!(
            !subagent.contains("with_subagent_override"),
            "agent_subagent_execution should not own child-agent model override logic"
        );
    }

    #[test]
    fn tui_subagent_child_request_construction_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("run_child_agent_prompt_with_tool_executor"),
            "agent_subagent_execution should delegate child request construction to runtime"
        );
        assert!(
            !subagent.contains("ChildAgentRequest::new"),
            "agent_subagent_execution should not construct child requests directly"
        );
    }

    #[test]
    fn tui_subagent_child_loop_setup_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("run_child_agent_prompt_with_tool_executor"),
            "agent_subagent_execution should delegate child loop orchestration to runtime"
        );
        assert!(
            !subagent.contains("prepare_child_agent_loop"),
            "agent_subagent_execution should not prepare child loop setup directly"
        );
        assert!(
            !subagent.contains("ProviderConfig"),
            "agent_subagent_execution should not construct child provider config"
        );
        assert!(
            !subagent.contains("Conversation::new"),
            "agent_subagent_execution should not bootstrap child conversation"
        );
        assert!(
            !subagent.contains("build_agent_system_prompt"),
            "agent_subagent_execution should not own child system prompt construction"
        );
    }

    #[test]
    fn tui_subagent_child_model_routing_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("route_child_agent_model"),
            "agent_subagent_execution should not route child models directly"
        );
        assert!(
            !subagent.contains("ModelRouteContext"),
            "agent_subagent_execution should not construct child model route context"
        );
        assert!(
            !subagent.contains("set_model"),
            "agent_subagent_execution should not update child cost model directly"
        );
    }

    #[test]
    fn tui_subagent_child_compaction_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("compact_child_agent_conversation_if_needed"),
            "agent_subagent_execution should not trigger child compaction directly"
        );
        assert!(
            !subagent.contains("needs_compaction_wire"),
            "agent_subagent_execution should not decide child compaction thresholds"
        );
        assert!(
            !subagent.contains("HookEvent::OnBudgetWarning"),
            "agent_subagent_execution should not run child budget-warning hooks directly"
        );
    }

    #[test]
    fn tui_subagent_child_provider_error_handling_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("handle_child_agent_provider_error"),
            "agent_subagent_execution should not handle child provider errors directly"
        );
        assert!(
            !subagent.contains("is_prompt_too_long_error"),
            "agent_subagent_execution should not classify prompt-too-long provider errors"
        );
        assert!(
            !subagent.contains("orca_provider::context::compact("),
            "agent_subagent_execution should not compact child conversations directly"
        );
    }

    #[test]
    fn tui_subagent_child_provider_turn_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("run_child_agent_provider_turn"),
            "agent_subagent_execution should not run child provider turns directly"
        );
        assert!(
            !subagent.contains("call_streaming"),
            "agent_subagent_execution should not call providers directly"
        );
        assert!(
            !subagent.contains("HookEvent::PreModelCall")
                && !subagent.contains("HookEvent::PostModelCall"),
            "agent_subagent_execution should not run child model hooks directly"
        );
        assert!(
            !subagent.contains("conversation_with_hook_context"),
            "agent_subagent_execution should not build child model hook conversations"
        );
    }

    #[test]
    fn tui_subagent_child_provider_response_folding_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("fold_child_agent_provider_response"),
            "agent_subagent_execution should not fold child provider responses directly"
        );
        assert!(
            !subagent.contains("add_usage"),
            "agent_subagent_execution should not update child provider usage directly"
        );
        assert!(
            !subagent.contains("tool_calls.is_empty"),
            "agent_subagent_execution should not decide terminal provider response state"
        );
        assert!(
            !subagent.contains("add_assistant"),
            "agent_subagent_execution should not record child assistant responses directly"
        );
    }

    #[test]
    fn tui_subagent_child_tool_result_folding_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("fold_child_agent_tool_result"),
            "agent_subagent_execution should not fold child tool results directly"
        );
        assert!(
            !subagent.contains("child_cost_tracker.merge"),
            "agent_subagent_execution should not merge child tool costs directly"
        );
        assert!(
            !subagent.contains("format_tool_result_for_model"),
            "agent_subagent_execution should not format child tool results for model context"
        );
        assert!(
            !subagent.contains("add_tool_result"),
            "agent_subagent_execution should not record child tool results directly"
        );
    }

    #[test]
    fn tui_subagent_child_turn_budget_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("advance_child_agent_turn"),
            "agent_subagent_execution should not advance child turns directly"
        );
        assert!(
            !subagent.contains("DEFAULT_MAX_TURNS"),
            "agent_subagent_execution should not own child max-turn limits"
        );
        assert!(
            !subagent.contains("turn += 1"),
            "agent_subagent_execution should not advance child turn counters directly"
        );
        assert!(
            !subagent.contains("RunStatus::BudgetExhausted"),
            "agent_subagent_execution should not construct child budget-exhausted results"
        );
    }

    #[test]
    fn tui_subagent_child_loop_state_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("let mut turn"),
            "agent_subagent_execution should not own child turn state"
        );
        assert!(
            !subagent.contains("reactive_compacted"),
            "agent_subagent_execution should not own child reactive compaction state"
        );
    }

    #[test]
    fn tui_subagent_child_tool_executor_uses_runtime_tool_context() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            subagent.contains("tool_context.policy"),
            "agent_subagent_execution should consume a narrow runtime tool context"
        );
        assert!(
            subagent.contains("tool_context.mcp_registry"),
            "agent_subagent_execution should consume runtime MCP context through tool context"
        );
        assert!(
            !subagent.contains("setup.policy") && !subagent.contains("setup.mcp_registry"),
            "agent_subagent_execution should not depend on child loop setup internals"
        );
    }

    #[test]
    fn tui_subagent_child_tool_request_extraction_is_runtime_owned() {
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let subagent =
            std::fs::read_to_string(format!("{manifest_dir}/src/agent_subagent_execution.rs"))
                .expect("TUI agent subagent execution module should exist");
        assert!(
            !subagent.contains("child_agent_tool_requests"),
            "agent_subagent_execution should not extract child provider tool calls directly"
        );
        assert!(
            !subagent.contains("ProviderStep"),
            "agent_subagent_execution should not inspect provider steps directly"
        );
        assert!(
            !subagent.contains("response.steps"),
            "agent_subagent_execution should not iterate provider response steps directly"
        );
    }
}
