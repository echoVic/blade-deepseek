use std::collections::hash_map::DefaultHasher;
use std::fs::{self, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::{Path, PathBuf};

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
    let mut hasher = DefaultHasher::new();
    canonical.display().to_string().hash(&mut hasher);
    hasher.finish()
}

fn read_trimmed(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

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
}
