use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::tool_types;

use crate::agent_child::{ChildAgentExecutor, ChildAgentRequest, ChildAgentRuntime};
use crate::agent_loop::execute_child_agent_loop;
use crate::hooks::HookRunner;
use crate::instructions;
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskStatus};
use crate::memory;
use crate::subagent::{self, SubagentIsolation};
use crate::subagent_execution::{append_worktree_outcome, validate_subagent_output_schema};
use crate::tasks::TaskRegistry;
use crate::worktree::WorktreeGuard;

#[derive(Clone, Debug)]
pub struct AsyncSubagentWorktree {
    pub repo_root: PathBuf,
    pub path: PathBuf,
}

pub fn run_async_subagent_worker(
    config: RunConfig,
    cwd: PathBuf,
    child_cwd: PathBuf,
    task_session_id: String,
    agent_id: String,
    request: subagent::SubagentRequest,
    child_depth: u32,
    worktree: Option<AsyncSubagentWorktree>,
) -> i32 {
    run_async_subagent_worker_with_executor(
        config,
        cwd,
        child_cwd,
        task_session_id,
        agent_id,
        request,
        child_depth,
        worktree,
        execute_child_agent_loop,
    )
}

pub(crate) fn run_async_subagent_worker_with_executor(
    config: RunConfig,
    cwd: PathBuf,
    child_cwd: PathBuf,
    task_session_id: String,
    agent_id: String,
    request: subagent::SubagentRequest,
    child_depth: u32,
    worktree: Option<AsyncSubagentWorktree>,
    child_executor: ChildAgentExecutor<io::Sink>,
) -> i32 {
    let task_registry = TaskRegistry::new_for_cwd(task_session_id, &cwd);
    let _ = task_registry.mark_running(&agent_id);
    let instructions = instructions::load_for_cwd_or_default(&cwd);
    let memory = memory::load_for_cwd(&cwd);
    let hooks = HookRunner::new(config.hooks.clone());
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let cancel = CancelToken::new();
    let child_request = ChildAgentRequest {
        prompt: request.prompt,
        subagent_type: request.subagent_type,
        model: request.model,
        depth: child_depth,
        emit_deltas: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
    };
    let mut child_events = EventFactory::new(format!("subagent-{agent_id}"));
    let mut child_lifecycle = RuntimeSessionLifecycle::new(format!("subagent-{agent_id}"));
    child_lifecycle.start_task(RuntimeTaskKind::Subagent);
    let mut child_sink = EventSink::new(io::sink(), config.output_format);
    let (child, child_cost_tracker) = {
        let mut child_runtime = ChildAgentRuntime::new(
            &child_cwd,
            &mut child_events,
            &mut child_sink,
            &instructions,
            &memory,
            &mcp_registry,
            &hooks,
            &cancel,
            Some(&mut child_lifecycle),
            child_executor,
        );
        crate::agent_child::run_child_agent(&config, &child_request, &mut child_runtime)
    };
    let completed_task = child_lifecycle
        .finish_task(child.status)
        .cloned()
        .unwrap_or_else(|| {
            child_lifecycle.active_task().cloned().unwrap_or_else(|| {
                RuntimeSessionLifecycle::new(format!("subagent-{agent_id}"))
                    .start_task(RuntimeTaskKind::Subagent)
                    .clone()
            })
        });
    let worktree = worktree.and_then(|worktree| {
        WorktreeGuard::finish_existing(worktree.repo_root, worktree.path).ok()
    });
    let usage = usage_totals_if_non_empty(child_cost_tracker.totals());
    if child.status == RunStatus::Success {
        let mut output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        if let Err(mut error) =
            validate_subagent_output_schema(&request.description, request.schema.as_ref(), &output)
        {
            append_worktree_outcome(&mut error, worktree.as_ref());
            let failed_task = completed_task.with_status(RuntimeTaskStatus::Failed);
            let error = async_subagent_result_payload(error, Some(failed_task.payload()));
            if task_registry
                .fail_with_usage(&agent_id, error, usage)
                .is_ok()
            {
                return 1;
            }
            return 1;
        }
        append_worktree_outcome(&mut output, worktree.as_ref());
        let output = async_subagent_result_payload(output, Some(completed_task.payload()));
        if task_registry
            .complete_with_usage(&agent_id, output, usage)
            .is_ok()
        {
            return 0;
        }
    } else {
        let mut error = child
            .error
            .or(child.final_message)
            .unwrap_or_else(|| format!("subagent ended with status {:?}", child.status));
        append_worktree_outcome(&mut error, worktree.as_ref());
        let error = async_subagent_result_payload(error, Some(completed_task.payload()));
        if task_registry
            .fail_with_usage(&agent_id, error, usage)
            .is_ok()
        {
            return 1;
        }
    }
    1
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn launch_async_subagent(
    config: &RunConfig,
    cwd: &Path,
    tool_request: &tool_types::ToolRequest,
    request: subagent::SubagentRequest,
    subagent_depth: u32,
    task_registry: &TaskRegistry,
) -> tool_types::ToolResult {
    let agent_type = serde_json::to_value(&request.subagent_type)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string));
    let task = task_registry.create_subagent(request.description.clone(), agent_type);
    let agent_id = task.id.clone();
    let worktree_guard = if request.isolation == SubagentIsolation::Worktree {
        match WorktreeGuard::create(cwd) {
            Ok(guard) => Some(guard),
            Err(error) => {
                let error = format!("failed to create subagent worktree: {error}");
                let _ = task_registry.fail(&agent_id, error.clone());
                return tool_types::ToolResult::failed(tool_request, error, None);
            }
        }
    } else {
        None
    };
    let child_cwd = worktree_guard
        .as_ref()
        .map(|guard| guard.path().to_path_buf())
        .unwrap_or_else(|| cwd.to_path_buf());
    let worktree = worktree_guard.as_ref().map(|guard| AsyncSubagentWorktree {
        repo_root: guard.repo_root().to_path_buf(),
        path: guard.path().to_path_buf(),
    });
    if let Err(error) = task_registry.mark_worker_spawned(&agent_id, 0) {
        let _ = task_registry.fail(&agent_id, error.clone());
        return tool_types::ToolResult::failed(tool_request, error, None);
    }
    match spawn_async_subagent_worker(
        config,
        cwd,
        &child_cwd,
        task_registry.session_id(),
        &agent_id,
        &request,
        subagent_depth + 1,
        worktree.as_ref(),
    ) {
        Ok(pid) => {
            let _ = task_registry.mark_worker_spawned(&agent_id, pid);
            std::mem::forget(worktree_guard);
        }
        Err(error) => {
            let worktree = worktree_guard.and_then(|guard| guard.finish().ok());
            let mut error = format!("failed to start async subagent worker: {error}");
            append_worktree_outcome(&mut error, worktree.as_ref());
            let _ = task_registry.fail(&agent_id, error.clone());
            return tool_types::ToolResult::failed(tool_request, error, None);
        }
    }

    let output = serde_json::json!({
        "status": "async_launched",
        "agent_id": agent_id,
        "description": request.description,
    })
    .to_string();
    tool_types::ToolResult::completed(tool_request, output, false)
}

