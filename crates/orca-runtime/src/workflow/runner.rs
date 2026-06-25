use std::fs;
use std::io;
use std::io::sink;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use orca_core::approval_types::ApprovalMode;
use orca_core::cancel::CancelToken;
use orca_core::config::{OutputFormat, RunConfig};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::EventFactory;
use orca_core::event_schema::RunStatus;
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::{TaskType, WorkflowPhaseTaskSummary, WorkflowTaskProgress};
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
use crate::worktree::{WorktreeGuard, WorktreeOutcome};

use super::host::{AgentCall, HostCommand, HostEvent, WorkflowHost};
use super::script::{ResolvedWorkflowScript, resolve_workflow_script_to_path};
use super::state::{
    WorkflowAgentRecord, WorkflowStateStore, WorkflowWorkerRecord, input_hash, workflow_agent_team,
};

const STOP_REQUESTED_ERROR: &str = "__orca_workflow_stop_requested__";
const STOPPED_SUMMARY: &str = "Workflow stopped";

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

#[derive(Debug)]
pub struct WorkflowBackgroundLaunch {
    pub task_id: String,
    pub run_id: String,
    pub workflow_name: String,
    pub phases: Vec<String>,
    pub output: WorkflowOutput,
    handle: thread::JoinHandle<io::Result<WorkflowLaunchResult>>,
}

impl WorkflowBackgroundLaunch {
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub fn join(self) -> thread::Result<io::Result<WorkflowLaunchResult>> {
        self.handle.join()
    }
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

#[derive(Clone, Debug)]
struct PreparedWorkflowRun {
    request: WorkflowLaunchRequest,
    resolved: ResolvedWorkflowScript,
    task_id: String,
    run_id: String,
    transcript_dir: PathBuf,
}

#[derive(Clone, Debug)]
struct WorkflowChildAgentCallOutput {
    message: String,
    usage: UsageTotals,
    worktree: Option<WorktreeOutcome>,
}

#[derive(Clone, Debug)]
struct WorkflowChildAgentCallError {
    message: String,
    usage: Option<UsageTotals>,
    retryable: bool,
}

#[derive(Clone, Copy, Debug)]
struct WorkflowAgentExecutionPolicy {
    max_agent_retries: u32,
    max_agent_tokens: Option<u64>,
}

impl From<io::Error> for WorkflowChildAgentCallError {
    fn from(error: io::Error) -> Self {
        Self {
            message: error.to_string(),
            usage: None,
            retryable: true,
        }
    }
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
        let prepared = self.prepare_launch(request)?;
        self.execute_prepared(prepared)
    }

    pub fn resume(&self, request: WorkflowLaunchRequest) -> io::Result<WorkflowLaunchResult> {
        let prepared = self.prepare_launch(request)?;
        self.execute_prepared(prepared)
    }

    pub fn launch_background(
        &self,
        request: WorkflowLaunchRequest,
    ) -> io::Result<WorkflowBackgroundLaunch> {
        let prepared = self.prepare_launch(request)?;
        let task_id = prepared.task_id.clone();
        let run_id = prepared.run_id.clone();
        let workflow_name = prepared.resolved.meta.name.clone();
        let phases = prepared.resolved.meta.phases.clone();
        let output = WorkflowOutput {
            status: "async_launched".to_string(),
            task_id: task_id.clone(),
            task_type: Some("local_workflow".to_string()),
            workflow_name: Some(workflow_name.clone()),
            run_id: Some(run_id.clone()),
            summary: Some("Workflow launched".to_string()),
            transcript_dir: Some(prepared.transcript_dir.display().to_string()),
            script_path: Some(prepared.resolved.persisted_path.display().to_string()),
            session_url: None,
        };
        self.state.write_worker_record(
            &run_id,
            &WorkflowWorkerRecord {
                pid: std::process::id(),
                active: true,
                started_at_ms: now_ms(),
                completed_at_ms: None,
            },
        )?;
        let runner = self.clone();
        let handle = thread::spawn(move || runner.execute_prepared(prepared));

        Ok(WorkflowBackgroundLaunch {
            task_id,
            run_id,
            workflow_name,
            phases,
            output,
            handle,
        })
    }

