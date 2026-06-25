use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub(crate) struct WorkflowIpcContext {
    mailbox: Arc<WorkflowMailbox>,
    task_lists: Arc<WorkflowTaskLists>,
    sender: String,
}

impl WorkflowIpcContext {
    #[allow(dead_code)]
    pub(crate) fn new() -> Self {
        Self {
            mailbox: Arc::new(WorkflowMailbox::default()),
            task_lists: Arc::new(WorkflowTaskLists::default()),
            sender: "workflow-agent".to_string(),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_durable_mailbox(mailbox_path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            mailbox: Arc::new(WorkflowMailbox::durable(mailbox_path)?),
            task_lists: Arc::new(WorkflowTaskLists::default()),
            sender: "workflow-agent".to_string(),
        })
    }

    #[cfg(test)]
    pub(crate) fn new_durable_task_lists(task_lists_path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            mailbox: Arc::new(WorkflowMailbox::default()),
            task_lists: Arc::new(WorkflowTaskLists::durable(task_lists_path)?),
            sender: "workflow-agent".to_string(),
        })
    }

    pub(crate) fn new_durable(mailbox_path: PathBuf, task_lists_path: PathBuf) -> io::Result<Self> {
        Ok(Self {
            mailbox: Arc::new(WorkflowMailbox::durable(mailbox_path)?),
            task_lists: Arc::new(WorkflowTaskLists::durable(task_lists_path)?),
            sender: "workflow-agent".to_string(),
        })
    }

    pub(crate) fn for_sender(&self, sender: impl Into<String>) -> Self {
        Self {
            mailbox: Arc::clone(&self.mailbox),
            task_lists: Arc::clone(&self.task_lists),
            sender: sender.into(),
        }
    }

    pub(crate) fn default_sender(&self) -> &str {
        &self.sender
    }

    pub(crate) fn send_message(
        &self,
        channel: &str,
        from: Option<&str>,
        message: Value,
    ) -> Result<Value, String> {
        self.mailbox
            .send_message(channel, from.unwrap_or(self.default_sender()), message)
    }

    pub(crate) fn read_messages(&self, channel: &str) -> Result<Value, String> {
        self.mailbox.read_messages(channel)
    }

    pub(crate) fn clear_messages(&self, channel: &str) -> Result<Value, String> {
        self.mailbox.clear_messages(channel)
    }

    pub(crate) fn create_task_list(&self, name: &str, items: Vec<Value>) -> Result<Value, String> {
        self.task_lists.create_task_list(name, items)
    }

    pub(crate) fn claim_task(&self, name: &str, by: Option<&str>) -> Result<Value, String> {
        self.task_lists
            .claim_task(name, by.unwrap_or(self.default_sender()))
    }

    pub(crate) fn complete_task(
        &self,
        name: &str,
        task_id: &str,
        result: Value,
        by: Option<&str>,
    ) -> Result<Value, String> {
        self.task_lists
            .complete_task(name, task_id, result, by.unwrap_or(self.default_sender()))
    }

    pub(crate) fn list_tasks(&self, name: &str) -> Result<Value, String> {
        self.task_lists.list_tasks(name)
    }
}

#[derive(Debug)]
struct WorkflowMailbox {
    state: Mutex<WorkflowMailboxState>,
    durable_path: Option<PathBuf>,
}

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowMailboxState {
    next_seq: u64,
    channels: HashMap<String, Vec<Value>>,
}

impl Default for WorkflowMailbox {
    fn default() -> Self {
        Self {
            state: Mutex::new(WorkflowMailboxState::default()),
            durable_path: None,
        }
    }
}

impl WorkflowMailbox {
    fn durable(path: PathBuf) -> io::Result<Self> {
        let state = if path.exists() {
            read_mailbox_state(&path)?
        } else {
            WorkflowMailboxState::default()
        };
        Ok(Self {
            state: Mutex::new(state),
            durable_path: Some(path),
        })
    }

    fn send_message(&self, channel: &str, from: &str, message: Value) -> Result<Value, String> {
        let channel = normalize_channel(channel)?;
        let from = normalize_sender(from);
        let mut state = self
            .state
            .lock()
            .map_err(|_| "workflow mailbox lock poisoned".to_string())?;
        state.next_seq += 1;
        let entry = json!({
            "seq": state.next_seq,
            "channel": channel,
            "from": from,
            "message": message,
        });
        state
            .channels
            .entry(channel)
            .or_default()
            .push(entry.clone());
        self.persist_locked_state(&state)?;
        Ok(entry)
    }

