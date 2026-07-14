use std::cmp::Ordering as CmpOrdering;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;

use crate::discovery::not_vcs_metadata;
use crate::eligibility::{ExcludeMatcher, candidate_from_path};
use crate::types::{MatchKind, SearchMatch};

const BROWSE_BATCH_SIZE: usize = 256;
const BROWSE_BATCH_BUDGET: Duration = Duration::from_millis(5);

pub(crate) struct BrowseScan {
    directory: String,
    roots: Vec<BrowseRoot>,
    active_root: usize,
    matches: Vec<SearchMatch>,
    result_limit: usize,
    case_sensitive: bool,
    complete: bool,
    error_count: usize,
    exclude: ExcludeMatcher,
}

struct BrowseRoot {
    root_index: usize,
    root: PathBuf,
    search_dir: PathBuf,
    walker: ignore::Walk,
}

impl BrowseScan {
    pub(crate) fn start(
        roots: &[PathBuf],
        directory: &str,
        result_limit: usize,
        respect_gitignore: bool,
        exclude: ExcludeMatcher,
    ) -> Result<Self, String> {
        let directory = directory.trim_end_matches('/');
        let mut browse_roots = Vec::new();
        for (root_index, root) in roots.iter().enumerate() {
            let requested_dir = if directory.is_empty() {
                root.clone()
            } else {
                root.join(directory)
            };
            let Ok(search_dir) = requested_dir.canonicalize() else {
                continue;
            };
            if !search_dir.starts_with(root) || !search_dir.is_dir() {
                continue;
            }

            let mut builder = WalkBuilder::new(&search_dir);
            builder
                .max_depth(Some(1))
                .hidden(false)
                .follow_links(false)
                .require_git(true)
                .filter_entry(not_vcs_metadata);
            if !respect_gitignore {
                builder
                    .git_ignore(false)
                    .git_global(false)
                    .git_exclude(false)
                    .ignore(false)
                    .parents(false);
            }
            browse_roots.push(BrowseRoot {
                root_index,
                root: root.clone(),
                search_dir,
                walker: builder.build(),
            });
        }
        if browse_roots.is_empty() {
            return Err(format!("failed to browse @{directory} in any search root"));
        }

        Ok(Self {
            directory: directory.to_string(),
            roots: browse_roots,
            active_root: 0,
            matches: Vec::with_capacity(result_limit),
            result_limit,
            case_sensitive: directory.chars().any(char::is_uppercase),
            complete: false,
            error_count: 0,
            exclude,
        })
    }

    pub(crate) fn advance(&mut self, shutdown: &AtomicBool) -> bool {
        if self.complete {
            return false;
        }
        let before = self.matches.clone();
        let started = Instant::now();
        let mut processed = 0usize;
        while processed < BROWSE_BATCH_SIZE && started.elapsed() < BROWSE_BATCH_BUDGET {
            if shutdown.load(Ordering::Acquire) {
                self.complete = true;
                break;
            }
            let Some(root) = self.roots.get_mut(self.active_root) else {
                self.complete = true;
                break;
            };
            let Some(entry) = root.walker.next() else {
                self.active_root += 1;
                continue;
            };
            processed += 1;
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    self.error_count += 1;
                    continue;
                }
            };
            if entry.depth() == 0 {
                continue;
            }
            let Some(candidate) = candidate_from_path(root.root_index, &root.root, entry.path())
            else {
                continue;
            };
            if self.exclude.matches(&candidate.path) {
                continue;
            }
            let Some(child) = entry
                .path()
                .strip_prefix(&root.search_dir)
                .ok()
                .and_then(Path::to_str)
            else {
                continue;
            };
            let child = child.replace('\\', "/");
            if child.is_empty() {
                continue;
            }
            let mut path = if self.directory.is_empty() {
                child
            } else {
                format!("{}/{child}", self.directory)
            };
            if candidate.kind == MatchKind::Directory {
                path.push('/');
            }
            let match_root = root.root.clone();
            self.insert(SearchMatch {
                root: match_root,
                path,
                kind: candidate.kind,
                score: 0,
                indices: Vec::new(),
            });
        }
        before != self.matches || self.complete
    }

    pub(crate) fn matches(&self) -> &[SearchMatch] {
        &self.matches
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.complete
    }

    pub(crate) fn error_message(&self) -> Option<String> {
        (self.complete && self.error_count > 0).then(|| {
            format!(
                "directory browse completed with {} traversal errors",
                self.error_count
            )
        })
    }

    fn insert(&mut self, candidate: SearchMatch) {
        let index = self
            .matches
            .binary_search_by(|existing| browse_order(existing, &candidate, self.case_sensitive))
            .unwrap_or_else(|index| index);
        self.matches.insert(index, candidate);
        self.matches.truncate(self.result_limit);
    }
}

fn browse_order(left: &SearchMatch, right: &SearchMatch, case_sensitive: bool) -> CmpOrdering {
    match_kind_order(left.kind)
        .cmp(&match_kind_order(right.kind))
        .then_with(|| {
            if case_sensitive {
                left.path.cmp(&right.path)
            } else {
                left.path
                    .to_lowercase()
                    .cmp(&right.path.to_lowercase())
                    .then_with(|| left.path.cmp(&right.path))
            }
        })
        .then_with(|| left.root.cmp(&right.root))
}

fn match_kind_order(kind: MatchKind) -> u8 {
    match kind {
        MatchKind::Directory => 0,
        MatchKind::File => 1,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::AtomicBool;

    use tempfile::tempdir;

    use super::BrowseScan;

    #[test]
    fn browse_sorts_directories_before_files() {
        let root = tempdir().unwrap();
        fs::create_dir(root.path().join(".git")).unwrap();
        fs::write(root.path().join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(root.path().join(".hidden.rs"), "hidden").unwrap();
        fs::write(root.path().join("ignored.rs"), "ignored").unwrap();
        fs::write(root.path().join("alpha.rs"), "alpha").unwrap();
        fs::create_dir(root.path().join("zeta")).unwrap();

        let roots = vec![root.path().canonicalize().unwrap()];
        let mut scan = BrowseScan::start(
            &roots,
            "",
            crate::types::RESULT_LIMIT,
            true,
            crate::eligibility::ExcludeMatcher::default(),
        )
        .unwrap();
        while !scan.is_complete() {
            scan.advance(&AtomicBool::new(false));
        }
        let matches = scan.matches();

        assert_eq!(matches[0].path, "zeta/");
        assert!(
            matches
                .iter()
                .any(|candidate| candidate.path == ".hidden.rs")
        );
        assert!(
            matches
                .iter()
                .all(|candidate| candidate.path != "ignored.rs")
        );
        assert!(
            matches
                .iter()
                .all(|candidate| !candidate.path.starts_with(".git/"))
        );
    }

    #[test]
    fn flat_directory_keeps_only_bounded_top_results() {
        let root = tempdir().unwrap();
        for index in (0..1_000).rev() {
            fs::write(root.path().join(format!("file-{index:04}.rs")), "file").unwrap();
        }

        let roots = vec![root.path().canonicalize().unwrap()];
        let mut scan = BrowseScan::start(
            &roots,
            "",
            crate::types::RESULT_LIMIT,
            true,
            crate::eligibility::ExcludeMatcher::default(),
        )
        .unwrap();
        while !scan.is_complete() {
            scan.advance(&AtomicBool::new(false));
            assert!(scan.matches().len() <= crate::types::RESULT_LIMIT);
        }

        assert_eq!(scan.matches()[0].path, "file-0000.rs");
        assert_eq!(scan.matches().len(), crate::types::RESULT_LIMIT);
    }
}
