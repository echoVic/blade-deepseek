pub mod agent_child;
pub mod agent_common;
pub mod agent_loop;
pub mod approval_resolution;
pub mod compaction;
pub mod controller;
pub mod cost;
pub mod goals;
pub mod history;
pub mod hooks;
pub mod instructions;
pub mod lifecycle;
pub mod memory;
pub mod mentions;
pub mod network_proxy;
pub mod notify;
pub mod protocol;
pub mod provider_turn;
pub(crate) mod runtime_bash;
mod runtime_conversation_bootstrap;
mod runtime_model_route;
mod runtime_normal_tool;
pub(crate) mod runtime_special;
mod runtime_steer;
mod runtime_turn_iteration;
mod runtime_turn_loop;
mod runtime_turn_opening;
mod runtime_turn_start;
pub(crate) mod sandbox_denial;
pub mod schema_validation;
pub mod server;
pub mod server_runtime;
pub mod session;
pub mod shell_session;
mod step_context;
pub mod subagent;
pub mod subagent_execution;
pub mod tasks;
pub mod thread;
pub mod thread_store;
pub mod tool_execution;
pub mod tool_invocation;
pub(crate) mod tool_item_projection;
mod tool_router;
pub mod tool_turn;
pub mod update_check;
pub mod workflow;
pub mod workflow_execution;
pub mod worktree;

#[cfg(test)]
mod tests {
    use crate::thread_store::{SessionStore, ThreadStore};

    #[test]
    fn thread_store_module_exports_session_store_boundary() {
        fn assert_thread_store<T: ThreadStore>(store: &T) {
            let _ = store;
        }

        assert_thread_store(&SessionStore::new());
    }

    #[test]
    fn server_operation_dispatch_is_owned_by_router_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let server_source =
            std::fs::read_to_string(manifest_dir.join("src/server.rs")).expect("server source");
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");

