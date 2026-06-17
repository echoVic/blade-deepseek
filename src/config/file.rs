use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::approval::policy::PermissionRules;
use crate::config::ThemeName;
use crate::runtime::subagent_config::SubagentConfig;

const ORCA_HOME_ENV: &str = "ORCA_HOME";

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FileConfig {
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    #[serde(default)]
    pub mcp_servers: Vec<crate::mcp::types::McpServerConfig>,
    #[serde(default)]
    pub hooks: Vec<crate::runtime::hooks::HookConfig>,
    #[serde(default)]
    pub permissions: PermissionRules,
    #[serde(default)]
    pub subagents: SubagentConfig,
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

fn default_true() -> bool {
    true
}

fn config_dir() -> Option<PathBuf> {
    std::env::var_os(ORCA_HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".orca")))
}

pub fn load_user_config() -> FileConfig {
    let Some(dir) = config_dir() else {
        return FileConfig::default();
    };

    let mut config = load_toml(&dir.join("config.toml"));
    if config.api_key.is_none() {
        config.api_key = load_auth_key(&dir.join("auth.json"));
    }
    config
}

fn load_toml(path: &Path) -> FileConfig {
    let Ok(content) = fs::read_to_string(path) else {
        return FileConfig::default();
    };
    toml::from_str(&content).unwrap_or_default()
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
[[permissions.allow]]
tool = "bash"
pattern = "cargo *"

[[permissions.deny]]
tool = "write_file"
pattern = "/etc/**"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.permissions.allow.len(), 1);
        assert_eq!(config.permissions.allow[0].tool, "bash");
        assert_eq!(config.permissions.allow[0].pattern, "cargo *");
        assert_eq!(config.permissions.deny.len(), 1);
        assert_eq!(config.permissions.deny[0].tool, "write_file");
        assert_eq!(config.permissions.deny[0].pattern, "/etc/**");
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
}
