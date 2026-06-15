use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct FileConfig {
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

pub fn load_user_config() -> FileConfig {
    let Some(config_dir) = dirs::config_dir() else {
        return FileConfig::default();
    };
    let path = config_dir.join("orca").join("config.toml");
    load_from_path(&path)
}

fn load_from_path(path: &Path) -> FileConfig {
    let Ok(content) = fs::read_to_string(path) else {
        return FileConfig::default();
    };
    toml::from_str(&content).unwrap_or_default()
}

pub fn save_api_key(api_key: &str) {
    let Some(config_dir) = dirs::config_dir() else {
        return;
    };
    let dir = config_dir.join("orca");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("config.toml");

    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut doc: toml::Table = toml::from_str(&existing).unwrap_or_default();
    doc.insert(
        "api_key".to_string(),
        toml::Value::String(api_key.to_string()),
    );

    if let Ok(content) = toml::to_string_pretty(&doc) {
        let _ = fs::write(&path, content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_full_config() {
        let toml = r#"
model = "deepseek-reasoner"
api_key = "sk-test-123"
base_url = "https://custom.api.com"
"#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("deepseek-reasoner"));
        assert_eq!(config.api_key.as_deref(), Some("sk-test-123"));
        assert_eq!(config.base_url.as_deref(), Some("https://custom.api.com"));
    }

    #[test]
    fn parse_partial_config() {
        let toml = r#"model = "deepseek-chat""#;
        let config: FileConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.model.as_deref(), Some("deepseek-chat"));
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
    fn load_nonexistent_returns_default() {
        let config = load_from_path(Path::new("/nonexistent/path/config.toml"));
        assert!(config.model.is_none());
    }

    #[test]
    fn load_invalid_toml_returns_default() {
        let dir = std::env::temp_dir().join("orca-test-invalid-toml");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not [valid toml {{{").unwrap();

        let config = load_from_path(&path);
        assert!(config.model.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