        assert!(
            server_source.contains("mod router;"),
            "server entry module must declare a focused router submodule"
        );
        assert!(
            server_source.contains("router::dispatch_submission("),
            "server entry module must delegate decoded submissions to router"
        );
        assert!(
            !server_source.contains("match &submission.op"),
            "server entry module must not own the ClientOp dispatch match"
        );
        assert!(
            router_source.contains("pub(super) fn dispatch_submission"),
            "server router must expose submission dispatch inside the server module"
        );
        assert!(
            router_source.contains("submit::dispatch_submit_operation("),
            "server router must delegate submit-family operations"
        );
        assert!(
            router_source.contains("command_exec::dispatch_command_exec_operation("),
            "server router must delegate command exec operations"
        );
        assert!(
            router_source.contains("permission::dispatch_permission_operation("),
            "server router must delegate permission operations"
        );
    }

    #[test]
    fn runtime_special_dispatch_is_owned_by_runtime_special_module() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let lib_source =
            std::fs::read_to_string(manifest_dir.join("src/lib.rs")).expect("lib source");
        let lifecycle_source = std::fs::read_to_string(manifest_dir.join("src/lifecycle.rs"))
            .expect("lifecycle source");
        let runtime_special_source =
            std::fs::read_to_string(manifest_dir.join("src/runtime_special.rs"))
                .expect("runtime special source");

        assert!(
            lib_source.contains("pub(crate) mod runtime_special;"),
            "runtime crate must declare a focused runtime_special module"
        );

        for marker in [
            "pub enum RuntimeSpecialToolDispatch",
            "pub fn classify_dispatch",
            "pub fn execute_request_permissions_tool",
            "pub fn execute_request_permissions_tool_with_handler",
            "pub fn execute_workflow_ipc_tool",
            "pub fn execute_subagent_status_tool",
            "pub fn execute_task_list_tool",
            "pub fn execute_task_stop_tool",
            "pub fn execute_workflow_draft_tool",
        ] {
            assert!(
                runtime_special_source.contains(marker),
                "runtime_special must own runtime-special detail {marker}"
            );
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own runtime-special detail {marker}"
            );
        }
    }

    #[test]
    fn server_thread_query_dispatch_is_owned_by_thread_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/thread.rs"))
                .expect("server thread processor source");

        assert!(
            router_source.contains("mod processors;"),
            "server router must declare focused processor modules"
        );
        assert!(
            router_source.contains("thread::dispatch_query_operation("),
            "server router must delegate thread query operations to the thread processor"
        );
        for variant in [
            "ClientOp::ThreadRead",
            "ClientOp::ThreadList",
            "ClientOp::ThreadSearch",
            "ClientOp::ThreadTurnsList",
            "ClientOp::ThreadItemsList",
            "ClientOp::ThreadMetadataUpdate",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server thread processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_query_operation"),
            "server thread processor must expose query dispatch inside the router module"
        );
    }

    #[test]
    fn server_turn_control_dispatch_is_owned_by_turn_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/turn.rs"))
                .expect("server turn processor source");

        assert!(
            router_source.contains("turn::dispatch_control_operation("),
            "server router must delegate turn control operations to the turn processor"
        );
        for variant in [
            "ClientOp::TurnInterrupt",
            "ClientOp::TurnResume",
            "ClientOp::TurnSteer",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server turn processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_control_operation"),
            "server turn processor must expose control dispatch inside the router module"
        );
    }

    #[test]
    fn server_shell_dispatch_is_owned_by_shell_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/shell.rs"))
                .expect("server shell processor source");

        assert!(
            router_source.contains("shell::dispatch_shell_operation("),
            "server router must delegate shell operations to the shell processor"
        );
        for variant in [
            "ClientOp::ShellStart",
            "ClientOp::ShellWrite",
            "ClientOp::ShellUpdate",
            "ClientOp::ShellClose",
            "ClientOp::ShellResize",
            "ClientOp::ShellList",
            "ClientOp::ShellRead",
            "ClientOp::ShellKill",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server shell processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_shell_operation"),
            "server shell processor must expose shell dispatch inside the router module"
        );
    }

    #[test]
    fn server_command_exec_dispatch_is_owned_by_command_exec_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/command_exec.rs"))
                .expect("server command exec processor source");

        assert!(
            router_source.contains("command_exec::dispatch_command_exec_operation("),
            "server router must delegate command exec operations to the command exec processor"
        );
        for variant in [
            "ClientOp::CommandExec",
            "ClientOp::CommandExecWrite",
            "ClientOp::CommandExecResize",
            "ClientOp::CommandExecTerminate",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server command exec processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_command_exec_operation"),
            "server command exec processor must expose command exec dispatch inside the router module"
        );
    }

    #[test]
    fn server_permission_dispatch_is_owned_by_permission_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/permission.rs"))
                .expect("server permission processor source");

        assert!(
            router_source.contains("permission::dispatch_permission_operation("),
            "server router must delegate permission operations to the permission processor"
        );
        assert!(
            !router_source.contains("ClientOp::PermissionRespond"),
            "server router must not own PermissionRespond dispatch details"
        );
        assert!(
            processor_source.contains("ClientOp::PermissionRespond"),
            "server permission processor must own PermissionRespond dispatch details"
        );
        assert!(
            processor_source.contains("fn dispatch_permission_operation"),
            "server permission processor must expose permission dispatch inside the router module"
        );
    }

    #[test]
    fn server_submit_dispatch_is_owned_by_submit_processor() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let router_source = std::fs::read_to_string(manifest_dir.join("src/server/router.rs"))
            .expect("server router source");
        let processor_source =
            std::fs::read_to_string(manifest_dir.join("src/server/processors/submit.rs"))
                .expect("server submit processor source");

        assert!(
            router_source.contains("submit::dispatch_submit_operation("),
            "server router must delegate submit-family operations to the submit processor"
        );
        for variant in [
            "ClientOp::Submit",
            "ClientOp::ThreadStart",
            "ClientOp::ThreadResume",
            "ClientOp::ThreadFork",
        ] {
            assert!(
                !router_source.contains(variant),
                "server router must not own {variant} dispatch details"
            );
            assert!(
                processor_source.contains(variant),
                "server submit processor must own {variant} dispatch details"
            );
        }
        assert!(
            processor_source.contains("fn dispatch_submit_operation"),
            "server submit processor must expose submit dispatch inside the router module"
        );
    }

    #[test]
    fn protocol_uses_focused_submodules() {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let protocol_dir = manifest_dir.join("src/protocol");
        let facade = std::fs::read_to_string(manifest_dir.join("src/protocol.rs"))
            .expect("protocol facade source");

        for module in [
            "command_exec",
            "events",
            "permissions",
            "shell",
            "thread",
            "turn",
            "wire",
        ] {
            let module_path = protocol_dir.join(format!("{module}.rs"));
            assert!(
                module_path.is_file(),
                "protocol module {module} must live in src/protocol/{module}.rs"
            );
            assert!(
                facade.contains(&format!("mod {module};")),
                "protocol facade must declare focused module {module}"
            );
        }

        let command_exec =
            std::fs::read_to_string(protocol_dir.join("command_exec.rs")).unwrap_or_default();
        assert!(
            command_exec.contains("pub struct CommandExecOptions")
                && command_exec.contains("pub enum CommandSandboxPolicy"),
            "command execution wire types must be owned by protocol::command_exec"
        );

        let events = std::fs::read_to_string(protocol_dir.join("events.rs")).unwrap_or_default();
        assert!(
            events.contains("pub enum ServerEvent") && events.contains("pub fn write_server_event"),
            "server event serialization must be owned by protocol::events"
        );

        let permissions =
            std::fs::read_to_string(protocol_dir.join("permissions.rs")).unwrap_or_default();
        assert!(
            permissions.contains("pub enum PermissionResponseDecision")
                && permissions.contains("pub struct RequestPermissionProfile"),
            "permission response wire types must be owned by protocol::permissions"
        );

        assert!(
            facade.lines().count() <= 180,
            "protocol facade should stay small enough to show module boundaries at a glance"
        );

        for reexport in [
            "pub use command_exec::",
            "pub use events::",
            "pub use permissions::",
            "pub use shell::",
            "pub use wire::",
        ] {
            assert!(
                facade.contains(reexport),
                "protocol facade must preserve external API with {reexport}"
            );
        }
    }

    #[test]
    fn runtime_turn_context_types_are_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        for type_name in [
            "RuntimeTurnConfig",
            "RuntimeTurnDeps",
            "RuntimeTurnState",
            "RuntimeTurnExecution",
        ] {
            assert!(
                !agent_loop_source.contains(&format!("struct {type_name}")),
                "agent_loop must not own runtime turn context type {type_name}"
            );
            assert!(
                !agent_loop_source.contains(&format!("impl<'a> {type_name}")),
                "agent_loop must not own runtime turn context behavior {type_name}"
            );
            assert!(
                lifecycle_source.contains(&format!("struct {type_name}")),
                "lifecycle must own runtime turn context type {type_name}"
            );
            assert!(
                lifecycle_source.contains(&format!("impl<'a> {type_name}")),
                "lifecycle must own runtime turn context behavior {type_name}"
            );
        }
    }

    #[test]
    fn thread_steer_handle_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        assert!(
            !agent_loop_source.contains("struct ThreadSteerHandle"),
            "agent_loop must not own the thread turn steer handle"
        );
        assert!(
            !agent_loop_source.contains("impl ThreadSteerHandle"),
            "agent_loop must not own thread turn steer handle behavior"
        );
        assert!(
            lifecycle_source.contains("struct ThreadSteerHandle"),
            "lifecycle must own the thread turn steer handle"
        );
        assert!(
            lifecycle_source.contains("impl ThreadSteerHandle"),
            "lifecycle must own thread turn steer handle behavior"
        );
    }

    #[test]
    fn runtime_steer_step_is_owned_by_runtime_steer_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_steer_source =
            std::fs::read_to_string("src/runtime_steer.rs").expect("runtime steer source");

        assert!(
            !agent_loop_source.contains("struct RuntimeSteerStep"),
            "agent_loop must not own runtime steer step state"
        );
        assert!(
            !agent_loop_source.contains("impl RuntimeSteerStep"),
            "agent_loop must not own runtime steer step behavior"
        );
        assert!(
            lib_source.contains("mod runtime_steer;"),
            "runtime crate must declare a focused runtime_steer module"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeSteerStep"),
            "lifecycle must not own runtime steer step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeSteerStep"),
            "lifecycle must not own runtime steer step behavior"
        );
        assert!(
            !agent_loop_source.contains("for input in steer_handle.drain()"),
            "agent_loop must not directly drain steer inputs into conversation"
        );
        for marker in [
            "struct RuntimeSteerStep",
            "struct RuntimeSteerInput",
            "impl RuntimeSteerStep",
            "for steer_input in steer_handle.drain()",
            "input.conversation.add_user(steer_input)",
            "writer.append_message(message)",
        ] {
            assert!(
                runtime_steer_source.contains(marker),
                "runtime_steer must own runtime steer detail {marker}"
            );
        }
    }

    #[test]
    fn agent_loop_context_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        assert!(
            !agent_loop_source.contains("struct AgentLoopContext"),
            "agent_loop must not own the runtime agent loop context"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> AgentLoopContext"),
            "agent_loop must not own runtime agent loop context behavior"
        );
        assert!(
            lifecycle_source.contains("struct AgentLoopContext"),
            "lifecycle must own the runtime agent loop context"
        );
        assert!(
            lifecycle_source.contains("impl<'a> AgentLoopContext"),
            "lifecycle must own runtime agent loop context behavior"
        );
    }

    #[test]
    fn agent_tool_policy_context_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("struct AgentToolPolicyContext"),
            "agent_loop must not own agent tool policy context"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> AgentToolPolicyContext"),
            "agent_loop must not own agent tool policy behavior"
        );
        assert!(
            tool_invocation_source.contains("struct AgentToolPolicyContext"),
            "tool_invocation must own agent tool policy context"
        );
        assert!(
            tool_invocation_source.contains("impl<'a> AgentToolPolicyContext"),
            "tool_invocation must own agent tool policy behavior"
        );
    }

    #[test]
    fn agent_tool_schema_override_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        for marker in [
            "deepseek_tools_schema_for_allowed_names_with_mcp_and_external",
            "deepseek_tools_schema_for_type_with_mcp_and_external",
            "deepseek_tools_schema_with_mcp_and_external",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own provider tool schema override detail {marker}"
            );
            assert!(
                tool_invocation_source.contains(marker),
                "tool_invocation must own provider tool schema override detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnSetupStep"),
            "agent_loop must delegate provider tool schema override through runtime turn setup"
        );
        assert!(
            include_str!("lifecycle.rs").contains("provider_config_for_agent_loop"),
            "lifecycle setup must delegate provider tool schema override through provider config construction"
        );
        assert!(
            tool_invocation_source.contains("pub(crate) fn provider_tool_schema_override"),
            "tool_invocation must expose provider tool schema override construction"
        );
    }

    #[test]
    fn provider_tool_request_extraction_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("ProviderStep::ToolCall"),
            "agent_loop must not match provider tool-call steps directly"
        );
        assert!(
            !agent_loop_source.contains("tool_requests_from_provider_steps("),
            "agent_loop must delegate provider tool request extraction through turn loop"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate provider tool request extraction through turn loop"
        );
        assert!(
            tool_invocation_source.contains("pub(crate) fn tool_requests_from_provider_steps"),
            "tool_invocation must expose provider tool request extraction"
        );
        assert!(
            tool_invocation_source.contains("ProviderStep::ToolCall"),
            "tool_invocation must own provider tool-call step matching"
        );
    }

    #[test]
    fn normal_tool_execution_entrypoint_is_owned_by_tool_execution_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_execution_source = include_str!("tool_execution.rs");

        assert!(
            !agent_loop_source.contains("fn execute_tool_with_approval"),
            "agent_loop must not own normal tool execution entrypoint"
        );
        assert!(
            !agent_loop_source.contains("execute_tool_with_approval("),
            "agent_loop must delegate normal tool execution through tool-turn dispatch runner"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate normal tool turn execution through turn loop"
        );
        assert!(
            tool_execution_source.contains("pub(crate) fn execute_tool_with_approval"),
            "tool_execution must expose normal tool execution entrypoint"
        );
        assert!(
            tool_execution_source.contains("ToolExecutionActor::new"),
            "tool_execution must own tool actor construction"
        );
    }

    #[test]
    fn tool_request_cursor_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        for marker in ["let mut index = 0", "index += 1", "index = batch_end"] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own tool request cursor detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("ToolRequestCursor"),
            "agent_loop must delegate tool request cursor state through tool-turn dispatch"
        );
        assert!(
            !tool_invocation_source.contains("struct ToolRequestCursor"),
            "tool_invocation must not own tool-turn cursor state"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate tool request cursor use through turn loop"
        );
        assert!(
            tool_turn_source.contains("pub(crate) struct ToolRequestCursor"),
            "tool_turn must own tool request cursor state"
        );
        assert!(
            tool_turn_source.contains("fn advance_to"),
            "tool_turn must own cursor batch advancement"
        );
    }

    #[test]
    fn tool_turn_outcome_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            !agent_loop_source.contains("return Ok(AgentLoopResult {\n                            status,\n                            final_message: None,\n                            error,\n                        });"),
            "agent_loop must not own tool-turn terminal result shape"
        );
        assert!(
            !agent_loop_source.contains("ToolTurnOutcome"),
            "agent_loop must delegate tool-turn outcome state through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseOutcome"),
            "agent_loop must delegate provider response outcome folding through lifecycle"
        );
        assert!(
            !tool_invocation_source.contains("enum ToolTurnOutcome"),
            "tool_invocation must not own tool-turn outcome state"
        );
        assert!(
            tool_turn_source.contains("pub(crate) enum ToolTurnOutcome"),
            "tool_turn must own tool-turn outcome state"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn terminal_tool_turn"),
            "tool_turn must expose terminal tool-turn construction"
        );
    }

    #[test]
    fn normal_tool_turn_runner_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        for marker in [
            "execute_tool_with_approval(",
            "ToolExecutionContext::new",
            "record_normal_tool_result(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own normal tool-turn runner detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("run_normal_tool_turn("),
            "agent_loop must delegate normal tool-turn execution through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate normal tool-turn execution through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn run_normal_tool_turn"),
            "tool_invocation must not own normal tool-turn runner"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn run_normal_tool_turn"),
            "tool_turn must expose normal tool-turn runner"
        );
        assert!(
            tool_turn_source.contains("execute_tool_with_approval"),
            "tool_turn must compose normal tool execution"
        );
        assert!(
            tool_turn_source.contains("record_normal_tool_result"),
            "tool_turn must compose normal tool result recording"
        );
    }

    #[test]
    fn normal_tool_turn_runner_uses_grouped_context() {
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeNormalToolTurnContext"),
            "tool_turn must group normal tool-turn inputs into RuntimeNormalToolTurnContext"
        );
        assert!(
            tool_turn_source.contains("context: RuntimeNormalToolTurnContext<"),
            "run_normal_tool_turn must accept the grouped normal tool-turn context"
        );
        assert!(
            tool_turn_source.contains("run_normal_tool_turn(RuntimeNormalToolTurnContext"),
            "run_tool_turns must pass normal tool-turn inputs as one grouped context"
        );
        assert!(
            !tool_turn_source.contains("run_normal_tool_turn(\n            config,"),
            "run_tool_turns must not call run_normal_tool_turn with the old long argument list"
        );
        for field_name in [
            "config:",
            "cwd:",
            "events:",
            "sink:",
            "conversation:",
            "history_writer:",
            "tool_request:",
            "subagent_depth:",
            "emit_deltas:",
            "policy:",
            "instructions:",
            "memory:",
            "mcp_registry:",
            "hooks:",
            "cost_tracker:",
            "cancel:",
            "task_registry:",
            "background_workflows:",
            "workflow_ipc:",
            "permission_overlay:",
            "permission_handler:",
            "child_executor:",
            "workflow_child_executor:",
        ] {
            assert!(
                tool_turn_source.contains(field_name),
                "RuntimeNormalToolTurnContext must carry normal tool-turn field {field_name}"
            );
        }
    }

    #[test]
    fn bash_runtime_runner_uses_grouped_invocation_context() {
        let normal_tool_source = include_str!("runtime_normal_tool.rs");
        let runtime_bash_source = include_str!("runtime_bash.rs");

        assert!(
            runtime_bash_source.contains("pub(crate) struct RuntimeBashInvocationContext"),
            "runtime_bash must group shell-session bash inputs into RuntimeBashInvocationContext"
        );
        assert!(
            runtime_bash_source.contains("context: RuntimeBashInvocationContext"),
            "execute_bash_with_shell_session must accept the grouped bash invocation context"
        );
        assert!(
            !runtime_bash_source.contains(
                "#[allow(clippy::too_many_arguments)]\npub(crate) fn execute_bash_with_shell_session"
            ),
            "runtime_bash must not need a too_many_arguments escape hatch for bash invocation"
        );
        assert!(
            normal_tool_source.contains("RuntimeBashInvocationContext"),
            "runtime_normal_tool must construct the grouped bash invocation context"
        );
        assert!(
            !normal_tool_source
                .contains("execute_bash_with_shell_session(\n                config,"),
            "runtime_normal_tool must not pass bash execution state through the old long argument list"
        );
        for field_name in [
            "config:",
            "request:",
            "cwd:",
            "additional_roots:",
            "output_truncation:",
            "shell_timeout_secs:",
            "task_registry:",
            "cancel:",
            "permission_handler:",
            "permission_overlay:",
        ] {
            assert!(
                runtime_bash_source.contains(field_name),
                "RuntimeBashInvocationContext must carry bash field {field_name}"
            );
        }
    }

    #[test]
    fn tool_turn_dispatch_uses_grouped_context() {
        let provider_turn_source = include_str!("provider_turn.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            tool_turn_source.contains("pub(crate) struct RuntimeToolTurnsContext"),
            "tool_turn must group dispatch-loop inputs into RuntimeToolTurnsContext"
        );
        assert!(
            tool_turn_source.contains("context: RuntimeToolTurnsContext<"),
            "run_tool_turns must accept the grouped dispatch-loop context"
        );
        assert!(
            provider_turn_source.contains("run_tool_turns(RuntimeToolTurnsContext"),
            "provider response must pass tool-turn dispatch inputs as one grouped context"
        );
        assert!(
            !provider_turn_source.contains("run_tool_turns(\n            step_context,"),
            "provider response must not call run_tool_turns with the old long argument list"
        );
        for field_name in [
            "step_context:",
            "events:",
            "sink:",
            "conversation:",
            "history_writer:",
            "tool_requests:",
            "cost_tracker:",
            "background_workflows:",
            "child_executor:",
            "workflow_child_executor:",
            "batch_child_executor:",
        ] {
            assert!(
                tool_turn_source.contains(field_name),
                "RuntimeToolTurnsContext must carry tool-turn dispatch field {field_name}"
            );
        }
    }

    #[test]
    fn readonly_tool_turn_runner_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        for marker in ["execute_readonly_batch(", "record_readonly_batch_results("] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own readonly tool-turn runner detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("run_readonly_tool_turn("),
            "agent_loop must delegate readonly tool-turn execution through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate readonly tool-turn execution through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn run_readonly_tool_turn"),
            "tool_invocation must not own readonly tool-turn runner"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn run_readonly_tool_turn"),
            "tool_turn must expose readonly tool-turn runner"
        );
        assert!(
            tool_turn_source.contains("execute_readonly_batch"),
            "tool_turn must compose readonly batch execution"
        );
        assert!(
            tool_turn_source.contains("record_readonly_batch_results"),
            "tool_turn must compose readonly batch result recording"
        );
    }

    #[test]
    fn child_tool_policy_gate_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("fn child_tool_policy_failure"),
            "agent_loop must not own child tool policy gate behavior"
        );
        assert!(
            tool_invocation_source.contains("fn child_tool_policy_failure"),
            "tool_invocation must own child tool policy gate behavior"
        );
        assert!(
            tool_invocation_source.contains("pub(crate) fn reject_disallowed_child_tool"),
            "tool_invocation must expose child tool policy gate to the agent loop"
        );
    }

    #[test]
    fn normal_tool_result_recording_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        for marker in [
            "record_plan_state_for_agent(",
            "status == RunStatus::ApprovalRequired",
            "tool_request.name == tool_types::ToolName::Subagent",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own normal tool result detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("record_normal_tool_result("),
            "agent_loop must delegate normal tool result recording through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate normal tool turn recording through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn record_normal_tool_result"),
            "tool_invocation must not own normal tool result recording"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn record_normal_tool_result"),
            "tool_turn must expose normal tool result recording"
        );
        assert!(
            tool_turn_source.contains("record_plan_state_for_agent"),
            "tool_turn must own normal tool plan-state recording"
        );
        assert!(
            tool_turn_source.contains("record_tool_result_for_agent"),
            "tool_turn must own normal tool result recording"
        );
    }

    #[test]
    fn readonly_tool_batch_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            !agent_loop_source.contains("fn execute_readonly_batch"),
            "agent_loop must not own readonly tool batch execution"
        );
        assert!(
            !tool_invocation_source.contains("fn execute_readonly_batch"),
            "tool_invocation must not own readonly tool batch execution"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn execute_readonly_batch"),
            "tool_turn must expose readonly tool batch execution"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn should_run_readonly_batch"),
            "tool_turn must expose readonly batch planning"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn collect_readonly_batch"),
            "tool_turn must expose readonly batch range collection"
        );
        for marker in [
            "orca_tools::should_run_readonly_batch",
            "orca_tools::collect_readonly_batch",
            "run_readonly_batch_parallel_with_policy",
            "HookEvent::PreToolUse",
            "HookEvent::PostToolUse",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own readonly batch detail {marker}"
            );
        }
    }

    #[test]
    fn readonly_tool_batch_result_recording_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        assert!(
            !agent_loop_source.contains("record_tool_result_for_agent("),
            "agent_loop must not own readonly batch result recording"
        );
        assert!(
            !agent_loop_source.contains("record_readonly_batch_results("),
            "agent_loop must delegate readonly batch result recording through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate readonly tool turn recording through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn record_readonly_batch_results"),
            "tool_invocation must not own readonly batch result recording"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn record_readonly_batch_results"),
            "tool_turn must expose readonly batch result recording"
        );
        assert!(
            tool_turn_source.contains("record_tool_result_for_agent"),
            "tool_turn must reuse shared session tool result recording"
        );
    }

    #[test]
    fn subagent_batch_result_recording_is_owned_by_subagent_execution_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let subagent_execution_source = include_str!("subagent_execution.rs");

        assert!(
            !agent_loop_source.contains("for (status, result) in results"),
            "agent_loop must not own subagent batch result recording"
        );
        assert!(
            subagent_execution_source.contains("pub(crate) fn record_subagent_batch_results"),
            "subagent_execution must expose subagent batch result recording"
        );
        assert!(
            subagent_execution_source.contains("record_tool_result_for_agent"),
            "subagent_execution must record subagent batch tool results"
        );
        assert!(
            subagent_execution_source.contains("RunStatus::ApprovalRequired"),
            "subagent_execution must own subagent batch approval folding"
        );
    }

    #[test]
    fn subagent_batch_tool_turn_runner_is_owned_by_subagent_execution_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let subagent_execution_source = include_str!("subagent_execution.rs");

        for marker in [
            "execute_subagent_batch(",
            "record_subagent_batch_results(",
            "SubagentBatchRecordOutcome",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own subagent batch tool-turn detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("run_subagent_batch_tool_turn("),
            "agent_loop must delegate subagent batch tool turns through tool-turn dispatch"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate subagent batch tool turns through turn loop"
        );
        assert!(
            subagent_execution_source.contains("pub(crate) fn run_subagent_batch_tool_turn"),
            "subagent_execution must expose subagent batch tool-turn runner"
        );
        assert!(
            subagent_execution_source.contains("execute_subagent_batch"),
            "subagent_execution must compose subagent batch execution"
        );
        assert!(
            subagent_execution_source.contains("record_subagent_batch_results"),
            "subagent_execution must compose subagent batch result recording"
        );
    }

    #[test]
    fn tool_turn_dispatch_loop_is_owned_by_tool_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");
        let tool_turn_source = include_str!("tool_turn.rs");

        for marker in [
            "let mut cursor = ToolRequestCursor::new",
            "while let Some(tool_request)",
            "collect_subagent_batch(",
            "collect_readonly_batch(",
            "run_normal_tool_turn(",
            "run_readonly_tool_turn(",
            "run_subagent_batch_tool_turn(",
            "reject_disallowed_child_tool(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own tool-turn dispatch loop detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("run_tool_turns("),
            "agent_loop must delegate tool-turn dispatch through turn loop"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate tool-turn dispatch through turn loop"
        );
        assert!(
            !tool_invocation_source.contains("fn run_tool_turns"),
            "tool_invocation must not own the tool-turn dispatch runner"
        );
        assert!(
            tool_turn_source.contains("pub(crate) fn run_tool_turns"),
            "tool_turn must expose the tool-turn dispatch runner"
        );
        assert!(
            tool_turn_source.contains("ToolRequestCursor::new"),
            "tool_turn must own dispatch cursor state"
        );
        assert!(
            tool_turn_source.contains("run_normal_tool_turn"),
            "tool_turn must compose normal tool turns"
        );
        assert!(
            tool_turn_source.contains("run_readonly_tool_turn"),
            "tool_turn must compose readonly tool turns"
        );
        assert!(
            tool_turn_source.contains("run_subagent_batch_tool_turn"),
            "tool_turn must compose subagent batch tool turns"
        );
    }

    #[test]
    fn agent_conversation_context_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        assert!(
            !agent_loop_source.contains("struct AgentConversationContext"),
            "agent_loop must not own agent conversation context"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> AgentConversationContext"),
            "agent_loop must not own agent conversation context behavior"
        );
        assert!(
            session_source.contains("struct AgentConversationContext"),
            "session must own agent conversation context"
        );
        assert!(
            session_source.contains("impl<'a> AgentConversationContext"),
            "session must own agent conversation context behavior"
        );
    }

    #[test]
    fn agent_tool_result_recording_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        assert!(
            !agent_loop_source.contains("format_tool_result_for_model"),
            "agent_loop must not own tool result model-content formatting"
        );
        assert!(
            !agent_loop_source.contains("append_tool_result_message"),
            "agent_loop must not own tool result history writing"
        );
        assert!(
            session_source.contains("pub(crate) fn record_tool_result_for_agent"),
            "session must expose agent tool result recording"
        );
        assert!(
            session_source.contains("format_tool_result_for_model"),
            "session must own tool result model-content formatting"
        );
        assert!(
            session_source.contains("append_tool_result_message"),
            "session must own tool result history writing"
        );
    }

    #[test]
    fn agent_plan_state_recording_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        for marker in [
            "orca_tools::update_plan::parse_args",
            "replace_plan_state",
            "append_plan_state",
            "format_context_message",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own plan-state recording detail {marker}"
            );
            assert!(
                session_source.contains(marker),
                "session must own plan-state recording detail {marker}"
            );
        }
        assert!(
            session_source.contains("pub(crate) fn record_plan_state_for_agent"),
            "session must expose agent plan-state recording"
        );
    }

    #[test]
    fn agent_assistant_response_recording_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        assert!(
            !agent_loop_source.contains("conversation.add_assistant"),
            "agent_loop must not own assistant response conversation recording"
        );
        assert!(
            session_source.contains("pub(crate) fn record_assistant_response_for_agent"),
            "session must expose agent assistant response recording"
        );
        assert!(
            session_source.contains("add_assistant"),
            "session must own assistant response conversation recording"
        );
        assert!(
            session_source.contains("append_message(message)"),
            "session must own assistant response history writing"
        );
    }

    #[test]
    fn final_memory_extraction_is_owned_by_memory_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let memory_source = include_str!("memory.rs");

        for marker in [
            "model::auxiliary_model",
            "memory::extract_project_memory(",
            "memory extraction failed:",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own final memory extraction detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("extract_project_memory_after_final_response("),
            "agent_loop must delegate final memory extraction through turn loop"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate final memory extraction through turn loop"
        );
        assert!(
            memory_source.contains("pub(crate) fn extract_project_memory_after_final_response"),
            "memory module must expose final memory extraction helper"
        );
        assert!(
            memory_source.contains("model::auxiliary_model"),
            "memory module must own auxiliary model selection for memory extraction"
        );
    }

    #[test]
    fn agent_initial_history_recording_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        for marker in ["writer.append_message", "append_summary_state"] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own initial history recording detail {marker}"
            );
            assert!(
                session_source.contains(marker),
                "session must own initial history recording detail {marker}"
            );
        }
        assert!(
            session_source.contains("pub(crate) fn record_initial_history_for_agent"),
            "session must expose initial history recording"
        );
    }

    #[test]
    fn runtime_conversation_bootstrap_step_is_owned_by_runtime_conversation_bootstrap_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_conversation_bootstrap_source =
            std::fs::read_to_string("src/runtime_conversation_bootstrap.rs")
                .expect("runtime conversation bootstrap source");
        let lib_source = include_str!("lib.rs");

        for marker in [
            "let mut owned_conversation",
            "bootstrap_agent_conversation_for_loop(",
            "record_initial_history_for_agent(",
            "resumed.is_some()",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime conversation bootstrap detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeConversationBootstrapStep"),
            "agent_loop must delegate runtime conversation bootstrap"
        );
        assert!(
            lib_source.contains("mod runtime_conversation_bootstrap;"),
            "runtime crate must declare a focused runtime_conversation_bootstrap module"
        );
        for marker in [
            "struct RuntimeConversationBootstrapStep",
            "impl RuntimeConversationBootstrapStep",
            "bootstrap_agent_conversation_for_loop(",
            "record_initial_history_for_agent(",
            "resumed.is_some()",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own runtime conversation bootstrap detail {marker}"
            );
            assert!(
                runtime_conversation_bootstrap_source.contains(marker),
                "runtime_conversation_bootstrap must own runtime conversation bootstrap detail {marker}"
            );
        }
    }

    #[test]
    fn agent_conversation_bootstrap_is_owned_by_session_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let session_source = include_str!("session.rs");

        for marker in [
            "agent_common::build_agent_system_prompt",
            "thread_store::resume_conversation",
            "Conversation::new()",
            "add_system(system_prompt)",
            "replace_skill_context",
            "add_user(prompt.to_string())",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own conversation bootstrap detail {marker}"
            );
            assert!(
                session_source.contains(marker),
                "session must own conversation bootstrap detail {marker}"
            );
        }
        assert!(
            session_source.contains("pub(crate) fn bootstrap_agent_conversation"),
            "session must expose agent conversation bootstrap"
        );
        assert!(
            agent_loop_source.contains("RuntimeConversationBootstrapStep"),
            "agent_loop must delegate system prompt construction with conversation bootstrap"
        );
        assert!(
            session_source.contains("pub(crate) fn bootstrap_agent_conversation_for_loop"),
            "session must expose agent-loop conversation bootstrap"
        );
    }

    #[test]
    fn agent_provider_config_construction_is_owned_by_tool_invocation_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_invocation_source = include_str!("tool_invocation.rs");

        assert!(
            !agent_loop_source.contains("ProviderConfig {"),
            "agent_loop must not own provider config construction"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnSetupStep"),
            "agent_loop must delegate runtime turn setup"
        );
        assert!(
            tool_invocation_source.contains("pub(crate) fn provider_config_for_agent_loop"),
            "tool_invocation must expose agent-loop provider config construction"
        );
        assert!(
            tool_invocation_source.contains("provider_tool_schema_override"),
            "tool_invocation must keep provider config close to tool schema selection"
        );
    }

    #[test]
    fn agent_tool_approval_policy_construction_is_owned_by_tool_execution_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let tool_execution_source = include_str!("tool_execution.rs");

        assert!(
            !agent_loop_source.contains("ApprovalPolicy::new"),
            "agent_loop must not own tool approval policy construction"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnSetupStep"),
            "agent_loop must delegate runtime turn setup"
        );
        assert!(
            tool_execution_source.contains("pub(crate) fn policy_for_tool_execution"),
            "tool_execution must expose approval policy construction"
        );
        assert!(
            tool_execution_source.contains("with_permission_rules"),
            "tool_execution must preserve config permission rules in approval policy"
        );
    }

    #[test]
    fn tool_execution_approval_gate_uses_grouped_context() {
        let tool_execution_source = include_str!("tool_execution.rs");

        assert!(
            tool_execution_source.contains("struct ToolApprovalGateContext"),
            "tool_execution must group approval gate inputs into ToolApprovalGateContext"
        );
        assert!(
            tool_execution_source.contains("fn handle_approval<W: io::Write>(")
                && tool_execution_source.contains("context: ToolApprovalGateContext<"),
            "ToolExecutionActor::handle_approval must accept the grouped approval gate context"
        );
        assert!(
            tool_execution_source.contains("self.handle_approval(ToolApprovalGateContext"),
            "ToolExecutionActor::execute must pass approval gate inputs as one grouped context"
        );
        assert!(
            !tool_execution_source.contains("self.handle_approval(\n            config,"),
            "ToolExecutionActor::execute must not call handle_approval with the old long argument list"
        );
        for field_name in [
            "config:",
            "events:",
            "sink:",
            "tool_request:",
            "invocation:",
            "policy:",
            "strict_auto_review:",
            "emit_deltas:",
        ] {
            assert!(
                tool_execution_source.contains(field_name),
                "ToolApprovalGateContext must carry approval input field {field_name}"
            );
        }
    }

    #[test]
    fn runtime_compaction_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let compaction_source = include_str!("compaction.rs");

        assert!(
            !agent_loop_source.contains("struct RuntimeCompactionStep"),
            "agent_loop must not own runtime compaction step state"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> RuntimeCompactionStep"),
            "agent_loop must not own runtime compaction step behavior"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeCompactionStep"),
            "lifecycle must not keep owning runtime compaction step state after extraction"
        );
        assert!(
            !lifecycle_source.contains("impl<'a, W: io::Write> RuntimeCompactionStep<'a, W>"),
            "lifecycle must not keep owning runtime compaction step behavior after extraction"
        );
        assert!(
            compaction_source.contains("struct RuntimeCompactionStep"),
            "compaction module must own runtime compaction step state"
        );
        assert!(
            compaction_source.contains("impl<'a, W: io::Write> RuntimeCompactionStep<'a, W>"),
            "compaction module must own runtime compaction step behavior"
        );

        for marker in [
            "HookEvent::OnBudgetWarning",
            "HookEvent::PreCompact",
            "HookEvent::PostCompact",
            "compact_with_summary(",
            "append_compaction(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime compaction detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_provider_turn_step_is_owned_by_provider_turn_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");

        assert!(
            !agent_loop_source.contains("struct RuntimeProviderTurnStep"),
            "agent_loop must not own runtime provider turn step state"
        );
        assert!(
            !agent_loop_source.contains("impl<'a> RuntimeProviderTurnStep"),
            "agent_loop must not own runtime provider turn step behavior"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeProviderTurnStep"),
            "lifecycle must not own runtime provider turn step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeProviderTurnStep"),
            "lifecycle must not own runtime provider turn step behavior"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeProviderTurnStep"),
            "provider_turn must own runtime provider turn step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderTurnStep"),
            "provider_turn must own runtime provider turn step behavior"
        );

        for marker in [
            "assistant_reasoning_delta",
            "assistant_message_delta",
            "usage_updated",
            "provider_replay_updated",
            "ProviderStep::ReplayState",
            "ProviderStep::Error",
            "is_prompt_too_long_error",
            "compaction.emit_error(",
            "append_usage(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own provider turn detail {marker}"
            );
            assert!(
                provider_turn_source.contains(marker),
                "provider_turn must own provider turn detail {marker}"
            );
        }
    }

    #[test]
    fn thread_store_trait_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        assert!(
            !history_source.contains("pub trait ThreadStore"),
            "history must not own the storage-neutral ThreadStore trait"
        );
        assert!(
            thread_store_source.contains("pub trait ThreadStore"),
            "thread_store must own the storage-neutral ThreadStore trait"
        );
    }

    #[test]
    fn thread_store_uses_focused_submodules() {
        let thread_store_source = include_str!("thread_store.rs");
        let types_source = include_str!("thread_store/types.rs");
        let local_source = include_str!("thread_store/local.rs");
        let live_thread_source = include_str!("thread_store/live_thread.rs");
        let projection_source = include_str!("thread_store/projection.rs");
        let pagination_source = include_str!("thread_store/pagination.rs");
        let writer_source = include_str!("thread_store/writer.rs");

        assert!(
            thread_store_source.contains("mod types;")
                && thread_store_source.contains("mod local;")
                && thread_store_source.contains("mod live_thread;")
                && thread_store_source.contains("mod projection;")
                && thread_store_source.contains("mod pagination;")
                && thread_store_source.contains("mod writer;"),
            "thread_store.rs should be a facade over focused storage modules"
        );
        assert!(
            types_source.contains("pub trait ThreadStore"),
            "thread_store/types.rs must own storage-neutral types and trait"
        );
        assert!(
            local_source.contains("impl ThreadStore for JsonlThreadStore"),
            "thread_store/local.rs must own JSONL-backed ThreadStore behavior"
        );
        assert!(
            live_thread_source.contains("pub struct LiveThread"),
            "thread_store/live_thread.rs must own live thread handles"
        );
        assert!(
            projection_source.contains("messages_to_thread_turns"),
            "thread_store/projection.rs must own thread projection helpers"
        );
        assert!(
            pagination_source.contains("page_vec"),
            "thread_store/pagination.rs must own pagination helpers"
        );
        assert!(
            writer_source.contains("pub struct SessionWriter"),
            "thread_store/writer.rs must own JSONL writing"
        );
    }

    #[test]
    fn jsonl_thread_store_impl_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        assert!(
            !history_source.contains("impl ThreadStore for JsonlThreadStore"),
            "history must not own the JSONL ThreadStore trait implementation"
        );
        assert!(
            thread_store_source.contains("impl ThreadStore for JsonlThreadStore"),
            "thread_store must own the JSONL ThreadStore trait implementation"
        );
    }

    #[test]
    fn thread_projection_helpers_are_owned_by_focused_thread_store_modules() {
        let history_source = include_str!("history.rs");
        let facade_source = include_str!("thread_store.rs");
        let local_source = include_str!("thread_store/local.rs");
        let projection_source = include_str!("thread_store/projection.rs");
        let pagination_source = include_str!("thread_store/pagination.rs");

        for function_name in [
            "thread_summary_matches",
            "thread_summary_matches_filters",
            "sort_thread_summaries",
            "sort_thread_search_hits",
        ] {
            let signature = format!("fn {function_name}(");
            assert!(
                !history_source.contains(&signature) && !facade_source.contains(&signature),
                "history/facade must not own ThreadStore summary helper {function_name}"
            );
            assert!(
                local_source.contains(&signature),
                "thread_store/local.rs must own ThreadStore summary helper {function_name}"
            );
        }

        for function_name in [
            "message_to_thread_json",
            "stored_message_to_thread_json",
            "messages_to_thread_turns",
            "messages_to_thread_items",
            "stored_messages_to_thread_turns",
            "stored_messages_to_thread_items",
            "next_turn_id_for_messages",
        ] {
            let signature = format!("fn {function_name}(");
            assert!(
                !history_source.contains(&signature) && !facade_source.contains(&signature),
                "history/facade must not own ThreadStore projection helper {function_name}"
            );
            assert!(
                projection_source.contains(&signature),
                "thread_store/projection.rs must own ThreadStore projection helper {function_name}"
            );
        }

        for function_name in ["page_thread_turns", "page_thread_items", "page_vec"] {
            let plain_fn = format!("fn {function_name}(");
            let generic_fn = format!("fn {function_name}<");
            assert!(
                !history_source.contains(&plain_fn)
                    && !history_source.contains(&generic_fn)
                    && !facade_source.contains(&plain_fn)
                    && !facade_source.contains(&generic_fn),
                "history/facade must not own ThreadStore pagination helper {function_name}"
            );
            assert!(
                pagination_source.contains(&plain_fn) || pagination_source.contains(&generic_fn),
                "thread_store/pagination.rs must own ThreadStore pagination helper {function_name}"
            );
        }
    }

    #[test]
    fn tool_item_projection_helpers_are_owned_by_shared_projection_module() {
        let server_runtime_source = include_str!("server_runtime.rs");
        let thread_store_source = include_str!("thread_store.rs");
        let thread_projection_source = include_str!("thread_store/projection.rs");
        let projection_source = include_str!("tool_item_projection.rs");

        for function_name in [
            "mcp_tool_parts",
            "parse_json_or_null",
            "mcp_result_from_content",
            "mcp_tool_started_item",
            "dynamic_tool_started_item",
            "mcp_tool_completed_item",
            "dynamic_tool_completed_item",
            "file_change_started_item",
            "file_change_completed_item",
            "workflow_started_item",
            "workflow_completed_item",
            "persisted_command_execution_started_item",
            "persisted_command_execution_completed_item",
            "persisted_file_change_started_item",
            "persisted_file_change_completed_item",
            "complete_projected_tool_item",
            "tool_error_object_from_value",
            "tool_status_is_completed",
        ] {
            let signature = format!("fn {function_name}(");
            assert!(
                !server_runtime_source.contains(&signature),
                "server_runtime must not own shared tool item projection helper {function_name}"
            );
            assert!(
                !thread_store_source.contains(&signature)
                    && !thread_projection_source.contains(&signature),
                "thread_store must not own shared tool item projection helper {function_name}"
            );
            assert!(
                projection_source.contains(&signature),
                "tool_item_projection must own shared tool item projection helper {function_name}"
            );
        }

        for completed_constructor in [
            "mcp_tool_completed_item(",
            "dynamic_tool_completed_item(",
            "persisted_command_execution_completed_item(",
            "persisted_file_change_completed_item(",
        ] {
            assert!(
                !thread_projection_source.contains(completed_constructor),
                "thread_store/projection.rs must complete projected tool items through the shared projection helper, not call {completed_constructor} directly"
            );
        }
    }

    #[test]
    fn server_thread_runtime_owns_agent_state_through_runtime_thread() {
        let server_runtime_source = include_str!("server_runtime.rs");

        assert!(
            server_runtime_source.contains("thread: RuntimeThread"),
            "ServerThread must hold runtime-owned agent state through RuntimeThread"
        );
        for forbidden in [
            "InteractiveSession",
            "RuntimeSessionLifecycle",
            "ThreadTurnExecutor",
        ] {
            assert!(
                !server_runtime_source.contains(forbidden),
                "server_runtime must not directly own or assemble {forbidden}; use RuntimeThread"
            );
        }
    }

    #[test]
    fn headless_run_inner_enters_agent_loop_through_runtime_thread() {
        let controller_source = include_str!("controller.rs");
        let run_inner_source = controller_source
            .split("fn run_inner")
            .nth(1)
            .and_then(|source| source.split("pub fn run_thread_turn_to_writer").next())
            .expect("controller run_inner body");

        assert!(
            run_inner_source.contains("RuntimeThread::start"),
            "headless run_inner must create long-lived agent state through RuntimeThread"
        );
        assert!(
            run_inner_source.contains(".run_request_with_event_factory("),
            "headless run_inner must delegate turn execution through RuntimeThread"
        );
        for forbidden in [
            "RuntimeSessionLifecycle::new(new_run_id())",
            "TaskRegistry::new_for_cwd",
            "run_agent_loop(",
        ] {
            assert!(
                !run_inner_source.contains(forbidden),
                "headless run_inner must not directly assemble {forbidden}; use RuntimeThread"
            );
        }
    }

    #[test]
    fn jsonl_thread_store_type_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        assert!(
            !history_source.contains("pub struct JsonlThreadStore"),
            "history must not own the JSONL ThreadStore backend type"
        );
        assert!(
            !history_source.contains("pub type SessionStore"),
            "history must not own the SessionStore compatibility alias"
        );
        assert!(
            thread_store_source.contains("pub struct JsonlThreadStore"),
            "thread_store must own the JSONL ThreadStore backend type"
        );
        assert!(
            thread_store_source.contains("pub type SessionStore"),
            "thread_store must own the SessionStore compatibility alias"
        );
    }

    #[test]
    fn thread_store_api_types_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        for type_name in [
            "StoredThreadProjection",
            "ThreadListFilters",
            "SortDirection",
            "TurnItemsView",
        ] {
            assert!(
                !history_source.contains(&format!("pub struct {type_name}"))
                    && !history_source.contains(&format!("pub enum {type_name}")),
                "history must not own ThreadStore API type {type_name}"
            );
            assert!(
                thread_store_source.contains(&format!("pub struct {type_name}"))
                    || thread_store_source.contains(&format!("pub enum {type_name}")),
                "thread_store must own ThreadStore API type {type_name}"
            );
        }
    }

    #[test]
    fn live_thread_handle_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/live_thread.rs");

        assert!(
            !history_source.contains("pub struct LiveThread"),
            "history must not own the live ThreadStore handle"
        );
        assert!(
            !history_source.contains("impl LiveThread"),
            "history must not own live ThreadStore handle behavior"
        );
        assert!(
            thread_store_source.contains("pub struct LiveThread"),
            "thread_store must own the live ThreadStore handle"
        );
        assert!(
            thread_store_source.contains("impl LiveThread"),
            "thread_store must own live ThreadStore handle behavior"
        );
    }

    #[test]
    fn session_meta_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        assert!(
            !history_source.contains("pub struct SessionMeta"),
            "history must not own ThreadStore session metadata"
        );
        assert!(
            thread_store_source.contains("pub struct SessionMeta"),
            "thread_store must own ThreadStore session metadata"
        );
    }

    #[test]
    fn session_summary_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        assert!(
            !history_source.contains("pub struct SessionSummary"),
            "history must not own ThreadStore session summary"
        );
        assert!(
            thread_store_source.contains("pub struct SessionSummary"),
            "thread_store must own ThreadStore session summary"
        );
    }

    #[test]
    fn session_transcript_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        assert!(
            !history_source.contains("pub struct SessionTranscript"),
            "history must not own ThreadStore session transcript"
        );
        assert!(
            thread_store_source.contains("pub struct SessionTranscript"),
            "thread_store must own ThreadStore session transcript"
        );
    }

    #[test]
    fn session_writer_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/writer.rs");

        assert!(
            !history_source.contains("pub struct SessionWriter"),
            "history must not own ThreadStore session writer"
        );
        assert!(
            !history_source.contains("impl SessionWriter"),
            "history must not own ThreadStore session writer behavior"
        );
        assert!(
            thread_store_source.contains("pub struct SessionWriter"),
            "thread_store must own ThreadStore session writer"
        );
        assert!(
            thread_store_source.contains("impl SessionWriter"),
            "thread_store must own ThreadStore session writer behavior"
        );
    }

    #[test]
    fn jsonl_record_types_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/types.rs");

        for type_name in ["SessionRecord", "StoredMessage"] {
            assert!(
                !history_source.contains(&format!("enum {type_name}")),
                "history must not own JSONL ThreadStore record type {type_name}"
            );
            assert!(
                thread_store_source.contains(&format!("enum {type_name}")),
                "thread_store must own JSONL ThreadStore record type {type_name}"
            );
        }
    }

    #[test]
    fn jsonl_append_writer_helpers_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/writer.rs");

        for function_name in [
            "write_record(",
            "write_record_line(",
            "redact_session_record(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own JSONL append helper {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own JSONL append helper {function_name}"
            );
        }
    }

    #[test]
    fn jsonl_read_rewrite_helpers_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/writer.rs");

        for function_name in ["read_records(", "rewrite_records(", "write_records_to("] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own JSONL read/rewrite helper {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own JSONL read/rewrite helper {function_name}"
            );
        }
    }

    #[test]
    fn session_read_models_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/writer.rs");

        for function_name in ["read_session_meta(", "read_transcript("] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore session reader {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore session reader {function_name}"
            );
        }
    }

    #[test]
    fn thread_record_lookup_is_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        for function_name in [
            "load_thread_records(",
            "find_session_path(",
            "collect_session_files(",
            "is_history_file(",
            "sessions_dir(",
            "archive_dir(",
            "orca_home(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore lookup helper {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore lookup helper {function_name}"
            );
        }
    }

    #[test]
    fn runtime_provider_response_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");

        for marker in [
            "record_assistant_response_for_agent(",
            "extract_project_memory_after_final_response(",
            "tool_requests_from_provider_steps(",
            "run_tool_turns(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own provider response handling detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate provider response handling through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseResultStep"),
            "agent_loop must delegate provider response result folding through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseOutcome::Continue"),
            "agent_loop must not own provider response continue outcome folding"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseOutcome::Success"),
            "agent_loop must not own provider response success outcome folding"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderResponseOutcome::Return"),
            "agent_loop must not own provider response return outcome folding"
        );
        for marker in [
            "struct RuntimeProviderResponseStep",
            "struct RuntimeProviderResponseResultStep",
            "impl RuntimeProviderResponseStep",
            "impl RuntimeProviderResponseResultStep",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own provider response step detail {marker}"
            );
        }
        assert!(
            provider_turn_source.contains("struct RuntimeProviderResponseStep"),
            "provider_turn must own provider response step state"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeProviderResponseResultStep"),
            "provider_turn must own provider response result folding step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderResponseStep"),
            "provider_turn must own provider response step behavior"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderResponseResultStep"),
            "provider_turn must own provider response result folding step behavior"
        );
        for marker in [
            "record_assistant_response_for_agent(",
            "extract_project_memory_after_final_response(",
            "tool_requests_from_provider_steps(",
            "run_tool_turns(",
        ] {
            assert!(
                provider_turn_source.contains(marker),
                "provider_turn must compose provider response handling detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_setup_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        for marker in [
            "ContextConfig::for_model_with_runtime",
            "policy_for_tool_execution(",
            "provider_config_for_agent_loop(",
            "let budget_model = config.model.as_option()",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime turn setup detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnSetupStep"),
            "agent_loop must delegate runtime turn setup"
        );
        assert!(
            lifecycle_source.contains("struct RuntimeTurnSetupStep"),
            "lifecycle must own runtime turn setup step state"
        );
        assert!(
            lifecycle_source.contains("impl RuntimeTurnSetupStep"),
            "lifecycle must own runtime turn setup step behavior"
        );
        assert!(
            lifecycle_source.contains("context::ContextConfig::for_model_with_runtime"),
            "lifecycle must own context config setup"
        );
        assert!(
            lifecycle_source.contains("policy_for_tool_execution("),
            "lifecycle must compose tool-execution-owned policy construction"
        );
        assert!(
            lifecycle_source.contains("provider_config_for_agent_loop("),
            "lifecycle must compose tool-invocation-owned provider config construction"
        );
    }

    #[test]
    fn agent_loop_result_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");

        for marker in [
            "struct AgentLoopResult",
            "impl AgentLoopResult",
            "status: RunStatus::Success",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own agent-loop result detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("AgentLoopResult"),
            "agent_loop must use the lifecycle-owned agent-loop result"
        );
        assert!(
            lifecycle_source.contains("struct AgentLoopResult"),
            "lifecycle must own agent-loop result shape"
        );
        assert!(
            lifecycle_source.contains("impl AgentLoopResult"),
            "lifecycle must own agent-loop result constructors"
        );
    }

    #[test]
    fn runtime_provider_turn_terminal_folding_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");

        for marker in [
            "provider_turn.response",
            "provider_turn.terminal_error",
            "provider_response_or_terminal(",
            "RunStatus::Cancelled",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own provider turn terminal folding detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate provider turn terminal folding through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderTurnResultResultStep"),
            "agent_loop must delegate provider turn result folding through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderTurnResultOutcome"),
            "agent_loop must not own provider turn result outcome folding"
        );
        for marker in [
            "struct RuntimeProviderTurnResultStep",
            "struct RuntimeProviderTurnResultResultStep",
            "impl RuntimeProviderTurnResultStep",
            "impl RuntimeProviderTurnResultResultStep",
            "pub(crate) fn provider_response_or_terminal",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own provider turn terminal folding detail {marker}"
            );
        }
        assert!(
            provider_turn_source.contains("struct RuntimeProviderTurnResultStep"),
            "provider_turn must own provider turn result step state"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeProviderTurnResultResultStep"),
            "provider_turn must own provider turn result folding step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderTurnResultStep"),
            "provider_turn must own provider turn result step behavior"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderTurnResultResultStep"),
            "provider_turn must own provider turn result folding step behavior"
        );
        assert!(
            provider_turn_source.contains("pub(crate) fn provider_response_or_terminal"),
            "provider_turn must expose provider turn terminal folding"
        );
        assert!(
            provider_turn_source.contains("terminal_error"),
            "provider_turn must own provider turn terminal error extraction"
        );
    }

    #[test]
    fn runtime_turn_start_step_is_owned_by_runtime_turn_start_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_start_source = std::fs::read_to_string("src/runtime_turn_start.rs")
            .expect("runtime turn start source");

        for marker in [
            ".active_task()",
            "actor.start_turn(",
            "started_turn.into_event()",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime turn-start detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("AgentLoopResult::failure(error.status, error.message)"),
            "agent_loop must not own runtime turn-start error result folding"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime turn start through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeTurnStartResultStep"),
            "agent_loop must delegate runtime turn-start result folding through turn loop"
        );
        assert!(
            lib_source.contains("mod runtime_turn_start;"),
            "runtime crate must declare a focused runtime_turn_start module"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnStartStep"),
            "lifecycle must not own runtime turn-start step state"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnStartResultStep"),
            "lifecycle must not own runtime turn-start result step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnStartStep"),
            "lifecycle must not own runtime turn-start step behavior"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnStartResultStep"),
            "lifecycle must not own runtime turn-start result step behavior"
        );
        for marker in [
            "struct RuntimeTurnStartStep",
            "struct RuntimeTurnStartResultStep",
            "struct RuntimeTurnStartStepOutput",
            "enum RuntimeTurnStartResult",
            "impl RuntimeTurnStartStep",
            "impl RuntimeTurnStartResultStep",
            "actor.start_turn(",
            "started_turn.into_event()",
            "AgentLoopResult::failure(",
            "error.status",
            "error.message",
        ] {
            assert!(
                runtime_turn_start_source.contains(marker),
                "runtime_turn_start must own runtime turn-start detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_model_route_step_is_owned_by_runtime_model_route_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lib_source = include_str!("lib.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_model_route_source = std::fs::read_to_string("src/runtime_model_route.rs")
            .expect("runtime model route source");

        for marker in [
            "actor.route_model_turn(",
            "events.model_routed(",
            "let turn_provider_config = routed_model.provider_config",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime model routing detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime model routing through turn loop"
        );
        assert!(
            lib_source.contains("mod runtime_model_route;"),
            "runtime crate must declare a focused runtime_model_route module"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeModelRouteStep"),
            "lifecycle must not own runtime model-route step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeModelRouteStep"),
            "lifecycle must not own runtime model-route step behavior"
        );
        for marker in [
            "struct RuntimeModelRouteStep",
            "struct RuntimeModelRouteInput",
            "impl RuntimeModelRouteStep",
            "actor.route_model_turn(",
            "events.model_routed(",
        ] {
            assert!(
                runtime_model_route_source.contains(marker),
                "runtime_model_route must own runtime model-route detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_opening_step_is_owned_by_runtime_turn_opening_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");
        let runtime_turn_opening_source = std::fs::read_to_string("src/runtime_turn_opening.rs")
            .expect("runtime turn opening source");

        for marker in [
            ".compact_if_needed(conversation)",
            "RuntimeTurnStartStep::new",
            "RuntimeTurnStartResultStep::new",
            "RuntimeModelRouteStep::new",
            "RuntimeSteerStep::new",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must delegate runtime turn opening detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime turn opening"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnOpeningStep"),
            "lifecycle must not own runtime turn opening step state"
        );
        assert!(
            !lifecycle_source.contains("enum RuntimeTurnOpeningResult"),
            "lifecycle must not own runtime turn opening result shape"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnOpeningInput"),
            "lifecycle must not own runtime turn opening input shape"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnOpeningStep"),
            "lifecycle must not own runtime turn opening step behavior"
        );
        assert!(
            runtime_turn_opening_source.contains("struct RuntimeTurnOpeningStep"),
            "runtime_turn_opening must own runtime turn opening step state"
        );
        assert!(
            runtime_turn_opening_source.contains("enum RuntimeTurnOpeningResult"),
            "runtime_turn_opening must own runtime turn opening result shape"
        );
        assert!(
            runtime_turn_opening_source.contains("struct RuntimeTurnOpeningInput"),
            "runtime_turn_opening must own grouped runtime turn opening input"
        );
        assert!(
            runtime_turn_opening_source.contains("impl RuntimeTurnOpeningStep"),
            "runtime_turn_opening must own runtime turn opening step behavior"
        );
        for marker in [
            "RuntimeCompactionStep::new",
            "RuntimeTurnStartStep::new",
            "RuntimeTurnStartResultStep::new",
            "RuntimeModelRouteStep::new",
            "RuntimeSteerStep::new",
        ] {
            assert!(
                runtime_turn_opening_source.contains(marker),
                "runtime_turn_opening must compose runtime turn opening detail {marker}"
            );
        }
        assert!(
            runtime_turn_iteration_source.contains("RuntimeTurnOpeningInput"),
            "runtime_turn_iteration must call runtime turn opening through its grouped input"
        );
    }

    #[test]
    fn runtime_provider_error_step_is_owned_by_lifecycle_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");

        for marker in [
            "let mut reactive_compacted",
            "RuntimeProviderErrorOutcome::ContinueAfterCompaction",
            "RuntimeProviderErrorOutcome::Failed",
            "RuntimeProviderErrorOutcome::NoError",
            "handle_provider_error(",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must not own runtime provider-error detail {marker}"
            );
        }
        assert!(
            !agent_loop_source.contains("RuntimeProviderErrorStepOutcome"),
            "agent_loop must not own runtime provider-error outcome folding"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime provider-error handling through turn loop"
        );
        assert!(
            !agent_loop_source.contains("RuntimeProviderErrorResultStep"),
            "agent_loop must delegate runtime provider-error result folding through turn loop"
        );
        for marker in [
            "struct RuntimeProviderErrorStep",
            "struct RuntimeProviderErrorResultStep",
            "impl RuntimeProviderErrorStep",
            "impl RuntimeProviderErrorResultStep",
            "handle_provider_error(",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own runtime provider-error detail {marker}"
            );
        }
        assert!(
            provider_turn_source.contains("struct RuntimeProviderErrorStep"),
            "provider_turn must own runtime provider-error step state"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeProviderErrorResultStep"),
            "provider_turn must own runtime provider-error result step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderErrorStep"),
            "provider_turn must own runtime provider-error step behavior"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeProviderErrorResultStep"),
            "provider_turn must own runtime provider-error result step behavior"
        );
        assert!(
            provider_turn_source.contains("reactive_compacted"),
            "provider_turn must own reactive compaction loop state"
        );
        assert!(
            provider_turn_source.contains("handle_provider_error("),
            "provider_turn must keep provider error classification behind the step"
        );
    }

    #[test]
    fn runtime_turn_provider_cycle_step_is_composed_by_runtime_turn_iteration_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let provider_turn_source = include_str!("provider_turn.rs");
        let runtime_turn_iteration_source = include_str!("runtime_turn_iteration.rs");

        for marker in [
            "RuntimeProviderTurnStep::new",
            "RuntimeProviderTurnResultStep::new",
            "RuntimeProviderTurnResultResultStep::new",
            "RuntimeProviderErrorStep::new",
            "RuntimeProviderErrorResultStep::new",
            "RuntimeProviderResponseStep::new",
            "RuntimeProviderResponseResultStep::new",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must delegate runtime provider cycle detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime provider cycle"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnProviderCycleStep"),
            "lifecycle must not own runtime provider cycle step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnProviderCycleStep"),
            "lifecycle must not own runtime provider cycle step behavior"
        );
        assert!(
            !lifecycle_source.contains("RuntimeTurnProviderCycleStep::new"),
            "lifecycle must not compose runtime provider cycle after the turn-iteration split"
        );
        assert!(
            runtime_turn_iteration_source.contains("RuntimeTurnProviderCycleStep::new"),
            "runtime_turn_iteration must compose runtime provider cycle through provider_turn boundary"
        );
        assert!(
            provider_turn_source.contains("struct RuntimeTurnProviderCycleStep"),
            "provider_turn must own runtime provider cycle step state"
        );
        assert!(
            provider_turn_source.contains("impl RuntimeTurnProviderCycleStep"),
            "provider_turn must own runtime provider cycle step behavior"
        );
        for marker in [
            "RuntimeProviderTurnStep::new",
            "RuntimeProviderTurnResultStep::new",
            "RuntimeProviderTurnResultResultStep::new",
            "RuntimeProviderErrorStep::new",
            "RuntimeProviderErrorResultStep::new",
            "RuntimeProviderResponseStep::new",
            "RuntimeProviderResponseResultStep::new",
        ] {
            assert!(
                !lifecycle_source.contains(marker),
                "lifecycle must not own runtime provider cycle detail {marker}"
            );
            assert!(
                provider_turn_source.contains(marker),
                "provider_turn must compose runtime provider cycle detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_iteration_step_is_owned_by_runtime_turn_iteration_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_iteration_source =
            std::fs::read_to_string("src/runtime_turn_iteration.rs")
                .expect("runtime turn iteration source");

        for marker in [
            "RuntimeTurnOpeningStep::new",
            "RuntimeTurnProviderCycleStep::new",
            "RuntimeTurnOpeningResult::Continue",
            "RuntimeTurnProviderCycleResult::ContinueLoop",
            "RuntimeTurnProviderCycleResult::ContinueTurn",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must delegate runtime turn iteration detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime turn loop"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnIterationStep"),
            "lifecycle must not own runtime turn iteration step state"
        );
        assert!(
            !lifecycle_source.contains("enum RuntimeTurnIterationResult"),
            "lifecycle must not own runtime turn iteration result shape"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnIterationStep"),
            "lifecycle must not own runtime turn iteration step behavior"
        );
        assert!(
            runtime_turn_iteration_source.contains("struct RuntimeTurnIterationStep"),
            "runtime_turn_iteration must own runtime turn iteration step state"
        );
        assert!(
            runtime_turn_iteration_source.contains("enum RuntimeTurnIterationResult"),
            "runtime_turn_iteration must own runtime turn iteration result shape"
        );
        assert!(
            runtime_turn_iteration_source.contains("impl RuntimeTurnIterationStep"),
            "runtime_turn_iteration must own runtime turn iteration step behavior"
        );
        for marker in [
            "RuntimeTurnOpeningStep::new",
            "RuntimeTurnProviderCycleStep::new",
            "RuntimeTurnOpeningResult::Continue",
            "RuntimeTurnProviderCycleResult::ContinueLoop",
            "RuntimeTurnProviderCycleResult::ContinueTurn",
        ] {
            assert!(
                runtime_turn_iteration_source.contains(marker),
                "runtime_turn_iteration must compose runtime turn iteration detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_loop_step_is_owned_by_runtime_turn_loop_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");

        for marker in [
            "loop {",
            "RuntimeTurnIterationStep::new",
            "RuntimeTurnIterationResult::ContinueLoop",
            "RuntimeTurnIterationResult::Return",
        ] {
            assert!(
                !agent_loop_source.contains(marker),
                "agent_loop must delegate runtime turn loop detail {marker}"
            );
        }
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopStep"),
            "agent_loop must delegate runtime turn loop"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnLoopStep"),
            "lifecycle must not own runtime turn loop step state"
        );
        assert!(
            !lifecycle_source.contains("impl RuntimeTurnLoopStep"),
            "lifecycle must not own runtime turn loop step behavior"
        );
        assert!(
            runtime_turn_loop_source.contains("struct RuntimeTurnLoopStep"),
            "runtime_turn_loop must own runtime turn loop step state"
        );
        assert!(
            runtime_turn_loop_source.contains("impl RuntimeTurnLoopStep"),
            "runtime_turn_loop must own runtime turn loop step behavior"
        );
        for marker in [
            "RuntimeTurnIterationStep::new",
            "RuntimeTurnIterationResult::ContinueLoop",
            "RuntimeTurnIterationResult::Return",
        ] {
            assert!(
                runtime_turn_loop_source.contains(marker),
                "runtime_turn_loop must compose runtime turn loop detail {marker}"
            );
        }
    }

    #[test]
    fn runtime_turn_loop_input_is_owned_by_runtime_turn_loop_module() {
        let agent_loop_source = include_str!("agent_loop.rs");
        let lifecycle_source = include_str!("lifecycle.rs");
        let runtime_turn_loop_source = include_str!("runtime_turn_loop.rs");

        assert!(
            agent_loop_source.contains("RuntimeTurnLoopInput"),
            "agent_loop must pass turn loop inputs through a lifecycle-owned input object"
        );
        assert!(
            agent_loop_source.contains("RuntimeTurnLoopExecutors"),
            "agent_loop must pass child executors through a lifecycle-owned executor object"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnLoopInput"),
            "lifecycle must not own runtime turn loop input shape"
        );
        assert!(
            !lifecycle_source.contains("struct RuntimeTurnLoopExecutors"),
            "lifecycle must not own runtime turn loop executor shape"
        );
        assert!(
            !lifecycle_source.contains("impl<'a, 'runtime, W: io::Write> RuntimeTurnLoopInput"),
            "lifecycle must not own runtime turn loop input behavior"
        );
        assert!(
            !lifecycle_source.contains("impl<W: io::Write> RuntimeTurnLoopExecutors<W>"),
            "lifecycle must not own runtime turn loop executor behavior"
        );
        assert!(
            runtime_turn_loop_source.contains("struct RuntimeTurnLoopInput"),
            "runtime_turn_loop must own runtime turn loop input shape"
        );
        assert!(
            runtime_turn_loop_source.contains("struct RuntimeTurnLoopExecutors"),
            "runtime_turn_loop must own runtime turn loop executor shape"
        );
        assert!(
            runtime_turn_loop_source
                .contains("impl<'a, 'runtime, W: io::Write> RuntimeTurnLoopInput"),
            "runtime_turn_loop must own runtime turn loop input behavior"
        );
        assert!(
            runtime_turn_loop_source.contains("impl<W: io::Write> RuntimeTurnLoopExecutors<W>"),
            "runtime_turn_loop must own runtime turn loop executor behavior"
        );
        assert!(
            !agent_loop_source.contains("execute_child_agent_loop,\n        execute_child_agent_loop,\n        execute_child_agent_loop"),
            "agent_loop must not pass child executors as a raw repeated argument list"
        );
    }

    #[test]
    fn session_list_load_operations_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        for function_name in [
            "list_sessions(",
            "list_sessions_with_archived(",
            "load_session(",
            "summarize_session_with_archive_flag(",
            "collect_summaries_from_root(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore read operation {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore read operation {function_name}"
            );
        }
    }

    #[test]
    fn session_search_operations_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        assert!(
            !history_source.contains("pub struct SearchHit"),
            "history must not own ThreadStore search result type"
        );
        assert!(
            thread_store_source.contains("pub struct SearchHit"),
            "thread_store must own ThreadStore search result type"
        );

        for function_name in [
            "search_sessions(",
            "search_roots_with_ripgrep(",
            "search_root_in_process(",
            "search_compressed_root(",
            "push_matching_lines(",
            "push_search_hit(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore search operation {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore search operation {function_name}"
            );
        }
    }

    #[test]
    fn session_mutation_operations_are_owned_by_thread_store_module() {
        let history_source = include_str!("history.rs");
        let thread_store_source = include_str!("thread_store/local.rs");

        for function_name in [
            "delete_session(",
            "archive_session(",
            "rename_session(",
            "compress_session(",
        ] {
            assert!(
                !history_source.contains(&format!("fn {function_name}")),
                "history must not own ThreadStore mutation operation {function_name}"
            );
            assert!(
                thread_store_source.contains(&format!("fn {function_name}")),
                "thread_store must own ThreadStore mutation operation {function_name}"
            );
        }
    }

    #[test]
    fn protocol_imports_thread_types_from_thread_store_boundary() {
        let protocol_source = include_str!("protocol.rs");

        assert!(
            !protocol_source.contains("use crate::history"),
            "protocol must import thread protocol types through thread_store"
        );
    }

    #[test]
    fn agent_loop_imports_session_types_from_thread_store_boundary() {
        let agent_loop_source = include_str!("agent_loop.rs");

        assert!(
            !agent_loop_source.contains("use crate::history"),
            "agent loop must import session transcript/writer types through thread_store"
        );
    }

    #[test]
    fn session_imports_session_types_from_thread_store_boundary() {
        let session_source = include_str!("session.rs");

        assert!(
            !session_source.contains("use crate::history::{self, SessionWriter};"),
            "session production code must import session transcript/writer types through thread_store"
        );
    }
}
