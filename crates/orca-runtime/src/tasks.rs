use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use orca_core::cancel::CancelToken;
use orca_core::conversation::RawToolCall;
use orca_core::cost_types::UsageTotals;
use orca_core::provider_types::{ProviderResponse, ProviderStep, ToolCallProgress, Usage};
use orca_core::task_types::{
    BackgroundTaskSummary, PendingToolCallSummary, TaskStatus, TaskType, WorkflowAgentTaskSummary,
    WorkflowPhaseTaskSummary, WorkflowTaskProgress,
};
use orca_core::tool_types::ToolRequest;
use orca_core::workflow_types::WorkflowInput;
use serde::{Deserialize, Serialize};

use crate::lifecycle::{
    RuntimeSubagentStatusLookup, RuntimeSubagentStatusRecord, RuntimeUsageTotals,
};

#[derive(Clone, Debug)]
pub struct TaskRegistry {
    session_id: String,
    inner: Arc<Mutex<HashMap<String, TaskRecord>>>,
    persistence: Option<Arc<TaskPersistence>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskHandle {
    pub id: String,
    pub task_type: TaskType,
    pub workflow_run_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TaskRecord {
    pub id: String,
    pub task_type: TaskType,
    pub status: TaskStatus,
    pub is_backgrounded: bool,
    pub description: String,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub name: Option<String>,
    pub agent_type: Option<String>,
    pub tool: Option<String>,
    pub pending_tool_call: Option<PendingToolCallSummary>,
    pub pending_tool_approval_response: Option<bool>,
    pub pending_provider_response: Option<ProviderResponse>,
    pub workflow_run_id: Option<String>,
    pub phase_count: Option<usize>,
    pub workflow_progress: Option<WorkflowTaskProgress>,
    pub workflow_phases: Vec<WorkflowPhaseTaskSummary>,
    pub workflow_agents: Vec<WorkflowAgentTaskSummary>,
    pub workflow_script_path: Option<String>,
    pub workflow_launch_input: Option<WorkflowInput>,
    pub workflow_final_summary: Option<String>,
    pub workflow_failure_count: u32,
    pub usage: Option<UsageTotals>,
    pub subagent_current_activity: Option<String>,
    pub subagent_turn: Option<u32>,
    pub last_activity_at_ms: Option<i64>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub worker_pid: Option<u32>,
    pub command: Option<String>,
    pub control: TaskControl,
}

#[derive(Clone, Debug)]
pub struct TaskControl {
    pub cancel: CancelToken,
    pub pause: Arc<AtomicBool>,
}

#[derive(Clone, Debug)]
struct TaskPersistence {
    root: PathBuf,
    session_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedTaskRecord {
    id: String,
    task_type: TaskType,
    status: TaskStatus,
    #[serde(default)]
    is_backgrounded: bool,
    description: String,
    created_at_ms: i64,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
    name: Option<String>,
    agent_type: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    pending_tool_call: Option<PendingToolCallSummary>,
    #[serde(default)]
    pending_provider_response: Option<serde_json::Value>,
    workflow_run_id: Option<String>,
    phase_count: Option<usize>,
    workflow_progress: Option<WorkflowTaskProgress>,
    #[serde(default)]
    workflow_phases: Vec<WorkflowPhaseTaskSummary>,
    #[serde(default)]
    workflow_agents: Vec<WorkflowAgentTaskSummary>,
    #[serde(default)]
    workflow_script_path: Option<String>,
    #[serde(default)]
    workflow_launch_input: Option<WorkflowInput>,
    #[serde(default)]
    workflow_final_summary: Option<String>,
    #[serde(default)]
    workflow_failure_count: u32,
    usage: Option<UsageTotals>,
    #[serde(default)]
    subagent_current_activity: Option<String>,
    #[serde(default)]
    subagent_turn: Option<u32>,
    #[serde(default)]
    last_activity_at_ms: Option<i64>,
    result: Option<String>,
    error: Option<String>,
    #[serde(default)]
    worker_pid: Option<u32>,
    #[serde(default)]
    command: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedProviderResponse {
    #[serde(default)]
    steps: Vec<PersistedProviderStep>,
    assistant_content: Option<String>,
    assistant_reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
    usage: Option<Usage>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum PersistedProviderStep {
    ReasoningDelta(String),
    MessageDelta(String),
    ToolCallProgress(ToolCallProgress),
    ToolCall(ToolRequest),
    Error(String),
}

impl TaskRegistry {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            inner: Arc::new(Mutex::new(HashMap::new())),
            persistence: None,
        }
    }

    pub fn new_persistent(session_id: String, root: PathBuf) -> io::Result<Self> {
        let persistence = Arc::new(TaskPersistence::new(root, session_id.clone()));
        let mut records = persistence.load_session_records(&session_id)?;
        let mut changed = false;
        for record in records.values_mut() {
            changed |= mark_interrupted_if_active(record);
        }
        if changed {
            persistence.write_session_records(&session_id, &records)?;
        }
        Ok(Self {
            session_id,
            inner: Arc::new(Mutex::new(records)),
            persistence: Some(persistence),
        })
    }

    pub fn new_for_cwd(session_id: String, _cwd: &Path) -> Self {
        let Some(root) = task_sessions_root() else {
            return Self::new(session_id);
        };
        let legacy_root = legacy_project_task_sessions_root(_cwd);
        let _ = migrate_legacy_task_sessions(&legacy_root, &root);
        Self::new_persistent(session_id.clone(), root).unwrap_or_else(|_| Self::new(session_id))
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn create_workflow(
        &self,
        workflow_run_id: String,
        name: String,
        description: String,
        phase_count: usize,
    ) -> TaskHandle {
        let id = new_task_id();
        let created_at_ms = now_ms();
        let control = TaskControl {
            cancel: CancelToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
        };
        let record = TaskRecord {
            id: id.clone(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Queued,
            is_backgrounded: false,
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: Some(name),
            agent_type: None,
            tool: None,
            pending_tool_call: None,
            pending_tool_approval_response: None,
            pending_provider_response: None,
            workflow_run_id: Some(workflow_run_id.clone()),
            phase_count: Some(phase_count),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
            worker_pid: None,
            command: None,
            control,
        };

        self.insert_task(id.clone(), record)
            .expect("task registry insert failed");

        TaskHandle {
            id,
            task_type: TaskType::Workflow,
            workflow_run_id: Some(workflow_run_id),
        }
    }

    pub fn create_subagent(&self, description: String, agent_type: Option<String>) -> TaskHandle {
        let id = new_task_id();
        let created_at_ms = now_ms();
        let control = TaskControl {
            cancel: CancelToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
        };
        let record = TaskRecord {
            id: id.clone(),
            task_type: TaskType::Subagent,
            status: TaskStatus::Queued,
            is_backgrounded: false,
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: None,
            agent_type,
            tool: None,
            pending_tool_call: None,
            pending_tool_approval_response: None,
            pending_provider_response: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
            worker_pid: None,
            command: None,
            control,
        };

        self.insert_task(id.clone(), record)
            .expect("task registry insert failed");

        TaskHandle {
            id,
            task_type: TaskType::Subagent,
            workflow_run_id: None,
        }
    }

    pub fn create_main_session(&self, description: String) -> TaskHandle {
        let id = new_task_id();
        let created_at_ms = now_ms();
        let control = TaskControl {
            cancel: CancelToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
        };
        let record = TaskRecord {
            id: id.clone(),
            task_type: TaskType::MainSession,
            status: TaskStatus::Queued,
            is_backgrounded: false,
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: None,
            agent_type: Some("main-session".to_string()),
            tool: None,
            pending_tool_call: None,
            pending_tool_approval_response: None,
            pending_provider_response: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
            worker_pid: None,
            command: None,
            control,
        };

        self.insert_task(id.clone(), record)
            .expect("task registry insert failed");

        TaskHandle {
            id,
            task_type: TaskType::MainSession,
            workflow_run_id: None,
        }
    }

    pub fn create_shell(&self, description: String, command: String) -> TaskHandle {
        let id = new_task_id();
        let created_at_ms = now_ms();
        let control = TaskControl {
            cancel: CancelToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
        };
        let record = TaskRecord {
            id: id.clone(),
            task_type: TaskType::Shell,
            status: TaskStatus::Queued,
            is_backgrounded: false,
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: None,
            agent_type: None,
            tool: None,
            pending_tool_call: None,
            pending_tool_approval_response: None,
            pending_provider_response: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            result: None,
            error: None,
            worker_pid: None,
            command: Some(command),
            control,
        };

        self.insert_task(id.clone(), record)
            .expect("task registry insert failed");

        TaskHandle {
            id,
            task_type: TaskType::Shell,
            workflow_run_id: None,
        }
    }

    pub fn list(&self) -> Vec<BackgroundTaskSummary> {
        let mut summaries = self
            .with_tasks(|tasks| {
                tasks
                    .values()
                    .map(|record| BackgroundTaskSummary {
                        id: record.id.clone(),
                        task_type: record.task_type,
                        status: record.status,
                        is_backgrounded: record.is_backgrounded,
                        description: record.description.clone(),
                        created_at_ms: record.created_at_ms,
                        started_at_ms: record.started_at_ms,
                        completed_at_ms: record.completed_at_ms,
                        command: record.command.clone(),
                        agent_type: record.agent_type.clone(),
                        server: None,
                        tool: record.tool.clone(),
                        pending_tool_call: record.pending_tool_call.clone(),
                        name: record.name.clone(),
                        workflow_run_id: record.workflow_run_id.clone(),
                        phase_count: record.phase_count,
                        workflow_progress: record.workflow_progress,
                        workflow_phases: record.workflow_phases.clone(),
                        workflow_agents: record.workflow_agents.clone(),
                        workflow_script_path: record.workflow_script_path.clone(),
                        workflow_launch_input: record.workflow_launch_input.clone(),
                        workflow_final_summary: record.workflow_final_summary.clone(),
                        workflow_failure_count: record.workflow_failure_count,
                        usage: record.usage,
                        subagent_current_activity: record.subagent_current_activity.clone(),
                        subagent_turn: record.subagent_turn,
                        last_activity_at_ms: record.last_activity_at_ms,
                        result: record.result.clone(),
                        error: record.error.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .expect("task registry lock poisoned");
        summaries.sort_by(|left, right| left.id.cmp(&right.id));
        summaries
    }

    pub fn get(&self, id: &str) -> Option<TaskRecord> {
        if let Some(record) = self
            .with_tasks(|tasks| tasks.get(id).cloned())
            .expect("task registry lock poisoned")
        {
            return Some(record);
        }

        let persistence = self.persistence.as_ref()?;
        let record = persistence.load_record_by_id(id).ok()??;
        self.with_tasks(|tasks| {
            tasks.insert(id.to_string(), record.clone());
        })
        .expect("task registry lock poisoned");
        Some(record)
    }

    pub fn update_workflow_progress(
        &self,
        id: &str,
        progress: WorkflowTaskProgress,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_progress = Some(progress);
            Ok(())
        })
    }

    pub fn update_workflow_agents(
        &self,
        id: &str,
        agents: Vec<WorkflowAgentTaskSummary>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_agents = agents;
            Ok(())
        })
    }

    pub fn update_workflow_phases(
        &self,
        id: &str,
        phases: Vec<WorkflowPhaseTaskSummary>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_phases = phases;
            Ok(())
        })
    }

    pub fn update_workflow_artifacts(
        &self,
        id: &str,
        script_path: String,
        launch_input: WorkflowInput,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_script_path = Some(script_path);
            record.workflow_launch_input = Some(launch_input);
            Ok(())
        })
    }

    pub fn update_workflow_result_summary(
        &self,
        id: &str,
        final_summary: Option<String>,
        failure_count: u32,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_final_summary = final_summary;
            record.workflow_failure_count = failure_count;
            Ok(())
        })
    }

    pub fn update_subagent_activity(
        &self,
        id: &str,
        activity: String,
        turn: Option<u32>,
        usage: Option<UsageTotals>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            if record.task_type != TaskType::Subagent {
                return Err(format!("task '{id}' is not a subagent"));
            }
            record.subagent_current_activity = Some(activity);
            if let Some(turn) = turn {
                record.subagent_turn = Some(turn);
            }
            if let Some(usage) = usage {
                record.usage = Some(usage);
            }
            record.last_activity_at_ms = Some(now_ms());
            Ok(())
        })
    }

    pub fn mark_running(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) || record.control.cancel.is_cancelled() {
                return Err(task_state_error("mark_running", record.status));
            }

            record.status = TaskStatus::Running;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn mark_backgrounded(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if record.task_type != TaskType::MainSession {
                return Err("mark_backgrounded requires a main session task".to_string());
            }
            if record.status != TaskStatus::Running {
                return Err(task_state_error("mark_backgrounded", record.status));
            }

            record.is_backgrounded = true;
            record.last_activity_at_ms = Some(now_ms());
            Ok(())
        })
    }

    pub fn mark_foregrounded(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if record.task_type != TaskType::MainSession {
                return Err("mark_foregrounded requires a main session task".to_string());
            }
            if record.status != TaskStatus::Running {
                return Err(task_state_error("mark_foregrounded", record.status));
            }
            if !record.is_backgrounded {
                return Err("mark_foregrounded requires a backgrounded task".to_string());
            }

            record.is_backgrounded = false;
            record.last_activity_at_ms = Some(now_ms());
            Ok(())
        })
    }

    pub fn mark_worker_spawned(&self, id: &str, pid: u32) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) {
                return Err(task_state_error("mark_worker_spawned", record.status));
            }
            record.worker_pid = Some(pid);
            Ok(())
        })
    }

    pub fn complete(&self, id: &str, result: String) -> Result<(), String> {
        self.complete_with_usage(id, result, None)
    }

    pub fn complete_with_usage(
        &self,
        id: &str,
        result: String,
        usage: Option<UsageTotals>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::Completed;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.result = Some(result);
            record.error = None;
            record.usage = usage;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn fail(&self, id: &str, error: String) -> Result<(), String> {
        self.fail_with_usage(id, error, None)
    }

    pub fn fail_with_usage(
        &self,
        id: &str,
        error: String,
        usage: Option<UsageTotals>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::Failed;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.error = Some(error);
            record.result = None;
            record.usage = usage;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn approval_required(&self, id: &str, summary: String) -> Result<(), String> {
        self.approval_required_for_tool(id, summary, None)
    }

    pub fn approval_required_for_tool(
        &self,
        id: &str,
        summary: String,
        tool: Option<String>,
    ) -> Result<(), String> {
        self.approval_required_with_pending_tool(id, summary, tool, None)
    }

    pub fn approval_required_for_pending_tool(
        &self,
        id: &str,
        summary: String,
        pending_tool_call: Option<PendingToolCallSummary>,
    ) -> Result<(), String> {
        let tool = pending_tool_call
            .as_ref()
            .map(|pending_tool_call| pending_tool_call.name.clone());
        self.approval_required_with_pending_tool(id, summary, tool, pending_tool_call)
    }

    pub fn approval_required_for_pending_provider_response(
        &self,
        id: &str,
        summary: String,
        response: ProviderResponse,
    ) -> Result<(), String> {
        let pending_tool_call = pending_tool_call_from_provider_response(&response);
        let tool = pending_tool_call
            .as_ref()
            .map(|pending_tool_call| pending_tool_call.name.clone());
        self.approval_required_with_pending_provider_response(
            id,
            summary,
            tool,
            pending_tool_call,
            Some(response),
        )
    }

    fn approval_required_with_pending_tool(
        &self,
        id: &str,
        summary: String,
        tool: Option<String>,
        pending_tool_call: Option<PendingToolCallSummary>,
    ) -> Result<(), String> {
        self.approval_required_with_pending_provider_response(
            id,
            summary,
            tool,
            pending_tool_call,
            None,
        )
    }

    fn approval_required_with_pending_provider_response(
        &self,
        id: &str,
        summary: String,
        tool: Option<String>,
        pending_tool_call: Option<PendingToolCallSummary>,
        pending_provider_response: Option<ProviderResponse>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::ApprovalRequired;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.result = Some(summary);
            record.error = None;
            record.tool = tool;
            record.pending_tool_call = pending_tool_call;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = pending_provider_response;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn submit_pending_tool_approval_response(
        &self,
        id: &str,
        approved: bool,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            if record.status != TaskStatus::ApprovalRequired || record.pending_tool_call.is_none() {
                return Err(format!(
                    "cannot submit approval response without pending approval_required tool for task '{}'",
                    record.id
                ));
            }
            record.pending_tool_approval_response = Some(approved);
            Ok(())
        })
    }

    pub fn submit_pending_tool_approval_response_by_request_id(
        &self,
        request_id: &str,
        approved: bool,
    ) -> Result<String, String> {
        self.with_tasks(|tasks| {
            let mut matching_task_ids = tasks
                .iter()
                .filter(|(_, record)| {
                    record.status == TaskStatus::ApprovalRequired
                        && record
                            .pending_tool_call
                            .as_ref()
                            .is_some_and(|pending_tool_call| pending_tool_call.id == request_id)
                })
                .map(|(task_id, _)| task_id.clone());
            let Some(task_id) = matching_task_ids.next() else {
                return Err(format!("pending approval request '{request_id}' not found"));
            };
            if matching_task_ids.next().is_some() {
                return Err(format!(
                    "pending approval request '{request_id}' matched multiple tasks"
                ));
            }
            let record = tasks
                .get_mut(&task_id)
                .ok_or_else(|| format!("task '{task_id}' not found"))?;
            if record.pending_tool_approval_response.is_some() {
                return Err(format!(
                    "pending approval request '{request_id}' already has a response"
                ));
            }
            record.pending_tool_approval_response = Some(approved);
            Ok(task_id)
        })
        .map_err(|_| "task registry lock poisoned".to_string())?
    }

    pub fn take_pending_tool_approval_response(&self, id: &str) -> Result<Option<bool>, String> {
        self.with_tasks(|tasks| {
            let record = tasks
                .get_mut(id)
                .ok_or_else(|| format!("task '{id}' not found"))?;
            Ok(record.pending_tool_approval_response.take())
        })
        .map_err(|_| "task registry lock poisoned".to_string())?
    }

    pub fn take_approved_pending_provider_response(
        &self,
        id: &str,
    ) -> Result<Option<ProviderResponse>, String> {
        self.with_tasks(|tasks| {
            let record = tasks
                .get_mut(id)
                .ok_or_else(|| format!("task '{id}' not found"))?;
            if record.status != TaskStatus::ApprovalRequired
                || record.pending_tool_approval_response != Some(true)
            {
                return Ok(None);
            }

            let Some(response) = record.pending_provider_response.take() else {
                return Ok(None);
            };

            record.status = TaskStatus::Running;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = None;
            record.result = None;
            record.error = None;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.worker_pid = None;
            record.last_activity_at_ms = Some(now_ms());
            record.control.pause.store(false, Ordering::Release);
            self.persist_current_session(tasks)?;
            Ok(Some(response))
        })
        .map_err(|_| "task registry lock poisoned".to_string())?
    }

    pub fn finish_denied_pending_tool_approval(&self, id: &str) -> Result<bool, String> {
        let mut consumed = false;
        self.update_task(id, |record| {
            if record.status != TaskStatus::ApprovalRequired
                || record.pending_tool_call.is_none()
                || record.pending_tool_approval_response != Some(false)
            {
                return Ok(());
            }

            record.status = TaskStatus::Stopped;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.result = Some("Approval denied".to_string());
            record.error = None;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            consumed = true;
            Ok(())
        })?;
        Ok(consumed)
    }

    pub fn stop(&self, id: &str, summary: String) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::Stopped;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.result = Some(summary);
            record.error = None;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn request_stop(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) {
                return Err(task_state_error("request_stop", record.status));
            }

            record.status = TaskStatus::Stopping;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.control.cancel.cancel();
            Ok(())
        })
    }

    pub fn request_pause(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) {
                return Err(task_state_error("request_pause", record.status));
            }

            record.status = TaskStatus::Paused;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.control.pause.store(true, Ordering::Release);
            Ok(())
        })
    }

    pub fn request_resume(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) || record.control.cancel.is_cancelled() {
                return Err(task_state_error("request_resume", record.status));
            }

            record.status = TaskStatus::Running;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn is_cancelled(&self, id: &str) -> bool {
        self.with_tasks(|tasks| {
            tasks
                .get(id)
                .is_some_and(|record| record.control.cancel.is_cancelled())
        })
        .unwrap_or(false)
    }

    fn update_task<F>(&self, id: &str, update: F) -> Result<(), String>
    where
        F: FnOnce(&mut TaskRecord) -> Result<(), String>,
    {
        self.with_tasks(|tasks| {
            let record = tasks
                .get_mut(id)
                .ok_or_else(|| format!("task '{id}' not found"))?;
            update(record)?;
            self.persist_current_session(tasks)
        })
        .map_err(|_| "task registry lock poisoned".to_string())?
    }

    fn insert_task(&self, id: String, record: TaskRecord) -> Result<(), String> {
        self.with_tasks(|tasks| {
            tasks.insert(id, record);
            self.persist_current_session(tasks)
        })
        .map_err(|_| "task registry lock poisoned".to_string())?
    }

    fn persist_current_session(&self, tasks: &HashMap<String, TaskRecord>) -> Result<(), String> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        persistence
            .write_current_session(tasks)
            .map_err(|error| error.to_string())
    }

    fn with_tasks<R, F>(&self, f: F) -> Result<R, ()>
    where
        F: FnOnce(&mut HashMap<String, TaskRecord>) -> R,
    {
        let mut tasks = self.inner.lock().map_err(|_| ())?;
        Ok(f(&mut tasks))
    }
}

