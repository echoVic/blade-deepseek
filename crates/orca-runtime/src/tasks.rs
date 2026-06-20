use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use orca_core::cancel::CancelToken;
use orca_core::task_types::{BackgroundTaskSummary, TaskStatus, TaskType};

#[derive(Clone, Debug)]
pub struct TaskRegistry {
    session_id: String,
    inner: Arc<Mutex<HashMap<String, TaskRecord>>>,
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
    pub name: Option<String>,
    pub workflow_run_id: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub control: TaskControl,
}

#[derive(Clone, Debug)]
pub struct TaskControl {
    pub cancel: CancelToken,
    pub pause: Arc<AtomicBool>,
}

impl TaskRegistry {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn create_workflow(
        &self,
        workflow_run_id: String,
        name: String,
        description: String,
    ) -> TaskHandle {
        let id = new_task_id();
        let control = TaskControl {
            cancel: CancelToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
        };
        let record = TaskRecord {
            id: id.clone(),
            task_type: TaskType::Workflow,
            status: TaskStatus::Queued,
            description,
            name: Some(name),
            workflow_run_id: Some(workflow_run_id.clone()),
            result: None,
            error: None,
            control,
        };

        self.with_tasks(|tasks| {
            tasks.insert(id.clone(), record);
        })
        .expect("task registry lock poisoned");

        TaskHandle {
            id,
            task_type: TaskType::Workflow,
            workflow_run_id: Some(workflow_run_id),
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
                        command: None,
                        agent_type: None,
                        server: None,
                        tool: None,
                        name: record.name.clone(),
                    })
                    .collect::<Vec<_>>()
            })
            .expect("task registry lock poisoned");
        summaries.sort_by(|left, right| left.id.cmp(&right.id));
        summaries
    }

    pub fn get(&self, id: &str) -> Option<TaskRecord> {
        self.with_tasks(|tasks| tasks.get(id).cloned())
            .expect("task registry lock poisoned")
    }

    pub fn mark_running(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) || record.control.cancel.is_cancelled() {
                return Err(task_state_error("mark_running", record.status));
            }

            record.status = TaskStatus::Running;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn complete(&self, id: &str, result: String) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::Completed;
            record.result = Some(result);
            record.error = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn fail(&self, id: &str, error: String) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::Failed;
            record.error = Some(error);
            record.result = None;
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
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    fn update_task<F>(&self, id: &str, update: F) -> Result<(), String>
    where
        F: FnOnce(&mut TaskRecord) -> Result<(), String>,
    {
        self.with_tasks(|tasks| {
            let record = tasks
                .get_mut(id)
                .ok_or_else(|| format!("task '{id}' not found"))?;
            update(record)
        })
        .map_err(|_| "task registry lock poisoned".to_string())?
    }

    fn with_tasks<R, F>(&self, f: F) -> Result<R, ()>
    where
        F: FnOnce(&mut HashMap<String, TaskRecord>) -> R,
    {
        let mut tasks = self.inner.lock().map_err(|_| ())?;
        Ok(f(&mut tasks))
    }
}

fn new_task_id() -> String {
    format!("task-{}", uuid::Uuid::new_v4())
}

fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Stopped
            | TaskStatus::Completed
            | TaskStatus::Failed
            | TaskStatus::Cancelled
    )
}

fn task_state_error(action: &str, status: TaskStatus) -> String {
    format!("cannot {action} task in {status:?} state")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_creates_and_lists_workflow_tasks() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
        );

        assert!(task.id.starts_with("task-"));
        assert_eq!(task.workflow_run_id.as_deref(), Some("workflow-run-1"));

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, TaskType::Workflow);
        assert_eq!(list[0].status, TaskStatus::Queued);
        assert_eq!(list[0].name.as_deref(), Some("audit"));
    }

    #[test]
    fn stop_sets_cancel_flag_and_status() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
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
        );

        registry.complete(&task.id, "done".to_string()).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Completed);
        assert_eq!(record.result.as_deref(), Some("done"));
    }

    #[test]
    fn pause_and_resume_toggle_state() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
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
        );

        registry.fail(&task.id, "boom".to_string()).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Failed);
        assert_eq!(record.error.as_deref(), Some("boom"));
    }
}
