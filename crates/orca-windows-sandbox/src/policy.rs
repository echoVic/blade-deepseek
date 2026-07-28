use std::path::PathBuf;

use orca_platform::fs::{PathIdentity, PathPolicy};

use crate::WindowsSandboxError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SandboxFilesystemMode {
    WorkspaceWrite,
    ReadOnly { allow_global_read: bool },
}

#[derive(Clone, Debug)]
pub struct WindowsSandboxPolicyInput {
    pub mode: SandboxFilesystemMode,
    pub cwd: PathBuf,
    pub readable_roots: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    pub denied_roots: Vec<PathBuf>,
    pub network_access: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WindowsSandboxPlan {
    pub mode: SandboxFilesystemMode,
    pub cwd: PathBuf,
    pub readable_roots: Vec<PathBuf>,
    pub writable_roots: Vec<PathBuf>,
    pub denied_roots: Vec<PathBuf>,
    pub network_access: bool,
}

impl WindowsSandboxPlan {
    pub fn build(input: WindowsSandboxPolicyInput) -> Result<Self, WindowsSandboxError> {
        let cwd = PolicyRoot::parse(input.cwd)?;
        let mut readable = parse_roots(input.readable_roots)?;
        let mut writable = parse_roots(input.writable_roots)?;
        let mut denied = parse_roots(input.denied_roots)?;

        if matches!(input.mode, SandboxFilesystemMode::WorkspaceWrite) {
            push_unique(&mut writable, cwd.clone());
            for metadata in [".git", ".agents", ".codex"] {
                push_unique(&mut denied, PolicyRoot::parse(cwd.path.join(metadata))?);
            }
        }
        for root in &writable {
            push_unique(&mut readable, root.clone());
        }
        push_unique(&mut readable, cwd.clone());

        for writable_root in &writable {
            if denied
                .iter()
                .any(|denied_root| writable_root.identity.is_within(&denied_root.identity))
            {
                return Err(WindowsSandboxError::InvalidPolicy(format!(
                    "writable root {} is contained by a denied root",
                    writable_root.path.display()
                )));
            }
        }

        Ok(Self {
            mode: input.mode,
            cwd: cwd.path,
            readable_roots: into_paths(readable),
            writable_roots: into_paths(writable),
            denied_roots: into_paths(denied),
            network_access: input.network_access,
        })
    }
}

#[derive(Clone, Debug)]
struct PolicyRoot {
    path: PathBuf,
    identity: PathIdentity,
}

impl PolicyRoot {
    fn parse(path: PathBuf) -> Result<Self, WindowsSandboxError> {
        let identity = PathPolicy::windows_sandbox().identity(&path.to_string_lossy())?;
        let path = identity.display_path();
        Ok(Self { path, identity })
    }
}

fn parse_roots(paths: Vec<PathBuf>) -> Result<Vec<PolicyRoot>, WindowsSandboxError> {
    let mut roots = Vec::new();
    for path in paths {
        push_unique(&mut roots, PolicyRoot::parse(path)?);
    }
    Ok(roots)
}

fn push_unique(roots: &mut Vec<PolicyRoot>, candidate: PolicyRoot) {
    if !roots.iter().any(|root| root.identity == candidate.identity) {
        roots.push(candidate);
    }
}

fn into_paths(roots: Vec<PolicyRoot>) -> Vec<PathBuf> {
    roots.into_iter().map(|root| root.path).collect()
}
