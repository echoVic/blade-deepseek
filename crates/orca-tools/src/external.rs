use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{
    ToolOutputTruncation, ToolRequest, ToolResult, truncate_output_with_policy,
};

// Security: only loads from ORCA_HOME/tools/ (user-controlled), never from
// project-level directories, to prevent repo poisoning attacks.
pub fn default_tools_dir() -> Option<PathBuf> {
    std::env::var_os("ORCA_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .map(|home| home.join("tools"))
}

pub fn load_default_external_tools() -> Vec<ExternalToolConfig> {
    default_tools_dir()
        .as_deref()
        .map(load_external_tools_dir)
        .unwrap_or_default()
}

pub fn load_external_tools_dir(dir: &Path) -> Vec<ExternalToolConfig> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            eprintln!(
                "orca: warning: failed to read external tools directory '{}': {error}",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut tools = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .filter_map(|path| {
            let content = fs::read_to_string(&path).ok()?;
            match toml::from_str::<ExternalToolConfig>(&content) {
                Ok(tool) if is_valid_tool_name(&tool.name) => Some(tool),
                Ok(tool) => {
                    eprintln!(
                        "orca: warning: ignoring external tool with invalid name '{}'",
                        tool.name
                    );
                    None
                }
                Err(error) => {
                    eprintln!(
                        "orca: warning: failed to parse external tool '{}': {error}",
                        path.display()
                    );
                    None
                }
            }
        })
        .collect::<Vec<_>>();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

pub fn execute_external_tool(
    config: &ExternalToolConfig,
    request: &ToolRequest,
    cwd: &Path,
    max_output_bytes: usize,
) -> ToolResult {
    execute_external_tool_with_policy(
        config,
        request,
        cwd,
        ToolOutputTruncation::bytes(max_output_bytes),
    )
}

pub fn execute_external_tool_with_policy(
    config: &ExternalToolConfig,
    request: &ToolRequest,
    cwd: &Path,
    output_truncation: ToolOutputTruncation,
) -> ToolResult {
    let args = request.raw_arguments.as_deref().unwrap_or("{}");
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(&config.command)
        .current_dir(cwd)
        .env("ORCA_TOOL_NAME", &config.name)
        .env("ORCA_TOOL_ARGS", args)
        .env(
            "ORCA_TOOL_TARGET",
            request.target.as_deref().unwrap_or_default(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("external tool '{}' failed to start: {error}", config.name),
                None,
            );
        }
    };

    if let Some(mut stdin) = child.stdin.take()
        && let Err(error) = stdin.write_all(args.as_bytes())
    {
        return ToolResult::failed(
            request,
            format!(
                "external tool '{}' failed to receive input: {error}",
                config.name
            ),
            None,
        );
    }

    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => {
            return ToolResult::failed(
                request,
                format!("external tool '{}' failed: {error}", config.name),
                None,
            );
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    if output.status.success() {
        let (output, truncated) = truncate_output_with_policy(stdout, output_truncation);
        return ToolResult::completed(request, output, truncated);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let detail = if stderr.is_empty() {
        stdout.trim().to_string()
    } else {
        stderr
    };
    ToolResult::failed(
        request,
        if detail.is_empty() {
            format!(
                "external tool '{}' exited with {}",
                config.name, output.status
            )
        } else {
            format!(
                "external tool '{}' exited with {}: {detail}",
                config.name, output.status
            )
        },
        output.status.code(),
    )
}

fn is_valid_tool_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}
