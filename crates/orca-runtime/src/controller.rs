use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use orca_core::cancel::CancelToken;
use orca_core::config::{OutputFormat, RunConfig};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
#[cfg(test)]
use orca_core::tool_types;
#[cfg(test)]
use orca_mcp::McpRegistry;
#[cfg(test)]
use orca_tools;

use crate::agent_common;
use crate::agent_loop::run_agent_loop;
#[cfg(test)]
use crate::cost::CostTracker;
use crate::hooks::HookContext;
#[cfg(test)]
use crate::hooks::HookRunner;
#[cfg(test)]
use crate::instructions::ProjectInstructions;
#[cfg(test)]
use crate::lifecycle::RuntimeTaskKind;
use crate::lifecycle::{
    AgentLoopContext, RuntimePermissionRequestHandler, RuntimeSessionLifecycle, ThreadSteerHandle,
};
use crate::session::{
    AgentConversationContext, InteractiveSession, InteractiveSessionRuntimeParts,
};
#[cfg(test)]
use crate::tasks::TaskRegistry;
use crate::thread::RuntimeThread;
#[cfg(test)]
use crate::thread_store::SessionStore;
use crate::tool_invocation::AgentToolPolicyContext;
#[cfg(test)]
use crate::tool_invocation::{
    apply_pre_tool_outcome_with_external, prepare_tool_invocation_with_external,
};
use crate::workflow_execution::{BackgroundWorkflowRun, observe_background_workflows};
use orca_core::hook_types::HookEvent;

#[cfg(test)]
const DEFAULT_MAX_TURNS: u32 = 128;

#[derive(Clone, Copy, Debug)]
pub struct ControllerRunOptions {
    pub wait_for_background_workflows: bool,
}

impl Default for ControllerRunOptions {
    fn default() -> Self {
        Self {
            wait_for_background_workflows: true,
        }
    }
}

impl ControllerRunOptions {
    fn for_run_config(config: &RunConfig) -> Self {
        Self {
            wait_for_background_workflows: config.output_format == OutputFormat::Jsonl,
        }
    }
}

pub struct ThreadTurnExecutor<'a> {
    config: &'a RunConfig,
    session: &'a mut InteractiveSession,
    lifecycle: &'a mut RuntimeSessionLifecycle,
}

pub struct ThreadTurnContext<'a> {
    cwd: PathBuf,
    prompt: String,
    parts: InteractiveSessionRuntimeParts<'a>,
}

pub struct ThreadTurnExecution<W: io::Write> {
    events: EventFactory,
    sink: EventSink<W>,
    cancel: CancelToken,
    background_workflows: Vec<BackgroundWorkflowRun>,
}

#[derive(Clone)]
pub struct ThreadTurnRequest {
    prompt: String,
    options: ControllerRunOptions,
    emit_session_completed: bool,
    steer_handle: Option<ThreadSteerHandle>,
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
}

impl<'a> ThreadTurnExecutor<'a> {
    pub fn new(
        config: &'a RunConfig,
        session: &'a mut InteractiveSession,
        lifecycle: &'a mut RuntimeSessionLifecycle,
    ) -> Self {
        Self {
            config,
            session,
            lifecycle,
        }
    }

    pub fn run<W: io::Write>(&mut self, prompt: &str, writer: W) -> io::Result<RunStatus> {
        self.run_request(&ThreadTurnRequest::new(prompt), writer)
    }

    pub fn run_request<W: io::Write>(
        &mut self,
        request: &ThreadTurnRequest,
        writer: W,
    ) -> io::Result<RunStatus> {
        self.run_request_with_cancel(request, writer, CancelToken::new())
    }

    pub fn run_request_with_cancel<W: io::Write>(
        &mut self,
        request: &ThreadTurnRequest,
        writer: W,
        cancel: CancelToken,
    ) -> io::Result<RunStatus> {
        run_thread_turn_inner(
            self.config,
            self.session,
            self.lifecycle,
            request,
            writer,
            cancel,
        )
    }

