use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::tool_types::ToolRequest;
use orca_mcp::McpRegistry;

use crate::agent_child::ChildAgentExecutor;
use crate::cost::CostTracker;
use crate::extension::RuntimeExtensionContext;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{RuntimePermissionRequestHandler, RuntimeUserInputHandler};
use crate::memory::MemoryBlock;
#[cfg(test)]
use crate::runtime_readonly_tool_turn::record_readonly_batch_results;
use crate::runtime_readonly_tool_turn::{
    RuntimeReadonlyToolTurnContext, collect_readonly_batch, run_readonly_tool_turn,
    should_run_readonly_batch,
};
use crate::step_context::{
    RuntimeSamplingRequestState, RuntimeStepContext, RuntimeToolResultRecordOutcome,
};
use crate::subagent_execution::{
    collect_subagent_batch, run_subagent_batch_tool_turn, should_run_subagent_batch,
};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_execution::{ToolExecutionContext, execute_tool_with_approval};
use crate::tool_invocation::reject_disallowed_child_tool;
use crate::workflow::ipc::WorkflowIpcContext;
use crate::workflow::runner::SharedEventBuffer;
use crate::workflow_execution::BackgroundWorkflowRun;

pub(crate) enum ToolTurnOutcome {
    Continue,
    Return {
        status: RunStatus,
        error: Option<String>,
    },
}

pub(crate) struct RuntimeToolTurnsContext<'a, W: io::Write> {
    pub(crate) step_context: RuntimeStepContext<'a>,
    pub(crate) sampling_state: &'a mut RuntimeSamplingRequestState,
    pub(crate) io: RuntimeToolTurnsIo<'a, W>,
    pub(crate) tool_requests: &'a [ToolRequest],
    pub(crate) executors: RuntimeToolTurnsExecutors<W>,
}

pub(crate) struct RuntimeToolTurnsIo<'a, W: io::Write> {
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
}

pub(crate) struct RuntimeToolTurnsExecutors<W: io::Write> {
    pub(crate) child_executor: ChildAgentExecutor<W>,
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    pub(crate) batch_child_executor: ChildAgentExecutor<io::Sink>,
}

pub(crate) struct RuntimeNormalToolTurnContext<'a, W: io::Write> {
    pub(crate) sampling_state: &'a mut RuntimeSamplingRequestState,
    pub(crate) config: &'a RunConfig,
    pub(crate) cwd: &'a Path,
    pub(crate) events: &'a mut EventFactory,
    pub(crate) sink: &'a mut EventSink<W>,
    pub(crate) conversation: &'a mut Conversation,
    pub(crate) history_writer: Option<&'a mut SessionWriter>,
    pub(crate) tool_request: &'a ToolRequest,
    pub(crate) subagent_depth: u32,
    pub(crate) emit_deltas: bool,
    pub(crate) policy: &'a ApprovalPolicy,
    pub(crate) instructions: &'a ProjectInstructions,
    pub(crate) memory: &'a MemoryBlock,
    pub(crate) mcp_registry: &'a McpRegistry,
    pub(crate) hooks: &'a HookRunner,
    pub(crate) cost_tracker: &'a mut CostTracker,
    pub(crate) cancel: &'a CancelToken,
    pub(crate) task_registry: &'a TaskRegistry,
    pub(crate) background_workflows: &'a mut Vec<BackgroundWorkflowRun>,
    pub(crate) workflow_ipc: Option<&'a WorkflowIpcContext>,
    pub(crate) permission_handler: Option<&'a (dyn RuntimePermissionRequestHandler + Send + Sync)>,
    pub(crate) user_input_handler: Option<&'a dyn RuntimeUserInputHandler>,
    pub(crate) extensions: Option<RuntimeExtensionContext<'a>>,
    pub(crate) child_executor: ChildAgentExecutor<W>,
    pub(crate) workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
}

impl ToolTurnOutcome {
    #[cfg(test)]
    pub(crate) fn from_terminal(status: RunStatus, error: Option<String>) -> Self {
        Self::Return { status, error }
    }