impl TaskPersistence {
    fn new(root: PathBuf, session_id: String) -> Self {
        Self { root, session_id }
    }

    fn write_current_session(&self, tasks: &HashMap<String, TaskRecord>) -> io::Result<()> {
        self.write_session_records(&self.session_id, tasks)?;
        let mut index = self.load_index()?;
        for id in tasks.keys() {
            index.insert(id.clone(), self.session_id.clone());
        }
        self.write_index(&index)
    }

    fn load_record_by_id(&self, id: &str) -> io::Result<Option<TaskRecord>> {
        let index = self.load_index()?;
        let Some(session_id) = index.get(id) else {
            return Ok(None);
        };
        let mut records = self.load_session_records(session_id)?;
        let Some(record) = records.get_mut(id) else {
            return Ok(None);
        };
        if mark_interrupted_if_active(record) {
            self.write_session_records(session_id, &records)?;
        }
        Ok(records.get(id).cloned())
    }

    fn load_session_records(&self, session_id: &str) -> io::Result<HashMap<String, TaskRecord>> {
        let path = self.session_tasks_path(session_id);
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let persisted: HashMap<String, PersistedTaskRecord> = read_json(&path)?;
        let mut changed = false;
        let records = persisted
            .into_iter()
            .map(|(id, record)| {
                let (record, record_changed) = TaskRecord::from_persisted(record);
                changed |= record_changed;
                (id, record)
            })
            .collect::<HashMap<_, _>>();
        if changed {
            self.write_session_records(session_id, &records)?;
        }
        Ok(records)
    }

