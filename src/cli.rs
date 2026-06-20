use std::env;
use std::fs;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::process::Stdio;
use std::time::SystemTime;

use clap::{Parser, Subcommand, ValueEnum};
use orca_core::workflow_types::{WorkflowInput, WorkflowRunState};
use orca_runtime::tasks::TaskRegistry;
use orca_runtime::workflow::state::WorkflowStateStore;
use orca_runtime::workflow::{WorkflowLaunchRequest, WorkflowRunner};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::approval::policy::ApprovalMode;
use crate::config::file;
use crate::config::file::ConfigOverrides;
use crate::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};
use crate::model::ModelSelection;
use crate::runtime::controller;
use crate::runtime::history;
use crate::tui::app;

#[derive(Debug, Parser)]
#[command(name = "orca")]
#[command(version)]
#[command(about = "A DeepSeek-native coding agent runtime by Blade.")]
pub struct Cli {
    /// Resume a saved conversation in TUI mode by ID, prefix, or 'latest'.
    #[arg(long)]
    resume: Option<String>,

    /// Fork a saved conversation in TUI mode by ID, prefix, or 'latest'.
    #[arg(long, alias = "fork-session")]
    fork: Option<String>,

    /// Continue the latest saved conversation in TUI mode.
    #[arg(long = "continue", alias = "last")]
    continue_latest: bool,

    /// Show the TUI session picker at startup.
    #[arg(long)]
    session_picker: bool,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(long)]
    model: Option<String>,

    /// Approval mode to use, or 'server' to start stdin/stdout JSON-RPC mode.
    #[arg(long = "mode", alias = "approval-mode")]
    mode: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    #[command(subcommand)]
    command: Option<Command>,

    /// Prompt to run in the default interactive placeholder.
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a task and emit events.
    Exec(ExecArgs),
    /// Inspect saved conversation history.
    History(HistoryArgs),
    /// Run and inspect local workflows.
    Workflow(WorkflowArgs),
}

#[derive(Debug, Parser)]
struct ExecArgs {
    /// Output format: text (human-readable) or jsonl (machine-readable).
    #[arg(long, value_enum, default_value_t = OutputFormatArg::Text)]
    output_format: OutputFormatArg,

    /// Workspace directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Approval policy for tool actions.
    #[arg(long = "mode", alias = "approval-mode", value_enum)]
    approval_mode: Option<ApprovalMode>,

    /// Model to use (overrides config file and DEEPSEEK_MODEL env).
    #[arg(long)]
    model: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// API base URL (overrides config file and DEEPSEEK_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Optional verifier command to run after completion.
    #[arg(long)]
    verifier: Option<String>,

    /// Maximum estimated USD budget for this run.
    #[arg(long)]
    max_budget: Option<f64>,

    /// Resume a saved history session by ID, prefix, or 'latest'.
    #[arg(long)]
    resume: Option<String>,

    /// Fork a saved history session by ID, prefix, or 'latest'.
    #[arg(long, alias = "fork-session")]
    fork: Option<String>,

    /// Continue from the latest saved conversation.
    #[arg(long = "continue", alias = "last")]
    continue_latest: bool,

    /// Do not write this run to local history.
    #[arg(long)]
    no_history: bool,

    /// Write local history even when using machine-readable jsonl output.
    #[arg(long)]
    save_history: bool,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    /// Prompt to execute.
    prompt: Vec<String>,
}

#[derive(Debug, Parser)]
struct HistoryArgs {
    #[command(subcommand)]
    command: HistoryCommand,
}

#[derive(Debug, Parser)]
struct WorkflowArgs {
    #[command(subcommand)]
    command: WorkflowCommand,
}

#[derive(Debug, Subcommand)]
enum WorkflowCommand {
    /// Launch a workflow script or named workflow.
    Run(WorkflowRunArgs),
    /// List persisted workflow runs for the current project.
    List,
    /// Show a persisted workflow run by task id.
    Show { task_id: String },
    /// Request stop for a workflow task.
    Stop { task_id: String },
    /// Resume a workflow run from a prior run id.
    Resume { run_id: String },
    #[command(hide = true)]
    Worker(WorkflowWorkerArgs),
}

#[derive(Debug, Parser)]
struct WorkflowRunArgs {
    /// Workspace directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(long)]
    model: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Workflow arguments as JSON.
    #[arg(long)]
    args: Option<String>,

