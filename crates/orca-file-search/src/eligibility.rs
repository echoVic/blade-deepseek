use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::types::MatchKind;

#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub path: String,
    pub kind: MatchKind,
}

pub(crate) fn candidate_from_path(root: &Path, path: &Path) -> Option<Candidate> {
    if path == root {
        return None;
    }
    let relative = path.strip_prefix(root).ok()?;
    if contains_vcs_metadata(relative) {
        return None;
    }

    let metadata = std::fs::symlink_metadata(path).ok()?;
    let kind = if metadata.file_type().is_symlink() {
        let canonical = path.canonicalize().ok()?;
        if !canonical.starts_with(root) {
            return None;
        }
        let target = std::fs::metadata(&canonical).ok()?;
        if target.is_dir() {
            MatchKind::Directory
        } else if target.is_file() {
            MatchKind::File
        } else {
            return None;
        }
    } else if metadata.is_dir() {
        MatchKind::Directory
    } else if metadata.is_file() {
        MatchKind::File
    } else {
        return None;
    };

    let mut path = normalize_relative(relative)?;
    if kind == MatchKind::Directory {
        path.push('/');
    }
    Some(Candidate { path, kind })
}

pub(crate) fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    root.canonicalize()
        .map_err(|error| format!("failed to resolve search root {}: {error}", root.display()))
}

fn normalize_relative(path: &Path) -> Option<String> {
    let value = path.to_str()?.replace('\\', "/");
    if value.is_empty() { None } else { Some(value) }
}

fn contains_vcs_metadata(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::Normal(name) if is_vcs_metadata_name(name)))
}

pub(crate) fn is_vcs_metadata_name(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(".git" | ".svn" | ".hg" | ".bzr" | ".jj" | ".sl")
    )
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::candidate_from_path;

    #[test]
    fn excludes_vcs_metadata() {
        let dir = tempdir().unwrap();
        let git = dir.path().join(".git");
        fs::create_dir(&git).unwrap();
        fs::write(git.join("index"), "index").unwrap();

        assert!(candidate_from_path(dir.path(), &git.join("index")).is_none());
    }

    #[test]
    fn directory_candidates_have_a_trailing_slash() {
        let root = tempdir().unwrap();
        let nested = root.path().join("src");
        fs::create_dir(&nested).unwrap();

        let candidate = candidate_from_path(root.path(), &nested).unwrap();

        assert_eq!(candidate.path, "src/");
    }

    #[cfg(unix)]
    #[test]
    fn excludes_symlink_targets_outside_root() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let target = outside.path().join("secret.txt");
        fs::write(&target, "secret").unwrap();
        let link = root.path().join("secret-link.txt");
        symlink(target, &link).unwrap();

        assert!(candidate_from_path(root.path(), &link).is_none());
    }
}
