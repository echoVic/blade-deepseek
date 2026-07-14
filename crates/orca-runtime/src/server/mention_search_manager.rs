use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use orca_file_search::{
    SearchMode, SearchPhase, SearchProgress, SearchSession, SearchSessionOptions, SessionGeneration,
};
use serde_json::{Value, json};

use crate::mentions::{MentionCandidate, MentionCatalog, merge_candidates};
use crate::protocol::{self, ServerEvent};

#[derive(Default)]
pub(super) struct MentionSearchManager {
    sessions: HashMap<String, MentionSearchHandle>,
    reapers: Vec<JoinHandle<()>>,
    next_generation: u64,
}

impl MentionSearchManager {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn start<W: Write + Send + 'static>(
        &mut self,
        session_id: String,
        roots: Vec<PathBuf>,
        mcp_registry: orca_mcp::McpRegistry,
        exclude: Vec<String>,
        respect_gitignore: bool,
        result_limit: usize,
        event_id: Value,
        writer: Arc<Mutex<W>>,
    ) -> Result<(), String> {
        self.reap_finished();
        self.next_generation = self.next_generation.wrapping_add(1);
        let generation = SessionGeneration(self.next_generation);
        let handle = MentionSearchHandle::start(
            session_id.clone(),
            roots,
            mcp_registry,
            exclude,
            respect_gitignore,
            result_limit,
            generation,
            event_id,
            writer,
        )?;
        if let Some(previous) = self.sessions.insert(session_id, handle) {
            self.reapers.push(previous.stop_async());
        }
        Ok(())
    }

    pub(super) fn update(&self, session_id: &str, query: String) -> Result<(), String> {
        let Some(session) = self.sessions.get(session_id) else {
            return Err(format!("mention search session not found: {session_id}"));
        };
        session.update(query);
        Ok(())
    }

    pub(super) fn stop(&mut self, session_id: &str) {
        self.reap_finished();
        if let Some(session) = self.sessions.remove(session_id) {
            self.reapers.push(session.stop_async());
        }
    }

    pub(super) fn stop_all(&mut self) {
        for (_, session) in self.sessions.drain() {
            self.reapers.push(session.stop_async());
        }
        for reaper in self.reapers.drain(..) {
            let _ = reaper.join();
        }
    }

    fn reap_finished(&mut self) {
        let mut pending = Vec::with_capacity(self.reapers.len());
        for reaper in self.reapers.drain(..) {
            if reaper.is_finished() {
                let _ = reaper.join();
            } else {
                pending.push(reaper);
            }
        }
        self.reapers = pending;
    }
}

impl Drop for MentionSearchManager {
    fn drop(&mut self) {
        self.stop_all();
    }
}

struct MentionSearchHandle {
    session: Arc<SearchSession>,
    query: Arc<Mutex<String>>,
    query_revision: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    wake_tx: mpsc::SyncSender<()>,
    relay: Option<JoinHandle<()>>,
}

impl MentionSearchHandle {
    #[allow(clippy::too_many_arguments)]
    fn start<W: Write + Send + 'static>(
        session_id: String,
        roots: Vec<PathBuf>,
        mcp_registry: orca_mcp::McpRegistry,
        exclude: Vec<String>,
        respect_gitignore: bool,
        result_limit: usize,
        generation: SessionGeneration,
        event_id: Value,
        writer: Arc<Mutex<W>>,
    ) -> Result<Self, String> {
        let (wake_tx, wake_rx) = mpsc::sync_channel(1);
        let notify = wake_tx.clone();
        let options = SearchSessionOptions::new(generation, move |_| {
            let _ = notify.try_send(());
        })
        .with_excludes(exclude)
        .with_respect_gitignore(respect_gitignore)
        .with_result_limit(result_limit);
        let session = Arc::new(SearchSession::start_roots(&roots, options)?);
        session.update(SearchMode::browse(""));
        let query = Arc::new(Mutex::new(String::new()));
        let query_revision = Arc::new(AtomicU64::new(0));
        let stopped = Arc::new(AtomicBool::new(false));

        let relay_session = Arc::clone(&session);
        let relay_query = Arc::clone(&query);
        let relay_revision = Arc::clone(&query_revision);
        let relay_stopped = Arc::clone(&stopped);
        let relay = thread::Builder::new()
            .name(format!("orca-mention-search-server-{session_id}"))
            .spawn(move || {
                relay_search_snapshots(
                    session_id,
                    event_id,
                    writer,
                    relay_session,
                    roots,
                    mcp_registry,
                    result_limit,
                    relay_query,
                    relay_revision,
                    relay_stopped,
                    wake_rx,
                );
            })
            .map_err(|error| format!("failed to start mention-search relay: {error}"))?;

        Ok(Self {
            session,
            query,
            query_revision,
            stopped,
            wake_tx,
            relay: Some(relay),
        })
    }

    fn update(&self, query: String) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        *self.query.lock().unwrap_or_else(|error| error.into_inner()) = query.clone();
        self.query_revision.fetch_add(1, Ordering::AcqRel);
        if query.is_empty() {
            self.session.update(SearchMode::browse(""));
        } else {
            self.session.update(SearchMode::fuzzy(query));
        }
        let _ = self.wake_tx.try_send(());
    }

    fn stop_async(mut self) -> JoinHandle<()> {
        self.stopped.store(true, Ordering::Release);
        self.session.cancel();
        let _ = self.wake_tx.try_send(());
        thread::Builder::new()
            .name("orca-mention-search-server-reaper".to_string())
            .spawn(move || {
                if let Some(relay) = self.relay.take() {
                    let _ = relay.join();
                }
                drop(self);
            })
            .expect("mention-search reaper thread should start")
    }
}