    pub fn run_request_with_event_factory<W: io::Write>(
        &mut self,
        request: &ThreadTurnRequest,
        writer: W,
        events: &mut EventFactory,
    ) -> io::Result<RunStatus> {
        run_thread_turn_inner_with_events(
            self.config,
            self.session,
            self.lifecycle,
            request,
            writer,
            CancelToken::new(),
            Some(events),
        )
    }
}

impl<'a> ThreadTurnContext<'a> {
    pub fn prepare(
        config: &RunConfig,
        session: &'a mut InteractiveSession,
        request: &ThreadTurnRequest,
    ) -> io::Result<Self> {
        let cwd = config.cwd.clone().unwrap_or(std::env::current_dir()?);
        let prompt = request.prompt().to_string();
        let mut parts = session.runtime_parts();
        parts
            .conversation
            .replace_skill_context(agent_common::explicit_skill_context(&cwd, &prompt));
        parts.conversation.add_user(prompt.clone());
        if let Some(writer) = parts.writer.as_deref_mut()
            && let Some(message) = parts.conversation.messages.last()
        {
            writer.append_message(message)?;
        }

        Ok(Self { cwd, prompt, parts })
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }
}

impl<W: io::Write> ThreadTurnExecution<W> {
    pub fn new(
        lifecycle: &RuntimeSessionLifecycle,
        writer: W,
        output_format: OutputFormat,
    ) -> Self {
        Self::new_with_cancel(lifecycle, writer, output_format, CancelToken::new())
    }

    pub fn new_with_cancel(
        lifecycle: &RuntimeSessionLifecycle,
        writer: W,
        output_format: OutputFormat,
        cancel: CancelToken,
    ) -> Self {
        Self::new_with_events(
            EventFactory::new(lifecycle.run_id().to_string()),
            writer,
            output_format,
            cancel,
        )
    }

    pub fn new_with_events(
        events: EventFactory,
        writer: W,
        output_format: OutputFormat,
        cancel: CancelToken,
    ) -> Self {
        Self {
            events,
            sink: EventSink::new(writer, output_format),
            cancel,
            background_workflows: Vec::new(),
        }
    }

    pub fn run_id(&self) -> &str {
        self.events.run_id()
    }

    pub fn background_workflow_count(&self) -> usize {
        self.background_workflows.len()
    }
}

impl ThreadTurnRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            options: ControllerRunOptions::default(),
            emit_session_completed: true,
            steer_handle: None,
            permission_handler: None,
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn options(&self) -> ControllerRunOptions {
        self.options
    }

    pub fn with_options(mut self, options: ControllerRunOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_wait_for_background_workflows(mut self, wait: bool) -> Self {
        self.options.wait_for_background_workflows = wait;
        self
    }

    pub fn with_session_completed_event(mut self, emit: bool) -> Self {
        self.emit_session_completed = emit;
        self
    }

    pub fn emit_session_completed(&self) -> bool {
        self.emit_session_completed
    }

    pub fn with_steer_handle(mut self, handle: ThreadSteerHandle) -> Self {
        self.steer_handle = Some(handle);
        self
    }

    pub fn with_permission_handler(
        mut self,
        handler: Arc<dyn RuntimePermissionRequestHandler + Send + Sync>,
    ) -> Self {
        self.permission_handler = Some(handler);
        self
    }

    pub fn steer_handle(&self) -> Option<&ThreadSteerHandle> {
        self.steer_handle.as_ref()
    }

    pub fn permission_handler(
        &self,
    ) -> Option<&(dyn RuntimePermissionRequestHandler + Send + Sync)> {
        self.permission_handler.as_deref()
    }
}

pub fn run(config: RunConfig) -> i32 {
    let stdout = io::stdout();
    let options = ControllerRunOptions::for_run_config(&config);
    match run_inner(config, stdout.lock(), options) {
        Ok(status) => status.exit_code(),
        Err(error) => {
            eprintln!("orca: {error}");
            RunStatus::Failed.exit_code()
        }
    }
}

pub fn run_to_writer<W: io::Write>(config: RunConfig, writer: W) -> i32 {
    let options = ControllerRunOptions::for_run_config(&config);
    run_to_writer_with_options(config, writer, options)
}

