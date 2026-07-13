use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use orca_file_search::{
    SearchMode, SearchPhase, SearchSession, SearchSessionOptions, SearchSnapshot, SessionGeneration,
};
use orca_runtime::mentions::{self, MentionToken};

use crate::types::{AppState, AppStatus, TuiEvent};

const WARM_IDLE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TokenIdentity {
    start: usize,
    quoted: bool,
}

impl From<&MentionToken> for TokenIdentity {
    fn from(token: &MentionToken) -> Self {
        Self {
            start: token.start,
            quoted: token.quoted,
        }
    }
}

pub(crate) struct MentionSearchManager {
    root: PathBuf,
    event_tx: mpsc::Sender<TuiEvent>,
    next_generation: u64,
    session: Option<SearchSession>,
    stopping: Option<SearchSession>,
    active_token: Option<TokenIdentity>,
    active_generation: Option<SessionGeneration>,
    active_query: Option<String>,
    warm_deadline: Option<Instant>,
    refreshing: bool,
}

impl MentionSearchManager {
    pub(crate) fn is_enabled(state: &AppState) -> bool {
        matches!(state.status, AppStatus::Idle | AppStatus::WaitingUserInput)
            && state.slash_menu.is_none()
    }

    pub(crate) fn new(root: PathBuf, event_tx: mpsc::Sender<TuiEvent>) -> Self {
        Self {
            root,
            event_tx,
            next_generation: 0,
            session: None,
            stopping: None,
            active_token: None,
            active_generation: None,
            active_query: None,
            warm_deadline: None,
            refreshing: false,
        }
    }

    pub(crate) fn set_root(&mut self, root: PathBuf, state: &mut AppState) {
        let root = root.canonicalize().unwrap_or(root);
        if self.root == root {
            return;
        }
        self.root = root;
        self.warm_deadline = None;
        self.active_token = None;
        self.active_generation = None;
        self.active_query = None;
        self.refreshing = false;
        state.mention.dismissed_query = None;
        state.mention.clear_projection();
        self.begin_stop();
    }

    #[cfg(test)]
    pub(crate) fn sync(&mut self, text: &str, enabled: bool, state: &mut AppState, now: Instant) {
        self.sync_at_cursor(text, text.len(), enabled, state, now);
    }

    pub(crate) fn sync_at_cursor(
        &mut self,
        text: &str,
        cursor: usize,
        enabled: bool,
        state: &mut AppState,
        now: Instant,
    ) {
        self.poll(now);
        let token = enabled
            .then(|| mentions::mention_token_at_cursor(text, cursor))
            .flatten();
        let Some(token) = token else {
            self.deactivate(state, now);
            return;
        };

        let identity = TokenIdentity::from(&token);
        if state
            .mention
            .dismissed_query
            .as_deref()
            .is_some_and(|dismissed| dismissed != token.query)
        {
            state.mention.dismissed_query = None;
        }
        if self.active_token != Some(identity) {
            let generation = self.advance_generation();
            self.active_generation = Some(generation);
            if let Some(session) = &self.session {
                session.set_generation(generation);
            }
        }
        self.active_token = Some(identity);
        self.active_query = Some(token.query.clone());
        state.mention.pending_query = Some(token.query.clone());

        if self.warm_deadline.take().is_some()
            && self
                .session
                .as_ref()
                .is_some_and(SearchSession::tracked_state_changed)
        {
            self.refreshing = true;
            self.begin_stop();
            state.mention.phase = Some(SearchPhase::Refreshing);
            return;
        }

        if self.session.is_none() {
            if self.stopping.is_some() {
                state.mention.phase = Some(if self.refreshing {
                    SearchPhase::Refreshing
                } else {
                    SearchPhase::Searching
                });
                return;
            }
            if let Err(error) = self.start_session() {
                state.mention.phase = Some(SearchPhase::Incomplete { message: error });
                return;
            }
        }

        let mode = if token.query.is_empty() || token.query.ends_with('/') {
            SearchMode::browse(token.query.clone())
        } else {
            SearchMode::fuzzy(token.query.clone())
        };
        if let Some(session) = &self.session {
            session.update(mode);
        }
        if state.mention.dismissed_query.as_deref() == Some(token.query.as_str()) {
            state.mention.phase = None;
            state.mention.candidates.clear();
        } else if state.mention.phase.is_none() {
            state.mention.phase = Some(SearchPhase::Searching);
        }
    }

