use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use toml::Value;

use crate::approval_rules::PermissionRules;
use crate::approval_types::ApprovalMode;
use crate::config::{
    DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN, DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS,
    MAX_WORKFLOW_AGENT_RETRIES, ModelRuntimeConfig, ThemeName, ToolConfig, WorkflowConfig,
};
use crate::subagent_config::SubagentConfig;

const ORCA_HOME_ENV: &str = "ORCA_HOME";

#[derive(Clone, Debug, Deserialize)]
#[serde(from = "RawFileConfig")]
pub struct FileConfig {
    pub model: Option<String>,
    pub mode: Option<ApprovalMode>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    #[serde(default)]
    pub model_runtime: ModelRuntimeConfig,
    #[serde(default)]
    pub mcp_servers: Vec<crate::mcp_types::McpServerConfig>,
    #[serde(default)]
    pub hooks: Vec<crate::hook_types::HookConfig>,
    #[serde(default)]
    pub permissions: PermissionRules,
    #[serde(default)]
    pub subagents: SubagentConfig,
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub workflows: WorkflowFileConfig,
    #[serde(default)]
    pub theme: ThemeName,
    #[serde(default)]
    pub vim_mode: bool,
    #[serde(default = "default_true")]
    pub update_check: bool,
    #[serde(default)]
    pub desktop_notifications: bool,
    #[serde(default)]
    pub auto_memory: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct RawFileConfig {
    pub model: Option<String>,
    pub mode: Option<ApprovalMode>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    #[serde(default, alias = "disableWorkflows")]
    legacy_disable_workflows: Option<bool>,
    #[serde(default, alias = "enableWorkflows")]
    legacy_enable_workflows: Option<bool>,
    #[serde(default, alias = "workflowKeywordTriggerEnabled")]
    legacy_workflow_keyword_trigger_enabled: Option<bool>,
    #[serde(default)]
    pub model_runtime: ModelRuntimeConfig,
    #[serde(default)]
    pub mcp_servers: Vec<crate::mcp_types::McpServerConfig>,
    #[serde(default)]
    pub hooks: Vec<crate::hook_types::HookConfig>,
    #[serde(default)]
    pub permissions: PermissionRules,
    #[serde(default)]
    pub subagents: SubagentConfig,
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub workflows: WorkflowFileConfig,
    #[serde(default)]
    pub theme: ThemeName,
    #[serde(default)]
    pub vim_mode: bool,
    #[serde(default = "default_true")]
    pub update_check: bool,
    #[serde(default)]
    pub desktop_notifications: bool,
    #[serde(default)]
    pub auto_memory: bool,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            model: None,
            mode: None,
            api_key: None,
            base_url: None,
            model_runtime: ModelRuntimeConfig::default(),
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            permissions: PermissionRules::default(),
            subagents: SubagentConfig::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowFileConfig::default(),
            theme: ThemeName::default(),
            vim_mode: false,
            update_check: true,
            desktop_notifications: false,
            auto_memory: false,
        }
    }
}

