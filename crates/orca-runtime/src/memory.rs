use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use orca_core::config::ProviderKind;
use orca_core::conversation::{Conversation, Message};
use orca_core::provider_types::ProviderStep;
use orca_provider::{self, ProviderConfig};

#[derive(Clone, Debug, Default)]
pub struct MemoryBlock {
    pub user: Option<String>,
    pub project: Option<String>,
}

impl MemoryBlock {
    pub fn is_empty(&self) -> bool {
        self.user
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .is_empty()
            && self
                .project
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .is_empty()
    }

    pub fn to_system_prompt_block(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut block = String::from("<memory>\n");
        if let Some(user) = self.user.as_deref().filter(|text| !text.trim().is_empty()) {
            block.push_str("<user>\n");
            block.push_str(user.trim());
            block.push_str("\n</user>\n");
        }
        if let Some(project) = self
            .project
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            block.push_str("<project>\n");
            block.push_str(project.trim());
            block.push_str("\n</project>\n");
        }
        block.push_str("</memory>");
        Some(block)
    }
}

pub fn load_for_cwd(cwd: &Path) -> MemoryBlock {
    let Some(root) = memory_root() else {
        return MemoryBlock::default();
    };
    MemoryBlock {
        user: read_trimmed(root.join("user.md")),
        project: read_trimmed(project_memory_path(&root, cwd)),
    }
}

pub fn remember_user(note: &str) -> Result<PathBuf, String> {
    let Some(root) = memory_root() else {
        return Err("cannot determine ORCA_HOME or home directory".to_string());
    };
    let path = root.join("user.md");
    append_note(&path, note)?;
    Ok(path)
}

pub fn remember_project(cwd: &Path, note: &str) -> Result<PathBuf, String> {
    let Some(root) = memory_root() else {
        return Err("cannot determine ORCA_HOME or home directory".to_string());
    };
    let path = project_memory_path(&root, cwd);
    append_note(&path, note)?;
    Ok(path)
}

pub fn extract_project_memory(
    provider_kind: ProviderKind,
    provider_config: &ProviderConfig,
    cwd: &Path,
    messages: &[Message],
) -> Result<Option<PathBuf>, String> {
    let source = format_messages_for_memory(messages);
    if source.trim().is_empty() {
        return Ok(None);
    }

    let mut conversation = Conversation::new();
    conversation.add_system(
        "Extract durable project memory from this coding session. Return only concise bullet points worth remembering for future sessions. If nothing is worth remembering, return NOTHING.".to_string(),
    );
    conversation.add_user(source);
    let summary_config = ProviderConfig {
        api_key: provider_config.api_key.clone(),
        base_url: provider_config.base_url.clone(),
        model: provider_config
            .model
            .clone()
            .or_else(|| Some("deepseek-v4-flash".to_string())),
        tools_override: Some(Vec::new()),
        mcp_registry: None,
        external_tools: Vec::new(),
    };
    let response = orca_provider::call(provider_kind, &conversation, &summary_config);
    if response
        .steps
        .iter()
        .any(|step| matches!(step, ProviderStep::Error(_)))
    {
        return Ok(None);
    }
    let Some(note) = response
        .assistant_content
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty() && text != "NOTHING")
    else {
        return Ok(None);
    };
    remember_project(cwd, &note).map(Some)
}

fn memory_root() -> Option<PathBuf> {
    std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .map(|root| root.join("memory"))
}

fn project_memory_path(root: &Path, cwd: &Path) -> PathBuf {
    root.join("projects")
        .join(format!("{:016x}", project_hash(cwd)))
        .join("memory.md")
}

fn project_hash(cwd: &Path) -> u64 {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let bytes = canonical.display().to_string();
    fnv1a_hash(bytes.as_bytes())
}

fn fnv1a_hash(data: &[u8]) -> u64 {
    const BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = BASIS;
    for &byte in data {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

// Append-only file write; no lock needed in current single-session usage.
fn append_note(path: &Path, note: &str) -> Result<(), String> {
    let note = note.trim();
    if note.is_empty() {
        return Err("memory note cannot be empty".to_string());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create memory dir: {error}"))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("failed to open memory file: {error}"))?;
    writeln!(file, "- {note}").map_err(|error| format!("failed to write memory: {error}"))
}

fn format_messages_for_memory(messages: &[Message]) -> String {
    const MAX_BYTES: usize = 32 * 1024;
    let mut output = String::new();
    for message in messages.iter().rev().take(40).rev() {
        match message {
            Message::System { .. } => {}
            Message::User { content, .. } => {
                output.push_str("[user]\n");
                output.push_str(content.trim());
                output.push_str("\n\n");
            }
            Message::Assistant { content, .. } => {
                if let Some(content) = content.as_deref().filter(|text| !text.trim().is_empty()) {
                    output.push_str("[assistant]\n");
                    output.push_str(content.trim());
                    output.push_str("\n\n");
                }
            }
            Message::Tool { content, .. } => {
                output.push_str("[tool]\n");
                output.push_str(content.trim());
                output.push_str("\n\n");
            }
        }
        if output.len() >= MAX_BYTES {
            break;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn memory_block_formats_prompt() {
        let block = MemoryBlock {
            user: Some("prefers concise output".to_string()),
            project: Some("use cargo test".to_string()),
        };
        let prompt = block.to_system_prompt_block().unwrap();
        assert!(prompt.contains("<user>"));
        assert!(prompt.contains("prefers concise output"));
        assert!(prompt.contains("<project>"));
    }

    #[test]
    fn append_note_writes_bullet() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("memory.md");
        append_note(&path, "remember this").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "- remember this\n");
    }

    #[test]
    fn format_messages_for_memory_skips_system_messages() {
        let messages = vec![
            Message::system("system".to_string()),
            Message::user("remember cargo test".to_string()),
        ];
        let formatted = format_messages_for_memory(&messages);
        assert!(!formatted.contains("system"));
        assert!(formatted.contains("remember cargo test"));
    }
}
