use std::fs;
use std::io;
use std::path::PathBuf;

use orca_core::config::RunConfig;
use orca_core::task_types::TaskType;
use orca_core::workflow_types::{
    WorkflowAgentStatus, WorkflowInput, WorkflowOutput, WorkflowRunState, WorkflowRunStatus,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

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
        let mut total_agents = 0u32;
        let mut failed_error = None;
        let mut completed_result = None;

        let events = WorkflowHost::run_collecting_events_with_agent(
            &resolved.persisted_path,
            args,
            |call| {
                total_agents += 1;
                self.answer_agent_call(
                    &run_id,
                    resume_from.as_deref(),
                    &transcript_dir,
                    call,
                    &mut cached_agents,
                )
            },
        )?;

        for event in events {
            match event {
                HostEvent::WorkflowCompleted { result } => {
                    completed_result = Some(result_to_summary(&result));
                }
                HostEvent::WorkflowFailed { error } => {
                    failed_error = Some(error);
                }
                _ => {}
            }
        }

        state.total_agent_count = total_agents;
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
    ) -> io::Result<HostCommand> {
        let hash = input_hash(&call.prompt, &call.opts);
        if let Some(resume_run_id) = resume_from {
            if let Some(output) =
                self.state
                    .find_cached_agent(resume_run_id, &call.call_path, &hash)
            {
                *cached_agents += 1;
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
                        output: Some(output.clone()),
                        error: None,
                        transcript_path: Some(transcript_path.display().to_string()),
                    },
                )?;
                return Ok(HostCommand::AgentResult {
                    call_id: call.call_id,
                    result: Value::String(output),
                });
            }
        }

        let output = run_mock_child_agent(&call);
        let transcript_path = write_agent_transcript(transcript_dir, &call, &output, false)?;
        self.state.record_agent_completed(
            run_id,
            WorkflowAgentRecord {
                call_id: call.call_id.clone(),
                call_path: call.call_path.clone(),
                prompt: call.prompt.clone(),
                opts: call.opts.clone(),
                input_hash: hash,
                status: WorkflowAgentStatus::Completed,
                output: Some(output.clone()),
                error: None,
                transcript_path: Some(transcript_path.display().to_string()),
            },
        )?;

        Ok(HostCommand::AgentResult {
            call_id: call.call_id,
            result: Value::String(output),
        })
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

fn run_mock_child_agent(call: &AgentCall) -> String {
    format!("Mock child agent completed prompt: {}", call.prompt)
}

fn result_to_summary(result: &Value) -> String {
    match result {
        Value::String(value) => value.clone(),
        Value::Null => String::new(),
        value => value.to_string(),
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
