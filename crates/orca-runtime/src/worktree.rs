use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorktreeOutcome {
    pub path: PathBuf,
    pub preserved: bool,
}

#[derive(Debug)]
pub struct WorktreeGuard {
    repo_root: PathBuf,
    path: PathBuf,
}

impl WorktreeGuard {
    pub fn create(cwd: &Path) -> io::Result<Self> {
        let repo_root = git_output(cwd, &["rev-parse", "--show-toplevel"])?;
        let repo_root = PathBuf::from(repo_root.trim());
        let base = repo_root.join(".orca").join("worktrees");
        fs::create_dir_all(&base)?;
        let path = base.join(format!("subagent-{}", uuid::Uuid::new_v4()));
        let path_text = path.display().to_string();
        git_status(
            &repo_root,
            &["worktree", "add", "--detach", &path_text, "HEAD"],
        )?;
        Ok(Self { repo_root, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn finish(self) -> io::Result<WorktreeOutcome> {
        let Self { repo_root, path } = self;
        Self::finish_existing(repo_root, path)
    }

    pub fn finish_existing(repo_root: PathBuf, path: PathBuf) -> io::Result<WorktreeOutcome> {
        let status = git_output(&path, &["status", "--short"])?;
        let dirty = !status.trim().is_empty();
        if !dirty {
            let path_text = path.display().to_string();
            git_status(&repo_root, &["worktree", "remove", "--force", &path_text])?;
        }
        Ok(WorktreeOutcome {
            path,
            preserved: dirty,
        })
    }
}

fn git_output(cwd: &Path, args: &[&str]) -> io::Result<String> {
    let output = Command::new("git").current_dir(cwd).args(args).output()?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(git_error(args, &output))
    }
}

fn git_status(cwd: &Path, args: &[&str]) -> io::Result<()> {
    let output = Command::new("git").current_dir(cwd).args(args).output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(git_error(args, &output))
    }
}

fn git_error(args: &[&str], output: &std::process::Output) -> io::Error {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    io::Error::other(format!(
        "git {} failed: {}{}",
        args.join(" "),
        stdout.trim(),
        stderr.trim()
    ))
}
