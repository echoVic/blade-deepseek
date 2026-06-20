use std::fs;
use std::io;
use std::io::sink;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::EventFactory;
use orca_core::event_schema::RunStatus;
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::TaskType;
use orca_core::workflow_types::{
    WorkflowAgentStatus, WorkflowInput, WorkflowOutput, WorkflowPhaseRecord, WorkflowRunState,
    WorkflowRunStatus,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::agent_child::{ChildAgentRequest, ChildAgentRuntime, run_child_agent};
use crate::controller::execute_child_agent_loop;
use crate::hooks::HookRunner;
use crate::instructions;
use crate::memory;
use crate::tasks::TaskRegistry;

use super::host::{AgentCall, HostCommand, HostEvent, WorkflowHost};
use super::script::resolve_workflow_script;
use super::state::{input_hash, WorkflowAgentRecord, WorkflowStateStore};

#[derive(Clone, Debug, Default)]
pub struct WorkflowLaunchRequest {
    input: WorkflowInput,
}

impl WorkflowLaunchRequest {
    pub fn from_script_path(script_path: String) -> Self {
        Self {
            input: WorkflowInput {
                script_path: Some(script_path),
                ..Default::default()
            },
        }
    }

    pub fn with_resume_from(mut self, run_id: String) -> Self {
        self.input.resume_from_run_id = Some(run_id);
        self
    }
}

impl From<WorkflowInput> for WorkflowLaunchRequest {
    fn from(input: WorkflowInput) -> Self {
        Self { input }
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowLaunchResult {
    pub task_id: String,
    pub output: WorkflowOutput,
    pub summary: String,
}

#[derive(Clone, Debug)]
pub struct WorkflowRunner {
    config: RunConfig,
    tasks: TaskRegistry,
    session_dir: PathBuf,
    state: WorkflowStateStore,
}

#[derive(Clone, Debug, Default)]
struct WorkflowExecutionCounters {
    total_agents: u32,
    active_agents: usize,
}

#[derive(Debug)]
struct WorkflowExecutionGate {
    counters: Mutex<WorkflowExecutionCounters>,
    condvar: Condvar,
}

impl WorkflowExecutionGate {
    fn new() -> Self {
        Self {
            counters: Mutex::new(WorkflowExecutionCounters::default()),
            condvar: Condvar::new(),
        }
    }

    fn begin_agent(
        self: &Arc<Self>,
        max_agents_per_run: u32,
        max_concurrent_agents: usize,
    ) -> io::Result<WorkflowAgentPermit> {
        let mut counters = self
            .counters
            .lock()
            .map_err(|_| io::Error::other("workflow execution counters poisoned"))?;
        if counters.total_agents >= max_agents_per_run {
            return Err(io::Error::other(format!(
                "maximum workflow agent count {max_agents_per_run} exceeded"
            )));
        }
        while counters.active_agents >= max_concurrent_agents {
            counters = self
                .condvar
                .wait(counters)
                .map_err(|_| io::Error::other("workflow execution counters poisoned"))?;
        }
        counters.total_agents += 1;
        counters.active_agents += 1;
        Ok(WorkflowAgentPermit {
            gate: Arc::clone(self),
        })
    }

    fn finish_agent(&self) {
        if let Ok(mut counters) = self.counters.lock() {
            if counters.active_agents > 0 {
                counters.active_agents -= 1;
            }
            self.condvar.notify_one();
        }
    }

    fn snapshot(&self) -> io::Result<WorkflowExecutionCounters> {
        self.counters
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| io::Error::other("workflow execution counters poisoned"))
    }

}

#[derive(Debug)]
struct WorkflowAgentPermit {
    gate: Arc<WorkflowExecutionGate>,
}

impl Drop for WorkflowAgentPermit {
    fn drop(&mut self) {
        self.gate.finish_agent();
    }
}

impl WorkflowRunner {
    pub fn new(config: RunConfig, tasks: TaskRegistry, session_dir: PathBuf) -> Self {
        let state = WorkflowStateStore::new(session_dir.join("workflow-runs"));
        Self {
            config,
            tasks,
            session_dir,
            state,
        }
    }

    pub fn launch(&self, request: WorkflowLaunchRequest) -> io::Result<WorkflowLaunchResult> {
        self.run(request)
    }

    pub fn resume(&self, request: WorkflowLaunchRequest) -> io::Result<WorkflowLaunchResult> {
        self.run(request)
    }

    fn run(&self, request: WorkflowLaunchRequest) -> io::Result<WorkflowLaunchResult> {
        let cwd = self.config.cwd.clone().unwrap_or(std::env::current_dir()?);
        fs::create_dir_all(&self.session_dir)?;

        let resolved = resolve_workflow_script(&request.input, &cwd, &self.session_dir)?;
        let run_id = format!("workflow-run-{}", uuid::Uuid::new_v4());
        let task = self.tasks.create_workflow(
            run_id.clone(),
            resolved.meta.name.clone(),
            resolved.meta.description.clone(),
        );
        let mut state = WorkflowRunState {
            run_id: run_id.clone(),
            task_id: task.id.clone(),
            session_id: self.tasks.session_id().to_string(),
            cwd: cwd.display().to_string(),
            workflow_name: resolved.meta.name.clone(),
            meta: resolved.meta.clone(),
            script_digest: resolved.script_digest.clone(),
            args_digest: digest_value(request.input.args.as_ref().unwrap_or(&Value::Null)),
            status: WorkflowRunStatus::Queued,
            phases: Vec::new(),
            total_agent_count: 0,
            final_summary: None,
            error: None,
        };
        self.state.create_run(&state)?;

        self.tasks
            .mark_running(&task.id)
            .map_err(io::Error::other)?;
        state.status = WorkflowRunStatus::Running;
        self.state.write_state(&state)?;

        let transcript_dir = self.state.transcript_dir(&run_id);
        let args = request.input.args.clone().unwrap_or(Value::Null);
        let resume_from = request.input.resume_from_run_id.clone();
        let mut cached_agents = 0u32;
        let mut failed_error = None;
        let mut completed_result = None;
        let gate = Arc::new(WorkflowExecutionGate::new());
        let workflow_limits = self.config.workflows.clone();

        let events = match WorkflowHost::run_collecting_events_with_agent(
            &resolved.persisted_path,
            args,
            |call| {
                self.answer_agent_call(
                    &run_id,
                    resume_from.as_deref(),
                    &transcript_dir,
                    call,
                    &mut cached_agents,
                    &gate,
                    &workflow_limits,
                )
            },
        ) {
            Ok(events) => events,
            Err(error) => {
                let message = error.to_string();
                state.total_agent_count = gate.snapshot()?.total_agents;
                state.status = WorkflowRunStatus::Failed;
                state.error = Some(message.clone());
                self.state.write_state(&state)?;
                self.tasks
                    .fail(&task.id, message.clone())
                    .map_err(io::Error::other)?;
                return Err(error);
            }
        };

        let mut phase_agent_baselines = std::collections::HashMap::new();
        let mut agent_events_seen = 0u32;
        for event in events {
            match event {
                HostEvent::PhaseStarted { name } => {
                    phase_agent_baselines.insert(name.clone(), agent_events_seen);
                    state.phases.push(WorkflowPhaseRecord {
                        name,
                        status: WorkflowRunStatus::Running,
                        started_at_ms: Some(now_ms()),
                        completed_at_ms: None,
                        agent_count: 0,
                    });
                }
                HostEvent::PhaseCompleted { name } => {
                    if let Some(phase) = state.phases.iter_mut().rev().find(|phase| phase.name == name)
                    {
                        phase.status = WorkflowRunStatus::Completed;
                        phase.completed_at_ms = Some(now_ms());
                        let baseline = phase_agent_baselines.get(&name).copied().unwrap_or(0);
                        phase.agent_count = agent_events_seen.saturating_sub(baseline);
                    }
                }
                HostEvent::AgentCall { .. } => {
                    agent_events_seen += 1;
                }
                HostEvent::WorkflowCompleted { result } => {
                    completed_result = Some(result_to_summary(&result));
                }
                HostEvent::WorkflowFailed { error } => {
                    finalize_open_phases_as_failed(
                        &mut state.phases,
                        &phase_agent_baselines,
                        agent_events_seen,
                    );
                    failed_error = Some(error);
                }
            }
        }

        state.total_agent_count = gate.snapshot()?.total_agents;
        if let Some(error) = failed_error {
            state.status = WorkflowRunStatus::Failed;
            state.error = Some(error.clone());
            self.state.write_state(&state)?;
            self.tasks
                .fail(&task.id, error.clone())
                .map_err(io::Error::other)?;
            return Err(io::Error::other(error));
        }

        let result = completed_result.unwrap_or_default();
        let cache_summary = if cached_agents == 1 {
            "cached 1 agent".to_string()
        } else {
            format!("cached {cached_agents} agents")
        };
        let summary = if cached_agents > 0 {
            format!("{result} ({cache_summary})")
        } else {
            result.clone()
        };

        state.status = WorkflowRunStatus::Completed;
        state.final_summary = Some(summary.clone());
        self.state.write_state(&state)?;
        self.tasks
            .complete(&task.id, result.clone())
            .map_err(io::Error::other)?;

        Ok(WorkflowLaunchResult {
            task_id: task.id.clone(),
            output: WorkflowOutput {
                status: "completed".to_string(),
                task_id: task.id,
                task_type: Some(task_type_name(task.task_type).to_string()),
                workflow_name: Some(resolved.meta.name),
                run_id: Some(run_id),
                summary: Some(summary.clone()),
                transcript_dir: Some(transcript_dir.display().to_string()),
                script_path: Some(resolved.persisted_path.display().to_string()),
                session_url: None,
            },
            summary,
        })
    }

    fn answer_agent_call(
        &self,
        run_id: &str,
        resume_from: Option<&str>,
        transcript_dir: &std::path::Path,
        call: AgentCall,
        cached_agents: &mut u32,
        gate: &Arc<WorkflowExecutionGate>,
        workflow_limits: &orca_core::config::WorkflowConfig,
    ) -> io::Result<HostCommand> {
        let _permit = match gate.begin_agent(
            workflow_limits.max_agents_per_run,
            workflow_limits.max_concurrent_agents,
        ) {
            Ok(permit) => permit,
            Err(error) => {
                return Ok(HostCommand::AgentError {
                    call_id: call.call_id,
                    error: error.to_string(),
                });
            }
        };
        let hash = input_hash(&call.prompt, &call.opts);
        if let Some(resume_run_id) = resume_from {
            if let Some(cached_value) =
                self.state
                    .find_cached_agent_value(resume_run_id, &call.call_path, &hash)
            {
                *cached_agents += 1;
                let normalized_cached_value = normalize_cached_agent_result(&call, cached_value);
                let output = result_to_summary(&normalized_cached_value);
                let transcript_path = write_agent_transcript(transcript_dir, &call, &output, true)?;
                self.state.record_agent_completed(
                    run_id,
                    WorkflowAgentRecord {
                        call_id: call.call_id.clone(),
                        call_path: call.call_path.clone(),
                        prompt: call.prompt.clone(),
                        opts: call.opts.clone(),
                        input_hash: hash,
                        status: WorkflowAgentStatus::Completed,
                        output: Some(normalized_cached_value.clone()),
                        error: None,
                        transcript_path: Some(transcript_path.display().to_string()),
                    },
                )?;
                return Ok(HostCommand::AgentResult {
                    call_id: call.call_id,
                    result: normalized_cached_value,
                });
            }
        }

        match self.run_child_agent_call(&call) {
            Ok(output) => {
                let output = child_agent_output(&call.prompt, &output);
                let transcript_path =
                    write_agent_transcript(transcript_dir, &call, &output, false)?;
                let result = workflow_agent_result(&call, Value::String(output.clone()), false);
                self.state.record_agent_completed(
                    run_id,
                    WorkflowAgentRecord {
                        call_id: call.call_id.clone(),
                        call_path: call.call_path.clone(),
                        prompt: call.prompt.clone(),
                        opts: call.opts.clone(),
                        input_hash: hash,
                        status: WorkflowAgentStatus::Completed,
                        output: Some(result.clone()),
                        error: None,
                        transcript_path: Some(transcript_path.display().to_string()),
                    },
                )?;

                Ok(HostCommand::AgentResult {
                    call_id: call.call_id,
                    result,
                })
            }
            Err(error) => {
                let error_message = error.to_string();
                let transcript_path =
                    write_agent_transcript(transcript_dir, &call, &error_message, false)?;
                self.state.record_agent_completed(
                    run_id,
                    WorkflowAgentRecord {
                        call_id: call.call_id.clone(),
                        call_path: call.call_path.clone(),
                        prompt: call.prompt.clone(),
                        opts: call.opts.clone(),
                        input_hash: hash,
                        status: WorkflowAgentStatus::Failed,
                        output: None,
                        error: Some(error_message.clone()),
                        transcript_path: Some(transcript_path.display().to_string()),
                    },
                )?;

                Ok(HostCommand::AgentError {
                    call_id: call.call_id,
                    error: error_message,
                })
            }
        }
    }

    fn run_child_agent_call(&self, call: &AgentCall) -> io::Result<String> {
        let cwd = self.config.cwd.as_deref().unwrap_or(self.session_dir.as_path());
        let mut events = EventFactory::new(format!("workflow-child-{}", call.call_id));
        let mut sink = EventSink::new(sink(), self.config.output_format);
        let instructions = instructions::load_for_cwd_or_default(cwd);
        let memory = memory::load_for_cwd(cwd);
        let (workflow_child_config, mcp_registry) =
            Self::workflow_child_runtime_parts(&self.config);
        let hooks = HookRunner::new(self.config.hooks.clone());
        let cancel = CancelToken::new();
        let child_request = ChildAgentRequest {
            prompt: call.prompt.clone(),
            subagent_type: SubagentType::General,
            model: None,
            depth: 1,
            emit_deltas: false,
        };
        let mut runtime = ChildAgentRuntime::new(
            cwd,
            &mut events,
            &mut sink,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            execute_child_agent_loop,
        );
        let (result, _) = run_child_agent(&workflow_child_config, &child_request, &mut runtime);

        match result.status {
            RunStatus::Success => Ok(result.final_message.unwrap_or_default()),
            _ => Err(io::Error::other(
                result
                    .error
                    .or(result.final_message)
                    .unwrap_or_else(|| "workflow child agent failed".to_string()),
            )),
        }
    }

    fn workflow_child_config(config: &RunConfig) -> RunConfig {
        let mut workflow_child_config = config.clone();
        workflow_child_config.approval_mode = ApprovalMode::AutoEdit;
        workflow_child_config
    }

    fn workflow_child_runtime_parts(
        config: &RunConfig,
    ) -> (RunConfig, orca_mcp::McpRegistry) {
        let workflow_child_config = Self::workflow_child_config(config);
        let mcp_registry = orca_mcp::initialize_registry(&workflow_child_config.mcp_servers);
        (workflow_child_config, mcp_registry)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn write_agent_transcript(
    transcript_dir: &std::path::Path,
    call: &AgentCall,
    output: &str,
    cached: bool,
) -> io::Result<PathBuf> {
    fs::create_dir_all(transcript_dir)?;
    let path = transcript_dir.join(format!("{}.json", call.call_id));
    let content = serde_json::json!({
        "callId": call.call_id,
        "callPath": call.call_path,
        "phase": call.phase,
        "prompt": call.prompt,
        "opts": call.opts,
        "cached": cached,
        "result": output,
    });
    let encoded = serde_json::to_string_pretty(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(&path, encoded)?;
    Ok(path)
}

fn result_to_summary(result: &Value) -> String {
    match result {
        Value::String(value) => value.clone(),
        Value::Object(map) => map
            .get("result")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| map.get("prompt").and_then(Value::as_str).map(ToOwned::to_owned))
            .unwrap_or_else(|| result.to_string()),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

fn finalize_open_phases_as_failed(
    phases: &mut [WorkflowPhaseRecord],
    phase_agent_baselines: &std::collections::HashMap<String, u32>,
    agent_events_seen: u32,
) {
    let completed_at_ms = now_ms();
    for phase in phases.iter_mut().filter(|phase| {
        phase.status == WorkflowRunStatus::Running && phase.completed_at_ms.is_none()
    }) {
        phase.status = WorkflowRunStatus::Failed;
        phase.completed_at_ms = Some(completed_at_ms);
        let baseline = phase_agent_baselines.get(&phase.name).copied().unwrap_or(0);
        phase.agent_count = agent_events_seen.saturating_sub(baseline);
    }
}

fn workflow_agent_result(call: &AgentCall, result: Value, cached: bool) -> Value {
    serde_json::json!({
        "callId": call.call_id,
        "callPath": call.call_path,
        "phase": call.phase,
        "prompt": call.prompt,
        "opts": call.opts,
        "cached": cached,
        "result": result,
    })
}

fn normalize_cached_agent_result(call: &AgentCall, cached_value: Value) -> Value {
    match cached_value {
        Value::Object(map)
            if map.contains_key("callId")
                && map.contains_key("callPath")
                && map.contains_key("prompt")
                && map.contains_key("cached") =>
        {
            Value::Object(map)
        }
        Value::Object(map) => Value::Object(map),
        other => workflow_agent_result(call, other, true),
    }
}

fn child_agent_output(prompt: &str, final_message: &str) -> String {
    if final_message.contains(prompt) {
        final_message.to_string()
    } else if final_message.trim().is_empty() {
        prompt.to_string()
    } else {
        format!("{prompt}\n\n{final_message}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::config::{
        HistoryMode, OutputFormat, ProviderKind, ToolConfig, WorkflowConfig,
    };
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;

    #[test]
    fn workflow_child_config_forces_autoedit_approval_mode() {
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Suggest;

        let child_config = WorkflowRunner::workflow_child_config(&config);
        assert_eq!(child_config.approval_mode, ApprovalMode::AutoEdit);
    }

    #[test]
    fn workflow_child_registry_uses_configured_mcp_servers() {
        let mut config = test_run_config();
        config.mcp_servers = vec![McpServerConfig {
            name: String::new(),
            ..Default::default()
        }];

        let (_, registry) = WorkflowRunner::workflow_child_runtime_parts(&config);
        let registry_error_count = registry.errors().len();
        assert!(
            registry_error_count > 0,
            "workflow child runtime should use initialized MCP registry from config"
        );
    }

    #[test]
    fn workflow_child_runtime_parts_force_autoedit_and_initialize_registry() {
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Suggest;
        config.mcp_servers = vec![McpServerConfig {
            name: String::new(),
            ..Default::default()
        }];

        let (child_config, registry) = WorkflowRunner::workflow_child_runtime_parts(&config);
        assert_eq!(child_config.approval_mode, ApprovalMode::AutoEdit);
        assert!(
            !registry.errors().is_empty(),
            "workflow child runtime should initialize MCP registry from configured servers"
        );
    }

    fn test_run_config() -> RunConfig {
        RunConfig {
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("auto".to_string())),
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            permission_rules: Default::default(),
            max_budget_usd: None,
            subagents: Default::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: Default::default(),
            vim_mode: false,
            update_check: false,
            desktop_notifications: false,
            auto_memory: false,
        }
    }
}

fn digest_value(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(value)
            .unwrap_or_else(|_| "null".to_string())
            .as_bytes(),
    );
    format!("{:x}", hasher.finalize())
}

fn task_type_name(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}