    /// Resume cached agent calls from a prior workflow run id.
    #[arg(long = "resume-from-run-id")]
    resume_from_run_id: Option<String>,

    /// Workflow script path or named workflow.
    script_or_name: String,
}

#[derive(Debug, Parser)]
struct WorkflowWorkerArgs {
    /// Workspace directory.
    #[arg(long)]
    cwd: PathBuf,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(long)]
    model: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Persisted workflow session identifier.
    #[arg(long)]
    session_id: String,

    /// Full workflow input payload as JSON.
    #[arg(long)]
    input_json: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowListEntry {
    task_id: String,
    run_id: String,
    session_id: String,
    workflow_name: String,
    status: orca_core::workflow_types::WorkflowRunStatus,
    cwd: String,
    total_agent_count: u32,
    final_summary: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowShowEntry {
    #[serde(flatten)]
    state: WorkflowRunState,
    session_id: String,
    run_dir: String,
    transcript_dir: String,
}

#[derive(Debug)]
struct PersistedWorkflowRun {
    session_id: String,
    state: WorkflowRunState,
    run_dir: PathBuf,
    state_mtime: Option<SystemTime>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowCliLaunchRecord {
    cwd: String,
    provider: ProviderKind,
    model: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
    input: WorkflowInput,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowStopResponse {
    status: &'static str,
    task_id: String,
    run_id: String,
}

#[derive(Debug, Subcommand)]
enum HistoryCommand {
    /// List saved conversation sessions, newest first.
    List {
        /// Maximum number of sessions to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,

        /// Include archived sessions.
        #[arg(long)]
        all: bool,
    },
    /// Show a saved conversation transcript.
    Show {
        /// Session ID, prefix, or 'latest'.
        session: String,
    },
    /// Archive an active conversation transcript.
    Archive {
        /// Session ID, prefix, or 'latest'.
        session: String,
    },
    /// Delete a saved or archived conversation transcript.
    Delete {
        /// Session ID, prefix, or 'latest'.
        session: String,
    },
    /// Rename a conversation transcript.
    Rename {
        /// Session ID, prefix, or 'latest'.
        session: String,
        /// New title.
        title: String,
    },
    /// Search saved conversation transcripts.
    Search {
        /// Text to search for.
        query: String,
        /// Include archived sessions.
        #[arg(long)]
        all: bool,
    },
    /// Compress a transcript with zstd.
    Compress {
        /// Session ID, prefix, or 'latest'.
        session: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum OutputFormatArg {
    Jsonl,
    Text,
}

impl From<OutputFormatArg> for OutputFormat {
    fn from(value: OutputFormatArg) -> Self {
        match value {
            OutputFormatArg::Jsonl => OutputFormat::Jsonl,
            OutputFormatArg::Text => OutputFormat::Text,
        }
    }
}

pub fn run() -> i32 {
    let cli = Cli::parse();

    if matches!(cli.mode.as_deref(), Some("server")) {
        return run_server(cli);
    }

    match cli.command {
        Some(Command::Exec(args)) => run_exec(args),
        Some(Command::History(args)) => run_history(args),
        Some(Command::Workflow(args)) => run_workflow(args),
        None => run_placeholder(cli),
    }
}

fn load_effective_file_config(
    cwd: &std::path::Path,
    cli: ConfigOverrides,
) -> Result<file::FileConfig, String> {
    let file_config = file::load_layered_config(cwd);
    let env = env_overrides()?;
    Ok(file::apply_override_layers(file_config, env, cli))
}

fn env_overrides() -> Result<ConfigOverrides, String> {
    Ok(ConfigOverrides {
        model: env::var("ORCA_MODEL")
            .ok()
            .or_else(|| env::var("DEEPSEEK_MODEL").ok()),
        mode: match env::var("ORCA_MODE") {
            Ok(mode) => Some(parse_approval_mode_value(&mode)?),
            Err(_) => None,
        },
        api_key: env::var("ORCA_API_KEY")
            .ok()
            .or_else(|| env::var("DEEPSEEK_API_KEY").ok()),
        base_url: env::var("ORCA_BASE_URL")
            .ok()
            .or_else(|| env::var("DEEPSEEK_BASE_URL").ok()),
    })
}

fn parse_approval_mode_value(mode: &str) -> Result<ApprovalMode, String> {
    ApprovalMode::from_str(mode, true).map_err(|_| {
        format!("unsupported mode '{mode}'. Use suggest, auto-edit, full-auto, or plan")
    })
}

fn run_exec(args: ExecArgs) -> i32 {
    if args.no_history && (args.resume.is_some() || args.fork.is_some() || args.continue_latest) {
        eprintln!("orca: --resume/--fork/--continue cannot be combined with --no-history");
        return 1;
    }
    if args.no_history && args.save_history {
        eprintln!("orca: --save-history cannot be combined with --no-history");
        return 1;
    }
    let resume_like =
        args.resume.is_some() as u8 + args.fork.is_some() as u8 + args.continue_latest as u8;
    if resume_like > 1 {
        eprintln!("orca: --resume, --fork, and --continue are mutually exclusive");
        return 1;
    }

    let prompt = args.prompt.join(" ");
    let cwd_for_mentions = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let file_config = match load_effective_file_config(
        &cwd_for_mentions,
        ConfigOverrides {
            model: args.model,
            mode: args.approval_mode,
            api_key: args.api_key,
            base_url: args.base_url,
        },
    ) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    let prompt = match crate::mentions::expand_file_mentions(&prompt, &cwd_for_mentions) {
        Ok(prompt) => prompt,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let api_key = file_config.api_key;
    let base_url = file_config.base_url;

    let model = file_config.model;
    let model = match ModelSelection::parse(model) {
        Ok(model) => model,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let output_format = args.output_format;
    let fallback =
        if args.no_history || (output_format == OutputFormatArg::Jsonl && !args.save_history) {
            HistoryMode::Disabled
        } else {
            HistoryMode::Record
        };
    let history_mode = resolve_history_mode(args.resume, args.fork, args.continue_latest, fallback);

    let config = RunConfig {
        prompt,
        cwd: args.cwd,
        output_format: output_format.into(),
        approval_mode: file_config.mode.unwrap_or_default(),
        provider: args.provider,
        verifier: args.verifier,
        model,
        api_key,
        base_url,
        history_mode,
        show_session_picker: false,
        permission_rules: file_config.permissions,
        max_budget_usd: args.max_budget,
        mcp_servers: file_config.mcp_servers,
        hooks: file_config.hooks,
        external_tools: crate::tools::external::load_default_external_tools(),
        subagents: file_config.subagents.normalized(),
        tools: file_config.tools.normalized(),
        workflows: file_config.workflows.resolved(),
        theme: file_config.theme,
        vim_mode: file_config.vim_mode,
        update_check: file_config.update_check,
        desktop_notifications: file_config.desktop_notifications,
        auto_memory: file_config.auto_memory,
    };

    controller::run(config)
}

fn run_history(args: HistoryArgs) -> i32 {
    match args.command {
        HistoryCommand::List { limit, all } => {
            match history::list_sessions_with_archived(limit, all) {
                Ok(sessions) => {
                    for session in sessions {
                        let model = session.model.as_deref().unwrap_or("-");
                        let state = if session.archived {
                            "archived"
                        } else {
                            "active"
                        };
                        println!(
                            "{}\t{}\t{}\t{}\t{}\t{}",
                            session.session_id,
                            session.updated_at.to_rfc3339(),
                            state,
                            session.provider,
                            model,
                            session.title
                        );
                    }
                    0
                }
                Err(error) => {
                    eprintln!("orca: failed to list history: {error}");
                    1
                }
            }
        }
        HistoryCommand::Show { session } => match history::load_session(&session) {
            Ok(transcript) => {
                println!("Session: {}", transcript.meta.session_id);
                println!("Title: {}", transcript.meta.title);
                println!("Created: {}", transcript.meta.created_at.to_rfc3339());
                println!("Provider: {}", transcript.meta.provider);
                println!("Model: {}", transcript.meta.model.as_deref().unwrap_or("-"));
                if let Some(parent_id) = &transcript.meta.parent_id {
                    println!("Parent: {parent_id}");
                }
                println!("Forked: {}", transcript.meta.forked);
                if !transcript.compactions.is_empty() {
                    println!("Compactions: {}", transcript.compactions.len());
                    for compaction in &transcript.compactions {
                        println!(
                            "  {} {} -> {} messages",
                            compaction.collapsed_at.to_rfc3339(),
                            compaction.before_messages,
                            compaction.after_messages
                        );
                    }
                }
                if !transcript.summaries.is_empty() {
                    println!("Summaries: {}", transcript.summaries.len());
                    for summary in &transcript.summaries {
                        println!(
                            "  {} {} -> {} messages: {}",
                            summary.summarized_at.to_rfc3339(),
                            summary.before_messages,
                            summary.after_messages,
                            summary.summary.lines().next().unwrap_or_default()
                        );
                    }
                }
                if let Some(usage) = transcript.usage {
                    println!(
                        "Usage: input={} output={} cache={} total={}",
                        usage.input_tokens,
                        usage.output_tokens,
                        usage.cache_tokens,
                        usage.total_tokens()
                    );
                    println!("Estimated cost: ${:.6}", usage.estimated_cost_usd);
                }
                println!("CWD: {}", transcript.meta.cwd);
                println!("Path: {}", transcript.path.display());
                println!();
                for message in transcript.messages {
                    print_message(message);
                }
                0
            }
            Err(error) => {
                eprintln!("orca: failed to show history: {error}");
                1
            }
        },
        HistoryCommand::Archive { session } => match history::archive_session(&session) {
            Ok(path) => {
                println!("archived {}", path.display());
                0
            }
            Err(error) => {
                eprintln!("orca: failed to archive history: {error}");
                1
            }
        },
        HistoryCommand::Delete { session } => match history::delete_session(&session) {
            Ok(path) => {
                println!("deleted {}", path.display());
                0
            }
            Err(error) => {
                eprintln!("orca: failed to delete history: {error}");
                1
            }
        },
        HistoryCommand::Rename { session, title } => {
            match history::rename_session(&session, &title) {
                Ok(path) => {
                    println!("renamed {}", path.display());
                    0
                }
                Err(error) => {
                    eprintln!("orca: failed to rename history: {error}");
                    1
                }
            }
        }
        HistoryCommand::Search { query, all } => match history::search_sessions(&query, all) {
            Ok(hits) => {
                for hit in hits {
                    let state = if hit.archived { "archived" } else { "active" };
                    println!(
                        "{}\t{}\t{}\t{}:{}\t{}",
                        hit.session_id,
                        state,
                        hit.title,
                        hit.path.display(),
                        hit.line_number,
                        hit.line
                    );
                }
                0
            }
            Err(error) => {
                eprintln!("orca: failed to search history: {error}");
                1
            }
        },
        HistoryCommand::Compress { session } => match history::compress_session(&session) {
            Ok(path) => {
                println!("compressed {}", path.display());
                0
            }
            Err(error) => {
                eprintln!("orca: failed to compress history: {error}");
                1
            }
        },
    }
}

fn run_workflow(args: WorkflowArgs) -> i32 {
    match args.command {
        WorkflowCommand::Run(args) => run_workflow_command(args),
        WorkflowCommand::List => workflow_list_command(),
        WorkflowCommand::Show { task_id } => workflow_show_command(&task_id),
        WorkflowCommand::Stop { task_id } => workflow_stop_command(&task_id),
        WorkflowCommand::Resume { run_id } => workflow_resume_command(&run_id),
        WorkflowCommand::Worker(args) => run_workflow_worker(args),
    }
}

fn run_workflow_command(args: WorkflowRunArgs) -> i32 {
    let cwd = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if let Err(error) = build_workflow_run_config(
        &cwd,
        args.provider,
        args.model.clone(),
        args.api_key.clone(),
        args.base_url.clone(),
    ) {
        eprintln!("orca: {error}");
        return 1;
    }
    let workflow_args = match parse_optional_json_arg(args.args.as_deref()) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let input = workflow_input_for_launch(
        &cwd,
        &args.script_or_name,
        workflow_args,
        args.resume_from_run_id,
    );
    if let Some(run_id) = input.resume_from_run_id.as_deref() {
        eprintln!(
            "orca: workflow resume from run '{run_id}' is only available inside the active Orca session that owns the workflow run"
        );
        return 1;
    }
    let session_id = match resolve_workflow_session_id(&cwd, input.resume_from_run_id.as_deref()) {
        Ok(session_id) => session_id,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    spawn_workflow_worker(
        &cwd,
        session_id,
        args.provider,
        args.model,
        args.api_key,
        args.base_url,
        &input,
    )
}

fn workflow_list_command() -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut runs = match load_persisted_workflow_runs(&cwd) {
        Ok(runs) => runs,
        Err(error) => {
            eprintln!("orca: failed to list workflows: {error}");
            return 1;
        }
    };
    runs.sort_by(|left, right| {
        right
            .state_mtime
            .cmp(&left.state_mtime)
            .then_with(|| right.state.run_id.cmp(&left.state.run_id))
    });

    let entries = runs
        .into_iter()
        .map(|run| WorkflowListEntry {
            task_id: run.state.task_id,
            run_id: run.state.run_id,
            session_id: run.session_id,
            workflow_name: run.state.workflow_name,
            status: run.state.status,
            cwd: run.state.cwd,
            total_agent_count: run.state.total_agent_count,
            final_summary: run.state.final_summary,
            error: run.state.error,
        })
        .collect::<Vec<_>>();

    match print_json_stdout(&entries) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("orca: failed to print workflow list: {error}");
            1
        }
    }
}

fn workflow_show_command(task_id: &str) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    let run = match find_workflow_by_task_id(&cwd, task_id) {
        Ok(Some(run)) => run,
        Ok(None) => {
            eprintln!("orca: workflow task '{task_id}' not found");
            return 1;
        }
        Err(error) => {
            eprintln!("orca: failed to show workflow: {error}");
            return 1;
        }
    };

    let response = WorkflowShowEntry {
        session_id: run.session_id,
        transcript_dir: run.run_dir.join("transcripts").display().to_string(),
        run_dir: run.run_dir.display().to_string(),
        state: run.state,
    };

    match print_json_stdout(&response) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("orca: failed to print workflow details: {error}");
            1
        }
    }
}

fn workflow_stop_command(task_id: &str) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    match find_workflow_by_task_id(&cwd, task_id) {
        Ok(Some(run)) => {
            if !matches!(
                run.state.status,
                orca_core::workflow_types::WorkflowRunStatus::Queued
                    | orca_core::workflow_types::WorkflowRunStatus::Running
                    | orca_core::workflow_types::WorkflowRunStatus::Stopping
            ) {
                eprintln!(
                    "orca: workflow task '{}' is not active (current status: {:?})",
                    task_id, run.state.status
                );
                return 1;
            }
            let store = WorkflowStateStore::new(run.run_dir.parent().unwrap().to_path_buf());
            if let Err(error) = store.request_stop(&run.state.run_id) {
                eprintln!("orca: failed to request workflow stop: {error}");
                return 1;
            }
            match print_json_stdout(&WorkflowStopResponse {
                status: "stop_requested",
                task_id: run.state.task_id,
                run_id: run.state.run_id,
            }) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("orca: failed to print workflow stop response: {error}");
                    1
                }
            }
        }
        Ok(None) => {
            eprintln!("orca: workflow task '{task_id}' not found");
            1
        }
        Err(error) => {
            eprintln!("orca: failed to inspect workflow state: {error}");
            1
        }
    }
}