pub fn run_to_writer_with_options<W: io::Write>(
    config: RunConfig,
    writer: W,
    options: ControllerRunOptions,
) -> i32 {
    match run_inner(config, writer, options) {
        Ok(status) => status.exit_code(),
        Err(error) => {
            eprintln!("orca: {error}");
            RunStatus::Failed.exit_code()
        }
    }
}

fn run_inner<W: io::Write>(
    config: RunConfig,
    writer: W,
    options: ControllerRunOptions,
) -> io::Result<RunStatus> {
    let cwd_path = config.cwd.clone().unwrap_or(std::env::current_dir()?);
    let cwd = cwd_path.display().to_string();
    let prompt = if config.prompt.trim().is_empty() {
        "(empty prompt)".to_string()
    } else {
        config.prompt.trim().to_string()
    };

    let mut sink = EventSink::new(writer, config.output_format);
    let mut thread = RuntimeThread::start(&config, &prompt)?;
    for error in thread.session().mcp_registry().errors() {
        eprintln!("orca: warning: {error}");
    }
    let mut events = EventFactory::new(thread.thread_id().to_string());
    sink.emit(&events.session_started(
        &cwd,
        config.approval_mode.as_str(),
        config.provider.as_str(),
        config.verifier.as_deref(),
    ))?;
    if let Err(error) = thread.session().hooks().run(
        HookEvent::SessionStart,
        HookContext {
            cwd: &cwd,
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        },
    ) {
        sink.emit(&events.error(&format!("session_start hook failed: {error}")))?;
    }

    let status = thread.run_request_with_event_factory(
        &config,
        &ThreadTurnRequest::new(&prompt)
            .with_options(options)
            .with_session_completed_event(false),
        sink.writer_mut(),
        &mut events,
    )?;

    if let Err(error) = thread.session().hooks().run(
        HookEvent::SessionEnd,
        HookContext {
            cwd: &cwd,
            session_status: Some(status.as_str()),
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        },
    ) {
        sink.emit(&events.error(&format!("session_end hook failed: {error}")))?;
    }

    sink.emit(&events.session_completed(status))?;
    if config.desktop_notifications {
        let _ = crate::notify::notify("Orca", &format!("Session {}", status.as_str()));
    }
    Ok(status)
}

pub fn run_thread_turn_to_writer<W: io::Write>(
    config: &RunConfig,
    session: &mut InteractiveSession,
    lifecycle: &mut RuntimeSessionLifecycle,
    prompt: &str,
    writer: W,
    options: ControllerRunOptions,
) -> io::Result<RunStatus> {
    ThreadTurnExecutor::new(config, session, lifecycle).run_request(
        &ThreadTurnRequest::new(prompt).with_options(options),
        writer,
    )
}

pub fn run_thread_turn_to_writer_with_cancel<W: io::Write>(
    config: &RunConfig,
    session: &mut InteractiveSession,
    lifecycle: &mut RuntimeSessionLifecycle,
    prompt: &str,
    writer: W,
    options: ControllerRunOptions,
    cancel: CancelToken,
) -> io::Result<RunStatus> {
    run_thread_turn_inner(
        config,
        session,
        lifecycle,
        &ThreadTurnRequest::new(prompt).with_options(options),
        writer,
        cancel,
    )
}

fn run_thread_turn_inner<W: io::Write>(
    config: &RunConfig,
    session: &mut InteractiveSession,
    lifecycle: &mut RuntimeSessionLifecycle,
    request: &ThreadTurnRequest,
    writer: W,
    cancel: CancelToken,
) -> io::Result<RunStatus> {
    run_thread_turn_inner_with_events(config, session, lifecycle, request, writer, cancel, None)
}

