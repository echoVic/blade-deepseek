use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::workflow_types::{WorkflowAgentStatus, WorkflowRunState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowAgentCacheRecord {
    pub call_path: String,
    pub input_hash: String,
    pub output: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentRecord {
    pub call_id: String,
    pub call_path: String,
    pub prompt: String,
    pub opts: Value,
    pub input_hash: String,
    pub status: WorkflowAgentStatus,
    pub output: Option<String>,
    pub error: Option<String>,
    pub transcript_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum WorkflowAgentCacheFile {
    Current(HashMap<String, WorkflowAgentRecord>),
    Legacy(HashMap<String, WorkflowAgentCacheRecord>),
}

#[derive(Clone, Debug)]
pub struct WorkflowStateStore {
    root: PathBuf,
}

impl WorkflowStateStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.join(run_id)
    }

    pub fn transcript_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("transcripts")
    }

    pub fn create_run(&self, state: &WorkflowRunState) -> io::Result<()> {
        fs::create_dir_all(self.transcript_dir(&state.run_id))?;
        self.write_state(state)
    }

    pub fn load_run(&self, run_id: &str) -> io::Result<WorkflowRunState> {
        self.load_state(run_id)
    }

    pub fn write_state(&self, state: &WorkflowRunState) -> io::Result<()> {
        let run_dir = self.run_dir(&state.run_id);
        fs::create_dir_all(&run_dir)?;
        write_json_pretty(&run_dir.join("state.json"), state)
    }

    pub fn load_state(&self, run_id: &str) -> io::Result<WorkflowRunState> {
        read_json(&self.run_dir(run_id).join("state.json"))
    }

    pub fn record_agent_completed(
        &self,
        run_id: &str,
        record: impl IntoWorkflowAgentRecord,
    ) -> io::Result<()> {
        let record = record.into_workflow_agent_record();
        let path = self.run_dir(run_id).join("agent-cache.json");
        let mut cache = if path.exists() {
            read_agent_cache(&path)?
        } else {
            HashMap::new()
        };
        cache.insert(cache_key(&record.call_path, &record.input_hash), record);
        write_json_pretty(&path, &cache)
    }

    pub fn find_cached_agent(
        &self,
        run_id: &str,
        call_path: &str,
        input_hash: &str,
    ) -> Option<String> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return None;
        }

        let cache = read_agent_cache(&path).ok()?;
        cache
            .get(&cache_key(call_path, input_hash))
            .filter(|record| record.status == WorkflowAgentStatus::Completed)
            .and_then(|record| record.output.clone())
    }

    pub fn cached_agent_result(
        &self,
        run_id: &str,
        call_path: &str,
        input_hash: &str,
    ) -> io::Result<Option<WorkflowAgentCacheRecord>> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return Ok(None);
        }

        let cache = read_agent_cache(&path)?;
        Ok(cache
            .get(&cache_key(call_path, input_hash))
            .map(WorkflowAgentCacheRecord::from))
    }
}

pub trait IntoWorkflowAgentRecord {
    fn into_workflow_agent_record(self) -> WorkflowAgentRecord;
}

impl IntoWorkflowAgentRecord for WorkflowAgentRecord {
    fn into_workflow_agent_record(self) -> WorkflowAgentRecord {
        self
    }
}

impl IntoWorkflowAgentRecord for WorkflowAgentCacheRecord {
    fn into_workflow_agent_record(self) -> WorkflowAgentRecord {
        WorkflowAgentRecord {
            call_id: self.call_path.clone(),
            call_path: self.call_path,
            prompt: String::new(),
            opts: Value::Null,
            input_hash: self.input_hash,
            status: WorkflowAgentStatus::Completed,
            output: Some(self.output.to_string()),
            error: None,
            transcript_path: None,
        }
    }
}

impl From<&WorkflowAgentRecord> for WorkflowAgentCacheRecord {
    fn from(record: &WorkflowAgentRecord) -> Self {
        let output = record
            .output
            .as_deref()
            .and_then(|value| serde_json::from_str(value).ok())
            .unwrap_or(Value::Null);
        Self {
            call_path: record.call_path.clone(),
            input_hash: record.input_hash.clone(),
            output,
        }
    }
}

pub fn input_hash(prompt: &str, opts: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        serde_json::to_string(opts)
            .unwrap_or_else(|_| "null".to_string())
            .as_bytes(),
    );
    format!("{:x}", hasher.finalize())
}

fn cache_key(call_path: &str, input_hash: &str) -> String {
    format!("{call_path}:{input_hash}")
}

fn read_agent_cache(path: &Path) -> io::Result<HashMap<String, WorkflowAgentRecord>> {
    let content = fs::read_to_string(path)?;
    let cache_file: WorkflowAgentCacheFile = serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(match cache_file {
        WorkflowAgentCacheFile::Current(cache) => cache,
        WorkflowAgentCacheFile::Legacy(cache) => cache
            .into_iter()
            .map(|(key, record)| (key, record.into_workflow_agent_record()))
            .collect(),
    })
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(path, content)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
