use std::collections::HashMap;
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
pub const DEFAULT_MAX_WORKFLOW_AGENT_RETRIES: u32 = 1;
pub const MAX_WORKFLOW_AGENT_RETRIES: u32 = 5;

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
    #[serde(default = "default_shell_timeout_secs")]
    pub shell_timeout_secs: u64,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            max_read_parallel: DEFAULT_MAX_READ_PARALLEL_TOOLS,
            output_truncation: ToolOutputTruncation::default(),
            shell_timeout_secs: default_shell_timeout_secs(),
        }
    }
}

impl ToolConfig {
    const MAX_READ_PARALLEL_UPPER: usize = 32;
    const MAX_SHELL_TIMEOUT_SECS: u64 = 3600;

    pub fn normalized(mut self) -> Self {
        if self.max_read_parallel == 0 {
            self.max_read_parallel = 1;
        } else if self.max_read_parallel > Self::MAX_READ_PARALLEL_UPPER {
            self.max_read_parallel = Self::MAX_READ_PARALLEL_UPPER;
        }
        if self.shell_timeout_secs == 0 {
            self.shell_timeout_secs = 1;
        } else if self.shell_timeout_secs > Self::MAX_SHELL_TIMEOUT_SECS {
            self.shell_timeout_secs = Self::MAX_SHELL_TIMEOUT_SECS;
        }
        self.output_truncation = self.output_truncation.normalized();
        self
    }
}

fn default_max_read_parallel() -> usize {
    DEFAULT_MAX_READ_PARALLEL_TOOLS
}

