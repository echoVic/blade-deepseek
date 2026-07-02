use std::io;
use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::hook_types::HookEvent;
use orca_core::tool_types::{ToolName, ToolOutputTruncation, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

use crate::agent_child::ChildAgentExecutor;
use crate::cost::CostTracker;
use crate::hooks::{HookContext, HookRunner};
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{RuntimePermissionRequestHandler, TurnPermissionOverlay};
use crate::memory::MemoryBlock;
use crate::session::{record_plan_state_for_agent, record_tool_result_for_agent};
use crate::subagent_execution::{
    collect_subagent_batch, run_subagent_batch_tool_turn, should_run_subagent_batch,
};
use crate::tasks::TaskRegistry;
use crate::thread_store::SessionWriter;
use crate::tool_execution::{ToolExecutionContext, execute_tool_with_approval};
use crate::tool_invocation::{
    AgentToolPolicyContext, apply_pre_tool_outcome_with_external,
    prepare_tool_invocation_with_external, reject_disallowed_child_tool,
};
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

pub(crate) struct ToolRequestCursor<'a> {
    requests: &'a [ToolRequest],
    index: usize,
}

impl<'a> ToolRequestCursor<'a> {
    pub(crate) fn new(requests: &'a [ToolRequest]) -> Self {
        Self { requests, index: 0 }
    }

    pub(crate) fn current(&self) -> Option<&'a ToolRequest> {
        self.requests.get(self.index)
    }

    pub(crate) fn position(&self) -> usize {
        self.index
    }

    pub(crate) fn advance_one(&mut self) {
        self.advance_to(self.index.saturating_add(1));
    }

    pub(crate) fn advance_to(&mut self, next_index: usize) {
        self.index = next_index.min(self.requests.len());
    }
}

impl ToolTurnOutcome {
    pub(crate) fn from_terminal(status: RunStatus, error: Option<String>) -> Self {
        Self::Return { status, error }
    }
}

pub(crate) fn terminal_tool_turn(status: RunStatus, error: Option<String>) -> ToolTurnOutcome {
    ToolTurnOutcome::from_terminal(status, error)
}