impl From<RawFileConfig> for FileConfig {
    fn from(raw: RawFileConfig) -> Self {
        let mut workflows = raw.workflows;
        workflows.apply_legacy_top_level_aliases(
            raw.legacy_disable_workflows,
            raw.legacy_enable_workflows,
            raw.legacy_workflow_keyword_trigger_enabled,
        );

        Self {
            model: raw.model,
            mode: raw.mode,
            api_key: raw.api_key,
            base_url: raw.base_url,
            model_runtime: raw.model_runtime.normalized(),
            mcp_servers: raw.mcp_servers,
            hooks: raw.hooks,
            permissions: raw.permissions,
            subagents: raw.subagents,
            tools: raw.tools,
            workflows,
            theme: raw.theme,
            vim_mode: raw.vim_mode,
            update_check: raw.update_check,
            desktop_notifications: raw.desktop_notifications,
            auto_memory: raw.auto_memory,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ConfigOverrides {
    pub model: Option<String>,
    pub mode: Option<ApprovalMode>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct WorkflowFileConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    #[serde(alias = "disableWorkflows")]
    pub disable_workflows: Option<bool>,
    #[serde(default)]
    #[serde(alias = "enableWorkflows")]
    pub enable_workflows: Option<bool>,
    #[serde(default)]
    pub max_concurrent_agents: Option<usize>,
    #[serde(default)]
    pub max_agents_per_run: Option<u32>,
    #[serde(default)]
    pub max_agent_retries: Option<u32>,
    #[serde(default)]
    pub max_agent_tokens: Option<u64>,
    #[serde(default)]
    #[serde(alias = "workflowKeywordTriggerEnabled")]
    pub workflow_keyword_trigger_enabled: Option<bool>,
}

impl WorkflowFileConfig {
    pub fn resolved(&self) -> WorkflowConfig {
        let mut config = WorkflowConfig::default();

        if let Some(enabled) = self.enabled {
            config.enabled = enabled;
        }
        if let Some(enable_workflows) = self.enable_workflows {
            config.enabled = enable_workflows;
        }
        if self.disable_workflows.unwrap_or(false) {
            config.enabled = false;
        }
        if let Some(max_concurrent_agents) = self.max_concurrent_agents {
            config.max_concurrent_agents =
                max_concurrent_agents.min(DEFAULT_MAX_WORKFLOW_CONCURRENT_AGENTS);
        }
        if let Some(max_agents_per_run) = self.max_agents_per_run {
            config.max_agents_per_run = max_agents_per_run.min(DEFAULT_MAX_WORKFLOW_AGENTS_PER_RUN);
        }
        if let Some(max_agent_retries) = self.max_agent_retries {
            config.max_agent_retries = max_agent_retries.min(MAX_WORKFLOW_AGENT_RETRIES);
        }
        if let Some(max_agent_tokens) = self.max_agent_tokens {
            config.max_agent_tokens = Some(max_agent_tokens.max(1));
        }
        if let Some(keyword_trigger_enabled) = self.workflow_keyword_trigger_enabled {
            config.keyword_trigger_enabled = keyword_trigger_enabled;
        }

        config
    }

    fn apply_legacy_top_level_aliases(
        &mut self,
        disable_workflows: Option<bool>,
        enable_workflows: Option<bool>,
        workflow_keyword_trigger_enabled: Option<bool>,
    ) {
        let nested_enabled_present = self.enabled.is_some()
            || self.enable_workflows.is_some()
            || self.disable_workflows.is_some();
        if !nested_enabled_present {
            if disable_workflows.unwrap_or(false) {
                self.enabled = Some(false);
            } else if let Some(enable_workflows) = enable_workflows {
                self.enabled = Some(enable_workflows);
            }
        }
        if self.workflow_keyword_trigger_enabled.is_none() {
            self.workflow_keyword_trigger_enabled = workflow_keyword_trigger_enabled;
        }
    }
}

fn default_true() -> bool {
    true
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os(ORCA_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".orca")))
}

pub fn load_layered_config(cwd: &Path) -> FileConfig {
    let Some(dir) = config_dir() else {
        return load_layered_config_from_optional_paths(None, cwd);
    };
    load_layered_config_from_optional_paths(Some(&dir.join("config.toml")), cwd)
}

#[cfg(test)]
fn load_layered_config_from_paths(user_path: &Path, project_root: &Path) -> FileConfig {
    load_layered_config_from_optional_paths(Some(user_path), project_root)
}

fn load_layered_config_from_optional_paths(
    user_path: Option<&Path>,
    project_root: &Path,
) -> FileConfig {
    let mut merged = Value::Table(Default::default());
    if let Some(path) = user_path {
        if let Some(user) = load_toml_value(path) {
            merge_toml_values(&mut merged, user);
        }
    }

    if let Some(mut project) = load_toml_value(&project_root.join(".orca/config.toml")) {
        remove_project_denied_fields(&mut project);
        merge_toml_values(&mut merged, project);
    }

    let mut config: FileConfig = match merged.try_into() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("orca: warning: config parse error, using defaults: {error}");
            FileConfig::default()
        }
    };
    if config.api_key.is_none() {
        if let Some(path) = user_path.and_then(Path::parent) {
            config.api_key = load_auth_key(&path.join("auth.json"));
        }
    }
    config
}

