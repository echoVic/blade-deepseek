use std::fs;
use std::path::Path;

use orca_core::tool_types::{ToolRequest, ToolResult};

use crate::resolve_workspace_path;

pub fn execute(request: &ToolRequest, cwd: &Path) -> ToolResult {
    let (path_str, old, new) = match parse_edit_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };

    let path = match resolve_workspace_path(cwd, Some(&path_str)) {
        Ok(p) => p,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    if !is_inside_workspace(cwd, &path) {
        return ToolResult::failed(request, "edit target is outside the workspace", None);
    }

    if old.is_empty() {
        return ToolResult::failed(request, "edit old text cannot be empty", None);
    }

    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("failed to read {}: {error}", path.display()),
                None,
            );
        }
    };

    let matches = contents.matches(&*old).count();
    if matches == 0 {
        return ToolResult::failed(request, "edit old text was not found", None);
    }
    if matches > 1 {
        return ToolResult::failed(request, "edit old text matched multiple locations", None);
    }

    let updated = contents.replacen(&*old, &new, 1);
    if let Err(error) = fs::write(&path, updated) {
        return ToolResult::failed(
            request,
            format!("failed to write {}: {error}", path.display()),
            None,
        );
    }

    ToolResult::completed(
        request,
        format!(
            "edited {}",
            path.strip_prefix(cwd).unwrap_or(&path).display()
        ),
        false,
    )
}

fn parse_edit_args(request: &ToolRequest) -> Result<(String, String, String), String> {
    if let Some(raw) = request.raw_arguments.as_deref() {
        let args: serde_json::Value =
            serde_json::from_str(raw).map_err(|e| format!("invalid edit arguments: {e}"))?;
        let path = args["path"]
            .as_str()
            .ok_or("edit requires 'path' argument")?
            .to_string();
        let old_text = args["old_text"]
            .as_str()
            .ok_or("edit requires 'old_text' argument")?
            .to_string();
        let new_text = args["new_text"]
            .as_str()
            .ok_or("edit requires 'new_text' argument")?
            .to_string();
        return Ok((path, old_text, new_text));
    }

    let spec = request.target.as_deref().ok_or("edit spec is required")?;
    let (path_part, replacement_part) = spec
        .split_once("::")
        .ok_or("edit spec must be: <path> :: <old> => <new>")?;
    let (old, new) = replacement_part
        .split_once("=>")
        .ok_or("edit replacement must be: <old> => <new>")?;

    Ok((
        path_part.trim().to_string(),
        old.trim().to_string(),
        new.trim().to_string(),
    ))
}

fn is_inside_workspace(cwd: &Path, path: &Path) -> bool {
    let Ok(workspace) = fs::canonicalize(cwd) else {
        return false;
    };
    let parent = path.parent().unwrap_or(path);
    let Ok(parent) = fs::canonicalize(parent) else {
        return false;
    };

    parent.starts_with(workspace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};
    use std::fs;

    fn make_request(target: Option<&str>, raw_arguments: Option<&str>) -> ToolRequest {
        ToolRequest {
            id: "test-edit".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: target.map(|s| s.to_string()),
            raw_arguments: raw_arguments.map(|s| s.to_string()),
        }
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orca-edit-test-{name}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn successful_edit_via_raw_arguments() {
        let dir = temp_dir("raw-args");
        let file = dir.join("test.txt");
        fs::write(&file, "hello world\n").unwrap();

        let raw = r#"{"path":"test.txt","old_text":"hello","new_text":"hi"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(fs::read_to_string(&file).unwrap(), "hi world\n");
    }

    #[test]
    fn successful_edit_via_dsl_target() {
        let dir = temp_dir("dsl");
        let file = dir.join("note.txt");
        fs::write(&file, "foo bar baz\n").unwrap();

        let req = make_request(Some("note.txt :: foo => qux"), None);
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(fs::read_to_string(&file).unwrap(), "qux bar baz\n");
    }

    #[test]
    fn fails_when_old_text_not_found() {
        let dir = temp_dir("not-found");
        let file = dir.join("test.txt");
        fs::write(&file, "hello world\n").unwrap();

        let raw = r#"{"path":"test.txt","old_text":"missing","new_text":"x"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.unwrap().contains("not found"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "hello world\n");
    }

    #[test]
    fn fails_when_old_text_matches_multiple() {
        let dir = temp_dir("multi-match");
        let file = dir.join("test.txt");
        fs::write(&file, "aaa\naaa\n").unwrap();

        let raw = r#"{"path":"test.txt","old_text":"aaa","new_text":"bbb"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.unwrap().contains("multiple"));
        assert_eq!(fs::read_to_string(&file).unwrap(), "aaa\naaa\n");
    }

    #[test]
    fn fails_when_old_text_is_empty() {
        let dir = temp_dir("empty-old");
        let file = dir.join("test.txt");
        fs::write(&file, "content\n").unwrap();

        let raw = r#"{"path":"test.txt","old_text":"","new_text":"x"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.unwrap().contains("empty"));
    }

    #[test]
    fn fails_when_file_does_not_exist() {
        let dir = temp_dir("no-file");

        let raw = r#"{"path":"nonexistent.txt","old_text":"x","new_text":"y"}"#;
        let req = make_request(None, Some(raw));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.unwrap().contains("failed to read"));
    }

    #[test]
    fn fails_with_invalid_json_arguments() {
        let dir = temp_dir("bad-json");

        let req = make_request(None, Some("not json"));
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Failed);
        assert!(result.error.unwrap().contains("invalid edit arguments"));
    }

    #[test]
    fn raw_arguments_takes_precedence_over_target() {
        let dir = temp_dir("precedence");
        let file = dir.join("a.txt");
        fs::write(&file, "old content\n").unwrap();

        let raw = r#"{"path":"a.txt","old_text":"old","new_text":"new"}"#;
        let req = ToolRequest {
            id: "test".to_string(),
            name: ToolName::Edit,
            action: ActionKind::Write,
            target: Some("a.txt :: something => else".to_string()),
            raw_arguments: Some(raw.to_string()),
        };
        let result = execute(&req, &dir);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(fs::read_to_string(&file).unwrap(), "new content\n");
    }
}