    fn write_session_records(
        &self,
        session_id: &str,
        records: &HashMap<String, TaskRecord>,
    ) -> io::Result<()> {
        let persisted = records
            .iter()
            .map(|(id, record)| (id.clone(), PersistedTaskRecord::from(record)))
            .collect::<HashMap<_, _>>();
        write_json_pretty(&self.session_tasks_path(session_id), &persisted)
    }

    fn load_index(&self) -> io::Result<HashMap<String, String>> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(HashMap::new());
        }
        read_json(&path)
    }

    fn write_index(&self, index: &HashMap<String, String>) -> io::Result<()> {
        write_json_pretty(&self.index_path(), index)
    }

    fn session_tasks_path(&self, session_id: &str) -> PathBuf {
        self.root
            .join(safe_path_component(session_id))
            .join("tasks.json")
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("task-index.json")
    }
}

impl RuntimeSubagentStatusLookup for TaskRegistry {
    fn subagent_status_record(&self, agent_id: &str) -> Option<RuntimeSubagentStatusRecord> {
        let record = self.get(agent_id)?;
        if record.task_type != TaskType::Subagent {
            return None;
        }
        Some(RuntimeSubagentStatusRecord {
            id: record.id,
            status: serde_json::to_value(record.status)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{:?}", record.status)),
            description: record.description,
            agent_type: record.agent_type,
            created_at_ms: record.created_at_ms,
            started_at_ms: record.started_at_ms,
            completed_at_ms: record.completed_at_ms,
            output: record.result,
            error: record.error,
            usage: record.usage.map(|usage| RuntimeUsageTotals {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_tokens: usage.cache_tokens,
                estimated_cost_usd: usage.estimated_cost_usd,
            }),
            subagent_current_activity: record.subagent_current_activity,
            subagent_turn: record.subagent_turn,
            last_activity_at_ms: record.last_activity_at_ms,
        })
    }
}