fn load_toml_value(path: &Path) -> Option<Value> {
    let content = fs::read_to_string(path).ok()?;
    let mut value = toml::from_str(&content).ok()?;
    fold_legacy_workflow_settings_into_value(&mut value);
    Some(value)
}

fn merge_toml_values(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Table(base), Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_toml_values(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (Value::Array(base), Value::Array(overlay)) => {
            base.extend(overlay);
        }
        (base, overlay) => *base = overlay,
    }
}

fn remove_project_denied_fields(value: &mut Value) {
    if let Some(table) = value.as_table_mut() {
        table.remove("api_key");
        table.remove("base_url");
        table.remove("hooks");
    }
}

fn fold_legacy_workflow_settings_into_value(value: &mut Value) {
    let Some(root) = value.as_table_mut() else {
        return;
    };

    let legacy_enabled = root
        .get("disableWorkflows")
        .and_then(Value::as_bool)
        .filter(|disabled| *disabled)
        .map(|_| false)
        .or_else(|| root.get("enableWorkflows").and_then(Value::as_bool));
    let legacy_keyword = root
        .get("workflowKeywordTriggerEnabled")
        .and_then(Value::as_bool);

    let workflows = root
        .entry("workflows")
        .or_insert_with(|| Value::Table(Default::default()));
    let Some(workflows_table) = workflows.as_table_mut() else {
        return;
    };

    let nested_enabled_present = workflows_table.contains_key("enabled")
        || workflows_table.contains_key("enableWorkflows")
        || workflows_table.contains_key("disableWorkflows");
    if !nested_enabled_present {
        if let Some(enabled) = legacy_enabled {
            workflows_table.insert("enabled".to_string(), Value::Boolean(enabled));
        }
    }

    if !workflows_table.contains_key("workflowKeywordTriggerEnabled") {
        if let Some(keyword_enabled) = legacy_keyword {
            workflows_table.insert(
                "workflowKeywordTriggerEnabled".to_string(),
                Value::Boolean(keyword_enabled),
            );
        }
    }

    root.remove("disableWorkflows");
    root.remove("enableWorkflows");
    root.remove("workflowKeywordTriggerEnabled");
}

pub fn apply_override_layers(
    mut config: FileConfig,
    env: ConfigOverrides,
    cli: ConfigOverrides,
) -> FileConfig {
    apply_overrides(&mut config, env);
    apply_overrides(&mut config, cli);
    config
}

fn apply_overrides(config: &mut FileConfig, overrides: ConfigOverrides) {
    if overrides.model.is_some() {
        config.model = overrides.model;
    }
    if overrides.mode.is_some() {
        config.mode = overrides.mode;
    }
    if overrides.api_key.is_some() {
        config.api_key = overrides.api_key;
    }
    if overrides.base_url.is_some() {
        config.base_url = overrides.base_url;
    }
}

fn load_auth_key(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let map: HashMap<String, String> = serde_json::from_str(&content).ok()?;
    map.get("DEEPSEEK_API_KEY").cloned()
}

