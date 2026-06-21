use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use orca_core::tool_types::{ToolRequest, ToolResult, ToolResultKind, truncate_output};
use serde::Deserialize;
use walkdir::WalkDir;

use crate::resolve_workspace_path;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}

pub fn execute(request: &ToolRequest, cwd: &Path, max_bytes: usize) -> ToolResult {
    let args = match parse_args(request) {
        Ok(args) => args,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let base = args.path.as_deref().unwrap_or(".");
    let search_root = match resolve_workspace_path(cwd, Some(base)) {
        Ok(path) => path,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    if !search_root.exists() {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    }

    let matcher = match build_matcher(&args.pattern) {
        Ok(matcher) => matcher,
        Err(error) => return ToolResult::failed(request, error, None),
    };
    let mut matches = if search_root.is_file() {
        match_file(cwd, &search_root, &matcher)
    } else {
        match_tree(cwd, &search_root, &matcher)
    };

    matches.sort();
    matches.dedup();
    if matches.is_empty() {
        return ToolResult::completed_kind(
            request,
            "(no matches)".to_string(),
            false,
            ToolResultKind::NoMatches,
        );
    }

    let (output, truncated) = truncate_output(matches.join("\n"), max_bytes);
    ToolResult::completed_kind(
        request,
        output,
        truncated,
        if truncated {
            ToolResultKind::Truncated
        } else {
            ToolResultKind::Success
        },
    )
}

fn parse_args(request: &ToolRequest) -> Result<GlobArgs, String> {
    let raw = request
        .raw_arguments
        .as_deref()
        .ok_or_else(|| "missing glob arguments JSON".to_string())?;
    let args: GlobArgs = serde_json::from_str(raw)
        .map_err(|error| format!("invalid glob arguments JSON: {error}"))?;
    if args.pattern.trim().is_empty() {
        return Err("missing required glob argument: pattern".to_string());
    }
    Ok(args)
}

fn build_matcher(pattern: &str) -> Result<globset::GlobSet, String> {
    let mut builder = GlobSetBuilder::new();
    let glob = Glob::new(pattern).map_err(|error| format!("invalid glob pattern: {error}"))?;
    builder.add(glob);
    builder
        .build()
        .map_err(|error| format!("invalid glob pattern: {error}"))
}

fn match_tree(cwd: &Path, search_root: &Path, matcher: &globset::GlobSet) -> Vec<String> {
    let mut matches = Vec::new();
    for entry in WalkDir::new(search_root).follow_links(false).into_iter() {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if path == search_root {
            continue;
        }
        let Ok(relative_to_root) = path.strip_prefix(search_root) else {
            continue;
        };
        if matcher.is_match(relative_to_root) {
            matches.push(relative_to_workspace(cwd, path));
        }
    }
    matches
}

fn match_file(cwd: &Path, path: &Path, matcher: &globset::GlobSet) -> Vec<String> {
    let name = path.file_name().map(PathBuf::from).unwrap_or_default();
    if matcher.is_match(name) {
        vec![relative_to_workspace(cwd, path)]
    } else {
        Vec::new()
    }
}

fn relative_to_workspace(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}
