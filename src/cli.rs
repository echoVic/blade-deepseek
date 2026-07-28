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
    /// Product version inherited from the parent executable.
    #[arg(long, hide = true, default_value = env!("CARGO_PKG_VERSION"))]
    app_version: String,

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

impl From<ExecArgs> for orca_runtime::command::exec::ExecCommandRequest {
    fn from(args: ExecArgs) -> Self {
        Self {
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            output_format: args.output_format.into(),
            cwd: args.cwd,
            approval_mode: args.approval_mode,
            model: args.model,
            api_key: args.api_key,
            base_url: args.base_url,
            verifier: args.verifier,
            max_budget: args.max_budget,
            resume: args.resume,
            fork: args.fork,
            continue_latest: args.continue_latest,
            no_history: args.no_history,
            save_history: args.save_history,
            provider: args.provider,
            prompt: args.prompt,
        }
    }
}

impl From<HistoryArgs> for orca_runtime::command::history::HistoryCommandRequest {
    fn from(args: HistoryArgs) -> Self {
        use orca_runtime::command::history::HistoryCommandRequest;

        match args.command {
            HistoryCommand::List { limit, all } => HistoryCommandRequest::List { limit, all },
            HistoryCommand::Show { session } => HistoryCommandRequest::Show { session },
            HistoryCommand::Archive { session } => HistoryCommandRequest::Archive { session },
            HistoryCommand::Delete { session } => HistoryCommandRequest::Delete { session },
            HistoryCommand::Rename { session, title } => {
                HistoryCommandRequest::Rename { session, title }
            }
            HistoryCommand::Search { query, all } => HistoryCommandRequest::Search { query, all },
            HistoryCommand::Compress { session } => HistoryCommandRequest::Compress { session },
        }
    }
}

impl From<TrustArgs> for orca_runtime::command::trust::TrustCommandRequest {
    fn from(args: TrustArgs) -> Self {
        use orca_runtime::command::trust::TrustAction as RuntimeTrustAction;

        Self {
            cwd: args.cwd,
            action: match args.action {
                TrustAction::Show => RuntimeTrustAction::Show,
                TrustAction::Add => RuntimeTrustAction::Add,
                TrustAction::Remove => RuntimeTrustAction::Remove,
            },
        }
    }
}

impl From<SubagentWorkerArgs> for orca_runtime::command::launch::SubagentWorkerLaunchRequest {
    fn from(args: SubagentWorkerArgs) -> Self {
        Self {
            app_version: args.app_version,
            cwd: args.cwd,
            child_cwd: args.child_cwd,
            provider: args.provider,
            model: args.model,
            api_key: args.api_key,
            api_key_stdin: args.api_key_stdin,
            base_url: args.base_url,
            session_id: args.session_id,
            agent_id: args.agent_id,
            subagent_depth: args.subagent_depth,
            request_json: args.request_json,
            worktree_repo_root: args.worktree_repo_root,
            worktree_path: args.worktree_path,
        }
    }
}

pub fn run() -> i32 {
    let cli = Cli::parse();

    if matches!(cli.mode.as_deref(), Some("server")) {
        return orca_runtime::command::launch::run_protocol(protocol_request(
            cli,
            orca_runtime::command::launch::ProtocolMode::Server,
        ));
    }
    if matches!(cli.mode.as_deref(), Some("acp")) {
        return orca_runtime::command::launch::run_protocol(protocol_request(
            cli,
            orca_runtime::command::launch::ProtocolMode::Acp,
        ));
    }

    match cli.command {
        Some(Command::Exec(args)) => orca_runtime::command::exec::run(args.into()),
        Some(Command::History(args)) => orca_runtime::command::history::run(args.into()),
        Some(Command::Workflow(args)) => orca_runtime::workflow::command::run(args.into()),
        Some(Command::Trust(args)) => orca_runtime::command::trust::run(args.into()),
        Some(Command::SubagentWorker(args)) => {
            orca_runtime::command::launch::run_subagent_worker(args.into())
        }
        None => {
            let request = orca_runtime::command::launch::InteractiveLaunchRequest {
                app_version: env!("CARGO_PKG_VERSION").to_string(),
                resume: cli.resume,
                fork: cli.fork,
                continue_latest: cli.continue_latest,
                session_picker: cli.session_picker,
                model: cli.model,
                mode: cli.mode,
                api_key: cli.api_key,
                base_url: cli.base_url,
                provider: cli.provider,
                prompt: cli.prompt,
            };
            match orca_runtime::command::launch::prepare_interactive(request) {
                Ok(config) => orca_tui::cli::run(config),
                Err(error) => {
                    eprintln!("orca: {error}");
                    1
                }
            }
        }
    }
}

fn protocol_request(
    cli: Cli,
    mode: orca_runtime::command::launch::ProtocolMode,
) -> orca_runtime::command::launch::ProtocolLaunchRequest {
    orca_runtime::command::launch::ProtocolLaunchRequest {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        mode,
        has_command: cli.command.is_some(),
        prompt: cli.prompt,
        cwd: cli.cwd,
        provider: cli.provider,
        model: cli.model,
        api_key: cli.api_key,
        base_url: cli.base_url,
    }
}