pub(crate) fn execute_readonly_batch(
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_requests: &[ToolRequest],
    emit_deltas: bool,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    output_truncation: ToolOutputTruncation,
) -> io::Result<Vec<ToolResult>> {
    let mut hook_failed: Vec<Option<ToolResult>> = vec![None; tool_requests.len()];
    let mut runnable = Vec::new();

    for (idx, tool_request) in tool_requests.iter().enumerate() {
        let invocation =
            prepare_tool_invocation_with_external(tool_request, 0, u32::MAX, mcp_registry, &[]);
        if emit_deltas {
            sink.emit(&events.tool_call_requested(tool_request))?;
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
                match apply_pre_tool_outcome_with_external(invocation, &outcome, mcp_registry, &[])
                {
                    Ok(invocation) => runnable.push((idx, invocation.effective)),
                    Err(error) => hook_failed[idx] = Some(error.into_result()),
                }
            }
            Err(error) => {
                hook_failed[idx] = Some(ToolResult::failed(
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
        if emit_deltas {
            sink.emit(&events.tool_call_completed(result))?;
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
                sink.emit(&events.error(&format!("post_tool_use hook failed: {error}")))?;
            }
        }
    }

    Ok(results)
}

pub(crate) fn should_run_readonly_batch(
    max_read_parallel: usize,
    tool_request: &ToolRequest,
) -> bool {
    orca_tools::should_run_readonly_batch(max_read_parallel, tool_request)
}

pub(crate) fn collect_readonly_batch(
    max_read_parallel: usize,
    tool_requests: &[ToolRequest],
    start: usize,
) -> usize {
    orca_tools::collect_readonly_batch(max_read_parallel, tool_requests, start)
}

pub(crate) fn record_readonly_batch_results(
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    results: Vec<ToolResult>,
    emit_deltas: bool,
) -> io::Result<()> {
    for result in results {
        record_tool_result_for_agent(
            conversation,
            history_writer.as_deref_mut(),
            &result,
            emit_deltas,
        )?;
    }
    Ok(())
}

pub(crate) fn run_readonly_tool_turn(
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    conversation: &mut Conversation,
    history_writer: Option<&mut SessionWriter>,
    tool_requests: &[ToolRequest],
    emit_deltas: bool,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    output_truncation: ToolOutputTruncation,
) -> io::Result<ToolTurnOutcome> {
    let results = execute_readonly_batch(
        cwd,
        events,
        sink,
        tool_requests,
        emit_deltas,
        mcp_registry,
        hooks,
        output_truncation,
    )?;

    record_readonly_batch_results(conversation, history_writer, results, emit_deltas)?;
    Ok(ToolTurnOutcome::Continue)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_tool_turns<W: io::Write>(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    tool_requests: &[ToolRequest],
    tool_policy: AgentToolPolicyContext<'_>,
    subagent_depth: u32,
    emit_deltas: bool,
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
    workflow_ipc: Option<&WorkflowIpcContext>,
    permission_handler: Option<&(dyn RuntimePermissionRequestHandler + Send + Sync)>,
    child_executor: ChildAgentExecutor<W>,
    workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
    batch_child_executor: ChildAgentExecutor<io::Sink>,
) -> io::Result<ToolTurnOutcome> {
    let mut cursor = ToolRequestCursor::new(tool_requests);
    let mut permission_overlay = TurnPermissionOverlay::default();
    while let Some(tool_request) = cursor.current() {
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
            let batch_end = collect_subagent_batch(config, tool_requests, cursor.position());
            match run_subagent_batch_tool_turn(
                config,
                cwd,
                events,
                sink,
                conversation,
                history_writer.as_deref_mut(),
                &tool_requests[cursor.position()..batch_end],
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
            cursor.advance_to(batch_end);
            continue;
        }

        if should_run_readonly_batch(config.tools.max_read_parallel, tool_request) {
            let batch_end = collect_readonly_batch(
                config.tools.max_read_parallel,
                tool_requests,
                cursor.position(),
            );
            match run_readonly_tool_turn(
                cwd,
                events,
                sink,
                conversation,
                history_writer.as_deref_mut(),
                &tool_requests[cursor.position()..batch_end],
                emit_deltas,
                mcp_registry,
                hooks,
                config.tools.output_truncation,
            )? {
                ToolTurnOutcome::Continue => {}
                ToolTurnOutcome::Return { status, error } => {
                    return Ok(ToolTurnOutcome::Return { status, error });
                }
            }
            cursor.advance_to(batch_end);
            continue;
        }

        match run_normal_tool_turn(
            config,
            cwd,
            events,
            sink,
            conversation,
            history_writer.as_deref_mut(),
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
            &mut permission_overlay,
            permission_handler,
            child_executor,
            workflow_child_executor,
        )? {
            ToolTurnOutcome::Continue => {}
            ToolTurnOutcome::Return { status, error } => {
                return Ok(ToolTurnOutcome::Return { status, error });
            }
        }
        cursor.advance_one();
    }

    Ok(ToolTurnOutcome::Continue)
}

pub(crate) fn record_normal_tool_result(
    conversation: &mut Conversation,
    mut history_writer: Option<&mut SessionWriter>,
    tool_request: &ToolRequest,
    result: &ToolResult,
    status: RunStatus,
    emit_deltas: bool,
) -> io::Result<ToolTurnOutcome> {
    record_plan_state_for_agent(
        conversation,
        history_writer.as_deref_mut(),
        tool_request,
        result,
    );
    record_tool_result_for_agent(
        conversation,
        history_writer.as_deref_mut(),
        result,
        emit_deltas,
    )?;

    if status == RunStatus::ApprovalRequired {
        return Ok(terminal_tool_turn(status, result.error.clone()));
    }
    if status == RunStatus::Failed && tool_request.name == ToolName::Subagent {
        return Ok(terminal_tool_turn(
            RunStatus::Failed,
            Some(result.error.clone().unwrap_or_default()),
        ));
    }

    Ok(ToolTurnOutcome::Continue)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_normal_tool_turn<W: io::Write>(
    config: &RunConfig,
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<W>,
    conversation: &mut Conversation,
    history_writer: Option<&mut SessionWriter>,
    tool_request: &ToolRequest,
    subagent_depth: u32,
    emit_deltas: bool,
    policy: &ApprovalPolicy,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    cost_tracker: &mut CostTracker,
    cancel: &CancelToken,
    task_registry: &TaskRegistry,
    background_workflows: &mut Vec<BackgroundWorkflowRun>,
    workflow_ipc: Option<&WorkflowIpcContext>,
    permission_overlay: &mut TurnPermissionOverlay,
    permission_handler: Option<&(dyn RuntimePermissionRequestHandler + Send + Sync)>,
    child_executor: ChildAgentExecutor<W>,
    workflow_child_executor: ChildAgentExecutor<SharedEventBuffer>,
) -> io::Result<ToolTurnOutcome> {
    let (status, result) = execute_tool_with_approval(
        config,
        events,
        sink,
        tool_request,
        ToolExecutionContext::new(cwd, subagent_depth, emit_deltas, policy)
            .with_services(instructions, memory, mcp_registry, hooks)
            .with_runtime(
                cost_tracker,
                cancel,
                task_registry,
                background_workflows,
                workflow_ipc,
            )
            .with_permission_overlay(permission_overlay)
            .with_permission_handler(permission_handler),
        child_executor,
        workflow_child_executor,
    )?;

    record_normal_tool_result(
        conversation,
        history_writer,
        tool_request,
        &result,
        status,
        emit_deltas,
    )
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
    use crate::hooks::HookRunner;
    use crate::tool_execution::policy_for_tool_execution;

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
    fn tool_request_cursor_advances_over_single_and_batch_requests() {
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

        let mut cursor = ToolRequestCursor::new(&requests);

        assert_eq!(
            cursor.current().map(|request| request.id.as_str()),
            Some("tool-1")
        );
        cursor.advance_one();
        assert_eq!(cursor.position(), 1);
        assert_eq!(
            cursor.current().map(|request| request.id.as_str()),
            Some("tool-2")
        );
        cursor.advance_to(3);
        assert_eq!(cursor.position(), 3);
        assert!(cursor.current().is_none());
        cursor.advance_to(99);
        assert_eq!(cursor.position(), 3);
    }

    #[test]
    fn record_normal_tool_result_returns_approval_required_after_recording_tool_message() {
        let mut conversation = Conversation::new();
        let request = request(
            ToolName::RequestPermissions,
            ActionKind::Read,
            Some("read"),
            None,
        );
        let result = ToolResult::denied(&request, "needs approval");

        let outcome = record_normal_tool_result(
            &mut conversation,
            None,
            &request,
            &result,
            RunStatus::ApprovalRequired,
            false,
        )
        .expect("record approval result");

        match outcome {
            ToolTurnOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::ApprovalRequired);
                assert_eq!(error.as_deref(), Some("needs approval"));
            }
            ToolTurnOutcome::Continue => panic!("approval-required result must return"),
        }
        assert_eq!(conversation.messages.len(), 1);
        assert!(
            matches!(&conversation.messages[0], Message::Tool { tool_call_id, .. } if tool_call_id == "tool-1")
        );
    }

    #[test]
    fn record_normal_tool_result_returns_subagent_failure_after_recording_tool_message() {
        let mut conversation = Conversation::new();
        let request = request(ToolName::Subagent, ActionKind::Agent, Some("audit"), None);
        let result = ToolResult::failed(&request, "child failed", None);

        let outcome = record_normal_tool_result(
            &mut conversation,
            None,
            &request,
            &result,
            RunStatus::Failed,
            false,
        )
        .expect("record subagent failure");

        match outcome {
            ToolTurnOutcome::Return { status, error } => {
                assert_eq!(status, RunStatus::Failed);
                assert_eq!(error.as_deref(), Some("child failed"));
            }
            ToolTurnOutcome::Continue => panic!("failed subagent result must return"),
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
        let mut permission_overlay = TurnPermissionOverlay::default();

        let outcome = run_normal_tool_turn(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &mut conversation,
            None,
            &request,
            0,
            false,
            &policy,
            &instructions,
            &memory,
            &registry,
            &hooks,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            &mut background_workflows,
            None,
            &mut permission_overlay,
            None,
            unused_child_executor,
            unused_child_executor,
        )
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

        let outcome = run_readonly_tool_turn(
            cwd.path(),
            &mut events,
            &mut sink,
            &mut conversation,
            None,
            &requests,
            false,
            &registry,
            &hooks,
            ToolConfig::default().output_truncation,
        )
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
        let policy = policy_for_tool_execution(&config);

        let outcome = run_tool_turns(
            &config,
            cwd.path(),
            &mut events,
            &mut sink,
            &mut conversation,
            None,
            &[request],
            AgentToolPolicyContext::new(Some(&allowed), Some("test child")),
            1,
            true,
            &policy,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &mut cost_tracker,
            &cancel,
            &task_registry,
            &mut background_workflows,
            None,
            None,
            unused_child_executor::<Vec<u8>>,
            unused_child_executor::<SharedEventBuffer>,
            unused_child_executor::<io::Sink>,
        )
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
