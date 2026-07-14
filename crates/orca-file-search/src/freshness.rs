use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::eligibility::canonical_root;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootFingerprint {
    pub canonical_root: PathBuf,
    pub git_index_path: Option<PathBuf>,
    pub git_index_mtime: Option<SystemTime>,
}

impl RootFingerprint {
    pub fn capture(root: &Path) -> Result<Self, String> {
        let canonical_root = canonical_root(root)?;
        let git_index_path = find_git_index(&canonical_root);
        let git_index_mtime = git_index_path.as_deref().and_then(file_mtime);
        Ok(Self {
            canonical_root,
            git_index_path,
            git_index_mtime,
        })
    }

    pub fn tracked_state_changed(&self) -> bool {
        self.git_index_path
            .as_deref()
            .is_some_and(|path| file_mtime(path) != self.git_index_mtime)
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
}

fn find_git_index(root: &Path) -> Option<PathBuf> {
    for directory in root.ancestors() {
        let dot_git = directory.join(".git");
        if dot_git.is_dir() {
            return Some(dot_git.join("index"));
        }
        if dot_git.is_file() {
            let contents = fs::read_to_string(&dot_git).ok()?;
            let git_dir = contents.trim().strip_prefix("gitdir:")?.trim();
            let git_dir = PathBuf::from(git_dir);
            let git_dir = if git_dir.is_absolute() {
                git_dir
            } else {
                directory.join(git_dir)
            };
            return Some(git_dir.join("index"));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::RootFingerprint;

    #[test]
    fn resolves_worktree_git_index_path() {
        let root = tempdir().unwrap();
        let git_dir = root.path().join("git-data");
        fs::create_dir(&git_dir).unwrap();
        fs::write(git_dir.join("index"), "index").unwrap();
        fs::write(root.path().join(".git"), "gitdir: git-data\n").unwrap();

        let fingerprint = RootFingerprint::capture(root.path()).unwrap();

        assert_eq!(
            fingerprint.git_index_path,
            Some(git_dir.canonicalize().unwrap().join("index"))
        );
        assert!(!fingerprint.tracked_state_changed());
    }
}
