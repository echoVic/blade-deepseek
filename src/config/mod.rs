use std::path::PathBuf;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::approval::policy::{ApprovalMode, PermissionRules};
use crate::mcp::types::McpServerConfig;
use crate::model::ModelSelection;
use crate::runtime::hooks::HookConfig;
use crate::runtime::subagent_config::SubagentConfig;

pub mod file;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
    Solarized,
    Catppuccin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    Jsonl,
    Text,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Mock,
    #[value(name = "deepseek-fixture")]
    DeepSeekFixture,
    #[value(name = "deepseek")]
    DeepSeek,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::DeepSeekFixture => "deepseek-fixture",
            Self::DeepSeek => "deepseek",
        }
    }
}

#[derive(Clone, Debug)]
pub enum HistoryMode {
    Record,
    Disabled,
    Resume(String),
    Fork(String),
}

pub const DEFAULT_MAX_READ_PARALLEL_TOOLS: usize = 8;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolConfig {
    #[serde(default = "default_max_read_parallel")]
    pub max_read_parallel: usize,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            max_read_parallel: DEFAULT_MAX_READ_PARALLEL_TOOLS,
        }
    }
}

impl ToolConfig {
    const MAX_READ_PARALLEL_UPPER: usize = 32;

    pub fn normalized(mut self) -> Self {
        if self.max_read_parallel == 0 {
            self.max_read_parallel = 1;
        } else if self.max_read_parallel > Self::MAX_READ_PARALLEL_UPPER {
            self.max_read_parallel = Self::MAX_READ_PARALLEL_UPPER;
        }
        self
    }
}

fn default_max_read_parallel() -> usize {
    DEFAULT_MAX_READ_PARALLEL_TOOLS
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub prompt: String,
    pub cwd: Option<PathBuf>,
    pub output_format: OutputFormat,
    pub approval_mode: ApprovalMode,
    pub provider: ProviderKind,
    pub verifier: Option<String>,
    pub model: ModelSelection,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub mcp_servers: Vec<McpServerConfig>,
    pub hooks: Vec<HookConfig>,
    pub history_mode: HistoryMode,
    pub show_session_picker: bool,
    pub permission_rules: PermissionRules,
    pub max_budget_usd: Option<f64>,
    pub subagents: SubagentConfig,
    pub tools: ToolConfig,
    pub theme: ThemeName,
    pub vim_mode: bool,
    pub update_check: bool,
    pub desktop_notifications: bool,
    pub auto_memory: bool,
}
