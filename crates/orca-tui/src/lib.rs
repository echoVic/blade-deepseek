mod agent_runner;
mod agent_subagent_execution;
mod agent_tool_execution;
mod agent_workflow_execution;
pub mod app;
pub mod bridge;
pub mod commands;
pub mod diff;
mod runtime_event_projection;
mod runtime_interaction_adapter;
pub mod shortcuts;
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