impl PersistedTaskRecord {
    fn into_task_record(self) -> (TaskRecord, bool) {
        let mut changed = false;
        let pending_provider_response = self
            .pending_provider_response
            .map(|value| serde_json::from_value::<PersistedProviderResponse>(value))
            .transpose();
        let mut record = TaskRecord {
            id: self.id,
            task_type: self.task_type,
            status: self.status,
            is_backgrounded: self.is_backgrounded,
            description: self.description,
            created_at_ms: self.created_at_ms,
            started_at_ms: self.started_at_ms,
            completed_at_ms: self.completed_at_ms,
            name: self.name,
            agent_type: self.agent_type,
            tool: self.tool,
            pending_tool_call: self.pending_tool_call,
            pending_tool_approval_response: None,
            pending_provider_response: None,
            workflow_run_id: self.workflow_run_id,
            phase_count: self.phase_count,
            workflow_progress: self.workflow_progress,
            workflow_phases: self.workflow_phases,
            workflow_agents: self.workflow_agents,
            workflow_script_path: self.workflow_script_path,
            workflow_launch_input: self.workflow_launch_input,
            workflow_final_summary: self.workflow_final_summary,
            workflow_failure_count: self.workflow_failure_count,
            usage: self.usage,
            subagent_current_activity: self.subagent_current_activity,
            subagent_turn: self.subagent_turn,
            last_activity_at_ms: self.last_activity_at_ms,
            result: self.result,
            error: self.error,
            worker_pid: self.worker_pid,
            command: self.command,
            control: new_task_control(),
        };
        match pending_provider_response {
            Ok(Some(response)) => {
                record.pending_provider_response = Some(response.into_provider_response());
            }
            Ok(None) => {}
            Err(error) => {
                fail_invalid_pending_provider_response(&mut record, &error);
                changed = true;
            }
        }
        (record, changed)
    }
}

impl TaskRecord {
    fn from_persisted(record: PersistedTaskRecord) -> (Self, bool) {
        record.into_task_record()
    }
}