pub fn save_api_key(api_key: &str) {
    let Some(dir) = config_dir() else {
        return;
    };
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("auth.json");

    let mut map: HashMap<String, String> = fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default();

    map.insert("DEEPSEEK_API_KEY".to_string(), api_key.to_string());

    if let Ok(content) = serde_json::to_string_pretty(&map) {
        let _ = fs::write(&path, content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_toml(path: &Path) -> FileConfig {
        let Ok(content) = fs::read_to_string(path) else {
            return FileConfig::default();
        };
        toml::from_str(&content).unwrap_or_default()
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
model = "deepseek-v4-flash"
base_url = "https://custom.api.com"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(config.base_url.as_deref(), Some("https://custom.api.com"));
    }

    #[test]
    fn parse_permission_rules() {
        let toml = r#"
[[permissions.rules]]
tool = "bash"
pattern = "cargo *"
decision = "allow"

[[permissions.rules]]
tool = "write_file"
pattern = "/etc/**"
decision = "deny"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.permissions.rules.len(), 2);
        assert_eq!(config.permissions.rules[0].tool, "bash");
        assert_eq!(config.permissions.rules[0].pattern, "cargo *");
        assert_eq!(
            config.permissions.rules[0].decision,
            crate::approval_types::Decision::Allow
        );
        assert_eq!(config.permissions.rules[1].tool, "write_file");
        assert_eq!(config.permissions.rules[1].pattern, "/etc/**");
        assert_eq!(
            config.permissions.rules[1].decision,
            crate::approval_types::Decision::Deny
        );
    }

    #[test]
    fn parse_partial_config() {
        let toml = r#"model = "deepseek-v4-flash""#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("deepseek-v4-flash"));
        assert!(config.api_key.is_none());
        assert!(config.base_url.is_none());
    }

    #[test]
    fn parse_empty_config() {
        let config: FileConfig = toml::from_str("").unwrap();
        assert!(config.model.is_none());
        assert!(config.api_key.is_none());
    }

    #[test]
    fn parse_mcp_servers() {
        let toml = r#"
[[mcp_servers]]
name = "demo"
transport = "stdio"
command = "node"
args = ["server.js"]
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.mcp_servers.len(), 1);
        assert_eq!(config.mcp_servers[0].name, "demo");
    }

    #[test]
    fn parse_hooks() {
        let toml = r#"
[[hooks]]
event = "post_tool_use"
tool = "bash"
command = "echo done"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks[0].tool.as_deref(), Some("bash"));
    }

    #[test]
    fn parse_subagent_config() {
        let toml = r#"
[subagents]
max_depth = 3
max_parallel = 6
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.subagents.max_depth, 3);
        assert_eq!(config.subagents.max_parallel, 6);
    }

    #[test]
    fn parse_tool_config() {
        let toml = r#"
[tools]
max_read_parallel = 5
output_truncation = { mode = "tokens", limit = 512 }
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.tools.max_read_parallel, 5);
        assert_eq!(
            config.tools.output_truncation,
            crate::tool_types::ToolOutputTruncation::tokens(512)
        );
    }

    #[test]
    fn parse_tool_config_normalizes_output_truncation_limit() {
        let toml = r#"
[tools]
max_read_parallel = 0
output_truncation = { mode = "bytes", limit = 0 }
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let normalized = config.tools.normalized();
        assert_eq!(normalized.max_read_parallel, 1);
        assert_eq!(
            normalized.output_truncation,
            crate::tool_types::ToolOutputTruncation::bytes(1)
        );
    }

    #[test]
    fn parse_model_runtime_config() {
        let toml = r#"
[model_runtime]
context_window = 128000
auto_compact_token_limit = 96000
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.model_runtime.context_window, Some(128_000));
        assert_eq!(config.model_runtime.auto_compact_token_limit, Some(96_000));
    }

    #[test]
    fn parse_workflow_config() {
        let toml = r#"
[workflows]
enabled = false
max_concurrent_agents = 7
max_agents_per_run = 99
max_agent_retries = 1
max_agent_tokens = 12345
workflowKeywordTriggerEnabled = false
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let workflows = config.workflows.resolved();
        assert!(!workflows.enabled);
        assert_eq!(workflows.max_concurrent_agents, 7);
        assert_eq!(workflows.max_agents_per_run, 99);
        assert_eq!(workflows.max_agent_retries, 1);
        assert_eq!(workflows.max_agent_tokens, Some(12_345));
        assert!(!workflows.keyword_trigger_enabled);
    }

    #[test]
    fn parse_workflow_enable_disable_aliases() {
        let disabled: FileConfig = toml::from_str(
            r#"
[workflows]
disableWorkflows = true
"#,
        )
        .unwrap();
        assert!(!disabled.workflows.resolved().enabled);

        let enabled_false: FileConfig = toml::from_str(
            r#"
[workflows]
enableWorkflows = false
"#,
        )
        .unwrap();
        assert!(!enabled_false.workflows.resolved().enabled);
    }

    #[test]
    fn parse_top_level_workflow_legacy_aliases() {
        let disabled: FileConfig = toml::from_str(
            r#"
disableWorkflows = true
"#,
        )
        .unwrap();
        assert!(!disabled.workflows.resolved().enabled);

        let enabled_false: FileConfig = toml::from_str(
            r#"
enableWorkflows = false
"#,
        )
        .unwrap();
        assert!(!enabled_false.workflows.resolved().enabled);

        let keyword_disabled: FileConfig = toml::from_str(
            r#"
workflowKeywordTriggerEnabled = false
"#,
        )
        .unwrap();
        assert!(
            !keyword_disabled
                .workflows
                .resolved()
                .keyword_trigger_enabled
        );
    }

    #[test]
    fn nested_workflow_values_override_top_level_legacy_aliases_in_same_file() {
        let config: FileConfig = toml::from_str(
            r#"
disableWorkflows = true
workflowKeywordTriggerEnabled = false

[workflows]
enabled = true
workflowKeywordTriggerEnabled = true
"#,
        )
        .unwrap();

        let workflows = config.workflows.resolved();
        assert!(workflows.enabled);
        assert!(workflows.keyword_trigger_enabled);
    }

    #[test]
    fn parse_workflow_config_clamps_numeric_values_to_runtime_caps() {
        let toml = r#"
[workflows]
max_concurrent_agents = 128
max_agents_per_run = 12000
max_agent_retries = 99
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        let workflows = config.workflows.resolved();
        assert_eq!(workflows.max_concurrent_agents, 16);
        assert_eq!(workflows.max_agents_per_run, 1_000);
        assert_eq!(workflows.max_agent_retries, 5);
    }

    #[test]
    fn parse_experience_config() {
        let toml = r#"
theme = "solarized"
vim_mode = true
update_check = false
desktop_notifications = true
auto_memory = true
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.theme, ThemeName::Solarized);
        assert!(config.vim_mode);
        assert!(!config.update_check);
        assert!(config.desktop_notifications);
        assert!(config.auto_memory);
    }

    #[test]
    fn load_nonexistent_returns_default() {
        let config = load_toml(Path::new("/nonexistent/path/config.toml"));
        assert!(config.model.is_none());
    }

    #[test]
    fn load_invalid_toml_returns_default() {
        let dir = std::env::temp_dir().join("orca-test-invalid-toml");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not [valid toml {{{").unwrap();

        let config = load_toml(&path);
        assert!(config.model.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_auth_key_from_json() {
        let dir = std::env::temp_dir().join("orca-test-auth-json");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, r#"{"DEEPSEEK_API_KEY": "sk-abc123"}"#).unwrap();

        let key = load_auth_key(&path);
        assert_eq!(key.as_deref(), Some("sk-abc123"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_auth_key_missing_file() {
        let key = load_auth_key(Path::new("/nonexistent/auth.json"));
        assert!(key.is_none());
    }

    #[test]
    fn layered_config_merges_user_and_project_with_project_security_deny_list() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(project_dir.join(".orca")).unwrap();
        let project_path = project_dir.join(".orca/config.toml");

        std::fs::write(
            &user_path,
            r#"
model = "deepseek-v4-pro"
mode = "suggest"
api_key = "sk-user"
base_url = "https://user.example"

[[hooks]]
event = "post_tool_use"
tool = "bash"
command = "echo user"

[tools]
max_read_parallel = 4
"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"
model = "deepseek-v4-flash"
mode = "full-auto"
api_key = "sk-project"
base_url = "https://project.example"

[[hooks]]
event = "post_tool_use"
tool = "bash"
command = "echo project"

[[permissions.rules]]
tool = "bash"
pattern = "cargo *"
decision = "allow"
"#,
        )
        .unwrap();

        let config = load_layered_config_from_paths(&user_path, &project_dir);

        assert_eq!(config.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(
            config.mode,
            Some(crate::approval_types::ApprovalMode::FullAuto)
        );
        assert_eq!(config.api_key.as_deref(), Some("sk-user"));
        assert_eq!(config.base_url.as_deref(), Some("https://user.example"));
        assert_eq!(config.hooks.len(), 1);
        assert_eq!(config.hooks[0].command, "echo user");
        assert_eq!(config.permissions.rules.len(), 1);
        assert_eq!(config.tools.max_read_parallel, 4);
    }

    #[test]
    fn env_and_cli_layers_override_files_in_priority_order() {
        let base = FileConfig {
            model: Some("deepseek-v4-flash".to_string()),
            mode: Some(crate::approval_types::ApprovalMode::Suggest),
            api_key: Some("sk-file".to_string()),
            ..Default::default()
        };

        let env = ConfigOverrides {
            model: Some("deepseek-v4-pro".to_string()),
            mode: Some(crate::approval_types::ApprovalMode::AutoEdit),
            api_key: Some("sk-env".to_string()),
            base_url: None,
        };
        let cli = ConfigOverrides {
            model: Some("auto".to_string()),
            mode: Some(crate::approval_types::ApprovalMode::Plan),
            api_key: Some("sk-cli".to_string()),
            base_url: Some("https://cli.example".to_string()),
        };

        let config = apply_override_layers(base, env, cli);

        assert_eq!(config.model.as_deref(), Some("auto"));
        assert_eq!(config.mode, Some(crate::approval_types::ApprovalMode::Plan));
        assert_eq!(config.api_key.as_deref(), Some("sk-cli"));
        assert_eq!(config.base_url.as_deref(), Some("https://cli.example"));
    }

    #[test]
    fn layered_config_concatenates_permission_rules_from_both_layers() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(project_dir.join(".orca")).unwrap();
        let project_path = project_dir.join(".orca/config.toml");

        std::fs::write(
            &user_path,
            r#"
[[permissions.rules]]
tool = "bash"
pattern = "rm -rf *"
decision = "deny"
"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"
[[permissions.rules]]
tool = "bash"
pattern = "cargo *"
decision = "allow"
"#,
        )
        .unwrap();

        let config = load_layered_config_from_paths(&user_path, &project_dir);

        assert_eq!(config.permissions.rules.len(), 2);
        assert_eq!(config.permissions.rules[0].pattern, "rm -rf *");
        assert_eq!(
            config.permissions.rules[0].decision,
            crate::approval_types::Decision::Deny
        );
        assert_eq!(config.permissions.rules[1].pattern, "cargo *");
        assert_eq!(
            config.permissions.rules[1].decision,
            crate::approval_types::Decision::Allow
        );
    }

    #[test]
    fn layered_config_applies_top_level_workflow_legacy_aliases_with_project_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let user_path = dir.path().join("user.toml");
        let project_dir = dir.path().join("project");
        std::fs::create_dir_all(project_dir.join(".orca")).unwrap();
        let project_path = project_dir.join(".orca/config.toml");

        std::fs::write(
            &user_path,
            r#"
disableWorkflows = true
workflowKeywordTriggerEnabled = false
"#,
        )
        .unwrap();
        std::fs::write(
            &project_path,
            r#"
enableWorkflows = true
workflowKeywordTriggerEnabled = true
"#,
        )
        .unwrap();

        let config = load_layered_config_from_paths(&user_path, &project_dir);
        let workflows = config.workflows.resolved();

        assert!(workflows.enabled);
        assert!(workflows.keyword_trigger_enabled);
    }
}
