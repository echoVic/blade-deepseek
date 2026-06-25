use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

#[derive(Clone, Debug)]
pub(crate) struct WorkflowIpcContext {
    mailbox: Arc<WorkflowMailbox>,
    sender: String,
}

impl WorkflowIpcContext {
    pub(crate) fn new() -> Self {
        Self {
            mailbox: Arc::new(WorkflowMailbox::default()),
            sender: "workflow-agent".to_string(),
        }
    }

    pub(crate) fn for_sender(&self, sender: impl Into<String>) -> Self {
        Self {
            mailbox: Arc::clone(&self.mailbox),
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
