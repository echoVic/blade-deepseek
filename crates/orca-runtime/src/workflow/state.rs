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
    #[serde(default, deserialize_with = "deserialize_optional_agent_output")]
    pub output: Option<Value>,
    pub error: Option<String>,
    pub transcript_path: Option<String>,
}

#[derive(Clone, Debug)]
struct CachedWorkflowAgentRecord {
    record: WorkflowAgentRecord,
    output_present: bool,
}

impl CachedWorkflowAgentRecord {
    fn from_record(record: WorkflowAgentRecord) -> Self {
        let output_present = record.output.is_some();
        Self {
            record,
            output_present,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowAgentRecordOnDisk {
    call_id: String,
    call_path: String,
    prompt: String,
    opts: Value,
    input_hash: String,
    status: WorkflowAgentStatus,
    #[serde(default, deserialize_with = "deserialize_agent_output_field")]
    output: AgentOutputField,
    error: Option<String>,
    transcript_path: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowAgentRecordOnDiskWritable {
    call_id: String,
    call_path: String,
    prompt: String,
    opts: Value,
    input_hash: String,
    status: WorkflowAgentStatus,
    #[serde(flatten)]
    output: AgentOutputField,
    error: Option<String>,
    transcript_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum WorkflowAgentCacheFileOnDisk {
    Current(HashMap<String, WorkflowAgentRecordOnDisk>),
    Legacy(HashMap<String, WorkflowAgentCacheRecord>),
}

#[derive(Clone, Debug, Default)]
struct AgentOutputField {
    present: bool,
    value: Option<Value>,
}

impl Serialize for AgentOutputField {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(if self.present { Some(1) } else { Some(0) })?;
        if self.present {
            map.serialize_entry("output", &self.value)?;
        }
        map.end()
    }
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
        cache.insert(
            cache_key(&record.call_path, &record.input_hash),
            CachedWorkflowAgentRecord::from_record(record),
        );
        write_agent_cache(&path, &cache)
    }

    pub fn find_cached_agent(
        &self,
        run_id: &str,
        call_path: &str,
        input_hash: &str,
    ) -> Option<String> {
        self.find_cached_agent_value(run_id, call_path, input_hash)
            .map(|value| value_to_output_string(value))
    }

    pub fn find_cached_agent_value(
        &self,
        run_id: &str,
        call_path: &str,
        input_hash: &str,
    ) -> Option<Value> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return None;
        }

        let cache = read_agent_cache(&path).ok()?;
        cache
            .get(&cache_key(call_path, input_hash))
            .filter(|entry| entry.record.status == WorkflowAgentStatus::Completed)
            .filter(|entry| entry.output_present)
            .map(|entry| entry.record.output.clone().unwrap_or(Value::Null))
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
            .filter(|entry| entry.record.status == WorkflowAgentStatus::Completed)
            .filter(|entry| entry.output_present)
            .map(|entry| WorkflowAgentCacheRecord {
                call_path: entry.record.call_path.clone(),
                input_hash: entry.record.input_hash.clone(),
                output: entry.record.output.clone().unwrap_or(Value::Null),
            }))
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
            output: Some(self.output),
            error: None,
            transcript_path: None,
        }
    }
}

impl From<&WorkflowAgentRecord> for WorkflowAgentCacheRecord {
    fn from(record: &WorkflowAgentRecord) -> Self {
        Self {
            call_path: record.call_path.clone(),
            input_hash: record.input_hash.clone(),
            output: record.output.clone().unwrap_or(Value::Null),
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

fn value_to_output_string(value: Value) -> String {
    match value {
        Value::String(output) => output,
        other => other.to_string(),
    }
}

fn deserialize_optional_agent_output<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum AgentOutputCompat {
        Value(Value),
        String(String),
    }

    Ok(match Option::<AgentOutputCompat>::deserialize(deserializer)? {
        Some(AgentOutputCompat::Value(value)) => Some(value),
        Some(AgentOutputCompat::String(output)) => Some(Value::String(output)),
        None => None,
    })
}

fn deserialize_agent_output_field<'de, D>(deserializer: D) -> Result<AgentOutputField, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(AgentOutputField {
        present: true,
        value: deserialize_optional_agent_output(deserializer)?,
    })
}

fn read_agent_cache(path: &Path) -> io::Result<HashMap<String, CachedWorkflowAgentRecord>> {
    let content = fs::read_to_string(path)?;
    let cache_file: WorkflowAgentCacheFileOnDisk = serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(match cache_file {
        WorkflowAgentCacheFileOnDisk::Current(cache) => cache
            .into_iter()
            .map(|(key, record)| {
                (
                    key,
                    CachedWorkflowAgentRecord {
                        record: WorkflowAgentRecord {
                            call_id: record.call_id,
                            call_path: record.call_path,
                            prompt: record.prompt,
                            opts: record.opts,
                            input_hash: record.input_hash,
                            status: record.status,
                            output: record.output.value,
                            error: record.error,
                            transcript_path: record.transcript_path,
                        },
                        output_present: record.output.present,
                    },
                )
            })
            .collect(),
        WorkflowAgentCacheFileOnDisk::Legacy(cache) => cache
            .into_iter()
            .map(|(key, record)| {
                (
                    key,
                    CachedWorkflowAgentRecord {
                        record: record.into_workflow_agent_record(),
                        output_present: true,
                    },
                )
            })
            .collect(),
    })
}

fn write_agent_cache(
    path: &Path,
    cache: &HashMap<String, CachedWorkflowAgentRecord>,
) -> io::Result<()> {
    let on_disk: HashMap<String, WorkflowAgentRecordOnDiskWritable> = cache
        .iter()
        .map(|(key, entry)| {
            (
                key.clone(),
                WorkflowAgentRecordOnDiskWritable {
                    call_id: entry.record.call_id.clone(),
                    call_path: entry.record.call_path.clone(),
                    prompt: entry.record.prompt.clone(),
                    opts: entry.record.opts.clone(),
                    input_hash: entry.record.input_hash.clone(),
                    status: entry.record.status,
                    output: AgentOutputField {
                        present: entry.output_present,
                        value: entry.record.output.clone(),
                    },
                    error: entry.record.error.clone(),
                    transcript_path: entry.record.transcript_path.clone(),
                },
            )
        })
        .collect();
    write_json_pretty(path, &on_disk)
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