fn workflow_resume_command(run_id: &str) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_default();
    match find_workflow_by_run_id(&cwd, run_id) {
        Ok(Some(run)) => {
            eprintln!(
                "orca: workflow run '{}' belongs to session '{}'; resume is only available inside that active Orca session",
                run.state.run_id, run.session_id
            );
            1
        }
        Ok(None) => {
            eprintln!("orca: workflow run '{run_id}' not found");
            1
        }
        Err(error) => {
            eprintln!("orca: failed to inspect workflow state: {error}");
            1
        }
    }
}

fn run_workflow_worker(args: WorkflowWorkerArgs) -> i32 {
    let input: WorkflowInput = match serde_json::from_str(&args.input_json) {
        Ok(input) => input,
        Err(error) => {
            eprintln!("orca: invalid workflow worker input JSON: {error}");
            return 1;
        }
    };
    let config = match build_workflow_run_config(
        &args.cwd,
        args.provider,
        args.model.clone(),
        args.api_key.clone(),
        args.base_url.clone(),
    ) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let session_dir = workflow_session_root(&args.cwd).join(&args.session_id);
    let tasks = TaskRegistry::new(args.session_id.clone());
    let runner = WorkflowRunner::new(config, tasks, session_dir.clone());
    let launch = match runner.launch_background(WorkflowLaunchRequest::from(input.clone())) {
        Ok(launch) => launch,
        Err(error) => {
            eprintln!("orca: failed to launch workflow: {error}");
            return 1;
        }
    };

    let run_dir = session_dir.join("workflow-runs").join(&launch.run_id);
    if let Err(error) = write_workflow_cli_launch_record(
        &run_dir,
        &WorkflowCliLaunchRecord {
            cwd: args.cwd.display().to_string(),
            provider: args.provider,
            model: args.model,
            api_key: args.api_key,
            base_url: args.base_url,
            input,
        },
    ) {
        eprintln!("orca: failed to persist workflow launch record: {error}");
        return 1;
    }

    if let Err(error) = print_json_stdout(&launch.output) {
        eprintln!("orca: failed to write workflow output: {error}");
        return 1;
    }

    match launch.join() {
        Ok(Ok(_)) => 0,
        Ok(Err(_)) => 1,
        Err(_) => 1,
    }
}

