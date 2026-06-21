use std::path::PathBuf;

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::approval_rules::PermissionRules;
use crate::approval_types::ApprovalMode;
use crate::external_config::ExternalToolConfig;
use crate::hook_types::HookConfig;
use crate::mcp_types::McpServerConfig;
use crate::model::ModelSelection;
use crate::subagent_config::SubagentConfig;
use crate::tool_types::ToolOutputTruncation;

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
pub const DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS: usize = 16;
pub const DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN: u32 = 1000;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelRuntimeConfig {
    #[serde(default)]
    pub context_window: Option<usize>,
    #[serde(default)]
    pub auto_compact_token_limit: Option<usize>,
}

impl ModelRuntimeConfig {
    pub fn normalized(self) -> Self {
        Self {
            context_window: self.context_window.map(|value| value.max(1)),
            auto_compact_token_limit: self.auto_compact_token_limit.map(|value| value.max(1)),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ToolConfig {
    #[serde(default = "default_max_read_parallel")]
    pub max_read_parallel: usize,
    #[serde(default)]
    pub output_truncation: ToolOutputTruncation,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            max_read_parallel: DEFAULT_MAX_READ_PARALLEL_TOOLS,
            output_truncation: ToolOutputTruncation::default(),
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
        self.output_truncation = self.output_truncation.normalized();
        self
    }
}

fn default_max_read_parallel() -> usize {
    DEFAULT_MAX_READ_PARALLEL_TOOLS
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowConfig {
    #[serde(default = "default_workflows_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_workflow_concurrent_agents")]
    pub max_concurrent_agents: usize,
    #[serde(default = "default_max_workflow_agents_per_run")]
    pub max_agents_per_run: u32,
    #[serde(default = "default_workflow_keyword_trigger_enabled")]
    pub keyword_trigger_enabled: bool,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent_agents: DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS,
            max_agents_per_run: DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN,
            keyword_trigger_enabled: true,
        }
    }
}

fn default_workflows_enabled() -> bool {
    true
}

fn default_max_workflow_concurrent_agents() -> usize {
    DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS
}

fn default_max_workflow_agents_per_run() -> u32 {
    DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN
}

fn default_workflow_keyword_trigger_enabled() -> bool {
    true
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub app_version: String,
    pub prompt: String,
    pub cwd: Option<PathBuf>,
    pub output_format: OutputFormat,
    pub approval_mode: ApprovalMode,
    pub provider: ProviderKind,
    pub verifier: Option<String>,
    pub model: ModelSelection,
    pub model_runtime: ModelRuntimeConfig,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub mcp_servers: Vec<McpServerConfig>,
    pub hooks: Vec<HookConfig>,
    pub external_tools: Vec<ExternalToolConfig>,
    pub history_mode: HistoryMode,
    pub show_session_picker: bool,
    pub permission_rules: PermissionRules,
    pub max_budget_usd: Option<f64>,
    pub subagents: SubagentConfig,
    pub tools: ToolConfig,
    pub workflows: WorkflowConfig,
    pub theme: ThemeName,
    pub vim_mode: bool,
    pub update_check: bool,
    pub desktop_notifications: bool,
    pub auto_memory: bool,
}

pub fn format_config_show(config: &RunConfig) -> String {
    let api_key = if config.api_key.is_some() {
        "<redacted>"
    } else {
        "<unset>"
    };
    let base_url = config.base_url.as_deref().unwrap_or("<default>");
    let cwd = config
        .cwd
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<current>".to_string());
    let verifier = config.verifier.as_deref().unwrap_or("<unset>");
    let max_budget = config
        .max_budget_usd
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "<unset>".to_string());

    format!(
        concat!(
            "model = \"{}\"\n",
            "mode = \"{}\"\n",
            "api_key = \"{}\"\n",
            "base_url = \"{}\"\n",
            "provider = \"{}\"\n",
            "model_context_window = \"{}\"\n",
            "model_auto_compact_token_limit = \"{}\"\n",
            "cwd = \"{}\"\n",
            "verifier = \"{}\"\n",
            "max_budget_usd = \"{}\"\n",
            "theme = \"{:?}\"\n",
            "vim_mode = {}\n",
            "update_check = {}\n",
            "desktop_notifications = {}\n",
            "auto_memory = {}\n",
            "\n",
            "[tools]\n",
            "max_read_parallel = {}\n",
            "output_truncation = \"{}\"\n",
            "\n",
            "[subagents]\n",
            "max_depth = {}\n",
            "max_parallel = {}\n",
            "\n",
            "[counts]\n",
            "mcp_servers = {}\n",
            "external_tools = {}\n",
            "hooks = {}\n",
            "permission_rules = {}"
        ),
        config.model.display_name(),
        config.approval_mode.as_str(),
        api_key,
        base_url,
        config.provider.as_str(),
        config
            .model_runtime
            .context_window
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<default>".to_string()),
        config
            .model_runtime
            .auto_compact_token_limit
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<default>".to_string()),
        cwd,
        verifier,
        max_budget,
        config.theme,
        config.vim_mode,
        config.update_check,
        config.desktop_notifications,
        config.auto_memory,
        config.tools.max_read_parallel,
        config.tools.output_truncation,
        config.subagents.max_depth,
        config.subagents.max_parallel,
        config.mcp_servers.len(),
        config.external_tools.len(),
        config.hooks.len(),
        config.permission_rules.rules.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval_types::ApprovalMode;
    use crate::model::ModelSelection;

    #[test]
    fn format_config_show_redacts_api_key_and_includes_effective_values() {
        let config = RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("deepseek-v4-flash".to_string())),
            model_runtime: ModelRuntimeConfig {
                context_window: Some(128_000),
                auto_compact_token_limit: Some(96_000),
            },
            api_key: Some("sk-secret".to_string()),
            base_url: Some("https://api.example".to_string()),
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            permission_rules: PermissionRules::default(),
            max_budget_usd: Some(1.25),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: true,
            update_check: false,
            desktop_notifications: true,
            auto_memory: true,
        };

        let shown = format_config_show(&config);

        assert!(shown.contains("model = \"deepseek-v4-flash\""));
        assert!(shown.contains("model_context_window = \"128000\""));
        assert!(shown.contains("model_auto_compact_token_limit = \"96000\""));
        assert!(shown.contains("mode = \"full-auto\""));
        assert!(shown.contains("api_key = \"<redacted>\""));
        assert!(!shown.contains("sk-secret"));
    }
}