#[allow(clippy::too_many_arguments)]
fn spawn_async_subagent_worker(
    config: &RunConfig,
    cwd: &Path,
    child_cwd: &Path,
    task_session_id: &str,
    agent_id: &str,
    request: &subagent::SubagentRequest,
    child_depth: u32,
    worktree: Option<&AsyncSubagentWorktree>,
) -> Result<u32, String> {
    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let request_json = serde_json::to_string(request).map_err(|error| error.to_string())?;
    let mut command = ProcessCommand::new(current_exe);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .arg("subagent-worker")
        .arg("--cwd")
        .arg(cwd)
        .arg("--child-cwd")
        .arg(child_cwd)
        .arg("--provider")
        .arg(config.provider.as_str())
        .arg("--session-id")
        .arg(task_session_id)
        .arg("--agent-id")
        .arg(agent_id)
        .arg("--subagent-depth")
        .arg(child_depth.to_string())
        .arg("--request-json")
        .arg(request_json);
    if let Some(model) = config.model.as_history_value() {
        command.arg("--model").arg(model);
    }
    if let Some(api_key) = config.api_key.as_deref() {
        command.arg("--api-key").arg(api_key);
    }
    if let Some(base_url) = config.base_url.as_deref() {
        command.arg("--base-url").arg(base_url);
    }
    if let Some(worktree) = worktree {
        command
            .arg("--worktree-repo-root")
            .arg(&worktree.repo_root)
            .arg("--worktree-path")
            .arg(&worktree.path);
    }
    command
        .spawn()
        .map(|child| child.id())
        .map_err(|error| error.to_string())
}

pub(crate) fn usage_totals_if_non_empty(usage: UsageTotals) -> Option<UsageTotals> {
    if usage.total_tokens() == 0 && usage.cache_tokens == 0 && usage.estimated_cost_usd == 0.0 {
        None
    } else {
        Some(usage)
    }
}

pub(crate) fn async_subagent_result_payload(
    output: String,
    task: Option<serde_json::Value>,
) -> String {
    serde_json::json!({
        "output": output,
        "task": task,
    })
    .to_string()
}
