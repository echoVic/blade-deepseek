use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::approval::policy::ApprovalMode;
use crate::config::{OutputFormat, RunConfig};
use crate::runtime::controller;

#[derive(Debug, Parser)]
#[command(name = "orca")]
#[command(version)]
#[command(about = "A DeepSeek-native coding agent runtime by Blade.")]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Prompt to run in the default interactive placeholder.
    prompt: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a task without the TUI and emit machine-readable events.
    Exec(ExecArgs),
}

#[derive(Debug, Parser)]
struct ExecArgs {
    /// Output format for the run.
    #[arg(long, value_enum, default_value_t = OutputFormatArg::Jsonl)]
    output_format: OutputFormatArg,

    /// Workspace directory.
    #[arg(long)]
    cwd: Option<PathBuf>,

    /// Approval policy for tool actions.
    #[arg(long, value_enum, default_value_t = ApprovalMode::WorkspaceWrite)]
    approval_mode: ApprovalMode,

    /// Maximum turns for this run.
    #[arg(long)]
    max_turns: Option<u32>,

    /// Optional verifier command.
    #[arg(long)]
    verifier: Option<String>,

    /// Prompt to execute. If omitted, stdin support will be added later.
    prompt: Vec<String>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
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
        None => run_placeholder(cli.prompt),
    }
}

fn run_exec(args: ExecArgs) -> i32 {
    let prompt = args.prompt.join(" ");
    let config = RunConfig {
        prompt,
        cwd: args.cwd,
        output_format: args.output_format.into(),
        approval_mode: args.approval_mode,
        max_turns: args.max_turns,
        verifier: args.verifier,
    };

    controller::run(config)
}

fn run_placeholder(prompt: Vec<String>) -> i32 {
    if prompt.is_empty() {
        println!("Orca");
        println!("A DeepSeek-native coding agent runtime by Blade.");
        println!();
        println!("Run `orca --help` for usage.");
    } else {
        println!("Orca runtime is not implemented yet.");
        println!("Received: {}", prompt.join(" "));
    }

    0
}
