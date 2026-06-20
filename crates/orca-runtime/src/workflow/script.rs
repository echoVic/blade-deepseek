use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::config::WorkflowConfig;
use orca_core::workflow_types::{WorkflowInput, WorkflowMeta};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkflowScriptSource {
    ScriptPath,
    InlineScript,
    NamedWorkflow,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedWorkflowScript {
    pub source_kind: WorkflowScriptSource,
    pub original_path: Option<PathBuf>,
    pub persisted_path: PathBuf,
    pub meta: WorkflowMeta,
    pub script: String,
    pub script_digest: String,
}

pub fn resolve_workflow_script(
    input: &WorkflowInput,
    cwd: &Path,
    session_dir: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let persisted_path = session_dir
        .join("workflows")
        .join("scripts")
        .join("script.js");
    resolve_workflow_script_to_path(input, cwd, &persisted_path)
}

pub fn resolve_workflow_script_to_path(
    input: &WorkflowInput,
    cwd: &Path,
    persisted_path: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let user_dir = dirs::home_dir()
        .map(|home| home.join(".claude").join("workflows"))
        .unwrap_or_else(|| PathBuf::from(".claude/workflows"));
    resolve_workflow_script_with_user_dir_to_path(input, cwd, &user_dir, persisted_path)
}

pub fn resolve_workflow_script_with_user_dir(
    input: &WorkflowInput,
    cwd: &Path,
    session_dir: &Path,
    user_workflow_dir: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let persisted_path = session_dir
        .join("workflows")
        .join("scripts")
        .join("script.js");
    resolve_workflow_script_with_user_dir_to_path(input, cwd, user_workflow_dir, &persisted_path)
}

pub fn resolve_workflow_script_with_user_dir_to_path(
    input: &WorkflowInput,
    cwd: &Path,
    user_workflow_dir: &Path,
    persisted_path: &Path,
) -> io::Result<ResolvedWorkflowScript> {
    let (source_kind, original_path, script) = if let Some(script_path) = input
        .script_path
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let path = resolve_path(cwd, script_path);
        let script = fs::read_to_string(&path)?;
        (WorkflowScriptSource::ScriptPath, Some(path), script)
    } else if let Some(script) = input.script.as_ref() {
        (WorkflowScriptSource::InlineScript, None, script.clone())
    } else if let Some(name) = input
        .name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        let path = find_named_workflow(cwd, name, user_workflow_dir)?;
        let script = fs::read_to_string(&path)?;
        (WorkflowScriptSource::NamedWorkflow, Some(path), script)
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workflow input must include scriptPath, script, or name",
        ));
    };

    let meta = parse_workflow_meta(&script)?;
    if let Some(parent) = persisted_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&persisted_path, &script)?;

    Ok(ResolvedWorkflowScript {
        source_kind,
        original_path,
        persisted_path: persisted_path.to_path_buf(),
        script_digest: sha256_hex(script.as_bytes()),
        meta,
        script,
    })
}

pub fn contains_workflow_keyword(prompt: &str, config: &WorkflowConfig) -> bool {
    config.keyword_trigger_enabled && prompt.split_whitespace().any(|word| word == "ultracode")
}