fn build_workflow_run_config(
    cwd: &Path,
    provider: ProviderKind,
    model_override: Option<String>,
    api_key_override: Option<String>,
    base_url_override: Option<String>,
) -> Result<RunConfig, String> {
    let file_config = load_effective_file_config(
        cwd,
        ConfigOverrides {
            model: model_override,
            mode: None,
            api_key: api_key_override,
            base_url: base_url_override,
        },
    )?;
    if !file_config.workflows.resolved().enabled {
        return Err("workflows are disabled".to_string());
    }
    let model = ModelSelection::parse(file_config.model)?;

    Ok(RunConfig {
        prompt: String::new(),
        cwd: Some(cwd.to_path_buf()),
        output_format: OutputFormat::Jsonl,
        approval_mode: file_config.mode.unwrap_or_default(),
        provider,
        verifier: None,
        model,
        api_key: file_config.api_key,
        base_url: file_config.base_url,
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        permission_rules: file_config.permissions,
        max_budget_usd: None,
        mcp_servers: file_config.mcp_servers,
        hooks: file_config.hooks,
        external_tools: crate::tools::external::load_default_external_tools(),
        subagents: file_config.subagents.normalized(),
        tools: file_config.tools.normalized(),
        workflows: file_config.workflows.resolved(),
        theme: file_config.theme,
        vim_mode: file_config.vim_mode,
        update_check: file_config.update_check,
        desktop_notifications: false,
        auto_memory: file_config.auto_memory,
    })
}

