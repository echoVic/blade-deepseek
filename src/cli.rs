use std::env;
use std::io;
use std::io::IsTerminal;
use std::io::Read;
use std::path::Path;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};
use orca_runtime::{
    subagent::SubagentRequest,
    subagent_async_worker::{self, AsyncSubagentWorktree},
};

use crate::approval::policy::ApprovalMode;
use crate::config::file;
use crate::config::file::ConfigOverrides;
use crate::config::{HistoryMode, OutputFormat, ProviderKind, ReasoningEffort, RunConfig};
use crate::model::ModelSelection;
use crate::runtime::controller;
use crate::runtime::history;

const MAX_WORKER_API_KEY_BYTES: u64 = 64 * 1024;

#[derive(Debug, Parser)]
#[command(name = "orca")]
#[command(version)]
#[command(about = "A DeepSeek-native coding agent.")]
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

    /// Approval mode to use, 'server' for stdin/stdout JSON-RPC mode, or 'acp' for Agent Client Protocol mode.
    #[arg(long = "mode", alias = "approval-mode")]
    mode: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Workspace directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

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
    /// Inspect or update folder trust.
    Trust(TrustArgs),
    /// Execute a persisted async subagent task.
    #[command(hide = true)]
    SubagentWorker(SubagentWorkerArgs),
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

#[derive(Debug, Parser)]
struct TrustArgs {
    /// Trust action.
    #[arg(value_enum, default_value_t = TrustAction::Show)]
    action: TrustAction,

    /// Folder to inspect or update.
    #[arg(long)]
    cwd: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum TrustAction {
    Show,
    Add,
    Remove,
}

#[derive(Debug, Subcommand)]
enum WorkflowCommand {
    /// Launch a workflow script or named workflow.
    Run(WorkflowRunArgs),
    /// List persisted workflow runs for the current project.
    List(WorkflowListArgs),
    /// Show a persisted workflow run by task id.
    Show { task_id: String },
    /// Show a saved workflow source by name.
    Source { name: String },
    /// Request stop for a workflow task.
    Stop { task_id: String },
    /// Request pause for a workflow task.
    Pause { task_id: String },
    /// Resume a paused workflow run.
    Resume { run_id: String },
    /// Clone a persisted workflow run as an editable draft.
    Clone { run_id: String },
    /// Restart failed agents from a persisted workflow run.
    RestartFailed { run_id: String },
    /// Restart one workflow phase while reusing cached results from other phases.
    RestartPhase { run_id: String, phase: String },
    #[command(hide = true)]
    Worker(WorkflowWorkerArgs),
}

#[derive(Debug, Default, Parser)]
struct WorkflowListArgs {
    /// Filter by workflow name.
    #[arg(long)]
    name: Option<String>,

    /// Filter by workflow run id.
    #[arg(long = "run-id")]
    run_id: Option<String>,

    /// Filter by workflow status, such as running, failed, or completed.
    #[arg(long)]
    status: Option<String>,
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
    /// Product version inherited from the parent executable.
    #[arg(long, hide = true, default_value = env!("CARGO_PKG_VERSION"))]
    app_version: String,

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

    /// Read the API key once from stdin (internal worker handoff).
    #[arg(long, hide = true)]
    api_key_stdin: bool,

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

impl From<WorkflowArgs> for orca_runtime::workflow::command::WorkflowCommandRequest {
    fn from(args: WorkflowArgs) -> Self {
        use orca_runtime::workflow::command::{
            WorkflowCommandRequest, WorkflowListRequest, WorkflowRunRequest, WorkflowWorkerRequest,
        };

        match args.command {
            WorkflowCommand::Run(args) => WorkflowCommandRequest::Run(WorkflowRunRequest {
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                cwd: args.cwd,
                provider: args.provider,
                model: args.model,
                api_key: args.api_key,
                base_url: args.base_url,
                args: args.args,
                resume_from_run_id: args.resume_from_run_id,
                script_or_name: args.script_or_name,
            }),
            WorkflowCommand::List(args) => WorkflowCommandRequest::List(WorkflowListRequest {
                name: args.name,
                run_id: args.run_id,
                status: args.status,
            }),
            WorkflowCommand::Show { task_id } => WorkflowCommandRequest::Show { task_id },
            WorkflowCommand::Source { name } => WorkflowCommandRequest::Source { name },
            WorkflowCommand::Stop { task_id } => WorkflowCommandRequest::Stop { task_id },
            WorkflowCommand::Pause { task_id } => WorkflowCommandRequest::Pause { task_id },
            WorkflowCommand::Resume { run_id } => WorkflowCommandRequest::Resume { run_id },
            WorkflowCommand::Clone { run_id } => WorkflowCommandRequest::Clone { run_id },
            WorkflowCommand::RestartFailed { run_id } => WorkflowCommandRequest::Restart {
                run_id,
                phase: None,
                app_version: env!("CARGO_PKG_VERSION").to_string(),
            },
            WorkflowCommand::RestartPhase { run_id, phase } => WorkflowCommandRequest::Restart {
                run_id,
                phase: Some(phase),
                app_version: env!("CARGO_PKG_VERSION").to_string(),
            },
            WorkflowCommand::Worker(args) => {
                WorkflowCommandRequest::Worker(WorkflowWorkerRequest {
                    app_version: args.app_version,
                    cwd: args.cwd,
                    provider: args.provider,
                    model: args.model,
                    api_key: args.api_key,
                    api_key_stdin: args.api_key_stdin,
                    base_url: args.base_url,
                    session_id: args.session_id,
                    input_json: args.input_json,
                })
            }
        }
    }
}

#[derive(Debug, Parser)]
struct SubagentWorkerArgs {
    /// Workspace directory where the parent async task was launched.
    #[arg(long)]
    cwd: PathBuf,

