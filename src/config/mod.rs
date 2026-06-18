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
            "\n",
            "[subagents]\n",
            "max_depth = {}\n",
            "max_parallel = {}\n",
            "\n",
            "[counts]\n",
            "mcp_servers = {}\n",
            "hooks = {}\n",
            "permission_rules = {}"
        ),
        config.model.display_name(),
        config.approval_mode.as_str(),
        api_key,
        base_url,
        config.provider.as_str(),
        cwd,
        verifier,
        max_budget,
        config.theme,
        config.vim_mode,
        config.update_check,
        config.desktop_notifications,
        config.auto_memory,
        config.tools.max_read_parallel,
        config.subagents.max_depth,
        config.subagents.max_parallel,
        config.mcp_servers.len(),
        config.hooks.len(),
        config.permission_rules.rules.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::policy::ApprovalMode;
    use crate::model::ModelSelection;

    #[test]
    fn format_config_show_redacts_api_key_and_includes_effective_values() {
        let config = RunConfig {
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("deepseek-v4-flash".to_string())),
            api_key: Some("sk-secret".to_string()),
            base_url: Some("https://api.example".to_string()),
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            permission_rules: PermissionRules::default(),
            max_budget_usd: Some(1.25),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: true,
            update_check: false,
            desktop_notifications: true,
            auto_memory: true,
        };

        let shown = format_config_show(&config);

        assert!(shown.contains("model = \"deepseek-v4-flash\""));
        assert!(shown.contains("mode = \"full-auto\""));
        assert!(shown.contains("api_key = \"<redacted>\""));
        assert!(!shown.contains("sk-secret"));
    }
}