fn parse_optional_json_arg(raw: Option<&str>) -> Result<Option<Value>, String> {
    match raw {
        Some(raw) => serde_json::from_str(raw)
            .map(Some)
            .map_err(|error| format!("invalid JSON for --args: {error}")),
        None => Ok(None),
    }
}

fn spawn_workflow_worker(
    cwd: &Path,
    session_id: String,
    provider: ProviderKind,
    model: Option<String>,
    api_key: Option<String>,
    base_url: Option<String>,
    input: &WorkflowInput,
) -> i32 {
    let current_exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("orca: failed to resolve current executable: {error}");
            return 1;
        }
    };
    let input_json = match serde_json::to_string(input) {
        Ok(json) => json,
        Err(error) => {
            eprintln!("orca: failed to encode workflow input: {error}");
            return 1;
        }
    };

    let mut command = ProcessCommand::new(current_exe);
    command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .arg("workflow")
        .arg("worker")
        .arg("--cwd")
        .arg(cwd)
        .arg("--provider")
        .arg(provider.to_possible_value().unwrap().get_name())
        .arg("--session-id")
        .arg(&session_id)
        .arg("--input-json")
        .arg(input_json);
    if let Some(model) = model {
        command.arg("--model").arg(model);
    }
    if let Some(api_key) = api_key {
        command.arg("--api-key").arg(api_key);
    }
    if let Some(base_url) = base_url {
        command.arg("--base-url").arg(base_url);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("orca: failed to start workflow worker: {error}");
            return 1;
        }
    };

    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            eprintln!("orca: workflow worker did not expose stdout");
            return 1;
        }
    };
    let mut reader = BufReader::new(stdout);
    let mut first_line = String::new();
    match reader.read_line(&mut first_line) {
        Ok(0) => {
            let _ = child.wait();
            eprintln!("orca: workflow worker exited before reporting launch output");
            1
        }
        Ok(_) => {
            print!("{}", first_line);
            0
        }
        Err(error) => {
            let _ = child.wait();
            eprintln!("orca: failed to read workflow worker launch output: {error}");
            1
        }
    }
}

