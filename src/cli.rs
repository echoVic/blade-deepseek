use std::env;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::approval::policy::ApprovalMode;
use crate::config::file;
use crate::config::{HistoryMode, OutputFormat, ProviderKind, RunConfig};
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
    #[arg(long, value_enum, default_value_t = ApprovalMode::Suggest)]
    approval_mode: ApprovalMode,

    /// Model to use (overrides config file and DEEPSEEK_MODEL env).
    #[arg(long)]
    model: Option<String>,

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

    match cli.command {
        Some(Command::Exec(args)) => run_exec(args),
        Some(Command::History(args)) => run_history(args),
        None => run_placeholder(cli),
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

    let file_config = file::load_user_config();

    let prompt = args.prompt.join(" ");

    let api_key = env::var("DEEPSEEK_API_KEY").ok().or(file_config.api_key);

    let base_url = args
        .base_url
        .or_else(|| env::var("DEEPSEEK_BASE_URL").ok())
        .or(file_config.base_url);

    let model = args
        .model
        .or_else(|| env::var("DEEPSEEK_MODEL").ok())
        .or(file_config.model);

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
        approval_mode: args.approval_mode,
        provider: args.provider,
        verifier: args.verifier,
        model,
        api_key,
        base_url,
        history_mode,
        show_session_picker: false,
        permission_rules: file_config.permissions,
        max_budget_usd: args.max_budget,
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

fn print_message(message: crate::provider::conversation::Message) {
    use crate::provider::conversation::Message;

    match message {
        Message::System(content) => println!("[system]\n{}\n", content.trim()),
        Message::User(content) => println!("[user]\n{}\n", content.trim()),
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
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

    let file_config = file::load_user_config();

    let api_key = env::var("DEEPSEEK_API_KEY").ok().or(file_config.api_key);

    let base_url = env::var("DEEPSEEK_BASE_URL").ok().or(file_config.base_url);

    let model = env::var("DEEPSEEK_MODEL").ok().or(file_config.model);

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
        approval_mode: ApprovalMode::Suggest,
        provider: ProviderKind::DeepSeek,
        verifier: None,
        model,
        api_key,
        base_url,
        history_mode,
        show_session_picker: cli.session_picker,
        permission_rules: file_config.permissions,
        max_budget_usd: None,
    };

    app::run_tui(config)
}