    pub(crate) fn from_record_outcome(outcome: RuntimeToolResultRecordOutcome) -> Self {
        match outcome {
            RuntimeToolResultRecordOutcome::Continue => Self::Continue,
            RuntimeToolResultRecordOutcome::Return { status, error } => {
                Self::Return { status, error }
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn terminal_tool_turn(status: RunStatus, error: Option<String>) -> ToolTurnOutcome {
    ToolTurnOutcome::from_terminal(status, error)
}

pub(crate) fn run_tool_turns<W: io::Write>(
    context: RuntimeToolTurnsContext<'_, W>,
) -> io::Result<ToolTurnOutcome> {
    let RuntimeToolTurnsContext {
        step_context,
        sampling_state,
        io,
        tool_requests,
        executors,
    } = context;
    let RuntimeToolTurnsExecutors {
        child_executor,
        workflow_child_executor,
        batch_child_executor,
    } = executors;
    let RuntimeToolTurnsIo {
        events,
        sink,
        conversation,
        mut history_writer,
        cost_tracker,
        background_workflows,
    } = io;
    let (step_snapshot, extensions) = step_context.into_parts();
    let config = step_snapshot.config;
    let cwd = step_snapshot.turn_context.cwd;
    let tool_policy = step_snapshot.tool_policy;
    let subagent_depth = step_snapshot.turn_context.subagent_depth;
    let emit_deltas = step_snapshot.turn_context.emit_deltas;
    let policy = step_snapshot.policy;
    let capabilities = step_snapshot.capabilities();
    let instructions = capabilities.instructions;
    let memory = capabilities.memory;
    let mcp_registry = capabilities.mcp_registry;
    let hooks = capabilities.hooks;
    let cancel = capabilities.cancel;
    let task_registry = capabilities.task_registry;
    let workflow_ipc = capabilities.workflow_ipc;
    let permission_handler = capabilities.permission_handler;
    let user_input_handler = capabilities.user_input_handler;
    while let Some(tool_request) = sampling_state.current_tool_request(tool_requests) {
        if let Some(result) = reject_disallowed_child_tool(
            tool_request,
            tool_policy,
            mcp_registry,
            &config.external_tools,
        ) {
            if emit_deltas {
                sink.emit(&events.tool_call_requested(tool_request))?;
                sink.emit(&events.tool_call_completed(&result))?;
            }
            return Ok(ToolTurnOutcome::Return {
                status: RunStatus::Failed,
                error: Some(result.error.clone().unwrap_or_default()),
            });
        }

        if should_run_subagent_batch(config, tool_request, subagent_depth) {
            let dispatch_window =
                sampling_state.tool_dispatch_window(tool_requests, |tool_requests, start_index| {
                    collect_subagent_batch(config, tool_requests, start_index)
                });
            match run_subagent_batch_tool_turn(
                config,
                cwd,
                events,
                sink,
                conversation,
                history_writer.as_deref_mut(),
                dispatch_window.tool_requests(),
                subagent_depth,
                emit_deltas,
                instructions,
                memory,
                mcp_registry,
                hooks,
                cost_tracker,
                cancel,
                workflow_ipc,
                batch_child_executor,
            )? {
                ToolTurnOutcome::Continue => {}
                ToolTurnOutcome::Return { status, error } => {
                    return Ok(ToolTurnOutcome::Return { status, error });
                }
            }
            sampling_state.advance_tool_cursor_to_window_end(&dispatch_window);
            continue;
        }

        if should_run_readonly_batch(config.tools.max_read_parallel, tool_request) {
            let dispatch_window =
                sampling_state.tool_dispatch_window(tool_requests, |tool_requests, start_index| {
                    collect_readonly_batch(
                        config.tools.max_read_parallel,
                        tool_requests,
                        start_index,
                    )
                });
            match run_readonly_tool_turn(RuntimeReadonlyToolTurnContext {
                cwd,
                events,
                sink,
                conversation,
                history_writer: history_writer.as_deref_mut(),
                tool_requests: dispatch_window.tool_requests(),
                emit_deltas,
                mcp_registry,
                hooks,
                output_truncation: config.tools.output_truncation,
            })? {
                ToolTurnOutcome::Continue => {}
                ToolTurnOutcome::Return { status, error } => {
                    return Ok(ToolTurnOutcome::Return { status, error });
                }
            }
            sampling_state.advance_tool_cursor_to_window_end(&dispatch_window);
            continue;
        }

        match run_normal_tool_turn(RuntimeNormalToolTurnContext {
            sampling_state,
            config,
            cwd,
            events,
            sink,
            conversation,
            history_writer: history_writer.as_deref_mut(),
            tool_request,
            subagent_depth,
            emit_deltas,
            policy,
            instructions,
            memory,
            mcp_registry,
            hooks,
            cost_tracker,
            cancel,
            task_registry,
            background_workflows,
            workflow_ipc,
            permission_handler,
            user_input_handler,
            extensions,
            child_executor,
            workflow_child_executor,
        })? {
            ToolTurnOutcome::Continue => {}
            ToolTurnOutcome::Return { status, error } => {
                return Ok(ToolTurnOutcome::Return { status, error });
            }
        }
        sampling_state.advance_tool_cursor_one(tool_requests.len());
    }

    Ok(ToolTurnOutcome::Continue)
}

pub(crate) fn run_normal_tool_turn<W: io::Write>(
    context: RuntimeNormalToolTurnContext<'_, W>,
) -> io::Result<ToolTurnOutcome> {
    let RuntimeNormalToolTurnContext {
        sampling_state,
        config,
        cwd,
        events,
        sink,
        conversation,
        history_writer,
        tool_request,
        subagent_depth,
        emit_deltas,
        policy,
        instructions,
        memory,
        mcp_registry,
        hooks,
        cost_tracker,
        cancel,
        task_registry,
        background_workflows,
        workflow_ipc,
        permission_handler,
        user_input_handler,
        extensions,
        child_executor,
        workflow_child_executor,
    } = context;
    let mut execution_context = ToolExecutionContext::new(cwd, subagent_depth, emit_deltas, policy)
        .with_services(instructions, memory, mcp_registry, hooks)
        .with_runtime(
            cost_tracker,
            cancel,
            task_registry,
            background_workflows,
            workflow_ipc,
        )
        .with_permission_overlay(sampling_state.permission_overlay_mut())
        .with_permission_handler(permission_handler)
        .with_user_input_handler(user_input_handler);
    if let Some(extensions) = extensions {
        execution_context =
            execution_context.with_extensions(extensions.registry(), extensions.stores());
    }
    let (status, result) = execute_tool_with_approval(
        config,
        events,
        sink,
        tool_request,
        execution_context,
        child_executor,
        workflow_child_executor,
    )?;

    sampling_state
        .record_normal_tool_result(
            conversation,
            history_writer,
            tool_request,
            &result,
            status,
            emit_deltas,
        )
        .map(ToolTurnOutcome::from_record_outcome)
}

#[cfg(test)]
mod tests {
    use std::io;

    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::conversation::{Conversation, Message};
    use orca_core::event_schema::{EventFactory, RunStatus};
    use orca_core::event_sink::EventSink;
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
    use serde_json::json;

    use super::*;
    use crate::agent_child::{ChildAgentRequest, ChildAgentResult, ChildAgentRuntime};
    use crate::extension::{ExtensionData, ExtensionRegistryBuilder};
    use crate::goals::{GoalToolProgressState, install_goal_tool_lifecycle};
    use crate::hooks::HookRunner;
    use crate::runtime_turn_kernel::RuntimeTurnKernel;
    use crate::tool_execution::policy_for_tool_execution;
    use crate::tool_invocation::AgentToolPolicyContext;

    fn config_with_external(external_tools: Vec<ExternalToolConfig>) -> RunConfig {
        RunConfig {
            prompt: "test".to_string(),
            app_version: "test".to_string(),
            cwd: Some(std::env::current_dir().expect("cwd")),
            provider: ProviderKind::Mock,
            model: ModelSelection::from_unchecked(Some("mock".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            approval_mode: ApprovalMode::Suggest,
            output_format: OutputFormat::Jsonl,
            verifier: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            theme: ThemeName::Dark,
            mcp_servers: Vec::new(),
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            hooks: Vec::new(),
            workflows: WorkflowConfig::default(),
            subagents: SubagentConfig {
                max_depth: 1,
                ..SubagentConfig::default()
            },
            tools: ToolConfig::default(),
            external_tools,
            max_budget_usd: None,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn request(
        name: ToolName,
        action: ActionKind,
        target: Option<&str>,
        raw: Option<&str>,
    ) -> ToolRequest {
        ToolRequest {
            id: "tool-1".to_string(),
            name,
            action,
            target: target.map(str::to_string),
            raw_arguments: raw.map(str::to_string),
        }
    }

    fn unused_child_executor<W: io::Write>(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, W>,
        _child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        panic!("read_file turn must not execute child agents")
    }

    #[test]
    fn sampling_request_state_advances_over_single_and_batch_tool_requests() {
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: None,
        };
        let third = ToolRequest {
            id: "tool-3".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let requests = vec![first, second, third];

        let mut sampling_state = RuntimeSamplingRequestState::new();

        assert_eq!(
            sampling_state
                .current_tool_request(&requests)
                .map(|request| request.id.as_str()),
            Some("tool-1")
        );
        sampling_state.advance_tool_cursor_one(requests.len());
        assert_eq!(sampling_state.tool_cursor_position(), 1);
        assert_eq!(
            sampling_state
                .current_tool_request(&requests)
                .map(|request| request.id.as_str()),
            Some("tool-2")
        );
        sampling_state.advance_tool_cursor_to(3, requests.len());
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert!(sampling_state.current_tool_request(&requests).is_none());
        sampling_state.advance_tool_cursor_to(99, requests.len());
        assert_eq!(sampling_state.tool_cursor_position(), 3);
    }

    #[test]
    fn sampling_request_state_builds_and_advances_dispatch_windows() {
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: None,
        };
        let third = ToolRequest {
            id: "tool-3".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let requests = vec![first, second, third];
        let mut sampling_state = RuntimeSamplingRequestState::new();

        sampling_state.advance_tool_cursor_one(requests.len());
        let dispatch_window = sampling_state
            .tool_dispatch_window(&requests, |_, start_index| start_index.saturating_add(99));

        assert_eq!(
            dispatch_window
                .tool_requests()
                .iter()
                .map(|request| request.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tool-2", "tool-3"]
        );
        assert_eq!(dispatch_window.end_index(), 3);
        sampling_state.advance_tool_cursor_to_window_end(&dispatch_window);
        assert_eq!(sampling_state.tool_cursor_position(), 3);
        assert!(sampling_state.current_tool_request(&requests).is_none());

        let mut sampling_state = RuntimeSamplingRequestState::new();
        sampling_state.advance_tool_cursor_one(requests.len());
        let stalled_window =
            sampling_state.tool_dispatch_window(&requests, |_, start_index| start_index);

        assert_eq!(
            stalled_window
                .tool_requests()
                .iter()
                .map(|request| request.id.as_str())
                .collect::<Vec<_>>(),
            vec!["tool-2"]
        );
        assert_eq!(stalled_window.end_index(), 2);
    }

    #[test]
    fn sampling_request_state_records_approval_required_normal_tool_result() {
        let mut conversation = Conversation::new();
        let sampling_state = RuntimeSamplingRequestState::new();
        let request = request(
            ToolName::RequestPermissions,
            ActionKind::Read,
            Some("read"),
            None,
        );
        let result = ToolResult::denied(&request, "needs approval");

        let outcome = sampling_state
            .record_normal_tool_result(
                &mut conversation,
                None,
                &request,
                &result,
                RunStatus::ApprovalRequired,
                false,
            )
            .expect("record approval result");

        match outcome {
            RuntimeToolResultRecordOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::ApprovalRequired);
                assert_eq!(error.as_deref(), Some("needs approval"));
            }
            RuntimeToolResultRecordOutcome::Continue => {
                panic!("approval-required result must return")
            }
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
    }

    #[test]
    fn sampling_request_state_records_subagent_failure_normal_tool_result() {
        let mut conversation = Conversation::new();
        let sampling_state = RuntimeSamplingRequestState::new();
        let request = request(ToolName::Subagent, ActionKind::Agent, Some("audit"), None);
        let result = ToolResult::failed(&request, "child failed", None);

        let outcome = sampling_state
            .record_normal_tool_result(
                &mut conversation,
                None,
                &request,
                &result,
                RunStatus::Failed,
                false,
            )
            .expect("record subagent failure");

        match outcome {
            RuntimeToolResultRecordOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(error.as_deref(), Some("child failed"));
            }
            RuntimeToolResultRecordOutcome::Continue => {
                panic!("failed subagent result must return")
            }
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
    }

    #[test]
    fn record_readonly_batch_results_records_each_tool_message_in_order() {
        let mut conversation = Conversation::new();
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::ListFiles,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: None,
        };
        let results = vec![
            ToolResult::completed(&first, "one".to_string(), false),
            ToolResult::completed(&second, "two".to_string(), false),
        ];

        record_readonly_batch_results(&mut conversation, None, results, false)
            .expect("record readonly batch results");

        assert_eq!(conversation.messages.len(), 2);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
        assert!(
            matches!(&conversation.messages[1], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-2")
        );
    }

    #[test]
    fn run_normal_tool_turn_executes_and_records_tool_result() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").expect("write file");
        std::fs::write(cwd.path().join("other.txt"), "world\n").expect("write file");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        let mut events = EventFactory::new("normal-tool-turn".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let request = request(
            ToolName::ReadFile,
            ActionKind::Read,
            Some("tracked.txt"),
            Some(json!({ "path": "tracked.txt" }).to_string().as_str()),
        );
        let policy = policy_for_tool_execution(&config);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("normal-tool-turn".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();

        let outcome = run_normal_tool_turn(RuntimeNormalToolTurnContext {
            sampling_state: &mut sampling_state,
            config: &config,
            cwd: cwd.path(),
            events: &mut events,
            sink: &mut sink,
            conversation: &mut conversation,
            history_writer: None,
            tool_request: &request,
            subagent_depth: 0,
            emit_deltas: false,
            policy: &policy,
            instructions: &instructions,
            memory: &memory,
            mcp_registry: &registry,
            hooks: &hooks,
            cost_tracker: &mut cost_tracker,
            cancel: &cancel,
            task_registry: &task_registry,
            background_workflows: &mut background_workflows,
            workflow_ipc: None,
            permission_handler: None,
            user_input_handler: None,
            extensions: None,
            child_executor: unused_child_executor,
            workflow_child_executor: unused_child_executor,
        })
        .expect("run normal tool turn");

        assert!(matches!(outcome, ToolTurnOutcome::Continue));
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, content, .. }
                if tool_call_id == "tool-1" && content.contains("hello"))
        );
    }

    #[test]
    fn run_readonly_tool_turn_executes_and_records_batch_results() {
        let cwd = tempfile::tempdir().expect("cwd");
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").expect("write file");
        let mut events = EventFactory::new("readonly-tool-turn".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let requests = vec![
            request(
                ToolName::ReadFile,
                ActionKind::Read,
                Some("tracked.txt"),
                Some(json!({ "path": "tracked.txt" }).to_string().as_str()),
            ),
            ToolRequest {
                id: "tool-2".to_string(),
                name: ToolName::ReadFile,
                action: ActionKind::Read,
                target: Some("other.txt".to_string()),
                raw_arguments: Some(json!({ "path": "other.txt" }).to_string()),
            },
        ];
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let outcome = run_readonly_tool_turn(RuntimeReadonlyToolTurnContext {
            cwd: cwd.path(),
            events: &mut events,
            sink: &mut sink,
            conversation: &mut conversation,
            history_writer: None,
            tool_requests: &requests,
            emit_deltas: false,
            mcp_registry: &registry,
            hooks: &hooks,
            output_truncation: ToolConfig::default().output_truncation,
        })
        .expect("run readonly tool turn");

        assert!(matches!(outcome, ToolTurnOutcome::Continue));
        assert_eq!(conversation.messages.len(), 2);
        let combined_tool_content = conversation
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. }
                if tool_call_id == "tool-1")
        );
        assert!(
            matches!(&conversation.messages[1], Message::Tool { tool_call_id, .. }
                if tool_call_id == "tool-2")
        );
        assert!(combined_tool_content.contains("hello"));
    }

    #[test]
    fn run_tool_turns_returns_failed_for_disallowed_child_tool() {
        let cwd = tempfile::tempdir().expect("cwd");
        let config = config_with_external(Vec::new());
        let mut events = EventFactory::new("tool-turns-disallowed".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let allowed = vec!["read_file".to_string()];
        let request = request(ToolName::Subagent, ActionKind::Agent, Some("audit"), None);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-turns-disallowed".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::new(Some(&allowed), Some("test child")),
            1,
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
            None,
        );

        let outcome = run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &[request],
            executors: RuntimeToolTurnsExecutors {
                child_executor: unused_child_executor::<Vec<u8>>,
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: unused_child_executor::<io::Sink>,
            },
        })
        .expect("run tool turns");

        match outcome {
            ToolTurnOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(
                    error.as_deref(),
                    Some("test child disallows tool 'subagent'")
                );
            }
            ToolTurnOutcome::Continue => panic!("disallowed child tool should end the turn"),
        }
        assert!(conversation.messages.is_empty());
    }

    #[test]
    fn run_tool_turns_notifies_extension_lifecycle_for_normal_tool() {
        let cwd = tempfile::tempdir().expect("cwd");
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        let mut events = EventFactory::new("tool-turns-extension-lifecycle".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let mut conversation = Conversation::new();
        let request = request(
            ToolName::Bash,
            ActionKind::Shell,
            Some("printf lifecycle"),
            Some(
                json!({ "command": "printf lifecycle" })
                    .to_string()
                    .as_str(),
            ),
        );
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let mcp_registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-turns-extension-lifecycle".to_string());
        let mut background_workflows = Vec::new();
        let mut sampling_state = RuntimeSamplingRequestState::new();
        let policy = policy_for_tool_execution(&config);
        let mut extension_builder = ExtensionRegistryBuilder::new();
        install_goal_tool_lifecycle(&mut extension_builder);
        let extension_registry = extension_builder.build();
        let thread_extensions = ExtensionData::new("session-1");
        let turn_extensions = ExtensionData::new("turn-1");
        let kernel = RuntimeTurnKernel::new(&thread_extensions, &turn_extensions);
        let step_context = RuntimeStepContext::new(
            &config,
            cwd.path(),
            AgentToolPolicyContext::unrestricted(),
            0,
            false,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            &task_registry,
            None,
            None,
            None,
        );
        let step_context = kernel.bind_step_context(step_context, &extension_registry);

        let outcome = run_tool_turns(RuntimeToolTurnsContext {
            step_context,
            sampling_state: &mut sampling_state,
            io: RuntimeToolTurnsIo {
                events: &mut events,
                sink: &mut sink,
                conversation: &mut conversation,
                history_writer: None,
                cost_tracker: &mut cost_tracker,
                background_workflows: &mut background_workflows,
            },
            tool_requests: &[request],
            executors: RuntimeToolTurnsExecutors {
                child_executor: unused_child_executor::<Vec<u8>>,
                workflow_child_executor: unused_child_executor::<SharedEventBuffer>,
                batch_child_executor: unused_child_executor::<io::Sink>,
            },
        })
        .expect("run tool turns");

        assert!(matches!(outcome, ToolTurnOutcome::Continue));
        let progress = thread_extensions
            .get::<GoalToolProgressState>()
            .expect("goal progress from tool lifecycle contributor");
        assert_eq!(progress.completed_tool_attempts(), 1);
        assert_eq!(progress.last_turn_id().as_deref(), Some("turn-1"));
        assert_eq!(progress.last_call_id().as_deref(), Some("tool-1"));
    }

    #[test]
    fn terminal_tool_turn_carries_status_and_optional_error() {
        match terminal_tool_turn(RunStatus::Failed, Some("tool failed".to_string())) {
            ToolTurnOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(error.as_deref(), Some("tool failed"));
            }
            ToolTurnOutcome::Continue => panic!("terminal tool turn must return"),
        }
    }
}