fn workflow_input_for_launch(
    cwd: &Path,
    script_or_name: &str,
    args: Option<Value>,
    resume_from_run_id: Option<String>,
) -> WorkflowInput {
    let script_path = PathBuf::from(script_or_name);
    WorkflowInput {
        script_path: if script_path.is_absolute() || cwd.join(script_or_name).exists() {
            Some(script_or_name.to_string())
        } else {
            None
        },
        name: if script_path.is_absolute() || cwd.join(script_or_name).exists() {
            None
        } else {
            Some(script_or_name.to_string())
        },
        description: None,
        title: None,
        script: None,
        args,
        resume_from_run_id,
    }
}

fn workflow_session_root(cwd: &Path) -> PathBuf {
    cwd.join(".orca").join("workflow-sessions")
}

fn resolve_workflow_session_id(
    cwd: &Path,
    resume_from_run_id: Option<&str>,
) -> Result<String, String> {
    match resume_from_run_id {
        Some(run_id) => find_workflow_by_run_id(cwd, run_id)?
            .map(|run| run.session_id)
            .ok_or_else(|| format!("workflow run '{run_id}' not found")),
        None => Ok(format!("workflow-cli-{}", uuid::Uuid::new_v4())),
    }
}

fn load_persisted_workflow_runs(cwd: &Path) -> Result<Vec<PersistedWorkflowRun>, String> {
    let root = workflow_session_root(cwd);
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut runs = Vec::new();
    for session_entry in fs::read_dir(&root).map_err(|error| error.to_string())? {
        let session_entry = session_entry.map_err(|error| error.to_string())?;
        if !session_entry
            .file_type()
            .map_err(|error| error.to_string())?
            .is_dir()
        {
            continue;
        }
        let session_id = session_entry.file_name().to_string_lossy().to_string();
        let runs_dir = session_entry.path().join("workflow-runs");
        if !runs_dir.exists() {
            continue;
        }
        for run_entry in fs::read_dir(&runs_dir).map_err(|error| error.to_string())? {
            let run_entry = run_entry.map_err(|error| error.to_string())?;
            if !run_entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                continue;
            }
            let state_path = run_entry.path().join("state.json");
            if !state_path.exists() {
                continue;
            }
            let state = read_workflow_state(&state_path)?;
            let state_mtime = fs::metadata(&state_path)
                .ok()
                .and_then(|metadata| metadata.modified().ok());
            runs.push(PersistedWorkflowRun {
                session_id: session_id.clone(),
                state,
                run_dir: run_entry.path(),
                state_mtime,
            });
        }
    }

    Ok(runs)
}

