use std::path::PathBuf;

use crate::approval::policy::ApprovalMode;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    Jsonl,
    Text,
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub prompt: String,
    pub cwd: Option<PathBuf>,
    pub output_format: OutputFormat,
    pub approval_mode: ApprovalMode,
    pub max_turns: Option<u32>,
    pub verifier: Option<String>,
}