    #[cfg(test)]
    pub(crate) fn consume_dirty(
        &mut self,
        generation: SessionGeneration,
        text: &str,
        state: &mut AppState,
    ) {
        self.consume_dirty_at_cursor(generation, text, text.len(), state);
    }

    pub(crate) fn consume_dirty_at_cursor(
        &mut self,
        _event_generation: SessionGeneration,
        text: &str,
        cursor: usize,
        state: &mut AppState,
    ) {
        let Some(session) = &self.session else {
            return;
        };
        let Some(snapshot) = session.take_latest_snapshot() else {
            return;
        };
        self.apply_snapshot_at_cursor(snapshot, text, cursor, state);
    }

    pub(crate) fn poll(&mut self, now: Instant) {
        if self.warm_deadline.is_some_and(|deadline| now >= deadline) {
            self.warm_deadline = None;
            self.begin_stop();
        }
        if self
            .stopping
            .as_ref()
            .is_some_and(SearchSession::is_finished)
            && let Some(mut session) = self.stopping.take()
        {
            session.join();
        }
    }

    fn start_session(&mut self) -> Result<(), String> {
        let generation = self.active_generation.unwrap_or_else(|| {
            let generation = self.advance_generation();
            self.active_generation = Some(generation);
            generation
        });
        let event_tx = self.event_tx.clone();
        let options = SearchSessionOptions::new(generation, move |generation| {
            let _ = event_tx.send(TuiEvent::MentionSearchDirty { generation });
        });
        self.session = Some(SearchSession::start(&self.root, options)?);
        self.refreshing = false;
        Ok(())
    }

    fn deactivate(&mut self, state: &mut AppState, now: Instant) {
        if self.active_token.take().is_some() && self.session.is_some() {
            self.warm_deadline = Some(now + WARM_IDLE);
            let generation = self.advance_generation();
            if let Some(session) = &self.session {
                session.set_generation(generation);
                session.clear_query();
            }
        }
        self.active_generation = None;
        self.active_query = None;
        self.refreshing = false;
        state.mention.clear_projection();
    }

    fn begin_stop(&mut self) {
        if self.stopping.is_none()
            && let Some(session) = self.session.take()
        {
            session.cancel();
            self.stopping = Some(session);
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.begin_stop();
        if let Some(session) = self.stopping.take() {
            session.cancel();
            let _ = std::thread::Builder::new()
                .name("orca-file-search-reaper".to_string())
                .spawn(move || {
                    let mut session = session;
                    session.join();
                });
        }
    }

    #[cfg(test)]
    fn apply_snapshot(&self, snapshot: SearchSnapshot, text: &str, state: &mut AppState) {
        self.apply_snapshot_at_cursor(snapshot, text, text.len(), state);
    }

    fn apply_snapshot_at_cursor(
        &self,
        snapshot: SearchSnapshot,
        text: &str,
        cursor: usize,
        state: &mut AppState,
    ) {
        if self.session.as_ref().map(SearchSession::generation) != Some(snapshot.generation) {
            return;
        }
        if self.active_generation != Some(snapshot.generation) {
            return;
        }
        let Some(token) = mentions::mention_token_at_cursor(text, cursor) else {
            return;
        };
        if self.active_token != Some(TokenIdentity::from(&token))
            || self.active_query.as_deref() != Some(token.query.as_str())
            || state.mention.pending_query.as_deref() != Some(token.query.as_str())
            || snapshot.mode.query() != token.query
            || state.mention.dismissed_query.as_deref() == Some(token.query.as_str())
        {
            return;
        }

        let previous_index = state.mention.selected;
        let anchored_path = state
            .mention
            .manual_selection
            .then(|| state.mention.selected_path.clone())
            .flatten();
        state.mention.candidates = snapshot.matches;
        state.mention.phase = Some(snapshot.phase);
        state.mention.progress = snapshot.progress;
        if let Some(path) = anchored_path {
            state.mention.selected = state
                .mention
                .candidates
                .iter()
                .position(|candidate| candidate.path == path)
                .unwrap_or_else(|| {
                    previous_index.min(state.mention.candidates.len().saturating_sub(1))
                });
        } else {
            state.mention.selected = 0;
        }
        state.mention.selected_path = state
            .mention
            .candidates
            .get(state.mention.selected)
            .map(|candidate| candidate.path.clone());
    }

    fn advance_generation(&mut self) -> SessionGeneration {
        self.next_generation = self.next_generation.wrapping_add(1);
        SessionGeneration(self.next_generation)
    }
}

impl Drop for MentionSearchManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use orca_file_search::{
        MatchKind, SearchMatch, SearchMode, SearchPhase, SearchProgress, SearchSnapshot,
        SessionGeneration,
    };
    use tempfile::tempdir;