fn find_workflow_by_task_id(
    cwd: &Path,
    task_id: &str,
) -> Result<Option<PersistedWorkflowRun>, String> {
    Ok(load_persisted_workflow_runs(cwd)?
        .into_iter()
        .find(|run| run.state.task_id == task_id))
}

fn find_workflow_by_run_id(
    cwd: &Path,
    run_id: &str,
) -> Result<Option<PersistedWorkflowRun>, String> {
    Ok(load_persisted_workflow_runs(cwd)?
        .into_iter()
        .find(|run| run.state.run_id == run_id))
}

fn read_workflow_state(path: &Path) -> Result<WorkflowRunState, String> {
    let content = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&content)
        .map_err(|error| format!("invalid workflow state at {}: {error}", path.display()))
}

fn print_json_stdout(value: &impl Serialize) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value).map_err(|error| error.to_string())?;
    stdout.write_all(b"\n").map_err(|error| error.to_string())?;
    stdout.flush().map_err(|error| error.to_string())
}

fn write_workflow_cli_launch_record(
    run_dir: &Path,
    record: &WorkflowCliLaunchRecord,
) -> Result<(), String> {
    fs::create_dir_all(run_dir).map_err(|error| error.to_string())?;
    let path = workflow_cli_launch_record_path(run_dir);
    let content = serde_json::to_string_pretty(record).map_err(|error| error.to_string())?;
    fs::write(path, content).map_err(|error| error.to_string())
}

fn workflow_cli_launch_record_path(run_dir: &Path) -> PathBuf {
    run_dir.join("cli-launch.json")
}

fn print_message(message: crate::provider::conversation::Message) {
    use crate::provider::conversation::Message;

    match message {
        Message::System { content, .. } => println!("[system]\n{}\n", content.trim()),
        Message::User { content, .. } => println!("[user]\n{}\n", content.trim()),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
            ..
        } => {
            println!("[assistant]");
            if let Some(reasoning) = reasoning_content.filter(|text| !text.trim().is_empty()) {
                println!("reasoning: {}", reasoning.trim());
            }
            if let Some(content) = content.filter(|text| !text.trim().is_empty()) {
                println!("{}", content.trim());
            }
            for tool_call in tool_calls {
                println!(
                    "tool_call {} {} {}",
                    tool_call.id, tool_call.function_name, tool_call.arguments
                );
            }
            println!();
        }
        Message::Tool {
            tool_call_id,
            content,
            ..
        } => println!("[tool {tool_call_id}]\n{}\n", content.trim()),
    }
}