    /// Workspace directory where the child agent should execute.
    #[arg(long)]
    child_cwd: PathBuf,

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    /// Model to use (overrides config file and ORCA_MODEL env).
    #[arg(long)]
    model: Option<String>,

    /// API key to use (overrides config file and ORCA_API_KEY env).
    #[arg(long)]
    api_key: Option<String>,

    /// Read the API key once from stdin (internal worker handoff).
    #[arg(long, hide = true)]
    api_key_stdin: bool,

    /// API base URL (overrides config file and ORCA_BASE_URL env).
    #[arg(long)]
    base_url: Option<String>,

    /// Persisted task session identifier.
    #[arg(long)]
    session_id: String,

    /// Persisted async subagent task identifier.
    #[arg(long)]
    agent_id: String,

    /// Child subagent depth.
    #[arg(long)]
    subagent_depth: u32,

    /// Full subagent request payload as JSON.
    #[arg(long)]
    request_json: String,

    /// Parent git repository root for isolated worktree cleanup.
    #[arg(long)]
    worktree_repo_root: Option<PathBuf>,

    /// Child git worktree path for isolated worktree cleanup.
    #[arg(long)]
    worktree_path: Option<PathBuf>,
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
    if matches!(cli.mode.as_deref(), Some("acp")) {
        return run_acp(cli);
    }

