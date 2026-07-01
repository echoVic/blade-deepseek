use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use super::shell::shell_join;
use super::wire::{CwdOrModelFilter, WireCommandParam, WireParams};

pub type CommandEnvOverrides = BTreeMap<String, Option<String>>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandExecOptions {
    pub stream_stdin: bool,
    pub stream_stdout_stderr: bool,
    pub has_size: bool,
    pub output_bytes_cap: Option<u64>,
    pub disable_output_cap: bool,
    pub disable_timeout: bool,
    pub timeout_ms: Option<i64>,
    pub sandbox_policy: CommandSandboxPolicy,
    pub permission_profile: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum CommandSandboxPolicy {
    #[default]
    Default,
    Other,
    ReadOnly {
        network_access: bool,
    },
    ExternalSandbox {
        network_access: NetworkAccess,
    },
    WorkspaceWrite {
        writable_roots: Vec<PathBuf>,
        network_access: bool,
        exclude_tmpdir_env_var: bool,
        exclude_slash_tmp: bool,
    },
    DangerFullAccess,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum NetworkAccess {
    #[default]
    Restricted,
    Enabled,
}
pub(super) fn command_text_from_wire(command: Option<&WireCommandParam>) -> Option<String> {
    match command? {
        WireCommandParam::Text(command) => Some(command.clone()),
        WireCommandParam::Args(args) => Some(shell_join(args)),
    }
}

pub(super) fn command_args_from_wire(command: Option<&WireCommandParam>) -> Option<Vec<String>> {
    match command? {
        WireCommandParam::Text(command) => Some(vec![command.clone()]),
        WireCommandParam::Args(args) => Some(args.clone()),
    }
}

pub(super) fn command_cwd_from_wire(cwd: Option<&CwdOrModelFilter>) -> Option<PathBuf> {
    match cwd {
        Some(CwdOrModelFilter::One(value)) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

pub(super) fn command_exec_options_from_params(params: &WireParams) -> CommandExecOptions {
    CommandExecOptions {
        stream_stdin: params.stream_stdin,
        stream_stdout_stderr: params.stream_stdout_stderr,
        has_size: params.size.is_some(),
        output_bytes_cap: params.output_bytes_cap,
        disable_output_cap: params.disable_output_cap,
        disable_timeout: params.disable_timeout,
        timeout_ms: params.timeout_ms,
        sandbox_policy: command_sandbox_policy_from_wire(params.sandbox_policy.as_ref()),
        permission_profile: params.permission_profile.clone(),
    }
}

pub(super) fn command_sandbox_policy_from_wire(value: Option<&Value>) -> CommandSandboxPolicy {
    let Some(value) = value else {
        return CommandSandboxPolicy::Default;
    };
    match value.get("type").and_then(Value::as_str) {
        Some("dangerFullAccess") => CommandSandboxPolicy::DangerFullAccess,
        Some("readOnly") => CommandSandboxPolicy::ReadOnly {
            network_access: value
                .get("networkAccess")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        },
        Some("externalSandbox") => CommandSandboxPolicy::ExternalSandbox {
            network_access: network_access_from_wire(value.get("networkAccess")),
        },
        Some("workspaceWrite") => {
            let writable_roots = value
                .get("writableRoots")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|root| !root.is_empty())
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            CommandSandboxPolicy::WorkspaceWrite {
                writable_roots,
                network_access: value
                    .get("networkAccess")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                exclude_tmpdir_env_var: value
                    .get("excludeTmpdirEnvVar")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                exclude_slash_tmp: value
                    .get("excludeSlashTmp")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }
        }
        Some(_) => CommandSandboxPolicy::Other,
        None => CommandSandboxPolicy::Other,
    }
}

pub(super) fn network_access_from_wire(value: Option<&Value>) -> NetworkAccess {
    match value.and_then(Value::as_str) {
        Some("enabled") => NetworkAccess::Enabled,
        _ => NetworkAccess::Restricted,
    }
}