    fn prepare_launch(&self, request: WorkflowLaunchRequest) -> io::Result<PreparedWorkflowRun> {
        let cwd = self.config.cwd.clone().unwrap_or(std::env::current_dir()?);
        fs::create_dir_all(&self.session_dir)?;

        let run_id = format!("workflow-run-{}", uuid::Uuid::new_v4());
        let persisted_script_path = self.state.run_dir(&run_id).join("script.js");
        let resolved =
            resolve_workflow_script_to_path(&request.input, &cwd, &persisted_script_path)?;
        let task = self.tasks.create_workflow(
            run_id.clone(),
            resolved.meta.name.clone(),
            resolved.meta.description.clone(),
            resolved.meta.phases.len(),
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
        self.state.write_launch_input(&run_id, &request.input)?;

        self.tasks
            .mark_running(&task.id)
            .map_err(io::Error::other)?;
        state.status = WorkflowRunStatus::Running;
        self.state.write_state(&state)?;

        let transcript_dir = self.state.transcript_dir(&run_id);

        Ok(PreparedWorkflowRun {
            request,
            resolved,
            task_id: task.id,
            run_id,
            transcript_dir,
        })
    }

    fn execute_prepared(&self, prepared: PreparedWorkflowRun) -> io::Result<WorkflowLaunchResult> {
        let PreparedWorkflowRun {
            request,
            resolved,
            task_id,
            run_id,
            transcript_dir,
        } = prepared;
        let mut state = self.state.load_run(&run_id)?;
        let args = request.input.args.clone().unwrap_or(Value::Null);
        let resume_from = request.input.resume_from_run_id.clone();
        let cached_agents = Arc::new(AtomicU32::new(0));
        let mut failed_error = None;
        let mut completed_result = None;
        let gate = Arc::new(WorkflowExecutionGate::new());
        let workflow_limits = self.config.workflows.clone();

        if self.state.stop_requested(&run_id)? {
            return self.finish_stopped_run(
                state,
                resolved.meta.name,
                resolved.persisted_path,
                transcript_dir,
                task_id,
                run_id,
                gate.snapshot()?.total_agents,
            );
        }

        let mut phase_agent_baselines = std::collections::HashMap::new();
        let mut agent_events_seen = 0u32;
        let events = match WorkflowHost::run_collecting_events_with_agent_and_event_callback(
            &resolved.persisted_path,
            args,
            |call| {
                self.answer_agent_call(
                    &run_id,
                    &task_id,
                    resume_from.as_deref(),
                    &transcript_dir,
                    call,
                    Arc::clone(&cached_agents),
                    &gate,
                    &workflow_limits,
                )
            },
            |event| {
                apply_host_event_to_state(
                    event,
                    &mut state,
                    &mut phase_agent_baselines,
                    &mut agent_events_seen,
                    &mut completed_result,
                    &mut failed_error,
                );
                self.state.write_state(&state)?;
                self.refresh_task_progress(&task_id, &state)
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
                    .fail(&task_id, message.clone())
                    .map_err(io::Error::other)?;
                let _ = self.state.mark_worker_exited(&run_id);
                return Err(error);
            }
        };

        let _ = events;

        state.total_agent_count = gate.snapshot()?.total_agents;
        if let Some(error) = failed_error {
            if is_stop_requested_error(&error) {
                return self.finish_stopped_run(
                    state,
                    resolved.meta.name,
                    resolved.persisted_path,
                    transcript_dir,
                    task_id,
                    run_id,
                    gate.snapshot()?.total_agents,
                );
            }
            state.status = WorkflowRunStatus::Failed;
            state.error = Some(error.clone());
            self.state.write_state(&state)?;
            self.tasks
                .fail(&task_id, error.clone())
                .map_err(io::Error::other)?;
            let _ = self.state.mark_worker_exited(&run_id);
            return Err(io::Error::other(error));
        }

        if self.state.stop_requested(&run_id)? {
            return self.finish_stopped_run(
                state,
                resolved.meta.name,
                resolved.persisted_path,
                transcript_dir,
                task_id,
                run_id,
                gate.snapshot()?.total_agents,
            );
        }

        let result = completed_result.unwrap_or_default();
        let cached_agents = cached_agents.load(Ordering::Relaxed);
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
        self.refresh_task_progress(&task_id, &state)?;
        self.tasks
            .complete(&task_id, result.clone())
            .map_err(io::Error::other)?;
        let _ = self.state.mark_worker_exited(&run_id);

        Ok(WorkflowLaunchResult {
            task_id: task_id.clone(),
            output: WorkflowOutput {
                status: "completed".to_string(),
                task_id,
                task_type: Some(task_type_name(TaskType::Workflow).to_string()),
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
        task_id: &str,
        resume_from: Option<&str>,
        transcript_dir: &std::path::Path,
        call: AgentCall,
        cached_agents: Arc<AtomicU32>,
        gate: &Arc<WorkflowExecutionGate>,
        workflow_limits: &orca_core::config::WorkflowConfig,
    ) -> io::Result<HostCommand> {
        if self.state.stop_requested(run_id)? {
            return Ok(HostCommand::AgentError {
                call_id: call.call_id,
                error: STOP_REQUESTED_ERROR.to_string(),
            });
        }
        let hash = input_hash(&call.prompt, &call.opts);
        if let Some(resume_run_id) = resume_from {
            if let Some(cached_value) =
                self.state
                    .find_cached_agent_value(resume_run_id, &call.call_path, &hash)
            {
                cached_agents.fetch_add(1, Ordering::Relaxed);
                let normalized_cached_value = normalize_cached_agent_result(&call, cached_value);
                if let Err(error_message) =
                    validate_workflow_agent_schema(&call, &normalized_cached_value)
                {
                    let transcript_path =
                        write_agent_transcript(transcript_dir, &call, &error_message, true)?;
                    self.state.record_agent_completed(
                        run_id,
                        WorkflowAgentRecord {
                            call_id: call.call_id.clone(),
                            call_path: call.call_path.clone(),
                            prompt: call.prompt.clone(),
                            opts: call.opts.clone(),
                            team: workflow_agent_team(&call.opts),
                            input_hash: hash,
                            status: WorkflowAgentStatus::Failed,
                            attempt: 1,
                            max_attempts: 1,
                            previous_errors: Vec::new(),
                            output: Some(normalized_cached_value),
                            error: Some(error_message.clone()),
                            transcript_path: Some(transcript_path.display().to_string()),
                            usage: None,
                        },
                    )?;
                    if let Ok(state) = self.state.load_run(run_id) {
                        let _ = self.refresh_task_progress(task_id, &state);
                    }
                    return Ok(HostCommand::AgentError {
                        call_id: call.call_id,
                        error: error_message,
                    });
                }
                let output = result_to_summary(&normalized_cached_value);
                let transcript_path = write_agent_transcript(transcript_dir, &call, &output, true)?;
                self.state.record_agent_completed(
                    run_id,
                    WorkflowAgentRecord {
                        call_id: call.call_id.clone(),
                        call_path: call.call_path.clone(),
                        prompt: call.prompt.clone(),
                        opts: call.opts.clone(),
                        team: workflow_agent_team(&call.opts),
                        input_hash: hash,
                        status: WorkflowAgentStatus::Completed,
                        attempt: 1,
                        max_attempts: 1,
                        previous_errors: Vec::new(),
                        output: Some(normalized_cached_value.clone()),
                        error: None,
                        transcript_path: Some(transcript_path.display().to_string()),
                        usage: None,
                    },
                )?;
                if let Ok(state) = self.state.load_run(run_id) {
                    let _ = self.refresh_task_progress(task_id, &state);
                }
                return Ok(HostCommand::AgentResult {
                    call_id: call.call_id,
                    result: normalized_cached_value,
                });
            }
        }

        let execution_policy = workflow_agent_execution_policy(&call, workflow_limits);
        let max_attempts = execution_policy.max_agent_retries.saturating_add(1).max(1);
        let mut previous_errors = Vec::new();
        for attempt in 1..=max_attempts {
            if self.state.stop_requested(run_id)? {
                return Ok(HostCommand::AgentError {
                    call_id: call.call_id,
                    error: STOP_REQUESTED_ERROR.to_string(),
                });
            }
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

            match self.run_child_agent_call(&call, execution_policy.max_agent_tokens) {
                Ok(child_output) => {
                    let mut output = child_agent_output(&call.prompt, &child_output.message);
                    append_worktree_outcome(&mut output, child_output.worktree.as_ref());
                    let transcript_path =
                        write_agent_transcript(transcript_dir, &call, &output, false)?;
                    let result = workflow_agent_result(&call, Value::String(output.clone()), false);
                    if let Err(error_message) = validate_workflow_agent_schema(&call, &result) {
                        self.state.record_agent_completed(
                            run_id,
                            WorkflowAgentRecord {
                                call_id: call.call_id.clone(),
                                call_path: call.call_path.clone(),
                                prompt: call.prompt.clone(),
                                opts: call.opts.clone(),
                                team: workflow_agent_team(&call.opts),
                                input_hash: hash.clone(),
                                status: WorkflowAgentStatus::Failed,
                                attempt,
                                max_attempts,
                                previous_errors: previous_errors.clone(),
                                output: Some(result),
                                error: Some(error_message.clone()),
                                transcript_path: Some(transcript_path.display().to_string()),
                                usage: Some(child_output.usage),
                            },
                        )?;
                        if let Ok(state) = self.state.load_run(run_id) {
                            let _ = self.refresh_task_progress(task_id, &state);
                        }

                        return Ok(HostCommand::AgentError {
                            call_id: call.call_id,
                            error: error_message,
                        });
                    }
                    self.state.record_agent_completed(
                        run_id,
                        WorkflowAgentRecord {
                            call_id: call.call_id.clone(),
                            call_path: call.call_path.clone(),
                            prompt: call.prompt.clone(),
                            opts: call.opts.clone(),
                            team: workflow_agent_team(&call.opts),
                            input_hash: hash.clone(),
                            status: WorkflowAgentStatus::Completed,
                            attempt,
                            max_attempts,
                            previous_errors: previous_errors.clone(),
                            output: Some(result.clone()),
                            error: None,
                            transcript_path: Some(transcript_path.display().to_string()),
                            usage: Some(child_output.usage),
                        },
                    )?;
                    if let Ok(state) = self.state.load_run(run_id) {
                        let _ = self.refresh_task_progress(task_id, &state);
                    }

                    return Ok(HostCommand::AgentResult {
                        call_id: call.call_id,
                        result,
                    });
                }
                Err(error) => {
                    drop(_permit);
                    let WorkflowChildAgentCallError {
                        message: error_message,
                        usage,
                        retryable,
                    } = error;
                    let transcript_path =
                        write_agent_transcript(transcript_dir, &call, &error_message, false)?;
                    self.state.record_agent_completed(
                        run_id,
                        WorkflowAgentRecord {
                            call_id: call.call_id.clone(),
                            call_path: call.call_path.clone(),
                            prompt: call.prompt.clone(),
                            opts: call.opts.clone(),
                            team: workflow_agent_team(&call.opts),
                            input_hash: hash.clone(),
                            status: WorkflowAgentStatus::Failed,
                            attempt,
                            max_attempts,
                            previous_errors: previous_errors.clone(),
                            output: None,
                            error: Some(error_message.clone()),
                            transcript_path: Some(transcript_path.display().to_string()),
                            usage,
                        },
                    )?;
                    if let Ok(state) = self.state.load_run(run_id) {
                        let _ = self.refresh_task_progress(task_id, &state);
                    }

                    if attempt < max_attempts && retryable {
                        previous_errors.push(error_message);
                        continue;
                    }

                    return Ok(HostCommand::AgentError {
                        call_id: call.call_id,
                        error: error_message,
                    });
                }
            }
        }

        Ok(HostCommand::AgentError {
            call_id: call.call_id,
            error: "workflow child agent exhausted retry attempts".to_string(),
        })
    }

    fn run_child_agent_call(
        &self,
        call: &AgentCall,
        max_agent_tokens: Option<u64>,
    ) -> Result<WorkflowChildAgentCallOutput, WorkflowChildAgentCallError> {
        let cwd = self
            .config
            .cwd
            .as_deref()
            .unwrap_or(self.session_dir.as_path());
        let worktree_guard = if workflow_agent_uses_worktree(&call.opts) {
            Some(WorktreeGuard::create(cwd)?)
        } else {
            None
        };
        let child_cwd = worktree_guard
            .as_ref()
            .map(|guard| guard.path())
            .unwrap_or(cwd);
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
            child_cwd,
            &mut events,
            &mut sink,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            execute_child_agent_loop,
        );
        let (result, child_cost_tracker) =
            run_child_agent(&workflow_child_config, &child_request, &mut runtime);
        drop(runtime);
        let usage = child_cost_tracker.totals();
        let worktree = worktree_guard.map(WorktreeGuard::finish).transpose()?;

        match result.status {
            RunStatus::Success => {
                if let Some(max_agent_tokens) = max_agent_tokens {
                    let total_tokens = usage.total_tokens();
                    if total_tokens > max_agent_tokens {
                        let mut error = format!(
                            "{total_tokens} tokens exceeded per-agent token budget {max_agent_tokens}"
                        );
                        append_worktree_outcome(&mut error, worktree.as_ref());
                        return Err(WorkflowChildAgentCallError {
                            message: error,
                            usage: Some(usage),
                            retryable: false,
                        });
                    }
                }
                Ok(WorkflowChildAgentCallOutput {
                    message: result.final_message.unwrap_or_default(),
                    usage,
                    worktree,
                })
            }
            _ => {
                let mut error = result
                    .error
                    .or(result.final_message)
                    .unwrap_or_else(|| "workflow child agent failed".to_string());
                append_worktree_outcome(&mut error, worktree.as_ref());
                Err(WorkflowChildAgentCallError {
                    message: error,
                    usage: Some(usage),
                    retryable: true,
                })
            }
        }
    }

    fn workflow_child_config(config: &RunConfig) -> RunConfig {
        let mut workflow_child_config = config.clone();
        if workflow_child_config.approval_mode != ApprovalMode::FullAuto {
            workflow_child_config.approval_mode = ApprovalMode::AutoEdit;
        }
        workflow_child_config.output_format = OutputFormat::Jsonl;
        workflow_child_config
    }

    fn workflow_child_runtime_parts(config: &RunConfig) -> (RunConfig, orca_mcp::McpRegistry) {
        let workflow_child_config = Self::workflow_child_config(config);
        let mcp_registry = orca_mcp::initialize_registry(&workflow_child_config.mcp_servers);
        (workflow_child_config, mcp_registry)
    }

    fn finish_stopped_run(
        &self,
        mut state: WorkflowRunState,
        workflow_name: String,
        script_path: PathBuf,
        transcript_dir: PathBuf,
        task_id: String,
        run_id: String,
        total_agent_count: u32,
    ) -> io::Result<WorkflowLaunchResult> {
        state.status = WorkflowRunStatus::Stopped;
        state.total_agent_count = total_agent_count;
        state.final_summary = Some(STOPPED_SUMMARY.to_string());
        state.error = None;
        self.state.write_state(&state)?;
        self.tasks
            .stop(&task_id, STOPPED_SUMMARY.to_string())
            .map_err(io::Error::other)?;
        let _ = self.state.mark_worker_exited(&run_id);

        Ok(WorkflowLaunchResult {
            task_id: task_id.clone(),
            output: WorkflowOutput {
                status: "stopped".to_string(),
                task_id,
                task_type: Some(task_type_name(TaskType::Workflow).to_string()),
                workflow_name: Some(workflow_name),
                run_id: Some(run_id),
                summary: Some(STOPPED_SUMMARY.to_string()),
                transcript_dir: Some(transcript_dir.display().to_string()),
                script_path: Some(script_path.display().to_string()),
                session_url: None,
            },
            summary: STOPPED_SUMMARY.to_string(),
        })
    }

    fn refresh_task_progress(&self, task_id: &str, state: &WorkflowRunState) -> io::Result<()> {
        let counts = self.state.agent_status_counts(&state.run_id)?;
        let terminal_agents = counts
            .completed
            .saturating_add(counts.failed)
            .saturating_add(counts.cancelled)
            .saturating_add(counts.cached);
        let running_agents = state.total_agent_count.saturating_sub(terminal_agents);
        let progress = WorkflowTaskProgress {
            total_agents: state.total_agent_count,
            running_agents,
            completed_agents: counts.completed.saturating_add(counts.cached),
            failed_agents: counts.failed.saturating_add(counts.cancelled),
            completed_phases: state
                .phases
                .iter()
                .filter(|phase| phase.status == WorkflowRunStatus::Completed)
                .count(),
            running_phases: state
                .phases
                .iter()
                .filter(|phase| phase.status == WorkflowRunStatus::Running)
                .count(),
            failed_phases: state
                .phases
                .iter()
                .filter(|phase| {
                    matches!(
                        phase.status,
                        WorkflowRunStatus::Failed
                            | WorkflowRunStatus::Cancelled
                            | WorkflowRunStatus::Stopped
                    )
                })
                .count(),
        };
        self.tasks
            .update_workflow_progress(task_id, progress)
            .map_err(io::Error::other)?;
        self.tasks
            .update_workflow_phases(task_id, workflow_phase_summaries(state))
            .map_err(io::Error::other)?;
        self.tasks
            .update_workflow_agents(task_id, self.state.agent_summaries(&state.run_id)?)
            .map_err(io::Error::other)
    }
}

fn workflow_phase_summaries(state: &WorkflowRunState) -> Vec<WorkflowPhaseTaskSummary> {
    state
        .phases
        .iter()
        .map(|phase| WorkflowPhaseTaskSummary {
            name: phase.name.clone(),
            status: phase.status,
            agent_count: phase.agent_count,
            error: phase.error.clone(),
            fallback: phase.fallback.clone(),
        })
        .collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn workflow_agent_uses_worktree(opts: &Value) -> bool {
    opts.get("isolation").and_then(Value::as_str) == Some("worktree")
}

fn workflow_agent_execution_policy(
    call: &AgentCall,
    workflow_limits: &orca_core::config::WorkflowConfig,
) -> WorkflowAgentExecutionPolicy {
    let mut policy = WorkflowAgentExecutionPolicy {
        max_agent_retries: workflow_limits.max_agent_retries,
        max_agent_tokens: workflow_limits.max_agent_tokens,
    };

    if let Some(team) = workflow_agent_team(&call.opts)
        && let Some(team_policy) = workflow_limits.teams.get(&team)
    {
        if let Some(max_agent_retries) = team_policy.max_agent_retries {
            policy.max_agent_retries = max_agent_retries;
        }
        if let Some(max_agent_tokens) = team_policy.max_agent_tokens {
            policy.max_agent_tokens = Some(max_agent_tokens);
        }
    }

    policy
}

fn validate_workflow_agent_schema(call: &AgentCall, result: &Value) -> Result<(), String> {
    let Some(schema) = call.opts.get("schema") else {
        return Ok(());
    };
    validate_json_schema_subset(schema, result, "$").map_err(|error| {
        format!(
            "workflow agent output schema validation failed for {}: {error}",
            call.call_id
        )
    })
}

fn validate_json_schema_subset(schema: &Value, value: &Value, path: &str) -> Result<(), String> {
    let schema_object = schema
        .as_object()
        .ok_or_else(|| format!("{path} schema must be an object"))?;

    if let Some(expected_type) = schema_object.get("type") {
        validate_schema_type(expected_type, value, path)?;
    }

    if let Some(required) = schema_object.get("required") {
        let required = required
            .as_array()
            .ok_or_else(|| format!("{path}.required must be an array"))?;
        let object = value
            .as_object()
            .ok_or_else(|| format!("{path} expected object for required fields"))?;
        for required_field in required {
            let field = required_field
                .as_str()
                .ok_or_else(|| format!("{path}.required entries must be strings"))?;
            if !object.contains_key(field) {
                return Err(format!("{path}.{field} is required"));
            }
        }
    }

    if let Some(properties) = schema_object.get("properties") {
        let properties = properties
            .as_object()
            .ok_or_else(|| format!("{path}.properties must be an object"))?;
        let Some(object) = value.as_object() else {
            return Ok(());
        };
        for (property, property_schema) in properties {
            if let Some(property_value) = object.get(property) {
                validate_json_schema_subset(
                    property_schema,
                    property_value,
                    &format!("{path}.{property}"),
                )?;
            }
        }
    }

    Ok(())
}

fn validate_schema_type(expected_type: &Value, value: &Value, path: &str) -> Result<(), String> {
    if let Some(expected) = expected_type.as_str() {
        return validate_schema_type_name(expected, value, path);
    }

    let expected_types = expected_type
        .as_array()
        .ok_or_else(|| format!("{path}.type must be a string or array"))?;
    let mut expected_names = Vec::new();
    for expected_type in expected_types {
        let expected = expected_type
            .as_str()
            .ok_or_else(|| format!("{path}.type entries must be strings"))?;
        expected_names.push(expected);
        if schema_type_matches(expected, value) {
            return Ok(());
        }
    }
    Err(format!(
        "{path} expected one of {}, got {}",
        expected_names.join(", "),
        json_type_name(value)
    ))
}

fn validate_schema_type_name(expected: &str, value: &Value, path: &str) -> Result<(), String> {
    if schema_type_matches(expected, value) {
        Ok(())
    } else {
        Err(format!(
            "{path} expected {expected}, got {}",
            json_type_name(value)
        ))
    }
}

fn schema_type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn append_worktree_outcome(output: &mut String, outcome: Option<&WorktreeOutcome>) {
    if let Some(outcome) = outcome {
        let status = if outcome.preserved {
            "preserved"
        } else {
            "cleaned"
        };
        output.push_str(&format!(
            "\n\nWorktree {status}: {}",
            outcome.path.display()
        ));
    }
}

fn apply_host_event_to_state(
    event: &HostEvent,
    state: &mut WorkflowRunState,
    phase_agent_baselines: &mut std::collections::HashMap<String, u32>,
    agent_events_seen: &mut u32,
    completed_result: &mut Option<String>,
    failed_error: &mut Option<String>,
) {
    match event {
        HostEvent::PhaseStarted { name } => {
            phase_agent_baselines.insert(name.clone(), *agent_events_seen);
            state.phases.push(WorkflowPhaseRecord {
                name: name.clone(),
                status: WorkflowRunStatus::Running,
                started_at_ms: Some(now_ms()),
                completed_at_ms: None,
                agent_count: 0,
                error: None,
                fallback: None,
            });
        }
        HostEvent::PhaseCompleted { name } => {
            if let Some(phase) = state
                .phases
                .iter_mut()
                .rev()
                .find(|phase| phase.name == *name)
            {
                phase.status = WorkflowRunStatus::Completed;
                phase.completed_at_ms = Some(now_ms());
                let baseline = phase_agent_baselines.get(name).copied().unwrap_or(0);
                phase.agent_count = agent_events_seen.saturating_sub(baseline);
            }
        }
        HostEvent::PhaseFailed {
            name,
            error,
            fallback,
        } => {
            if let Some(phase) = state
                .phases
                .iter_mut()
                .rev()
                .find(|phase| phase.name == *name)
            {
                phase.status = WorkflowRunStatus::Failed;
                phase.completed_at_ms = Some(now_ms());
                let baseline = phase_agent_baselines.get(name).copied().unwrap_or(0);
                phase.agent_count = agent_events_seen.saturating_sub(baseline);
                phase.error = Some(error.clone());
                phase.fallback = fallback.clone();
            }
        }
        HostEvent::AgentCall { phase, .. } => {
            *agent_events_seen += 1;
            state.total_agent_count = *agent_events_seen;
            if let Some(phase_name) = phase
                && let Some(phase) = state
                    .phases
                    .iter_mut()
                    .rev()
                    .find(|phase| phase.name == *phase_name)
            {
                let baseline = phase_agent_baselines.get(phase_name).copied().unwrap_or(0);
                phase.agent_count = agent_events_seen.saturating_sub(baseline);
            }
        }
        HostEvent::WorkflowCompleted { result } => {
            *completed_result = Some(result_to_summary(result));
        }
        HostEvent::WorkflowFailed { error } => {
            if is_stop_requested_error(error) {
                finalize_open_phases_as_stopped(
                    &mut state.phases,
                    phase_agent_baselines,
                    *agent_events_seen,
                );
            } else {
                finalize_open_phases_as_failed(
                    &mut state.phases,
                    phase_agent_baselines,
                    *agent_events_seen,
                );
            }
            *failed_error = Some(error.clone());
        }
    }
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
        Value::Object(map) => summary_from_dsl_phases(map.get("phases"))
            .or_else(|| {
                map.get("result").and_then(|value| match value {
                    Value::String(text) => Some(text.clone()),
                    other => Some(result_to_summary(other)),
                })
            })
            .or_else(|| {
                map.get("prompt")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| result.to_string()),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

fn summary_from_dsl_phases(phases: Option<&Value>) -> Option<String> {
    let phases = phases?.as_array()?;
    for phase in phases.iter().rev() {
        let Some(results) = phase.get("results").and_then(Value::as_array) else {
            continue;
        };
        for result in results.iter().rev() {
            let summary = result_to_summary(result);
            if !summary.trim().is_empty() {
                return Some(summary);
            }
        }
    }
    None
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

fn finalize_open_phases_as_stopped(
    phases: &mut [WorkflowPhaseRecord],
    phase_agent_baselines: &std::collections::HashMap<String, u32>,
    agent_events_seen: u32,
) {
    let completed_at_ms = now_ms();
    for phase in phases.iter_mut().filter(|phase| {
        phase.status == WorkflowRunStatus::Running && phase.completed_at_ms.is_none()
    }) {
        phase.status = WorkflowRunStatus::Stopped;
        phase.completed_at_ms = Some(completed_at_ms);
        let baseline = phase_agent_baselines.get(&phase.name).copied().unwrap_or(0);
        phase.agent_count = agent_events_seen.saturating_sub(baseline);
    }
}

fn is_stop_requested_error(error: &str) -> bool {
    error.contains(STOP_REQUESTED_ERROR)
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
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind, ToolConfig, WorkflowConfig};
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;

    #[test]
    fn workflow_child_config_defaults_to_autoedit_approval_mode() {
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Suggest;

        let child_config = WorkflowRunner::workflow_child_config(&config);
        assert_eq!(child_config.approval_mode, ApprovalMode::AutoEdit);
    }

    #[test]
    fn workflow_child_config_preserves_fullauto_approval_mode() {
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::FullAuto;

        let child_config = WorkflowRunner::workflow_child_config(&config);
        assert_eq!(child_config.approval_mode, ApprovalMode::FullAuto);
    }

    #[test]
    fn workflow_child_config_disables_interactive_text_prompts() {
        let mut config = test_run_config();
        config.output_format = OutputFormat::Text;

        let child_config = WorkflowRunner::workflow_child_config(&config);
        assert_eq!(child_config.output_format, OutputFormat::Jsonl);
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
    fn workflow_child_runtime_parts_make_child_noninteractive_and_initialize_registry() {
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Suggest;
        config.output_format = OutputFormat::Text;
        config.mcp_servers = vec![McpServerConfig {
            name: String::new(),
            ..Default::default()
        }];

        let (child_config, registry) = WorkflowRunner::workflow_child_runtime_parts(&config);
        assert_eq!(child_config.approval_mode, ApprovalMode::AutoEdit);
        assert_eq!(child_config.output_format, OutputFormat::Jsonl);
        assert!(
            !registry.errors().is_empty(),
            "workflow child runtime should initialize MCP registry from configured servers"
        );
    }

    fn test_run_config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("auto".to_string())),
            model_runtime: Default::default(),
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