    match cli.command {
        Some(Command::Exec(args)) => run_exec(args),
        Some(Command::History(args)) => run_history(args),
        Some(Command::Workflow(args)) => orca_runtime::workflow::command::run(args.into()),
        Some(Command::Trust(args)) => run_trust(args),
        Some(Command::SubagentWorker(args)) => run_subagent_worker(args),
        None => run_placeholder(cli),
    }
}

fn run_trust(args: TrustArgs) -> i32 {
    use crate::config::folder_trust::{self, TrustLevel};

    let cwd = args
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    match args.action {
        TrustAction::Show => {
            let level = match folder_trust::trust_level(&cwd) {
                Some(TrustLevel::Trusted) => "trusted",
                Some(TrustLevel::Untrusted) => "untrusted",
                None => "unknown (treated as untrusted)",
            };
            println!("{}: {level}", cwd.display());
            0
        }
        TrustAction::Add => match folder_trust::set_trust(&cwd, TrustLevel::Trusted) {
            Ok(()) => {
                println!("trusted {}", cwd.display());
                0
            }
            Err(error) => {
                eprintln!("orca: failed to trust folder: {error}");
                1
            }
        },
        TrustAction::Remove => match folder_trust::set_trust(&cwd, TrustLevel::Untrusted) {
            Ok(()) => {
                println!("marked {} untrusted", cwd.display());
                0
            }
            Err(error) => {
                eprintln!("orca: failed to update folder trust: {error}");
                1
            }
        },
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
        reasoning_effort: match env::var("ORCA_REASONING_EFFORT")
            .ok()
            .or_else(|| env::var("DEEPSEEK_REASONING_EFFORT").ok())
        {
            Some(value) => Some(parse_reasoning_effort_value(&value)?),
            None => None,
        },
    })
}

fn parse_approval_mode_value(mode: &str) -> Result<ApprovalMode, String> {
    ApprovalMode::from_str(mode, true).map_err(|_| {
        format!("unsupported mode '{mode}'. Use suggest, auto-edit, full-auto, or plan")
    })
}

fn parse_reasoning_effort_value(value: &str) -> Result<ReasoningEffort, String> {
    match value {
        "high" => Ok(ReasoningEffort::High),
        "max" => Ok(ReasoningEffort::Max),
        other => Err(format!(
            "unsupported reasoning_effort '{other}'. Use high or max"
        )),
    }
}

fn read_stdin_text() -> Result<String, String> {
    let mut buffer = String::new();
    io::stdin()
        .read_to_string(&mut buffer)
        .map_err(|error| format!("failed to read stdin: {error}"))?;
    Ok(buffer)
}

fn prompt_with_stdin_context(prompt: &str, stdin_text: &str) -> String {
    let mut combined = format!("{prompt}\n\n<stdin>\n{stdin_text}");
    if !stdin_text.ends_with('\n') {
        combined.push('\n');
    }
    combined.push_str("</stdin>");
    combined
}

fn resolve_exec_prompt_from_stdin(prompt_args: Vec<String>) -> Result<String, String> {
    let force_stdin = prompt_args.len() == 1 && prompt_args[0] == "-";
    let has_prompt = !prompt_args.is_empty() && !force_stdin;
    let prompt = if has_prompt {
        prompt_args.join(" ")
    } else {
        String::new()
    };

    if force_stdin || !has_prompt {
        if io::stdin().is_terminal() {
            return Err(
                "No prompt provided. Either specify one as an argument or pipe the prompt into stdin."
                    .to_string(),
            );
        }
        let stdin_text = read_stdin_text()?;
        if stdin_text.trim().is_empty() {
            return Err("No prompt provided via stdin.".to_string());
        }
        return Ok(stdin_text);
    }

    if io::stdin().is_terminal() {
        return Ok(prompt);
    }

    let stdin_text = read_stdin_text()?;
    if stdin_text.trim().is_empty() {
        Ok(prompt)
    } else {
        Ok(prompt_with_stdin_context(&prompt, &stdin_text))
    }
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

    let prompt = match resolve_exec_prompt_from_stdin(args.prompt) {
        Ok(prompt) => prompt,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    let config_cwd = args
        .cwd
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let file_config = match load_effective_file_config(
        &config_cwd,
        ConfigOverrides {
            model: args.model,
            mode: args.approval_mode,
            api_key: args.api_key,
            base_url: args.base_url,
            reasoning_effort: None,
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

    let output_format = args.output_format;
    let fallback =
        if args.no_history || (output_format == OutputFormatArg::Jsonl && !args.save_history) {
            HistoryMode::Disabled
        } else {
            HistoryMode::Record
        };
    let history_mode = resolve_history_mode(args.resume, args.fork, args.continue_latest, fallback);

    let config = RunConfig {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        prompt,
        cwd: args.cwd,
        output_format: output_format.into(),
        approval_mode: file_config.mode.unwrap_or_default(),
        provider: args.provider,
        verifier: args.verifier,
        model,
        model_runtime: file_config.model_runtime,
        reasoning_effort: file_config.reasoning_effort,
        api_key,
        base_url,
        history_mode,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: file_config.permission_profiles,
        runtime_workspace_roots: None,
        permission_rules: file_config.permissions,
        additional_working_directories: Vec::new(),
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

fn run_subagent_worker(args: SubagentWorkerArgs) -> i32 {
    let request: SubagentRequest = match serde_json::from_str(&args.request_json) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("orca: invalid subagent worker request JSON: {error}");
            return 1;
        }
    };
    let api_key = match resolve_worker_api_key(args.api_key, args.api_key_stdin) {
        Ok(api_key) => api_key,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    let config = match build_worker_run_config(
        &args.cwd,
        args.provider,
        args.model.clone(),
        api_key,
        args.base_url.clone(),
    ) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: {error}");
            return 1;
        }
    };
    let worktree = match (args.worktree_repo_root, args.worktree_path) {
        (Some(repo_root), Some(path)) => Some(AsyncSubagentWorktree { repo_root, path }),
        (None, None) => None,
        _ => {
            eprintln!("orca: --worktree-repo-root and --worktree-path must be provided together");
            return 1;
        }
    };

    subagent_async_worker::run_async_subagent_worker(
        subagent_async_worker::AsyncSubagentWorkerInput {
            config,
            cwd: args.cwd,
            child_cwd: args.child_cwd,
            task_session_id: args.session_id,
            agent_id: args.agent_id,
            request,
            child_depth: args.subagent_depth,
            worktree,
        },
    )
}

fn build_worker_run_config(
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
            reasoning_effort: None,
        },
    )?;
    let model = ModelSelection::parse(file_config.model)?;

    Ok(RunConfig {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        prompt: String::new(),
        cwd: Some(cwd.to_path_buf()),
        output_format: OutputFormat::Jsonl,
        approval_mode: file_config.mode.unwrap_or_default(),
        provider,
        verifier: None,
        model,
        model_runtime: file_config.model_runtime,
        reasoning_effort: file_config.reasoning_effort,
        api_key: file_config.api_key,
        base_url: file_config.base_url,
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: file_config.permission_profiles,
        runtime_workspace_roots: None,
        permission_rules: file_config.permissions,
        additional_working_directories: Vec::new(),
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

fn resolve_worker_api_key(
    api_key_arg: Option<String>,
    api_key_stdin: bool,
) -> Result<Option<String>, String> {
    resolve_worker_api_key_from_reader(api_key_arg, api_key_stdin, std::io::stdin())
}

fn resolve_worker_api_key_from_reader(
    api_key_arg: Option<String>,
    api_key_stdin: bool,
    reader: impl Read,
) -> Result<Option<String>, String> {
    if !api_key_stdin {
        return Ok(api_key_arg);
    }
    if api_key_arg.is_some() {
        return Err("--api-key and --api-key-stdin cannot be used together".to_string());
    }
    let mut api_key = String::new();
    reader
        .take(MAX_WORKER_API_KEY_BYTES + 1)
        .read_to_string(&mut api_key)
        .map_err(|error| format!("failed to read worker credential from stdin: {error}"))?;
    if api_key.len() as u64 > MAX_WORKER_API_KEY_BYTES {
        return Err("worker credential from stdin exceeds 64 KiB".to_string());
    }
    Ok(Some(api_key))
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
            reasoning_effort: None,
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
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        prompt: cli.prompt.join(" "),
        cwd: None,
        output_format: OutputFormat::Text,
        approval_mode: file_config.mode.unwrap_or_default(),
        provider: cli.provider,
        verifier: None,
        model,
        model_runtime: file_config.model_runtime,
        reasoning_effort: file_config.reasoning_effort,
        api_key,
        base_url,
        history_mode,
        show_session_picker: cli.session_picker,
        active_permission_profile: None,
        permission_profiles: file_config.permission_profiles,
        runtime_workspace_roots: None,
        permission_rules: file_config.permissions,
        additional_working_directories: Vec::new(),
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

    orca_tui::cli::run(config)
}

fn run_server(cli: Cli) -> i32 {
    if cli.command.is_some() || !cli.prompt.is_empty() {
        eprintln!("orca: --mode=server cannot be combined with a subcommand or prompt");
        return 1;
    }

    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let file_config = match load_effective_file_config(
        &cwd,
        ConfigOverrides {
            model: cli.model,
            mode: None,
            api_key: cli.api_key,
            base_url: cli.base_url,
            reasoning_effort: None,
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
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        prompt: String::new(),
        cwd: Some(cwd),
        output_format: OutputFormat::Jsonl,
        approval_mode: file_config.mode.unwrap_or_default(),
        provider: cli.provider,
        verifier: None,
        model,
        model_runtime: file_config.model_runtime,
        reasoning_effort: file_config.reasoning_effort,
        api_key: file_config.api_key,
        base_url: file_config.base_url,
        history_mode: HistoryMode::Record,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: file_config.permission_profiles,
        runtime_workspace_roots: None,
        permission_rules: file_config.permissions,
        additional_working_directories: Vec::new(),
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

fn run_acp(cli: Cli) -> i32 {
    if cli.command.is_some() || !cli.prompt.is_empty() {
        eprintln!("orca: --mode=acp cannot be combined with a subcommand or prompt");
        return 1;
    }

    let cwd = cli
        .cwd
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let file_config = match load_effective_file_config(
        &cwd,
        ConfigOverrides {
            model: cli.model,
            mode: None,
            api_key: cli.api_key,
            base_url: cli.base_url,
            reasoning_effort: None,
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
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        prompt: String::new(),
        cwd: Some(cwd),
        output_format: OutputFormat::Jsonl,
        approval_mode: file_config.mode.unwrap_or_default(),
        provider: cli.provider,
        verifier: None,
        model,
        model_runtime: file_config.model_runtime,
        reasoning_effort: file_config.reasoning_effort,
        api_key: file_config.api_key,
        base_url: file_config.base_url,
        history_mode: HistoryMode::Record,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: file_config.permission_profiles,
        runtime_workspace_roots: None,
        permission_rules: file_config.permissions,
        additional_working_directories: Vec::new(),
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

    crate::acp::run(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_key_arg_remains_compatible_without_stdin_handoff() {
        assert_eq!(
            resolve_worker_api_key_from_reader(Some("legacy-key".to_string()), false, io::empty(),)
                .unwrap(),
            Some("legacy-key".to_string())
        );
        assert!(
            resolve_worker_api_key_from_reader(Some("key".to_string()), true, io::empty()).is_err()
        );
    }

    #[test]
    fn worker_key_stdin_handoff_is_bounded() {
        assert_eq!(
            resolve_worker_api_key_from_reader(None, true, io::Cursor::new(b"private-key"))
                .unwrap(),
            Some("private-key".to_string())
        );
        let oversized = vec![b'x'; MAX_WORKER_API_KEY_BYTES as usize + 1];
        assert!(
            resolve_worker_api_key_from_reader(None, true, io::Cursor::new(oversized)).is_err()
        );
    }
}