#[allow(clippy::too_many_arguments)]
fn relay_search_snapshots<W: Write>(
    session_id: String,
    event_id: Value,
    writer: Arc<Mutex<W>>,
    session: Arc<SearchSession>,
    roots: Vec<PathBuf>,
    mcp_registry: orca_mcp::McpRegistry,
    result_limit: usize,
    query: Arc<Mutex<String>>,
    query_revision: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    wake_rx: mpsc::Receiver<()>,
) {
    let catalog = MentionCatalog::discover(&roots, &mcp_registry);
    let errors = catalog.errors().to_vec();
    let mut observed_revision = u64::MAX;
    let mut last_completed_revision = u64::MAX;
    let mut last_files = Vec::new();
    let mut last_phase = SearchPhase::Searching;
    let mut last_progress = SearchProgress::default();
    let mut last_emitted: Option<(u64, Vec<MentionCandidate>, SearchPhase, SearchProgress)> = None;

    while !stopped.load(Ordering::Acquire) {
        match wake_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if stopped.load(Ordering::Acquire) {
            break;
        }

        let current_query = query
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let revision = query_revision.load(Ordering::Acquire);
        if revision != observed_revision {
            observed_revision = revision;
            last_files.clear();
            last_phase = SearchPhase::Searching;
            last_progress = SearchProgress::default();
        }

        while let Some(snapshot) = session.take_latest_snapshot() {
            if snapshot.mode.query() != current_query {
                continue;
            }
            last_files = snapshot
                .matches
                .iter()
                .map(MentionCandidate::from_file_match)
                .collect();
            last_phase = snapshot.phase;
            last_progress = snapshot.progress;
        }

        let static_candidates = catalog.search(&current_query, result_limit);
        let candidates = merge_candidates(
            &current_query,
            static_candidates,
            last_files.clone(),
            result_limit,
        );
        let projection = (
            revision,
            candidates.clone(),
            last_phase.clone(),
            last_progress,
        );
        if last_emitted.as_ref() != Some(&projection) {
            let event = ServerEvent::MentionSearchSessionUpdated {
                session_id: session_id.clone(),
                query: current_query.clone(),
                candidates: json!(candidates),
                phase: search_phase_json(&last_phase),
                progress: json!({
                    "scannedPaths": last_progress.scanned_paths,
                    "walkComplete": last_progress.walk_complete,
                }),
                errors: json!(errors),
            };
            if !write_relay_event(&writer, &event_id, event, &stopped) {
                break;
            }
            last_emitted = Some(projection);
        }

        if matches!(last_phase, SearchPhase::Complete) && revision != last_completed_revision {
            if !write_relay_event(
                &writer,
                &event_id,
                ServerEvent::MentionSearchSessionCompleted {
                    session_id: session_id.clone(),
                    query: current_query.clone(),
                },
                &stopped,
            ) {
                break;
            }
            last_completed_revision = revision;
        }
    }
}

fn search_phase_json(phase: &SearchPhase) -> Value {
    match phase {
        SearchPhase::Searching => json!("searching"),
        SearchPhase::Scanning => json!("scanning"),
        SearchPhase::Refreshing => json!("refreshing"),
        SearchPhase::Stopping => json!("stopping"),
        SearchPhase::Complete => json!("complete"),
        SearchPhase::Incomplete { message } => {
            json!({"status": "incomplete", "message": message})
        }
    }
}

fn write_relay_event<W: Write>(
    writer: &Arc<Mutex<W>>,
    id: &Value,
    event: ServerEvent,
    stopped: &AtomicBool,
) -> bool {
    if stopped.load(Ordering::Acquire) {
        return false;
    }
    let Ok(mut writer) = writer.lock() else {
        return false;
    };
    if stopped.load(Ordering::Acquire) {
        return false;
    }
    protocol::write_server_event(&mut *writer, id, event).is_ok()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io;
    use std::sync::mpsc;

    use tempfile::tempdir;

    use super::*;

    struct BlockingWriter {
        entered: Option<mpsc::Sender<()>>,
        release: mpsc::Receiver<()>,
    }

    impl Write for BlockingWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
                let _ = self.release.recv_timeout(Duration::from_secs(2));
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stop_all_waits_for_reapers_created_by_individual_stop() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("main.rs"), "fn main() {}").unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer = Arc::new(Mutex::new(BlockingWriter {
            entered: Some(entered_tx),
            release: release_rx,
        }));
        let mut manager = MentionSearchManager::default();
        manager
            .start(
                "mentions".to_string(),
                vec![root.path().to_path_buf()],
                orca_mcp::McpRegistry::default(),
                Vec::new(),
                true,
                12,
                json!("mentions-start"),
                writer,
            )
            .unwrap();
        manager.update("mentions", "main".to_string()).unwrap();
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("relay started writing");
        manager.stop("mentions");

        let (done_tx, done_rx) = mpsc::channel();
        let waiter = thread::spawn(move || {
            manager.stop_all();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(50)).is_err(),
            "stop_all returned before the detached relay was joined"
        );
        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("stop_all joined the relay");
        waiter.join().unwrap();
    }
}