impl From<&TaskRecord> for PersistedTaskRecord {
    fn from(record: &TaskRecord) -> Self {
        Self {
            id: record.id.clone(),
            task_type: record.task_type,
            status: record.status,
            is_backgrounded: record.is_backgrounded,
            description: record.description.clone(),
            created_at_ms: record.created_at_ms,
            started_at_ms: record.started_at_ms,
            completed_at_ms: record.completed_at_ms,
            name: record.name.clone(),
            agent_type: record.agent_type.clone(),
            tool: record.tool.clone(),
            pending_tool_call: record.pending_tool_call.clone(),
            pending_provider_response: record.pending_provider_response.as_ref().and_then(
                |response| serde_json::to_value(PersistedProviderResponse::from(response)).ok(),
            ),
            workflow_run_id: record.workflow_run_id.clone(),
            phase_count: record.phase_count,
            workflow_progress: record.workflow_progress,
            workflow_phases: record.workflow_phases.clone(),
            workflow_agents: record.workflow_agents.clone(),
            workflow_script_path: record.workflow_script_path.clone(),
            workflow_launch_input: record.workflow_launch_input.clone(),
            workflow_final_summary: record.workflow_final_summary.clone(),
            workflow_failure_count: record.workflow_failure_count,
            usage: record.usage,
            subagent_current_activity: record.subagent_current_activity.clone(),
            subagent_turn: record.subagent_turn,
            last_activity_at_ms: record.last_activity_at_ms,
            result: record.result.clone(),
            error: record.error.clone(),
            worker_pid: record.worker_pid,
            command: record.command.clone(),
        }
    }
}

impl PersistedProviderResponse {
    fn into_provider_response(self) -> ProviderResponse {
        ProviderResponse {
            steps: self
                .steps
                .into_iter()
                .map(PersistedProviderStep::into_provider_step)
                .collect(),
            assistant_content: self.assistant_content,
            assistant_reasoning: self.assistant_reasoning,
            tool_calls: self.tool_calls,
            usage: self.usage,
        }
    }
}

impl From<&ProviderResponse> for PersistedProviderResponse {
    fn from(response: &ProviderResponse) -> Self {
        Self {
            steps: response
                .steps
                .iter()
                .filter_map(PersistedProviderStep::from_provider_step)
                .collect(),
            assistant_content: response.assistant_content.clone(),
            assistant_reasoning: response.assistant_reasoning.clone(),
            tool_calls: response.tool_calls.clone(),
            usage: response.usage,
        }
    }
}

impl PersistedProviderStep {
    fn from_provider_step(step: &ProviderStep) -> Option<Self> {
        match step {
            ProviderStep::ReasoningDelta(delta) => Some(Self::ReasoningDelta(delta.clone())),
            ProviderStep::MessageDelta(delta) => Some(Self::MessageDelta(delta.clone())),
            ProviderStep::ToolCallProgress(progress) => {
                Some(Self::ToolCallProgress(progress.clone()))
            }
            ProviderStep::ToolCall(request) => Some(Self::ToolCall(request.clone())),
            ProviderStep::ReplayState(_) => None,
            ProviderStep::Error(error) => Some(Self::Error(error.clone())),
        }
    }

    fn into_provider_step(self) -> ProviderStep {
        match self {
            Self::ReasoningDelta(delta) => ProviderStep::ReasoningDelta(delta),
            Self::MessageDelta(delta) => ProviderStep::MessageDelta(delta),
            Self::ToolCallProgress(progress) => ProviderStep::ToolCallProgress(progress),
            Self::ToolCall(request) => ProviderStep::ToolCall(request),
            Self::Error(error) => ProviderStep::Error(error),
        }
    }
}

fn fail_invalid_pending_provider_response(record: &mut TaskRecord, error: &serde_json::Error) {
    record.status = TaskStatus::Failed;
    if record.started_at_ms.is_none() {
        record.started_at_ms = Some(now_ms());
    }
    record.completed_at_ms = Some(now_ms());
    record.result = None;
    record.error = Some(format!(
        "invalid pending provider response; background continuation failed closed: {error}"
    ));
    record.tool = None;
    record.pending_tool_call = None;
    record.pending_tool_approval_response = None;
    record.pending_provider_response = None;
    record.worker_pid = None;
    record.control.cancel.cancel();
    record.control.pause.store(false, Ordering::Release);
}

fn new_task_control() -> TaskControl {
    TaskControl {
        cancel: CancelToken::new(),
        pause: Arc::new(AtomicBool::new(false)),
    }
}

fn new_task_id() -> String {
    format!("task-{}", uuid::Uuid::new_v4())
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Stopped
            | TaskStatus::Completed
            | TaskStatus::Failed
            | TaskStatus::ApprovalRequired
            | TaskStatus::Cancelled
    )
}

