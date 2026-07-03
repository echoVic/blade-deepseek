use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use orca_core::cancel::CancelToken;
use orca_core::cost_types::UsageTotals;
use orca_core::task_types::{
    BackgroundTaskSummary, TaskStatus, TaskType, WorkflowAgentTaskSummary,
    WorkflowPhaseTaskSummary, WorkflowTaskProgress,
};
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
    pub description: String,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub name: Option<String>,
    pub agent_type: Option<String>,
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
    description: String,
    created_at_ms: i64,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
    name: Option<String>,
    agent_type: Option<String>,
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

    pub fn new_for_cwd(session_id: String, cwd: &Path) -> Self {
        Self::new_persistent(session_id.clone(), cwd.join(".orca").join("task-sessions"))
            .unwrap_or_else(|_| Self::new(session_id))
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
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: Some(name),
            agent_type: None,
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
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: None,
            agent_type,
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
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: None,
            agent_type: None,
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
                        description: record.description.clone(),
                        created_at_ms: record.created_at_ms,
                        started_at_ms: record.started_at_ms,
                        completed_at_ms: record.completed_at_ms,
                        command: record.command.clone(),
                        agent_type: record.agent_type.clone(),
                        server: None,
                        tool: None,
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
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
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
        let records: HashMap<String, PersistedTaskRecord> = read_json(&path)?;
        Ok(records
            .into_iter()
            .map(|(id, record)| (id, TaskRecord::from_persisted(record)))
            .collect())
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
    fn into_task_record(self) -> TaskRecord {
        TaskRecord {
            id: self.id,
            task_type: self.task_type,
            status: self.status,
            description: self.description,
            created_at_ms: self.created_at_ms,
            started_at_ms: self.started_at_ms,
            completed_at_ms: self.completed_at_ms,
            name: self.name,
            agent_type: self.agent_type,
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
        }
    }
}

impl TaskRecord {
    fn from_persisted(record: PersistedTaskRecord) -> Self {
        record.into_task_record()
    }
}

impl From<&TaskRecord> for PersistedTaskRecord {
    fn from(record: &TaskRecord) -> Self {
        Self {
            id: record.id.clone(),
            task_type: record.task_type,
            status: record.status,
            description: record.description.clone(),
            created_at_ms: record.created_at_ms,
            started_at_ms: record.started_at_ms,
            completed_at_ms: record.completed_at_ms,
            name: record.name.clone(),
            agent_type: record.agent_type.clone(),
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
        TaskStatus::Stopped | TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
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
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        now_ms()
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

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
