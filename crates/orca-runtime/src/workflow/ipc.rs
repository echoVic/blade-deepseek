use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub(crate) struct WorkflowIpcContext {
    mailbox: Arc<WorkflowMailbox>,
    task_lists: Arc<WorkflowTaskLists>,
    sender: String,
}

impl WorkflowIpcContext {
    pub(crate) fn new() -> Self {
        Self {
            mailbox: Arc::new(WorkflowMailbox::default()),
            task_lists: Arc::new(WorkflowTaskLists::default()),
            sender: "workflow-agent".to_string(),
        }
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

#[derive(Default, Debug)]
struct WorkflowMailbox {
    state: Mutex<WorkflowMailboxState>,
}

#[derive(Default, Debug)]
struct WorkflowMailboxState {
    next_seq: u64,
    channels: HashMap<String, Vec<Value>>,
}

impl WorkflowMailbox {
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
        Ok(json!({ "cleared": cleared }))
    }
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

#[derive(Default, Debug)]
struct WorkflowTaskLists {
    state: Mutex<WorkflowTaskListState>,
}

#[derive(Default, Debug)]
struct WorkflowTaskListState {
    next_task_seq: u64,
    lists: HashMap<String, Vec<WorkflowTaskItem>>,
}

#[derive(Clone, Debug)]
struct WorkflowTaskItem {
    id: String,
    status: WorkflowTaskStatus,
    value: Value,
    claimed_by: Option<String>,
    completed_by: Option<String>,
    result: Value,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkflowTaskStatus {
    Pending,
    Running,
    Completed,
}

impl WorkflowTaskLists {
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
        Ok(task_to_value(task))
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
        Ok(task_to_value(task))
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