fn resolve_history_mode(
    resume: Option<String>,
    fork: Option<String>,
    continue_latest: bool,
    fallback: HistoryMode,
) -> HistoryMode {
    if let Some(selector) = fork {
        HistoryMode::Fork(selector)
    } else if let Some(selector) = resume.or_else(|| {
        if continue_latest {
            Some("latest".to_string())
        } else {
            None
        }
    }) {
        HistoryMode::Resume(selector)
    } else {
        fallback
    }
}

fn run_placeholder(cli: Cli) -> i32 {
    let resume_like =
        cli.resume.is_some() as u8 + cli.fork.is_some() as u8 + cli.continue_latest as u8;
    if resume_like > 1 {
        eprintln!("orca: --resume, --fork, and --continue are mutually exclusive");
        return 1;
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let mode = match cli.mode {
        Some(mode) => match parse_approval_mode_value(&mode) {
            Ok(mode) => Some(mode),
            Err(error) => {
                eprintln!("orca: {error}");
                return 1;
            }
        },
        None => None,
    };
    let file_config = match load_effective_file_config(
        &cwd,
        ConfigOverrides {
            model: cli.model,
            mode,
            api_key: cli.api_key,
            base_url: cli.base_url,
        },
    ) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let api_key = file_config.api_key;
    let base_url = file_config.base_url;

    let model = file_config.model;
    let model = match ModelSelection::parse(model) {
        Ok(model) => model,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let history_mode = resolve_history_mode(
        cli.resume,
        cli.fork,
        cli.continue_latest,
        HistoryMode::Record,
    );

    let config = RunConfig {
        prompt: cli.prompt.join(" "),
        cwd: None,
        output_format: OutputFormat::Text,
        approval_mode: file_config.mode.unwrap_or_default(),
        provider: cli.provider,
        verifier: None,
        model,
        api_key,
        base_url,
        history_mode,
        show_session_picker: cli.session_picker,
        permission_rules: file_config.permissions,
        max_budget_usd: None,
        mcp_servers: file_config.mcp_servers,
        hooks: file_config.hooks,
        external_tools: crate::tools::external::load_default_external_tools(),
        subagents: file_config.subagents.normalized(),
        tools: file_config.tools.normalized(),
        workflows: file_config.workflows.resolved(),
        theme: file_config.theme,
        vim_mode: file_config.vim_mode,
        update_check: file_config.update_check,
        desktop_notifications: file_config.desktop_notifications,
        auto_memory: file_config.auto_memory,
    };

    app::run_tui(config)
}

fn run_server(cli: Cli) -> i32 {
    if cli.command.is_some() || !cli.prompt.is_empty() {
        eprintln!("orca: --mode=server cannot be combined with a subcommand or prompt");
        return 1;
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let file_config = match load_effective_file_config(
        &cwd,
        ConfigOverrides {
            model: cli.model,
            mode: None,
            api_key: cli.api_key,
            base_url: cli.base_url,
        },
    ) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let model = match ModelSelection::parse(file_config.model) {
        Ok(model) => model,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };

    let config = RunConfig {
        prompt: String::new(),
        cwd: None,
        output_format: OutputFormat::Jsonl,
        approval_mode: file_config.mode.unwrap_or_default(),
        provider: cli.provider,
        verifier: None,
        model,
        api_key: file_config.api_key,
        base_url: file_config.base_url,
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        permission_rules: file_config.permissions,
        max_budget_usd: None,
        mcp_servers: file_config.mcp_servers,
        hooks: file_config.hooks,
        external_tools: crate::tools::external::load_default_external_tools(),
        subagents: file_config.subagents.normalized(),
        tools: file_config.tools.normalized(),
        workflows: file_config.workflows.resolved(),
        theme: file_config.theme,
        vim_mode: file_config.vim_mode,
        update_check: file_config.update_check,
        desktop_notifications: false,
        auto_memory: file_config.auto_memory,
    };

    crate::server::run(crate::server::ServerConfig { run_config: config })
}