fn resolve_path(cwd: &Path, raw_path: &str) -> PathBuf {
    let path = PathBuf::from(raw_path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn find_named_workflow(cwd: &Path, name: &str, user_workflow_dir: &Path) -> io::Result<PathBuf> {
    for ancestor in cwd.ancestors() {
        let candidate = ancestor
            .join(".claude")
            .join("workflows")
            .join(format!("{name}.js"));
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    let user_candidate = user_workflow_dir.join(format!("{name}.js"));
    if user_candidate.exists() {
        return Ok(user_candidate);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("workflow script `{name}` not found"),
    ))
}

fn parse_workflow_meta(script: &str) -> io::Result<WorkflowMeta> {
    let export_index = script
        .find("export const meta")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing `export const meta`"))?;
    let object_start = script[export_index..]
        .find('{')
        .map(|offset| export_index + offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing meta object"))?;
    let object_end = find_matching_brace(script, object_start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unterminated meta object"))?;
    let body = &script[object_start + 1..object_end];

    let mut name = None;
    let mut description = None;
    let mut phases = None;

    for field in split_top_level(body, ',') {
        let trimmed = field.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Some((key, value)) = split_key_value(trimmed) else {
            continue;
        };
        match key.trim() {
            "name" => name = Some(parse_quoted_string(value)?),
            "description" => description = Some(parse_quoted_string(value)?),
            "phases" => phases = Some(parse_phases(value)?),
            _ => {}
        }
    }

    Ok(WorkflowMeta {
        name: name.ok_or_else(|| missing_meta_field("name"))?,
        description: description.ok_or_else(|| missing_meta_field("description"))?,
        phases: phases.ok_or_else(|| missing_meta_field("phases"))?,
    })
}

fn find_matching_brace(script: &str, object_start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in script[object_start..].char_indices() {
        let absolute = object_start + index;
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(absolute);
                }
            }
            _ => {}
        }
    }

    None
}

fn split_top_level(input: &str, delimiter: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in input.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            _ if ch == delimiter && bracket_depth == 0 && brace_depth == 0 => {
                parts.push(&input[start..index]);
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&input[start..]);
    parts
}

fn split_key_value(input: &str) -> Option<(&str, &str)> {
    let mut bracket_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut quote = None;
    let mut escaped = false;

    for (index, ch) in input.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
                continue;
            }
            if ch == '\\' {
                escaped = true;
                continue;
            }
            if ch == active_quote {
                quote = None;
            }
            continue;
        }

        match ch {
            '\'' | '"' => quote = Some(ch),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            '{' => brace_depth += 1,
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ':' if bracket_depth == 0 && brace_depth == 0 => {
                return Some((&input[..index], &input[index + 1..]));
            }
            _ => {}
        }
    }

    None
}

fn parse_quoted_string(input: &str) -> io::Result<String> {
    let trimmed = input.trim();
    if trimmed.len() < 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected quoted string",
        ));
    }

    let quote = trimmed
        .chars()
        .next()
        .filter(|ch| *ch == '\'' || *ch == '"')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "expected quoted string"))?;
    if !trimmed.ends_with(quote) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unterminated quoted string",
        ));
    }

    Ok(trimmed[1..trimmed.len() - 1].to_string())
}

fn parse_phases(input: &str) -> io::Result<Vec<String>> {
    let trimmed = input.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected phases array",
        ));
    }

    let body = &trimmed[1..trimmed.len() - 1];
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }

    split_top_level(body, ',')
        .into_iter()
        .map(parse_quoted_string)
        .collect()
}

fn missing_meta_field(field: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("workflow meta missing `{field}`"),
    )
}

fn sha256_hex(input: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use orca_core::config::WorkflowConfig;

    use super::{contains_workflow_keyword, parse_workflow_meta};

    #[test]
    fn parser_accepts_double_quotes() {
        let meta = parse_workflow_meta(
            "export const meta = { name: \"audit\", description: \"Audit code\", phases: [] };",
        )
        .unwrap();
        assert_eq!(meta.name, "audit");
        assert!(meta.phases.is_empty());
    }

    #[test]
    fn workflow_keyword_requires_exact_word_and_enabled_switch() {
        let enabled = WorkflowConfig::default();
        assert!(contains_workflow_keyword(
            "please run ultracode now",
            &enabled
        ));
        assert!(!contains_workflow_keyword(
            "please run ultracode-now",
            &enabled
        ));

        let disabled = WorkflowConfig {
            keyword_trigger_enabled: false,
            ..WorkflowConfig::default()
        };
        assert!(!contains_workflow_keyword(
            "please run ultracode now",
            &disabled
        ));
    }
}