    fn read_messages(&self, channel: &str) -> Result<Value, String> {
        let channel = normalize_channel(channel)?;
        let state = self
            .state
            .lock()
            .map_err(|_| "workflow mailbox lock poisoned".to_string())?;
        Ok(Value::Array(
            state.channels.get(&channel).cloned().unwrap_or_default(),
        ))
    }

    fn clear_messages(&self, channel: &str) -> Result<Value, String> {
        let channel = normalize_channel(channel)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "workflow mailbox lock poisoned".to_string())?;
        let cleared = state
            .channels
            .remove(&channel)
            .map(|items| items.len())
            .unwrap_or(0);
        self.persist_locked_state(&state)?;
        Ok(json!({ "cleared": cleared }))
    }

    fn persist_locked_state(&self, state: &WorkflowMailboxState) -> Result<(), String> {
        let Some(path) = &self.durable_path else {
            return Ok(());
        };
        write_mailbox_state(path, state).map_err(|error| {
            format!(
                "failed to persist workflow mailbox {}: {error}",
                path.display()
            )
        })
    }
}

fn read_mailbox_state(path: &Path) -> io::Result<WorkflowMailboxState> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_mailbox_state(path: &Path, state: &WorkflowMailboxState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mailbox");
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn normalize_channel(channel: &str) -> Result<String, String> {
    let channel = channel.trim();
    if channel.is_empty() {
        return Err("workflow mailbox channel must be a non-empty string".to_string());
    }
    Ok(channel.to_string())
}

fn normalize_sender(sender: &str) -> String {
    let sender = sender.trim();
    if sender.is_empty() {
        "workflow-agent".to_string()
    } else {
        sender.to_string()
    }
}

#[derive(Debug)]
struct WorkflowTaskLists {
    state: Mutex<WorkflowTaskListState>,
    durable_path: Option<PathBuf>,
}

#[derive(Clone, Default, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowTaskListState {
    next_task_seq: u64,
    lists: HashMap<String, Vec<WorkflowTaskItem>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowTaskItem {
    id: String,
    status: WorkflowTaskStatus,
    value: Value,
    claimed_by: Option<String>,
    completed_by: Option<String>,
    result: Value,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum WorkflowTaskStatus {
    Pending,
    Running,
    Completed,
}

impl Default for WorkflowTaskLists {
    fn default() -> Self {
        Self {
            state: Mutex::new(WorkflowTaskListState::default()),
            durable_path: None,
        }
    }
}

impl WorkflowTaskLists {
    fn durable(path: PathBuf) -> io::Result<Self> {
        let state = if path.exists() {
            read_task_list_state(&path)?
        } else {
            WorkflowTaskListState::default()
        };
        Ok(Self {
            state: Mutex::new(state),
            durable_path: Some(path),
        })
    }

    fn create_task_list(&self, name: &str, items: Vec<Value>) -> Result<Value, String> {
        let name = normalize_task_list_name(name)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "workflow task list lock poisoned".to_string())?;
        let tasks = items
            .into_iter()
            .map(|value| {
                state.next_task_seq += 1;
                WorkflowTaskItem {
                    id: format!("workflow-task-{}", state.next_task_seq),
                    status: WorkflowTaskStatus::Pending,
                    value,
                    claimed_by: None,
                    completed_by: None,
                    result: Value::Null,
                }
            })
            .collect::<Vec<_>>();
        state.lists.insert(name.clone(), tasks);
        self.persist_locked_state(&state)?;
        Ok(tasks_to_value(
            state.lists.get(&name).expect("inserted task list"),
        ))
    }

    fn claim_task(&self, name: &str, by: &str) -> Result<Value, String> {
        let name = normalize_task_list_name(name)?;
        let by = normalize_task_worker(by);
        let mut state = self
            .state
            .lock()
            .map_err(|_| "workflow task list lock poisoned".to_string())?;
        let Some(task) = state.lists.get_mut(&name).and_then(|tasks| {
            tasks
                .iter_mut()
                .find(|task| task.status == WorkflowTaskStatus::Pending)
        }) else {
            return Ok(Value::Null);
        };
        task.status = WorkflowTaskStatus::Running;
        task.claimed_by = Some(by);
        let value = task_to_value(task);
        self.persist_locked_state(&state)?;
        Ok(value)
    }

    fn complete_task(
        &self,
        name: &str,
        task_id: &str,
        result: Value,
        by: &str,
    ) -> Result<Value, String> {
        let name = normalize_task_list_name(name)?;
        let task_id = normalize_task_id(task_id)?;
        let by = normalize_task_worker(by);
        let mut state = self
            .state
            .lock()
            .map_err(|_| "workflow task list lock poisoned".to_string())?;
        let Some(task) = state
            .lists
            .get_mut(&name)
            .and_then(|tasks| tasks.iter_mut().find(|task| task.id == task_id))
        else {
            return Err(format!("workflow task not found: {task_id}"));
        };
        task.status = WorkflowTaskStatus::Completed;
        task.completed_by = Some(by);
        task.result = result;
        let value = task_to_value(task);
        self.persist_locked_state(&state)?;
        Ok(value)
    }

    fn list_tasks(&self, name: &str) -> Result<Value, String> {
        let name = normalize_task_list_name(name)?;
        let state = self
            .state
            .lock()
            .map_err(|_| "workflow task list lock poisoned".to_string())?;
        Ok(tasks_to_value(
            state.lists.get(&name).map(Vec::as_slice).unwrap_or(&[]),
        ))
    }

    fn persist_locked_state(&self, state: &WorkflowTaskListState) -> Result<(), String> {
        let Some(path) = &self.durable_path else {
            return Ok(());
        };
        write_task_list_state(path, state).map_err(|error| {
            format!(
                "failed to persist workflow task lists {}: {error}",
                path.display()
            )
        })
    }
}

fn read_task_list_state(path: &Path) -> io::Result<WorkflowTaskListState> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn write_task_list_state(path: &Path, state: &WorkflowTaskListState) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("task-lists");
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

fn tasks_to_value(tasks: &[WorkflowTaskItem]) -> Value {
    Value::Array(tasks.iter().map(task_to_value).collect())
}

fn task_to_value(task: &WorkflowTaskItem) -> Value {
    json!({
        "id": task.id,
        "status": task.status.as_str(),
        "value": task.value,
        "claimedBy": task.claimed_by,
        "completedBy": task.completed_by,
        "result": task.result,
    })
}

impl WorkflowTaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
        }
    }
}