fn default_shell_timeout_secs() -> u64 {
    120
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowTeamConfig {
    #[serde(default)]
    pub max_agent_retries: Option<u32>,
    #[serde(default)]
    pub max_agent_tokens: Option<u64>,
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
}

impl WorkflowTeamConfig {
    pub fn normalized(mut self) -> Self {
        if let Some(max_agent_retries) = self.max_agent_retries {
            self.max_agent_retries = Some(max_agent_retries.min(MAX_WORKFLOW_AGENT_RETRIES));
        }
        if let Some(max_agent_tokens) = self.max_agent_tokens {
            self.max_agent_tokens = Some(max_agent_tokens.max(1));
        }
        self.allowed_tools = self.allowed_tools.map(|tools| {
            tools
                .into_iter()
                .map(|tool| tool.trim().to_string())
                .filter(|tool| !tool.is_empty())
                .collect::<Vec<_>>()
        });
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|tools| tools.is_empty())
        {
            self.allowed_tools = Some(Vec::new());
        }
        self
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowConfig {
    #[serde(default = "default_workflows_enabled")]
    pub enabled: bool,
    #[serde(default = "default_max_workflow_concurrent_agents")]
    pub max_concurrent_agents: usize,
    #[serde(default = "default_max_workflow_agents_per_run")]
    pub max_agents_per_run: u32,
    #[serde(default = "default_max_workflow_agent_retries")]
    pub max_agent_retries: u32,
    #[serde(default)]
    pub max_agent_tokens: Option<u64>,
    #[serde(default = "default_workflow_keyword_trigger_enabled")]
    pub keyword_trigger_enabled: bool,
    #[serde(default)]
    pub teams: HashMap<String, WorkflowTeamConfig>,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent_agents: DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS,
            max_agents_per_run: DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN,
            max_agent_retries: DEFAULT_MAX_WORKFLOW_AGENT_RETRIES,
            max_agent_tokens: None,
            keyword_trigger_enabled: true,
            teams: HashMap::new(),
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

fn default_max_workflow_agent_retries() -> u32 {
    DEFAULT_MAX_WORKFLOW_AGENT_RETRIES
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
    pub active_permission_profile: Option<ActivePermissionProfile>,
    pub permission_profiles: HashMap<String, PermissionProfileConfig>,
    pub runtime_workspace_roots: Option<Vec<PathBuf>>,
    pub permission_rules: PermissionRules,
    pub additional_working_directories: Vec<AdditionalWorkingDirectory>,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdditionalWorkingDirectory {
    pub path: PathBuf,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActivePermissionProfile {
    pub id: String,
    pub extends: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionProfileConfig {
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub filesystem: PermissionProfileFilesystemConfig,
    #[serde(default)]
    pub network: PermissionProfileNetworkConfig,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct PermissionProfileFilesystemConfig {
    entries: HashMap<PathBuf, PermissionProfileFileAccess>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum PermissionProfileFilesystemEntry {
    Access(PermissionProfileFileAccess),
    Scoped(HashMap<PathBuf, PermissionProfileFileAccess>),
}

impl<'de> Deserialize<'de> for PermissionProfileFilesystemConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = HashMap::<PathBuf, PermissionProfileFilesystemEntry>::deserialize(deserializer)?;
        let mut entries = HashMap::new();
        for (path, entry) in raw {
            let path = normalize_permission_profile_filesystem_path(path);
            match entry {
                PermissionProfileFilesystemEntry::Access(access) => {
                    entries.insert(path, access);
                }
                PermissionProfileFilesystemEntry::Scoped(scoped) => {
                    for (subpath, access) in scoped {
                        entries.insert(
                            normalize_permission_profile_filesystem_path(path.join(subpath)),
                            access,
                        );
                    }
                }
            }
        }
        Ok(Self { entries })
    }
}

fn normalize_permission_profile_filesystem_path(path: PathBuf) -> PathBuf {
    let Some(path_str) = path.to_str() else {
        return path;
    };
    let Some(stripped) = path_str.strip_suffix("/**") else {
        return path;
    };
    if stripped.is_empty() {
        return path;
    }
    PathBuf::from(stripped)
}

impl PermissionProfileFilesystemConfig {
    pub fn get(&self, path: &std::path::Path) -> Option<&PermissionProfileFileAccess> {
        self.entries.get(path)
    }

    pub fn entries(&self) -> impl Iterator<Item = (&PathBuf, &PermissionProfileFileAccess)> {
        self.entries.iter()
    }
}

impl From<HashMap<PathBuf, PermissionProfileFileAccess>> for PermissionProfileFilesystemConfig {
    fn from(entries: HashMap<PathBuf, PermissionProfileFileAccess>) -> Self {
        Self { entries }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionProfileNetworkConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub domains: PermissionProfileNetworkDomainsConfig,
    #[serde(default)]
    pub unix_sockets: PermissionProfileNetworkUnixSocketsConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionProfileNetworkDomainsConfig {
    #[serde(flatten)]
    entries: HashMap<String, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PermissionProfileNetworkUnixSocketsConfig {
    #[serde(flatten)]
    entries: HashMap<PathBuf, PermissionProfileNetworkAccess>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionProfileNetworkAccess {
    Allow,
    Deny,
}

impl PermissionProfileNetworkDomainsConfig {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, domain: &str) -> Option<&PermissionProfileNetworkAccess> {
        self.entries.get(domain)
    }
}

impl PermissionProfileNetworkUnixSocketsConfig {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionProfileFileAccess {
    Read,
    Write,
    ReadWrite,
    Deny,
}

impl PermissionProfileFileAccess {
    pub fn allows_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite)
    }

    pub fn allows_write(self) -> bool {
        matches!(self, Self::Write | Self::ReadWrite)
    }

    pub fn denies_write(self) -> bool {
        matches!(self, Self::Deny)
    }
}

impl ActivePermissionProfile {
    pub fn new(id: impl Into<String>, extends: Option<impl Into<String>>) -> Self {
        Self {
            id: id.into(),
            extends: extends.map(Into::into),
        }
    }
}

impl AdditionalWorkingDirectory {
    pub fn new(path: impl Into<PathBuf>, source: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            source: source.into(),
        }
    }
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
    let runtime = runtime_summary(config);

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
            "[runtime]\n",
            "approval = \"{}\"\n",
            "filesystem = \"{}\"\n",
            "network = \"{}\"\n",
            "history = \"{}\"\n",
            "context_window = \"{}\"\n",
            "auto_compact_token_limit = \"{}\"\n",
            "tool_output_truncation = \"{}\"\n",
            "workflow_agents = \"{}\"\n",
            "\n",
            "[tools]\n",
            "max_read_parallel = {}\n",
            "output_truncation = \"{}\"\n",
            "shell_timeout_secs = {}\n",
            "\n",
            "[subagents]\n",
            "max_depth = {}\n",
            "max_parallel = {}\n",
            "\n",
            "[counts]\n",
            "mcp_servers = {}\n",
            "external_tools = {}\n",
            "hooks = {}\n",
            "permission_rules = {}\n",
            "additional_working_directories = {}"
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
        runtime.approval,
        runtime.filesystem,
        runtime.network,
        runtime.history,
        runtime.context_window,
        runtime.auto_compact_token_limit,
        runtime.tool_output_truncation,
        runtime.workflow_agents,
        config.tools.max_read_parallel,
        config.tools.output_truncation,
        config.tools.shell_timeout_secs,
        config.subagents.max_depth,
        config.subagents.max_parallel,
        config.mcp_servers.len(),
        config.external_tools.len(),
        config.hooks.len(),
        config.permission_rules.rules.len(),
        config.additional_working_directories.len()
    )
}

struct RuntimeSummary {
    approval: &'static str,
    filesystem: &'static str,
    network: &'static str,
    history: &'static str,
    context_window: String,
    auto_compact_token_limit: String,
    tool_output_truncation: String,
    workflow_agents: String,
}

fn runtime_summary(config: &RunConfig) -> RuntimeSummary {
    RuntimeSummary {
        approval: config.approval_mode.as_str(),
        filesystem: filesystem_posture(config.approval_mode),
        network: network_posture(config),
        history: history_posture(&config.history_mode),
        context_window: config
            .model_runtime
            .context_window
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<model-default>".to_string()),
        auto_compact_token_limit: config
            .model_runtime
            .auto_compact_token_limit
            .map(|value| value.to_string())
            .unwrap_or_else(|| "<model-default>".to_string()),
        tool_output_truncation: config.tools.output_truncation.to_string(),
        workflow_agents: format!(
            "max_parallel={}, max_per_run={}, max_agent_tokens={}",
            config.workflows.max_concurrent_agents,
            config.workflows.max_agents_per_run,
            config
                .workflows
                .max_agent_tokens
                .map(|value| value.to_string())
                .unwrap_or_else(|| "<unset>".to_string())
        ),
    }
}

fn filesystem_posture(mode: ApprovalMode) -> &'static str {
    match mode {
        ApprovalMode::Plan => "read-only",
        ApprovalMode::Suggest => "approval-required",
        ApprovalMode::AutoEdit | ApprovalMode::FullAuto => "workspace-write",
    }
}

fn network_posture(config: &RunConfig) -> &'static str {
    if config.provider == ProviderKind::Mock && config.mcp_servers.is_empty() {
        "not-configured"
    } else {
        "allowed"
    }
}

fn history_posture(history_mode: &HistoryMode) -> &'static str {
    match history_mode {
        HistoryMode::Record => "recording",
        HistoryMode::Disabled => "disabled",
        HistoryMode::Resume(_) => "resume",
        HistoryMode::Fork(_) => "fork",
    }
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
            provider: ProviderKind::DeepSeekFixture,
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
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
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
        assert!(shown.contains("[runtime]"));
        assert!(shown.contains("filesystem = \"workspace-write\""));
        assert!(shown.contains("network = \"allowed\""));
        assert!(shown.contains("approval = \"full-auto\""));
        assert!(shown.contains("history = \"disabled\""));
        assert!(shown.contains("api_key = \"<redacted>\""));
        assert!(!shown.contains("sk-secret"));
    }
}
