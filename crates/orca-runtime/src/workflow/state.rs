use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orca_core::cost_types::UsageTotals;
use orca_core::task_types::WorkflowAgentTaskSummary;
use orca_core::workflow_types::{WorkflowAgentStatus, WorkflowInput, WorkflowRunState};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub input_hash: String,
    pub status: WorkflowAgentStatus,
    #[serde(default = "default_agent_attempt")]
    pub attempt: u32,
    #[serde(default = "default_agent_attempt")]
    pub max_attempts: u32,
    #[serde(default)]
    pub previous_errors: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional_agent_output")]
    pub output: Option<Value>,
    pub error: Option<String>,
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageTotals>,
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
    #[serde(default)]
    team: Option<String>,
    input_hash: String,
    status: WorkflowAgentStatus,
    #[serde(default = "default_agent_attempt")]
    attempt: u32,
    #[serde(default = "default_agent_attempt")]
    max_attempts: u32,
    #[serde(default)]
    previous_errors: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_agent_output_field")]
    output: AgentOutputField,
    error: Option<String>,
    transcript_path: Option<String>,
    #[serde(default)]
    usage: Option<UsageTotals>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowAgentRecordOnDiskWritable {
    call_id: String,
    call_path: String,
    prompt: String,
    opts: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<String>,
    input_hash: String,
    status: WorkflowAgentStatus,
    attempt: u32,
    max_attempts: u32,
    previous_errors: Vec<String>,
    #[serde(flatten)]
    output: AgentOutputField,
    error: Option<String>,
    transcript_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageTotals>,
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
    agent_cache_write_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWorkerRecord {
    pub pid: u32,
    pub active: bool,
    pub started_at_ms: i64,
    pub completed_at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkflowAgentStatusCounts {
    pub completed: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub cached: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowStopRequest {
    stop_requested: bool,
    requested_at_ms: i64,
}

impl WorkflowStateStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            agent_cache_write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.join(run_id)
    }

    pub fn transcript_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("transcripts")
    }

    pub fn state_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("state.json")
    }

    pub fn launch_input_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("launch-input.json")
    }

    pub fn worker_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("worker.json")
    }

    pub fn stop_request_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("control.json")
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
        write_json_pretty(&self.state_path(&state.run_id), state)
    }

    pub fn load_state(&self, run_id: &str) -> io::Result<WorkflowRunState> {
        read_json(&self.state_path(run_id))
    }

    pub fn write_launch_input(&self, run_id: &str, input: &WorkflowInput) -> io::Result<()> {
        write_json_pretty(&self.launch_input_path(run_id), input)
    }

    pub fn load_launch_input(&self, run_id: &str) -> io::Result<WorkflowInput> {
        read_json(&self.launch_input_path(run_id))
    }

    pub fn write_worker_record(
        &self,
        run_id: &str,
        worker: &WorkflowWorkerRecord,
    ) -> io::Result<()> {
        write_json_pretty(&self.worker_path(run_id), worker)
    }

    pub fn load_worker_record(&self, run_id: &str) -> io::Result<WorkflowWorkerRecord> {
        read_json(&self.worker_path(run_id))
    }

    pub fn mark_worker_exited(&self, run_id: &str) -> io::Result<()> {
        let mut worker = self.load_worker_record(run_id)?;
        worker.active = false;
        worker.completed_at_ms = Some(now_ms());
        self.write_worker_record(run_id, &worker)
    }

    pub fn request_stop(&self, run_id: &str) -> io::Result<()> {
        write_json_pretty(
            &self.stop_request_path(run_id),
            &WorkflowStopRequest {
                stop_requested: true,
                requested_at_ms: now_ms(),
            },
        )
    }

    pub fn stop_requested(&self, run_id: &str) -> io::Result<bool> {
        let path = self.stop_request_path(run_id);
        if !path.exists() {
            return Ok(false);
        }
        let request: WorkflowStopRequest = read_json(&path)?;
        Ok(request.stop_requested)
    }

    pub fn record_agent_completed(
        &self,
        run_id: &str,
        record: impl IntoWorkflowAgentRecord,
    ) -> io::Result<()> {
        let _lock = self
            .agent_cache_write_lock
            .lock()
            .map_err(|_| io::Error::other("workflow agent cache lock poisoned"))?;
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

    pub fn agent_status_counts(&self, run_id: &str) -> io::Result<WorkflowAgentStatusCounts> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return Ok(WorkflowAgentStatusCounts::default());
        }

        let cache = read_agent_cache(&path)?;
        let mut counts = WorkflowAgentStatusCounts::default();
        for entry in cache.values() {
            match entry.record.status {
                WorkflowAgentStatus::Completed => {
                    counts.completed += 1;
                    counts.failed += entry.record.previous_errors.len() as u32;
                }
                WorkflowAgentStatus::Failed => {
                    counts.failed += entry.record.previous_errors.len() as u32 + 1;
                }
                WorkflowAgentStatus::Cancelled => counts.cancelled += 1,
                WorkflowAgentStatus::Cached => counts.cached += 1,
                WorkflowAgentStatus::Pending | WorkflowAgentStatus::Running => {}
            }
        }
        Ok(counts)
    }

    pub fn agent_summaries(&self, run_id: &str) -> io::Result<Vec<WorkflowAgentTaskSummary>> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return Ok(Vec::new());
        }

        let cache = read_agent_cache(&path)?;
        let mut summaries = cache
            .values()
            .map(|entry| WorkflowAgentTaskSummary {
                call_id: entry.record.call_id.clone(),
                call_path: entry.record.call_path.clone(),
                team: entry
                    .record
                    .team
                    .clone()
                    .or_else(|| workflow_agent_team(&entry.record.opts)),
                status: entry.record.status,
                attempt: entry.record.attempt,
                max_attempts: entry.record.max_attempts,
                previous_errors: entry.record.previous_errors.clone(),
                error: entry.record.error.clone(),
                transcript_path: entry.record.transcript_path.clone(),
                usage: entry.record.usage,
            })
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| {
            left.call_path
                .cmp(&right.call_path)
                .then_with(|| left.call_id.cmp(&right.call_id))
        });
        Ok(summaries)
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn default_agent_attempt() -> u32 {
    1
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
            team: None,
            input_hash: self.input_hash,
            status: WorkflowAgentStatus::Completed,
            attempt: 1,
            max_attempts: 1,
            previous_errors: Vec::new(),
            output: Some(self.output),
            error: None,
            transcript_path: None,
            usage: None,
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

    Ok(
        match Option::<AgentOutputCompat>::deserialize(deserializer)? {
            Some(AgentOutputCompat::Value(value)) => Some(value),
            Some(AgentOutputCompat::String(output)) => Some(Value::String(output)),
            None => None,
        },
    )
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
                            team: record.team,
                            input_hash: record.input_hash,
                            status: record.status,
                            attempt: record.attempt,
                            max_attempts: record.max_attempts,
                            previous_errors: record.previous_errors,
                            output: record.output.value,
                            error: record.error,
                            transcript_path: record.transcript_path,
                            usage: record.usage,
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
                    team: entry
                        .record
                        .team
                        .clone()
                        .or_else(|| workflow_agent_team(&entry.record.opts)),
                    input_hash: entry.record.input_hash.clone(),
                    status: entry.record.status,
                    attempt: entry.record.attempt,
                    max_attempts: entry.record.max_attempts,
                    previous_errors: entry.record.previous_errors.clone(),
                    output: AgentOutputField {
                        present: entry.output_present,
                        value: entry.record.output.clone(),
                    },
                    error: entry.record.error.clone(),
                    transcript_path: entry.record.transcript_path.clone(),
                    usage: entry.record.usage,
                },
            )
        })
        .collect();
    write_json_pretty(path, &on_disk)
}

pub fn workflow_agent_team(opts: &Value) -> Option<String> {
    opts.get("team")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|team| !team.is_empty())
        .map(str::to_string)
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
        .unwrap_or("state");
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