fn normalize_task_list_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("workflow task list name must be a non-empty string".to_string());
    }
    Ok(name.to_string())
}

fn normalize_task_id(task_id: &str) -> Result<String, String> {
    let task_id = task_id.trim();
    if task_id.is_empty() {
        return Err("workflow task id must be a non-empty string".to_string());
    }
    Ok(task_id.to_string())
}

fn normalize_task_worker(worker: &str) -> String {
    let worker = worker.trim();
    if worker.is_empty() {
        "workflow-agent".to_string()
    } else {
        worker.to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use tempfile::tempdir;

    use super::WorkflowIpcContext;

    #[test]
    fn durable_mailbox_preserves_messages_across_context_reloads() {
        let temp = tempdir().unwrap();
        let mailbox_path = temp.path().join("mailbox.json");
        let first = WorkflowIpcContext::new_durable_mailbox(mailbox_path.clone()).unwrap();

        let sent = first
            .send_message("findings", Some("scanner"), json!({ "severity": "high" }))
            .expect("send durable message");
        assert_eq!(sent["seq"], 1);

        let second = WorkflowIpcContext::new_durable_mailbox(mailbox_path).unwrap();
        let messages = second
            .read_messages("findings")
            .expect("read durable messages");

        assert_eq!(messages.as_array().unwrap().len(), 1);
        assert_eq!(messages[0]["from"], "scanner");
        assert_eq!(messages[0]["message"]["severity"], "high");
    }

    #[test]
    fn durable_task_lists_preserve_claimed_tasks_across_context_reloads() {
        let temp = tempdir().unwrap();
        let task_lists_path = temp.path().join("task-lists.json");
        let first = WorkflowIpcContext::new_durable_task_lists(task_lists_path.clone()).unwrap();

        first
            .create_task_list("audit", vec![json!({ "area": "api" })])
            .expect("create durable task list");
        let claimed = first
            .claim_task("audit", Some("worker-a"))
            .expect("claim durable task");
        assert_eq!(claimed["id"], "workflow-task-1");
        assert_eq!(claimed["status"], "running");

        let second = WorkflowIpcContext::new_durable_task_lists(task_lists_path).unwrap();
        let tasks = second.list_tasks("audit").expect("list durable tasks");

        assert_eq!(tasks.as_array().unwrap().len(), 1);
        assert_eq!(tasks[0]["id"], "workflow-task-1");
        assert_eq!(tasks[0]["status"], "running");
        assert_eq!(tasks[0]["claimedBy"], "worker-a");
    }
}
