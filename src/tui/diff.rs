use std::fs;
use std::path::{Path, PathBuf};

use similar::TextDiff;

use crate::tools::{ToolName, ToolRequest};

#[derive(Clone, Debug)]
pub struct BeforeSnapshot {
    path: PathBuf,
    relative_path: String,
    content: String,
}

pub fn capture_before(tool_request: &ToolRequest, cwd: &Path) -> Option<BeforeSnapshot> {
    if !matches!(tool_request.name, ToolName::Edit | ToolName::WriteFile) {
        return None;
    }
    let relative_path = tool_path(tool_request)?;
    let path = resolve_inside_workspace(cwd, &relative_path).ok()?;
    let content = fs::read_to_string(&path).unwrap_or_default();
    Some(BeforeSnapshot {
        path,
        relative_path,
        content,
    })
}

pub fn render_after(snapshot: BeforeSnapshot) -> Option<String> {
    let after = fs::read_to_string(&snapshot.path).ok()?;
    if snapshot.content == after {
        return None;
    }

    let diff = TextDiff::from_lines(&snapshot.content, &after);
    Some(
        diff.unified_diff()
            .header(
                &format!("a/{}", snapshot.relative_path),
                &format!("b/{}", snapshot.relative_path),
            )
            .to_string(),
    )
}

fn tool_path(tool_request: &ToolRequest) -> Option<String> {
    if let Some(raw) = tool_request.raw_arguments.as_deref()
        && let Ok(value) = serde_json::from_str::<serde_json::Value>(raw)
        && let Some(path) = value["path"].as_str()
    {
        return Some(path.to_string());
    }
    tool_request.target.as_deref().map(|target| {
        target
            .split("::")
            .next()
            .unwrap_or(target)
            .trim()
            .to_string()
    })
}

fn resolve_inside_workspace(cwd: &Path, path: &str) -> Result<PathBuf, String> {
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|error| format!("cannot resolve cwd: {error}"))?;
    let joined = canonical_cwd.join(path);

    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            _ => normalized.push(component),
        }
    }

    if !normalized.starts_with(&canonical_cwd) {
        return Err(format!("path escapes workspace: {path}"));
    }

    if normalized.exists() {
        let real = normalized
            .canonicalize()
            .map_err(|e| format!("cannot resolve path: {e}"))?;
        if !real.starts_with(&canonical_cwd) {
            return Err(format!("path escapes workspace via symlink: {path}"));
        }
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::policy::ActionKind;

    #[test]
    fn renders_unified_diff_for_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        fs::write(&path, "old\nsame\n").unwrap();
        let request = ToolRequest {
            id: "tool-1".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some("notes.txt :: old => new".to_string()),
            raw_arguments: None,
        };

        let snapshot = capture_before(&request, dir.path()).unwrap();
        fs::write(&path, "new\nsame\n").unwrap();
        let rendered = render_after(snapshot).unwrap();

        assert!(rendered.contains("--- a/notes.txt"));
        assert!(rendered.contains("+++ b/notes.txt"));
        assert!(rendered.contains("-old"));
        assert!(rendered.contains("+new"));
    }
}