fn run_thread_turn_inner_with_events<W: io::Write>(
    config: &RunConfig,
    session: &mut InteractiveSession,
    lifecycle: &mut RuntimeSessionLifecycle,
    request: &ThreadTurnRequest,
    writer: W,
    cancel: CancelToken,
    events: Option<&mut EventFactory>,
) -> io::Result<RunStatus> {
    let context = ThreadTurnContext::prepare(config, session, request)?;
    let ThreadTurnContext { cwd, prompt, parts } = context;

    if let Some(events) = events {
        let mut sink = EventSink::new(writer, config.output_format);
        let cancel_ref = cancel;
        let mut background_workflows = Vec::new();
        let result = run_agent_loop(
            config,
            AgentLoopContext::new(&cwd, &prompt, 0, true, &SubagentType::General)
                .with_services(
                    parts.instructions,
                    parts.memory,
                    parts.mcp_registry,
                    parts.hooks,
                )
                .with_runtime(parts.cost_tracker, &cancel_ref, parts.task_registry)
                .with_execution(&mut background_workflows, None, Some(lifecycle))
                .with_steer_handle(request.steer_handle())
                .with_permission_handler(request.permission_handler()),
            events,
            &mut sink,
            AgentConversationContext::new()
                .with_history_writer(parts.writer)
                .with_conversation(Some(parts.conversation)),
            AgentToolPolicyContext::unrestricted(),
        )?;
        let status = result.status;
        lifecycle.finish_task(status);
        observe_background_workflows(
            request.options().wait_for_background_workflows,
            events,
            &mut sink,
            &mut background_workflows,
        )?;
        let status = run_verifier_if_needed(status, config.verifier.as_deref(), events, &mut sink)?;
        session.complete(status.as_str());
        if request.emit_session_completed() {
            sink.emit(&events.session_completed(status))?;
        }
        return Ok(status);
    }

    let mut execution =
        ThreadTurnExecution::new_with_cancel(lifecycle, writer, config.output_format, cancel);
    let result = run_agent_loop(
        config,
        AgentLoopContext::new(&cwd, &prompt, 0, true, &SubagentType::General)
            .with_services(
                parts.instructions,
                parts.memory,
                parts.mcp_registry,
                parts.hooks,
            )
            .with_runtime(parts.cost_tracker, &execution.cancel, parts.task_registry)
            .with_execution(&mut execution.background_workflows, None, Some(lifecycle))
            .with_steer_handle(request.steer_handle())
            .with_permission_handler(request.permission_handler()),
        &mut execution.events,
        &mut execution.sink,
        AgentConversationContext::new()
            .with_history_writer(parts.writer)
            .with_conversation(Some(parts.conversation)),
        AgentToolPolicyContext::unrestricted(),
    )?;
    let status = result.status;
    lifecycle.finish_task(status);
    observe_background_workflows(
        request.options().wait_for_background_workflows,
        &mut execution.events,
        &mut execution.sink,
        &mut execution.background_workflows,
    )?;
    let status = run_verifier_if_needed(
        status,
        config.verifier.as_deref(),
        &mut execution.events,
        &mut execution.sink,
    )?;
    session.complete(status.as_str());
    if request.emit_session_completed() {
        execution
            .sink
            .emit(&execution.events.session_completed(status))?;
    }
    Ok(status)
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

#[cfg(test)]
fn execute_readonly_batch(
    cwd: &Path,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
    tool_requests: &[tool_types::ToolRequest],
    emit_deltas: bool,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    output_truncation: tool_types::ToolOutputTruncation,
) -> io::Result<Vec<tool_types::ToolResult>> {
    let mut hook_failed: Vec<Option<tool_types::ToolResult>> = vec![None; tool_requests.len()];
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

fn run_verifier_if_needed(
    status: RunStatus,
    verifier: Option<&str>,
    events: &mut EventFactory,
    sink: &mut EventSink<impl io::Write>,
) -> io::Result<RunStatus> {
    if status != RunStatus::Success {
        return Ok(status);
    }

    let Some(command) = verifier else {
        return Ok(status);
    };

    sink.emit(&events.verification_started(command))?;
    let result = orca_core::verification::run(command);
    let success = result.success;
    sink.emit(&events.verification_completed(&result))?;

    if success {
        Ok(RunStatus::Success)
    } else {
        Ok(RunStatus::VerificationFailed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::execute_child_agent_loop;
    use crate::hooks::HookOutcome;
    use crate::hooks::conversation_with_hook_context;
    use crate::lifecycle::{RuntimeTaskStatus, RuntimeToolActorContext};
    use crate::memory::MemoryBlock;
    use crate::subagent_execution::{collect_subagent_batch, should_run_subagent_batch};
    use crate::tool_execution::{ToolExecutionActor, ToolExecutionContext};
    use crate::tool_invocation::prepare_tool_invocation;
    use orca_approval::ApprovalPolicy;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind};
    use orca_core::conversation::Conversation;
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;

    fn config(subagents: SubagentConfig) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
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
            subagents,
            tools: Default::default(),
            workflows: Default::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }

    fn subagent_request(id: &str) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            id: id.to_string(),
            name: tool_types::ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("task".to_string()),
            raw_arguments: None,
        }
    }

    fn tool_request(
        id: &str,
        name: tool_types::ToolName,
        action: ActionKind,
    ) -> tool_types::ToolRequest {
        tool_types::ToolRequest {
            id: id.to_string(),
            name,
            action,
            target: Some("target".to_string()),
            raw_arguments: None,
        }
    }

    #[test]
    fn workflow_ipc_tool_requires_workflow_child_context() {
        let mut context = RuntimeToolActorContext::new("test-run", DEFAULT_MAX_TURNS);
        let request = tool_types::ToolRequest {
            id: "mailbox".to_string(),
            name: tool_types::ToolName::WorkflowReadMessages,
            action: ActionKind::Agent,
            target: Some("findings".to_string()),
            raw_arguments: Some(serde_json::json!({ "channel": "findings" }).to_string()),
        };

        let result = context.execute_workflow_ipc_tool(&request, None);

        assert_eq!(result.status, tool_types::ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("only available inside workflow child agents")
        );
    }

    #[test]
    fn subagent_batch_respects_parallel_limit() {
        let config = config(SubagentConfig::default());
        let requests = vec![
            subagent_request("a"),
            subagent_request("b"),
            subagent_request("c"),
            subagent_request("d"),
            subagent_request("e"),
            subagent_request("f"),
            subagent_request("g"),
        ];

        assert!(should_run_subagent_batch(&config, &requests[0], 0));
        assert_eq!(collect_subagent_batch(&config, &requests, 0), 6);
    }

    #[test]
    fn async_subagent_skips_sync_batch_path() {
        let config = config(SubagentConfig::default());
        let request = tool_types::ToolRequest {
            id: "async".to_string(),
            name: tool_types::ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some("async task".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async task",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
        };

        assert!(!should_run_subagent_batch(&config, &request, 0));
    }

    #[test]
    fn max_parallel_one_uses_sequential_subagent_path() {
        let config = config(
            SubagentConfig {
                max_depth: 2,
                max_parallel: 1,
            }
            .normalized(),
        );
        let request = subagent_request("a");

        assert!(!should_run_subagent_batch(&config, &request, 0));
    }

    #[test]
    fn subagent_batch_stops_at_first_non_subagent_tool() {
        let config = config(SubagentConfig::default());
        let mut requests = vec![subagent_request("a"), subagent_request("b")];
        requests.push(tool_types::ToolRequest {
            id: "read".to_string(),
            name: tool_types::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("src/main.rs".to_string()),
            raw_arguments: None,
        });
        requests.push(subagent_request("c"));

        assert_eq!(collect_subagent_batch(&config, &requests, 0), 2);
    }

    #[test]
    fn subagent_batch_stops_at_first_async_subagent() {
        let config = config(SubagentConfig::default());
        let async_request = tool_types::ToolRequest {
            id: "async".to_string(),
            name: tool_types::ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some("async task".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async task",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
        };
        let requests = vec![subagent_request("a"), async_request, subagent_request("b")];

        assert_eq!(collect_subagent_batch(&config, &requests, 0), 1);
    }

    #[test]
    fn subagent_status_returns_session_local_task_result() {
        let mut context = RuntimeToolActorContext::new("test-run", DEFAULT_MAX_TURNS);
        let registry = TaskRegistry::new("session-status".to_string());
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));
        registry
            .complete(&task.id, "finished async audit".to_string())
            .unwrap();
        let request = tool_types::ToolRequest {
            id: "status".to_string(),
            name: tool_types::ToolName::SubagentStatus,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(serde_json::json!({ "agent_id": task.id }).to_string()),
        };

        let result = context.execute_subagent_status_tool(&request, &registry);

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        let payload: serde_json::Value =
            serde_json::from_str(result.output.as_deref().unwrap()).unwrap();
        assert_eq!(payload["status"], "completed");
        assert_eq!(payload["description"], "inspect auth");
        assert_eq!(payload["agent_type"], "general");
        assert!(payload["created_at_ms"].as_i64().unwrap() > 0);
        assert!(payload["started_at_ms"].as_i64().unwrap() > 0);
        assert!(payload["completed_at_ms"].as_i64().unwrap() > 0);
        assert_eq!(payload["output"], "finished async audit");
        assert_eq!(payload["error"], serde_json::Value::Null);
    }

    #[test]
    fn readonly_batch_respects_parallel_limit() {
        let mut config = config(SubagentConfig::default());
        config.tools.max_read_parallel = 2;
        let requests = vec![
            tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Read),
            tool_request("b", tool_types::ToolName::Grep, ActionKind::Read),
            tool_request("c", tool_types::ToolName::ListFiles, ActionKind::Read),
        ];

        assert!(orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &requests[0]
        ));
        assert_eq!(
            orca_tools::collect_readonly_batch(config.tools.max_read_parallel, &requests, 0),
            2
        );
    }

    #[test]
    fn readonly_batch_stops_at_first_mutating_tool() {
        let config = config(SubagentConfig::default());
        let requests = vec![
            tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Read),
            tool_request("b", tool_types::ToolName::Bash, ActionKind::Shell),
            tool_request("c", tool_types::ToolName::Grep, ActionKind::Read),
        ];

        assert_eq!(
            orca_tools::collect_readonly_batch(config.tools.max_read_parallel, &requests, 0),
            1
        );
        assert!(!orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &requests[1]
        ));
    }

    #[test]
    fn readonly_batch_uses_spec_not_request_action() {
        let config = config(SubagentConfig::default());
        let request = tool_request("a", tool_types::ToolName::ReadFile, ActionKind::Write);

        assert!(orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &request
        ));
    }

    #[test]
    fn readonly_batch_rejects_shell_by_capability() {
        let config = config(SubagentConfig::default());
        let request = tool_request("bash", tool_types::ToolName::Bash, ActionKind::Read);

        assert!(!orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &request
        ));
    }

    #[test]
    fn approval_action_rejects_caller_supplied_read_for_shell() {
        let request = tool_request("bash", tool_types::ToolName::Bash, ActionKind::Read);
        let registry = McpRegistry::default();

        assert_eq!(
            canonical_action_for_tool(&request, &registry, &[]),
            ActionKind::Shell
        );
    }

    #[test]
    fn readonly_batch_skips_network_actions() {
        let config = config(SubagentConfig::default());
        let request = tool_request("a", tool_types::ToolName::WebSearch, ActionKind::Network);

        assert!(!orca_tools::should_run_readonly_batch(
            config.tools.max_read_parallel,
            &request
        ));
    }

    #[test]
    fn readonly_batch_executes_results_in_request_order() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha").unwrap();
        std::fs::write(dir.path().join("b.txt"), "bravo").unwrap();
        let requests = vec![
            tool_types::ToolRequest {
                target: Some("a.txt".to_string()),
                raw_arguments: Some(r#"{"path":"a.txt"}"#.to_string()),
                ..tool_request("first", tool_types::ToolName::ReadFile, ActionKind::Read)
            },
            tool_types::ToolRequest {
                target: Some("b.txt".to_string()),
                raw_arguments: Some(r#"{"path":"b.txt"}"#.to_string()),
                ..tool_request("second", tool_types::ToolName::ReadFile, ActionKind::Read)
            },
        ];
        let mut events = EventFactory::new("test-run".to_string());
        let mut output = Vec::new();
        let mut sink = EventSink::new(&mut output, OutputFormat::Jsonl);
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();

        let results = execute_readonly_batch(
            dir.path(),
            &mut events,
            &mut sink,
            &requests,
            true,
            &registry,
            &hooks,
            tool_types::ToolOutputTruncation::default(),
        )
        .unwrap();

        assert_eq!(
            results
                .iter()
                .map(|result| result.id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert_eq!(results[0].output.as_deref(), Some("alpha"));
        assert_eq!(results[1].output.as_deref(), Some("bravo"));
    }

    #[test]
    fn pre_model_hook_context_is_added_as_pinned_system_message() {
        let mut conversation = Conversation::new();
        conversation.add_system("base system".to_string());
        conversation.add_user("do work".to_string());
        let outcome = HookOutcome {
            modified_target: None,
            injected_context: vec!["policy hint".to_string(), "repo hint".to_string()],
        };

        let model_conversation = conversation_with_hook_context(&conversation, &outcome);

        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(model_conversation.messages.len(), 3);
        assert!(matches!(
            model_conversation.messages.last(),
            Some(orca_core::conversation::Message::System { content, pinned: true })
                if content.contains("policy hint") && content.contains("repo hint")
        ));
    }

    #[test]
    fn agent_conversation_context_groups_history_inputs() {
        let cwd = tempfile::tempdir().unwrap();
        let mut writer = SessionStore::new()
            .start_writer(
                cwd.path(),
                "test-provider",
                Some("test-model".to_string()),
                "agent-conversation-context",
            )
            .unwrap();
        let mut conversation = Conversation::new();
        conversation.add_system("system".to_string());

        let context = AgentConversationContext::new()
            .with_history_writer(Some(&mut writer))
            .with_conversation(Some(&mut conversation));

        assert!(context.resumed().is_none());
        assert!(context.history_writer().is_some());
        assert!(context.conversation().is_some());
    }

    #[test]
    fn agent_tool_policy_context_groups_child_tool_policy() {
        let allowed_tools = vec!["read".to_string(), "edit".to_string()];
        let context =
            AgentToolPolicyContext::new(Some(allowed_tools.as_slice()), Some("review-only"));

        assert_eq!(context.allowed_tools().unwrap(), allowed_tools.as_slice());
        assert_eq!(context.label(), Some("review-only"));
    }

    #[test]
    fn tool_execution_context_groups_tool_services() {
        let cwd = PathBuf::from("/tmp/orca-tool-execution-services");
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);

        let context = ToolExecutionContext::new(&cwd, 1, true, &policy).with_services(
            &instructions,
            &memory,
            &registry,
            &hooks,
        );

        assert_eq!(context.cwd(), cwd.as_path());
        assert_eq!(context.subagent_depth(), 1);
        assert!(context.emit_deltas());
        assert!(std::ptr::eq(context.policy(), &policy));
        assert!(std::ptr::eq(context.instructions(), &instructions));
        assert!(std::ptr::eq(context.memory(), &memory));
        assert!(std::ptr::eq(context.mcp_registry(), &registry));
        assert!(std::ptr::eq(context.hooks(), &hooks));
    }

    #[test]
    fn tool_execution_context_groups_runtime_state() {
        let cwd = PathBuf::from("/tmp/orca-tool-execution-runtime");
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-execution-runtime".to_string());
        let mut background_workflows = Vec::new();

        let context = ToolExecutionContext::new(&cwd, 0, false, &policy).with_runtime(
            &mut cost_tracker,
            &cancel,
            &task_registry,
            &mut background_workflows,
            None,
        );

        assert_eq!(context.cost_tracker().totals().total_tokens(), 0);
        assert!(std::ptr::eq(context.cancel(), &cancel));
        assert!(std::ptr::eq(context.task_registry(), &task_registry));
        assert_eq!(context.background_workflow_count(), 0);
        assert!(context.workflow_ipc().is_none());
    }

    #[test]
    fn tool_execution_actor_owns_runtime_tool_actor_state() {
        let actor = ToolExecutionActor::new("tool-actor-run", DEFAULT_MAX_TURNS);
        let task = actor.active_task().expect("active task");

        assert_eq!(task.kind(), RuntimeTaskKind::Agent);
        assert_eq!(task.status(), RuntimeTaskStatus::Running);
    }

    #[test]
    fn tool_execution_actor_executes_normal_tool_from_context() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").unwrap();
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("tool-actor-execute".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            id: "read-file".to_string(),
            name: tool_types::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("tracked.txt".to_string()),
            raw_arguments: Some(serde_json::json!({ "path": "tracked.txt" }).to_string()),
        };
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let registry = McpRegistry::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-actor-execute".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = crate::lifecycle::TurnPermissionOverlay::default();
        let context = ToolExecutionContext::new(cwd.path(), 0, true, &policy)
            .with_services(&instructions, &memory, &registry, &hooks)
            .with_runtime(
                &mut cost_tracker,
                &cancel,
                &task_registry,
                &mut background_workflows,
                None,
            )
            .with_permission_overlay(&mut permission_overlay);

        let mut actor = ToolExecutionActor::new(events.run_id().to_string(), DEFAULT_MAX_TURNS);
        let (status, result) = actor
            .execute(
                &config,
                &mut events,
                &mut sink,
                &request,
                context,
                execute_child_agent_loop,
                execute_child_agent_loop,
            )
            .unwrap();

        assert_eq!(status, RunStatus::Success);
        assert_eq!(result.status, tool_types::ToolStatus::Completed);
        assert_eq!(result.id, "read-file");
    }

    #[test]
    fn tool_execution_actor_approval_allows_read_tool_to_continue() {
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("tool-actor-approval".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            id: "read-file".to_string(),
            name: tool_types::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("tracked.txt".to_string()),
            raw_arguments: Some(serde_json::json!({ "path": "tracked.txt" }).to_string()),
        };
        let registry = McpRegistry::default();
        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);
        let policy = ApprovalPolicy::new(ApprovalMode::FullAuto);

        let mut actor = ToolExecutionActor::new(events.run_id().to_string(), DEFAULT_MAX_TURNS);
        let result = actor
            .handle_approval(
                &config,
                &mut events,
                &mut sink,
                &request,
                &invocation,
                &policy,
                false,
                true,
            )
            .unwrap();

        assert!(result.is_none());
    }

    #[test]
    fn tool_execution_actor_dispatches_normal_tool() {
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("tracked.txt"), "hello\n").unwrap();
        let config = config(SubagentConfig::default());
        let mut events = EventFactory::new("tool-actor-dispatch".to_string());
        let mut sink = EventSink::new(Vec::new(), OutputFormat::Jsonl);
        let request = tool_types::ToolRequest {
            id: "read-file".to_string(),
            name: tool_types::ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("tracked.txt".to_string()),
            raw_arguments: Some(serde_json::json!({ "path": "tracked.txt" }).to_string()),
        };
        let registry = McpRegistry::default();
        let instructions = ProjectInstructions::default();
        let memory = MemoryBlock::default();
        let hooks = HookRunner::default();
        let mut cost_tracker = CostTracker::new(None);
        let cancel = CancelToken::new();
        let task_registry = TaskRegistry::new("tool-actor-dispatch".to_string());
        let mut background_workflows = Vec::new();
        let mut permission_overlay = crate::lifecycle::TurnPermissionOverlay::default();

        let mut actor = ToolExecutionActor::new(events.run_id().to_string(), DEFAULT_MAX_TURNS);
        let result = actor
            .dispatch_tool(
                &config,
                cwd.path(),
                &mut events,
                &mut sink,
                &request,
                0,
                &instructions,
                &memory,
                &registry,
                &hooks,
                true,
                &mut cost_tracker,
                &cancel,
                &task_registry,
                &mut background_workflows,
                None,
                &mut permission_overlay,
                None,
                execute_child_agent_loop,
                execute_child_agent_loop,
            )
            .unwrap();

        assert_eq!(result.status, tool_types::ToolStatus::Completed);
    }
}
