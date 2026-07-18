//! Real DeepSeek regression for RuntimeHost-owned Goal Mode tools.
//!
//! Makes billed API calls. The caller should provide an isolated ORCA_HOME
//! containing auth.json, or set DEEPSEEK_API_KEY directly.

use std::collections::HashMap;
use std::error::Error;
use std::io;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, ReasoningEffort, RunConfig,
    ThemeName, ToolConfig, WorkflowConfig,
};
use orca_core::conversation::Message;
use orca_core::event_schema::RunStatus;
use orca_core::goal_types::ThreadGoalStatus;
use orca_core::model::{FLASH_MODEL, ModelSelection};
use orca_core::subagent_config::SubagentConfig;
use orca_runtime::agent_common::format_goal_mode_instructions;
use orca_runtime::goals::GoalStore;
use orca_runtime::runtime_host::{
    HostedTurnRequest, OperationOutcome, RuntimeHost, RuntimeThreadMutation,
};

const OBJECTIVE: &str = "Inspect the current runtime task list once, then mark this goal complete.";
const PROMPT: &str = "Complete the active goal now. Make exactly two sequential tool calls and no others: first call task_list with {}, wait for its result, then call update_goal with {\"status\":\"complete\"}. Do not answer in prose before the tool calls.";

fn parse_max_budget() -> Result<f64, String> {
    let mut args = std::env::args().skip(1);
    let mut max_budget = 0.02;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--max-budget" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--max-budget requires a value".to_string())?;
                max_budget = value
                    .parse::<f64>()
                    .map_err(|error| format!("invalid --max-budget value '{value}': {error}"))?;
            }
            _ => return Err(format!("unknown argument: {arg}")),
        }
    }
    if !max_budget.is_finite() || max_budget <= 0.0 {
        return Err("--max-budget must be a positive finite number".to_string());
    }
    Ok(max_budget)
}

fn load_api_key() -> Option<String> {
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY")
        && !key.is_empty()
    {
        return Some(key);
    }
    let home = std::env::var_os("ORCA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))?;
    let content = std::fs::read_to_string(home.join("auth.json")).ok()?;
    let auth: HashMap<String, String> = serde_json::from_str(&content).ok()?;
    auth.get("DEEPSEEK_API_KEY")
        .filter(|key| !key.is_empty())
        .cloned()
}

fn real_api_config(api_key: String, max_budget_usd: f64) -> Result<RunConfig, String> {
    Ok(RunConfig {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        prompt: String::new(),
        cwd: Some(std::env::current_dir().map_err(|error| error.to_string())?),
        output_format: OutputFormat::Jsonl,
        approval_mode: ApprovalMode::FullAuto,
        provider: ProviderKind::DeepSeek,
        verifier: None,
        model: ModelSelection::parse(Some(FLASH_MODEL.to_string()))?,
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: ReasoningEffort::Max,
        api_key: Some(api_key),
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Record,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: HashMap::new(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        max_budget_usd: Some(max_budget_usd),
        subagents: SubagentConfig::default(),
        tools: ToolConfig::default(),
        workflows: WorkflowConfig::default(),
        theme: ThemeName::Dark,
        vim_mode: false,
        update_check: false,
        desktop_notifications: false,
        auto_memory: false,
    })
}

fn fail(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::other(message.into()))
}

fn main() -> Result<(), Box<dyn Error>> {
    let max_budget = parse_max_budget().map_err(fail)?;
    let api_key = load_api_key().ok_or_else(|| {
        fail("DEEPSEEK_API_KEY not found in the environment or ORCA_HOME/auth.json")
    })?;
    let config = real_api_config(api_key, max_budget).map_err(fail)?;
    let host = RuntimeHost::start().map_err(|error| fail(error.to_string()))?;
    let thread = host
        .start_thread(config, "Goal Mode real API control-plane regression")
        .map_err(|error| fail(error.to_string()))?;
    let session_id = thread
        .session_id()
        .ok_or_else(|| fail("recorded RuntimeHost thread did not expose a session id"))?
        .to_string();

    let mut store = GoalStore::load_default();
    let active_goal = store.replace(&session_id, OBJECTIVE, ThreadGoalStatus::Active, None)?;
    thread
        .mutate(RuntimeThreadMutation::ReplaceGoalContext(Some(
            format_goal_mode_instructions(&active_goal),
        )))
        .map_err(|error| fail(error.to_string()))?;

    let operation = thread
        .start_turn(
            HostedTurnRequest::new(PROMPT)
                .with_goal_tools(true)
                .with_goal_usage_tracking(true),
            io::sink(),
        )
        .map_err(|error| fail(error.to_string()))?;
    let terminal = operation.wait();
    if terminal.outcome() != &OperationOutcome::Completed(RunStatus::Success) {
        return Err(fail(format!(
            "Goal Mode operation did not succeed: {:?}",
            terminal.outcome()
        )));
    }

    let snapshot = thread.snapshot().map_err(|error| fail(error.to_string()))?;
    let tool_names = snapshot
        .messages()
        .iter()
        .filter_map(|message| match message {
            Message::Assistant { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flatten()
        .map(|call| call.function_name.as_str())
        .collect::<Vec<_>>();
    let task_list_index = tool_names
        .iter()
        .position(|name| *name == "task_list")
        .ok_or_else(|| fail(format!("DeepSeek did not call task_list: {tool_names:?}")))?;
    let update_goal_indexes = tool_names
        .iter()
        .enumerate()
        .filter_map(|(index, name)| (*name == "update_goal").then_some(index))
        .collect::<Vec<_>>();
    if update_goal_indexes.len() != 1 {
        return Err(fail(format!(
            "expected exactly one update_goal call, got {}: {tool_names:?}",
            update_goal_indexes.len()
        )));
    }
    if update_goal_indexes[0] <= task_list_index {
        return Err(fail(format!(
            "update_goal did not follow task_list: {tool_names:?}"
        )));
    }
    let non_goal_tools = tool_names
        .iter()
        .filter(|name| !matches!(**name, "get_goal" | "create_goal" | "update_goal"))
        .count();
    if non_goal_tools == 0 {
        return Err(fail(format!(
            "Goal Mode did not execute a non-goal tool: {tool_names:?}"
        )));
    }

    let persisted = store
        .get(&session_id)?
        .ok_or_else(|| fail("persisted goal disappeared after the hosted turn"))?;
    if persisted.status != ThreadGoalStatus::Complete {
        return Err(fail(format!(
            "persisted goal status is {:?}, expected Complete; tools={tool_names:?}",
            persisted.status
        )));
    }
    let continuations = usize::from(persisted.status.should_continue());
    if continuations != 0 {
        return Err(fail(
            "completed goal remained eligible for automatic continuation",
        ));
    }

    thread.shutdown().map_err(|error| fail(error.to_string()))?;
    host.shutdown().map_err(|error| fail(error.to_string()))?;
    println!(
        "Goal Mode real API e2e verified: status=complete non_goal_tools={non_goal_tools} update_goal_calls=1 continuations={continuations}"
    );
    Ok(())
}
