use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::workflow_types::WorkflowRunState;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowAgentCacheRecord {
    pub call_path: String,
    pub input_hash: String,
    pub output: Value,
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
        record: WorkflowAgentCacheRecord,
    ) -> io::Result<()> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        let mut cache: HashMap<String, WorkflowAgentCacheRecord> = if path.exists() {
            read_json(&path)?
        } else {
            HashMap::new()
        };
        cache.insert(cache_key(&record.call_path, &record.input_hash), record);
        write_json_pretty(&path, &cache)
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

        let cache: HashMap<String, WorkflowAgentCacheRecord> = read_json(&path)?;
        Ok(cache.get(&cache_key(call_path, input_hash)).cloned())
    }
}

fn cache_key(call_path: &str, input_hash: &str) -> String {
    format!("{call_path}:{input_hash}")
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