    use super::{MentionSearchManager, TokenIdentity};
    use crate::types::AppState;

    fn state() -> AppState {
        let (action_tx, _action_rx) = mpsc::channel();
        AppState::new(
            action_tx,
            "test".to_string(),
            "model".to_string(),
            "/tmp".to_string(),
        )
    }

    fn snapshot(generation: SessionGeneration, query: &str, paths: &[&str]) -> SearchSnapshot {
        SearchSnapshot {
            generation,
            mode: SearchMode::fuzzy(query),
            matches: paths
                .iter()
                .map(|path| SearchMatch {
                    path: (*path).to_string(),
                    kind: MatchKind::File,
                    score: 1,
                    indices: Vec::new(),
                })
                .collect(),
            phase: SearchPhase::Scanning,
            progress: SearchProgress {
                scanned_paths: paths.len(),
                walk_complete: false,
            },
        }
    }

    #[test]
    fn warm_session_expires_after_thirty_seconds() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("main.rs"), "main").unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        let now = Instant::now();

        manager.sync("@main", true, &mut state, now);
        manager.sync("", true, &mut state, now);
        manager.poll(now + Duration::from_secs(29));
        assert!(manager.session.is_some());

        manager.poll(now + Duration::from_secs(30));
        assert!(manager.session.is_none());
    }

    #[test]
    fn dismissed_query_stays_hidden_until_query_changes() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("main.rs"), "main").unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        state.mention.dismissed_query = Some("main".to_string());

        manager.sync("@main", true, &mut state, Instant::now());
        assert_eq!(state.mention.phase, None);

        manager.sync("@mainr", true, &mut state, Instant::now());
        assert_ne!(state.mention.phase, Some(SearchPhase::Complete));
        assert_eq!(state.mention.dismissed_query, None);
    }

    #[test]
    fn stale_mention_generation_is_ignored_before_snapshot_consumption() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("main.rs"), "main").unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@main", true, &mut state, Instant::now());
        state.mention.candidates = snapshot(SessionGeneration(0), "main", &["sentinel.rs"]).matches;

        let current = manager.session.as_ref().unwrap().generation();
        manager.consume_dirty(SessionGeneration(current.0 + 1), "@main", &mut state);

        assert_eq!(state.mention.candidates[0].path, "sentinel.rs");
    }

    #[test]
    fn stale_mention_token_identity_is_ignored() {
        let root = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@main", true, &mut state, Instant::now());
        let generation = manager.session.as_ref().unwrap().generation();
        state.mention.candidates = snapshot(generation, "main", &["sentinel.rs"]).matches;

        manager.apply_snapshot(
            snapshot(generation, "main", &["new.rs"]),
            "prefix @main",
            &mut state,
        );

        assert_eq!(state.mention.candidates[0].path, "sentinel.rs");
    }

    #[test]
    fn stale_mention_pending_query_is_ignored() {
        let root = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@main", true, &mut state, Instant::now());
        let generation = manager.session.as_ref().unwrap().generation();
        state.mention.pending_query = Some("older".to_string());
        state.mention.candidates = snapshot(generation, "main", &["sentinel.rs"]).matches;

        manager.apply_snapshot(
            snapshot(generation, "main", &["new.rs"]),
            "@main",
            &mut state,
        );

        assert_eq!(state.mention.candidates[0].path, "sentinel.rs");
    }

    #[test]
    fn mention_selection_stays_anchored_by_path_across_snapshots() {
        let root = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@m", true, &mut state, Instant::now());
        let generation = manager.session.as_ref().unwrap().generation();
        manager.active_token = Some(TokenIdentity {
            start: 0,
            quoted: false,
        });
        manager.active_query = Some("m".to_string());
        state.mention.pending_query = Some("m".to_string());
        state.mention.candidates = snapshot(generation, "m", &["a.rs", "b.rs", "c.rs"]).matches;
        state.mention.selected = 1;
        state.mention.selected_path = Some("b.rs".to_string());
        state.mention.manual_selection = true;

        manager.apply_snapshot(
            snapshot(generation, "m", &["c.rs", "a.rs", "b.rs"]),
            "@m",
            &mut state,
        );
        assert_eq!(state.mention.selected, 2);
        assert_eq!(state.mention.selected_path.as_deref(), Some("b.rs"));

        manager.apply_snapshot(
            snapshot(generation, "m", &["c.rs", "a.rs"]),
            "@m",
            &mut state,
        );
        assert_eq!(state.mention.selected, 1);
        assert_eq!(state.mention.selected_path.as_deref(), Some("a.rs"));
    }

    #[test]
    fn moving_between_same_query_tokens_advances_generation_without_a_second_catalog() {
        let root = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        let text = "@same and @same";

        manager.sync_at_cursor(text, 5, true, &mut state, Instant::now());
        let first = manager.session.as_ref().unwrap().generation();
        manager.sync_at_cursor(text, text.len(), true, &mut state, Instant::now());
        let second = manager.session.as_ref().unwrap().generation();

        assert_ne!(first, second);
        assert!(manager.stopping.is_none());
    }

    #[test]
    fn shutdown_cancels_without_joining_on_the_tui_thread() {
        let root = tempdir().unwrap();
        for index in 0..2_000 {
            fs::write(root.path().join(format!("file-{index}.rs")), "file").unwrap();
        }
        let (event_tx, _event_rx) = mpsc::channel();
        let mut manager = MentionSearchManager::new(root.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@file", true, &mut state, Instant::now());

        let started = Instant::now();
        manager.shutdown();

        assert!(started.elapsed() < Duration::from_millis(50));
        assert!(manager.session.is_none());
        assert!(manager.stopping.is_none());
    }

    #[test]
    fn changing_root_stops_old_generation_before_starting_new_one() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let (event_tx, _event_rx) = mpsc::channel();
        let mut manager = MentionSearchManager::new(first.path().to_path_buf(), event_tx);
        let mut state = state();
        manager.sync("@main", true, &mut state, Instant::now());
        let first_generation = manager.session.as_ref().unwrap().generation();

        manager.set_root(second.path().to_path_buf(), &mut state);

        assert!(manager.session.is_none());
        let mut stopping = manager.stopping.take().unwrap();
        stopping.join();
        manager.sync("@main", true, &mut state, Instant::now());
        let second_generation = manager.session.as_ref().unwrap().generation();
        assert_ne!(first_generation, second_generation);
        assert_eq!(manager.root, second.path().canonicalize().unwrap());
    }
}