fn pending_tool_call_from_provider_response(
    response: &ProviderResponse,
) -> Option<PendingToolCallSummary> {
    response
        .steps
        .iter()
        .find_map(|step| match step {
            ProviderStep::ToolCall(request) => Some(PendingToolCallSummary {
                id: request.id.clone(),
                name: request.name.as_str().to_string(),
                action: request.action,
                target: request.target.clone(),
                arguments: request
                    .raw_arguments
                    .clone()
                    .unwrap_or_else(|| "{}".to_string()),
            }),
            _ => None,
        })
        .or_else(|| {
            response
                .tool_calls
                .first()
                .map(|tool_call| PendingToolCallSummary {
                    id: tool_call.id.clone(),
                    name: tool_call.function_name.clone(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: tool_call.arguments.clone(),
                })
        })
}

fn mark_interrupted_if_active(record: &mut TaskRecord) -> bool {
    if is_terminal(record.status) {
        return false;
    }
    if record.task_type == TaskType::Subagent && record.worker_pid.is_some() {
        return false;
    }
    record.status = TaskStatus::Failed;
    if record.started_at_ms.is_none() {
        record.started_at_ms = Some(now_ms());
    }
    record.completed_at_ms = Some(now_ms());
    record.result = None;
    record.error = Some(format!(
        "{} interrupted before completion; async task execution is process-local",
        task_type_label(record.task_type)
    ));
    record.control.cancel.cancel();
    record.control.pause.store(false, Ordering::Release);
    true
}

fn task_type_label(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::MainSession => "main session",
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}

fn safe_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn task_state_error(action: &str, status: TaskStatus) -> String {
    format!("cannot {action} task in {status:?} state")
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("tasks");
    let temp_path = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}-{}",
        std::process::id(),
        now_ms(),
        uuid::Uuid::new_v4()
    ));
    fs::write(&temp_path, content)?;
    fs::rename(&temp_path, path).inspect_err(|_| {
        let _ = fs::remove_file(&temp_path);
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn task_sessions_root() -> Option<PathBuf> {
    std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .map(|home| home.join("task-sessions"))
}

fn legacy_project_task_sessions_root(cwd: &Path) -> PathBuf {
    cwd.join(".orca").join("task-sessions")
}

fn migrate_legacy_task_sessions(legacy_root: &Path, target_root: &Path) -> io::Result<()> {
    if legacy_root == target_root || !legacy_root.exists() {
        return Ok(());
    }

    let legacy = TaskPersistence::new(legacy_root.to_path_buf(), String::new());
    let target = TaskPersistence::new(target_root.to_path_buf(), String::new());
    let legacy_index = legacy.load_index()?;
    if legacy_index.is_empty() {
        return Ok(());
    }

    let mut target_index = target.load_index()?;
    let mut changed_index = false;
    let session_ids = legacy_index.values().cloned().collect::<HashSet<_>>();
    for session_id in session_ids {
        let legacy_records = legacy.load_session_records(&session_id)?;
        if legacy_records.is_empty() {
            continue;
        }

        let mut target_records = target.load_session_records(&session_id)?;
        let mut changed_session = false;
        for (id, record) in legacy_records {
            if legacy_index.get(&id) != Some(&session_id) || target_index.contains_key(&id) {
                continue;
            }
            target_records.insert(id.clone(), record);
            target_index.insert(id, session_id.clone());
            changed_session = true;
            changed_index = true;
        }

        if changed_session {
            target.write_session_records(&session_id, &target_records)?;
        }
    }

    if changed_index {
        target.write_index(&target_index)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn recover_test_lock<T>(lock: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn persistent_registry_recovers_interrupted_subagent_task_by_id() {
        let temp = tempfile::tempdir().unwrap();
        let registry =
            TaskRegistry::new_persistent("session-1".to_string(), temp.path().join("tasks"))
                .unwrap();
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));
        registry.mark_running(&task.id).unwrap();

        let reloaded =
            TaskRegistry::new_persistent("session-2".to_string(), temp.path().join("tasks"))
                .unwrap();
        let recovered = reloaded.get(&task.id).expect("persistent task record");

        assert_eq!(recovered.task_type, TaskType::Subagent);
        assert_eq!(recovered.status, TaskStatus::Failed);
        assert_eq!(recovered.description, "inspect auth");
        assert_eq!(recovered.agent_type.as_deref(), Some("general"));
        assert!(
            recovered
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("interrupted before completion")
        );
        assert!(recovered.completed_at_ms.is_some());
    }

    #[test]
    fn persistent_registry_keeps_worker_owned_subagent_active() {
        let temp = tempfile::tempdir().unwrap();
        let registry =
            TaskRegistry::new_persistent("session-1".to_string(), temp.path().join("tasks"))
                .unwrap();
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));
        registry.mark_running(&task.id).unwrap();
        registry.mark_worker_spawned(&task.id, 12345).unwrap();

        let reloaded =
            TaskRegistry::new_persistent("session-2".to_string(), temp.path().join("tasks"))
                .unwrap();
        let recovered = reloaded.get(&task.id).expect("persistent task record");

        assert_eq!(recovered.status, TaskStatus::Running);
        assert_eq!(recovered.error, None);
        assert_eq!(recovered.worker_pid, Some(12345));
        assert_eq!(recovered.completed_at_ms, None);
    }

    #[test]
    fn cwd_constructor_migrates_legacy_project_task_sessions_to_orca_home() {
        let _guard = recover_test_lock(&TEST_ENV_LOCK);
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("ORCA_HOME");
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }

        let result = std::panic::catch_unwind(|| {
            let legacy_root = cwd.path().join(".orca").join("task-sessions");
            let legacy =
                TaskRegistry::new_persistent("legacy-session".to_string(), legacy_root).unwrap();
            let task = legacy
                .create_subagent("legacy async task".to_string(), Some("general".to_string()));
            legacy
                .complete(&task.id, "legacy result".to_string())
                .unwrap();
            drop(legacy);

            let registry = TaskRegistry::new_for_cwd("new-session".to_string(), cwd.path());
            let recovered = registry.get(&task.id).expect("legacy task should migrate");

            assert_eq!(recovered.status, TaskStatus::Completed);
            assert_eq!(recovered.result.as_deref(), Some("legacy result"));
            assert!(
                home.path()
                    .join("task-sessions")
                    .join("task-index.json")
                    .exists(),
                "migrated task index should be written under ORCA_HOME"
            );
        });

        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("ORCA_HOME", previous);
            } else {
                std::env::remove_var("ORCA_HOME");
            }
        }
        result.unwrap();
    }

    #[test]
    fn registry_creates_and_lists_workflow_tasks() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            2,
        );

        assert!(task.id.starts_with("task-"));
        assert_eq!(task.workflow_run_id.as_deref(), Some("workflow-run-1"));

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, TaskType::Workflow);
        assert_eq!(list[0].status, TaskStatus::Queued);
        assert_eq!(list[0].name.as_deref(), Some("audit"));
        assert_eq!(list[0].workflow_run_id.as_deref(), Some("workflow-run-1"));
        assert_eq!(list[0].phase_count, Some(2));
    }

    #[test]
    fn registry_creates_and_lists_subagent_tasks() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));

        assert!(task.id.starts_with("task-"));
        assert_eq!(task.task_type, TaskType::Subagent);

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, TaskType::Subagent);
        assert_eq!(list[0].status, TaskStatus::Queued);
        assert_eq!(list[0].description, "inspect auth");
        assert_eq!(list[0].agent_type.as_deref(), Some("general"));
        assert!(list[0].created_at_ms > 0);
        assert_eq!(list[0].started_at_ms, None);
        assert_eq!(list[0].completed_at_ms, None);
    }

    #[test]
    fn registry_creates_and_lists_main_session_tasks() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Summarize architecture".to_string());

        assert!(task.id.starts_with("task-"));
        assert_eq!(task.task_type, TaskType::MainSession);

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, TaskType::MainSession);
        assert_eq!(list[0].status, TaskStatus::Queued);
        assert_eq!(list[0].description, "Summarize architecture");
        assert_eq!(list[0].agent_type.as_deref(), Some("main-session"));
        assert!(list[0].created_at_ms > 0);
        assert_eq!(list[0].started_at_ms, None);
        assert_eq!(list[0].completed_at_ms, None);
    }

    #[test]
    fn registry_marks_running_main_session_backgrounded() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Long analysis".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, TaskType::MainSession);
        assert_eq!(list[0].status, TaskStatus::Running);
        assert!(list[0].is_backgrounded);
    }

    #[test]
    fn registry_marks_backgrounded_main_session_approval_required() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs a tool".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required(&task.id, "approval_required".to_string())
            .unwrap();

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.task_type, TaskType::MainSession);
        assert_eq!(record.status, TaskStatus::ApprovalRequired);
        assert!(record.is_backgrounded);
        assert_eq!(record.result.as_deref(), Some("approval_required"));
        assert_eq!(record.error, None);
        assert!(record.completed_at_ms.is_some());
    }

    #[test]
    fn registry_lists_approval_required_tool_name() {
        use orca_core::approval_types::ActionKind;
        use orca_core::task_types::PendingToolCallSummary;

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs a tool".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, TaskStatus::ApprovalRequired);
        assert_eq!(list[0].tool.as_deref(), Some("task_list"));
        let pending_tool = list[0].pending_tool_call.as_ref().unwrap();
        assert_eq!(pending_tool.id, "mock-tool-1");
        assert_eq!(pending_tool.name, "task_list");
        assert_eq!(pending_tool.action, ActionKind::Read);
        assert_eq!(pending_tool.arguments, "{}");
    }

    #[test]
    fn registry_records_pending_tool_approval_response_once() {
        use orca_core::approval_types::ActionKind;
        use orca_core::task_types::PendingToolCallSummary;

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();

        assert_eq!(
            registry
                .take_pending_tool_approval_response(&task.id)
                .unwrap(),
            None
        );

        registry
            .submit_pending_tool_approval_response(&task.id, true)
            .unwrap();

        assert_eq!(
            registry
                .take_pending_tool_approval_response(&task.id)
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            registry
                .take_pending_tool_approval_response(&task.id)
                .unwrap(),
            None
        );
    }

    #[test]
    fn registry_records_pending_tool_approval_response_by_request_id() {
        use orca_core::approval_types::ActionKind;
        use orca_core::task_types::PendingToolCallSummary;

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(PendingToolCallSummary {
                    id: "approval-request-1".to_string(),
                    name: "task_list".to_string(),
                    action: ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();

        let resolved_task_id = registry
            .submit_pending_tool_approval_response_by_request_id("approval-request-1", true)
            .unwrap();

        assert_eq!(resolved_task_id, task.id);
        assert!(
            registry
                .submit_pending_tool_approval_response_by_request_id("approval-request-1", false)
                .is_err()
        );
        assert_eq!(
            registry
                .take_pending_tool_approval_response(&task.id)
                .unwrap(),
            Some(true)
        );
    }

    #[test]
    fn registry_finishes_denied_pending_tool_approval() {
        use orca_core::approval_types::ActionKind;
        use orca_core::task_types::PendingToolCallSummary;

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry.mark_worker_spawned(&task.id, 42).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();
        registry
            .submit_pending_tool_approval_response(&task.id, false)
            .unwrap();

        assert!(
            registry
                .finish_denied_pending_tool_approval(&task.id)
                .unwrap()
        );

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Stopped);
        assert_eq!(record.result.as_deref(), Some("Approval denied"));
        assert_eq!(record.error, None);
        assert_eq!(record.tool, None);
        assert_eq!(record.pending_tool_call, None);
        assert_eq!(record.pending_tool_approval_response, None);
        assert_eq!(record.worker_pid, None);
        assert!(record.completed_at_ms.is_some());

        assert!(
            !registry
                .finish_denied_pending_tool_approval(&task.id)
                .unwrap()
        );
    }

    #[test]
    fn registry_takes_approved_pending_provider_response() {
        use orca_core::approval_types::ActionKind;
        use orca_core::provider_types::{ProviderResponse, ProviderStep};
        use orca_core::tool_types::{ToolName, ToolRequest};

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        let tool_request = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let response = ProviderResponse {
            steps: vec![ProviderStep::ToolCall(tool_request.clone())],
            assistant_content: Some("I need to inspect tasks.".to_string()),
            assistant_reasoning: Some("Need task_list.".to_string()),
            tool_calls: Vec::new(),
            usage: None,
        };

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_provider_response(
                &task.id,
                "approval_required".to_string(),
                response,
            )
            .unwrap();

        let pending = registry.get(&task.id).unwrap();
        assert_eq!(pending.status, TaskStatus::ApprovalRequired);
        assert_eq!(
            pending.pending_tool_call.as_ref().unwrap().id,
            "mock-tool-1"
        );
        assert!(
            registry
                .take_approved_pending_provider_response(&task.id)
                .unwrap()
                .is_none()
        );

        registry
            .submit_pending_tool_approval_response(&task.id, true)
            .unwrap();

        let approved = registry
            .take_approved_pending_provider_response(&task.id)
            .unwrap()
            .expect("approved provider response");
        assert_eq!(
            approved.assistant_content.as_deref(),
            Some("I need to inspect tasks.")
        );
        assert_eq!(approved.steps.len(), 1);

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Running);
        assert_eq!(record.result, None);
        assert_eq!(record.error, None);
        assert_eq!(record.tool, None);
        assert_eq!(record.pending_tool_call, None);
        assert_eq!(record.pending_tool_approval_response, None);
        assert_eq!(record.completed_at_ms, None);

        assert!(
            registry
                .take_approved_pending_provider_response(&task.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn runtime_continuation_takes_approved_provider_response_with_preapproved_tool_id() {
        use crate::background_turn::take_approved_background_turn_continuation;
        use orca_core::approval_types::ActionKind;
        use orca_core::provider_types::{ProviderResponse, ProviderStep};
        use orca_core::tool_types::{ToolName, ToolRequest};

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        let tool_request = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_provider_response(
                &task.id,
                "approval_required".to_string(),
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool_request)],
                    assistant_content: Some("I need to inspect tasks.".to_string()),
                    assistant_reasoning: Some("Need task_list.".to_string()),
                    tool_calls: Vec::new(),
                    usage: None,
                },
            )
            .unwrap();
        registry
            .submit_pending_tool_approval_response(&task.id, true)
            .unwrap();

        let continuation = take_approved_background_turn_continuation(&registry, &task.id)
            .unwrap()
            .expect("approved background continuation");

        assert_eq!(
            continuation.preapproved_tool_call_id.as_deref(),
            Some("mock-tool-1")
        );
        assert_eq!(
            continuation.response.assistant_content.as_deref(),
            Some("I need to inspect tasks.")
        );
        assert!(
            take_approved_background_turn_continuation(&registry, &task.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn persistent_registry_restores_pending_provider_response_for_background_continuation() {
        use crate::background_turn::take_approved_background_turn_continuation;
        use orca_core::approval_types::ActionKind;
        use orca_core::provider_types::{ProviderResponse, ProviderStep};
        use orca_core::tool_types::{ToolName, ToolRequest};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = registry.create_main_session("Needs approval".to_string());
        let tool_request = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_provider_response(
                &task.id,
                "approval_required".to_string(),
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool_request)],
                    assistant_content: Some("I need to inspect tasks.".to_string()),
                    assistant_reasoning: Some("Need task_list.".to_string()),
                    tool_calls: Vec::new(),
                    usage: None,
                },
            )
            .unwrap();
        drop(registry);

        let reloaded = TaskRegistry::new_persistent("session-1".to_string(), root).unwrap();
        let pending = reloaded.get(&task.id).expect("persistent task record");
        assert_eq!(pending.status, TaskStatus::ApprovalRequired);
        assert_eq!(
            pending
                .pending_tool_call
                .as_ref()
                .map(|tool| tool.id.as_str()),
            Some("mock-tool-1")
        );

        reloaded
            .submit_pending_tool_approval_response(&task.id, true)
            .unwrap();
        let continuation = take_approved_background_turn_continuation(&reloaded, &task.id)
            .unwrap()
            .expect("approved background continuation after reload");

        assert_eq!(
            continuation.preapproved_tool_call_id.as_deref(),
            Some("mock-tool-1")
        );
        assert_eq!(
            continuation.response.assistant_content.as_deref(),
            Some("I need to inspect tasks.")
        );
        assert_eq!(continuation.response.steps.len(), 1);
    }

    #[test]
    fn persistent_registry_fails_closed_invalid_pending_provider_response() {
        use orca_core::approval_types::ActionKind;
        use orca_core::provider_types::{ProviderResponse, ProviderStep};
        use orca_core::tool_types::{ToolName, ToolRequest};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = registry.create_main_session("Needs approval".to_string());
        let tool_request = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_provider_response(
                &task.id,
                "approval_required".to_string(),
                ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool_request)],
                    assistant_content: Some("I need to inspect tasks.".to_string()),
                    assistant_reasoning: Some("Need task_list.".to_string()),
                    tool_calls: Vec::new(),
                    usage: None,
                },
            )
            .unwrap();
        drop(registry);

        let tasks_path = root.join("session-1").join("tasks.json");
        let mut tasks: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&tasks_path).unwrap()).unwrap();
        tasks[&task.id]["pending_provider_response"]["steps"][0]["type"] =
            serde_json::Value::String("future_step".to_string());
        std::fs::write(&tasks_path, serde_json::to_string_pretty(&tasks).unwrap()).unwrap();

        let reloaded = TaskRegistry::new_persistent("session-1".to_string(), root)
            .expect("invalid pending continuation should not prevent registry recovery");
        let recovered = reloaded.get(&task.id).expect("recovered task record");

        assert_eq!(recovered.status, TaskStatus::Failed);
        assert_eq!(recovered.pending_tool_call, None);
        assert_eq!(recovered.pending_tool_approval_response, None);
        assert!(recovered.pending_provider_response.is_none());
        assert!(recovered.completed_at_ms.is_some());
        assert!(
            recovered
                .error
                .as_deref()
                .is_some_and(|error| error.contains("invalid pending provider response")),
            "expected invalid continuation error, got {:?}",
            recovered.error
        );
        let rewritten: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&tasks_path).unwrap()).unwrap();
        assert_eq!(rewritten[&task.id]["status"], serde_json::json!("failed"));
        assert!(rewritten[&task.id]["pending_provider_response"].is_null());
    }

    #[test]
    fn registry_rejects_approval_response_without_pending_tool() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("No pending tool".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let error = registry
            .submit_pending_tool_approval_response(&task.id, true)
            .expect_err("approval response should require pending tool approval");

        assert!(error.contains("approval_required"), "{error}");
    }

    #[test]
    fn registry_tracks_task_lifecycle_timestamps() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_subagent("inspect auth".to_string(), None);

        registry.mark_running(&task.id).unwrap();
        let running = registry.get(&task.id).unwrap();
        assert!(running.started_at_ms.is_some());
        assert_eq!(running.completed_at_ms, None);

        registry
            .complete(&task.id, "finished audit".to_string())
            .unwrap();
        let completed = registry.list().into_iter().next().unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
        assert!(completed.started_at_ms.is_some());
        assert!(completed.completed_at_ms.is_some());
    }

    #[test]
    fn registry_lists_workflow_progress() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            3,
        );

        registry
            .update_workflow_progress(
                &task.id,
                orca_core::task_types::WorkflowTaskProgress {
                    total_agents: 5,
                    running_agents: 2,
                    completed_agents: 2,
                    failed_agents: 1,
                    completed_phases: 1,
                    running_phases: 1,
                    failed_phases: 0,
                },
            )
            .unwrap();

        let list = registry.list();
        assert_eq!(
            list[0].workflow_progress,
            Some(orca_core::task_types::WorkflowTaskProgress {
                total_agents: 5,
                running_agents: 2,
                completed_agents: 2,
                failed_agents: 1,
                completed_phases: 1,
                running_phases: 1,
                failed_phases: 0,
            })
        );
    }

    #[test]
    fn registry_lists_workflow_phase_details() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            1,
        );
        let phase = WorkflowPhaseTaskSummary {
            name: "scan".to_string(),
            status: orca_core::workflow_types::WorkflowRunStatus::Failed,
            agent_count: 1,
            error: Some("scan failed".to_string()),
            fallback: Some("value".to_string()),
        };

        registry
            .update_workflow_phases(&task.id, vec![phase.clone()])
            .unwrap();

        let list = registry.list();
        assert_eq!(list[0].workflow_phases, vec![phase]);
    }

    #[test]
    fn stop_sets_cancel_flag_and_status() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.request_stop(&task.id).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Stopping);
        assert!(record.control.cancel.is_cancelled());
    }

    #[test]
    fn complete_stores_result() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.complete(&task.id, "done".to_string()).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Completed);
        assert_eq!(record.result.as_deref(), Some("done"));
        let summary = registry.list().into_iter().next().unwrap();
        assert_eq!(summary.result.as_deref(), Some("done"));
        assert_eq!(summary.error, None);
    }

    #[test]
    fn complete_with_usage_stores_task_usage() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_subagent("inspect auth".to_string(), None);
        let usage = UsageTotals {
            input_tokens: 120,
            output_tokens: 30,
            cache_tokens: 10,
            estimated_cost_usd: 0.0000252,
        };

        registry
            .complete_with_usage(&task.id, "done".to_string(), Some(usage))
            .unwrap();

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.usage, Some(usage));
        let summary = registry.list().into_iter().next().unwrap();
        assert_eq!(summary.usage, Some(usage));
    }

    #[test]
    fn subagent_activity_updates_live_task_summary() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));
        registry.mark_running(&task.id).unwrap();

        registry
            .update_subagent_activity(&task.id, "bash: cargo test".to_string(), Some(2), None)
            .unwrap();

        let record = registry.get(&task.id).unwrap();
        assert_eq!(
            record.subagent_current_activity.as_deref(),
            Some("bash: cargo test")
        );
        assert_eq!(record.subagent_turn, Some(2));
        assert!(record.last_activity_at_ms.is_some());

        let summary = registry.list().into_iter().next().unwrap();
        assert_eq!(
            summary.subagent_current_activity.as_deref(),
            Some("bash: cargo test")
        );
        assert_eq!(summary.subagent_turn, Some(2));
        assert!(summary.last_activity_at_ms.is_some());
    }

    #[test]
    fn pause_and_resume_toggle_state() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.request_pause(&task.id).unwrap();
        let paused = registry.get(&task.id).unwrap();
        assert_eq!(paused.status, TaskStatus::Paused);
        assert!(paused.control.pause.load(Ordering::SeqCst));

        registry.request_resume(&task.id).unwrap();
        let running = registry.get(&task.id).unwrap();
        assert_eq!(running.status, TaskStatus::Running);
        assert!(!running.control.pause.load(Ordering::SeqCst));
    }

    #[test]
    fn registry_marks_backgrounded_main_session_foregrounded() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("long prompt".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        registry.mark_foregrounded(&task.id).unwrap();

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Running);
        assert!(!record.is_backgrounded);
        assert!(record.last_activity_at_ms.is_some());
    }

    #[test]
    fn mark_running_updates_status() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.mark_running(&task.id).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Running);
    }

    #[test]
    fn fail_stores_error() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.fail(&task.id, "boom".to_string()).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Failed);
        assert_eq!(record.error.as_deref(), Some("boom"));
        let summary = registry.list().into_iter().next().unwrap();
        assert_eq!(summary.result, None);
        assert_eq!(summary.error.as_deref(), Some("boom"));
    }
}
