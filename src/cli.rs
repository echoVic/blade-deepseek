use std::env;
use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::approval::policy::ApprovalMode;
use crate::config::file;
use crate::config::{OutputFormat, ProviderKind, RunConfig};
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
    /// Run a task and emit events.
    Exec(ExecArgs),
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

    /// Provider implementation (internal, for testing).
    #[arg(long, value_enum, default_value_t = ProviderKind::DeepSeek, hide = true)]
    provider: ProviderKind,

    /// Prompt to execute.
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
    let file_config = file::load_user_config();

    let prompt = args.prompt.join(" ");

    let api_key = env::var("DEEPSEEK_API_KEY")
        .ok()
        .or(file_config.api_key);

    let base_url = args
        .base_url
        .or_else(|| env::var("DEEPSEEK_BASE_URL").ok())
        .or(file_config.base_url);

    let model = args
        .model
        .or_else(|| env::var("DEEPSEEK_MODEL").ok())
        .or(file_config.model);

    let config = RunConfig {
        prompt,
        cwd: args.cwd,
        output_format: args.output_format.into(),
        approval_mode: args.approval_mode,
        provider: args.provider,
        verifier: args.verifier,
        model,
        api_key,
        base_url,
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
