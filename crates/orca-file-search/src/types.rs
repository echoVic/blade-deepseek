#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct SessionGeneration(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchMode {
    Fuzzy { query: String },
    Browse { directory: String },
}

impl SearchMode {
    pub fn fuzzy(query: impl Into<String>) -> Self {
        Self::Fuzzy {
            query: query.into(),
        }
    }

    pub fn browse(directory: impl Into<String>) -> Self {
        Self::Browse {
            directory: directory.into(),
        }
    }

    pub fn query(&self) -> &str {
        match self {
            Self::Fuzzy { query } => query,
            Self::Browse { directory } => directory,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchMatch {
    pub path: String,
    pub kind: MatchKind,
    pub score: u32,
    /// Unicode character positions matched by the active fuzzy pattern.
    pub indices: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchPhase {
    Searching,
    Scanning,
    Refreshing,
    Complete,
    Incomplete { message: String },
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchProgress {
    pub scanned_paths: usize,
    pub walk_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSnapshot {
    pub generation: SessionGeneration,
    pub mode: SearchMode,
    pub matches: Vec<SearchMatch>,
    pub phase: SearchPhase,
    pub progress: SearchProgress,
}

pub const RESULT_LIMIT: usize = 12;

#[cfg(test)]
mod tests {
    use super::{
        MatchKind, SearchMatch, SearchMode, SearchPhase, SearchProgress, SearchSnapshot,
        SessionGeneration,
    };

    #[test]
    fn complete_snapshot_round_trips_as_one_value() {
        let snapshot = SearchSnapshot {
            generation: SessionGeneration(7),
            mode: SearchMode::fuzzy("SrcM"),
            matches: vec![SearchMatch {
                path: "src/main.rs".to_string(),
                kind: MatchKind::File,
                score: 42,
                indices: vec![0, 4],
            }],
            phase: SearchPhase::Scanning,
            progress: SearchProgress {
                scanned_paths: 128,
                walk_complete: false,
            },
        };

        assert_eq!(snapshot.clone(), snapshot);
        assert_eq!(snapshot.mode.query(), "SrcM");
    }
}
