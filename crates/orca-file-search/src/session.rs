use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, bounded};
use nucleo::pattern::{CaseMatching, Normalization};
use nucleo::{Config, Matcher, Nucleo};

use crate::browse::BrowseScan;
use crate::discovery::{DiscoveryControl, spawn_synthetic_discovery, spawn_workspace_discovery};
use crate::eligibility::{Candidate, ExcludeMatcher};
use crate::freshness::RootFingerprint;
use crate::types::{
    MatchKind, RESULT_LIMIT, SearchMatch, SearchMode, SearchPhase, SearchProgress, SearchSnapshot,
    SessionGeneration,
};

const MATCH_TICK_BUDGET_MS: u64 = 4;
const ACTIVE_POLL_MS: u64 = 1;
const IDLE_POLL_MS: u64 = 10;
const SNAPSHOT_INTERVAL: Duration = Duration::from_millis(16);
const MAX_WALK_THREADS: usize = 4;
const MAX_MATCH_THREADS: usize = 4;

type Notify = Arc<dyn Fn(SessionGeneration) + Send + Sync>;

#[derive(Clone)]
pub struct SearchSessionOptions {
    generation: SessionGeneration,
    walk_threads: NonZeroUsize,
    match_threads: NonZeroUsize,
    result_limit: usize,
    exclude: Vec<String>,
    respect_gitignore: bool,
    notify: Notify,
}

impl SearchSessionOptions {
    pub fn new(
        generation: SessionGeneration,
        notify: impl Fn(SessionGeneration) + Send + Sync + 'static,
    ) -> Self {
        let parallelism = std::thread::available_parallelism()
            .map(NonZeroUsize::get)
            .unwrap_or(1);
        let walk_threads = NonZeroUsize::new((parallelism / 2).clamp(1, MAX_WALK_THREADS)).unwrap();
        let match_threads =
            NonZeroUsize::new(parallelism.div_ceil(3).clamp(1, MAX_MATCH_THREADS)).unwrap();
        Self {
            generation,
            walk_threads,
            match_threads,
            result_limit: RESULT_LIMIT,
            exclude: Vec::new(),
            respect_gitignore: true,
            notify: Arc::new(notify),
        }
    }

    pub fn with_threads(mut self, walk_threads: NonZeroUsize, match_threads: NonZeroUsize) -> Self {
        self.walk_threads = NonZeroUsize::new(walk_threads.get().min(MAX_WALK_THREADS)).unwrap();
        self.match_threads = NonZeroUsize::new(match_threads.get().min(MAX_MATCH_THREADS)).unwrap();
        self
    }

    pub fn with_result_limit(mut self, result_limit: usize) -> Self {
        self.result_limit = result_limit.clamp(1, 100);
        self
    }

    pub fn with_excludes(mut self, excludes: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.exclude = excludes.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_respect_gitignore(mut self, respect_gitignore: bool) -> Self {
        self.respect_gitignore = respect_gitignore;
        self
    }
}

pub struct SearchSession {
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
    fingerprints: Vec<RootFingerprint>,
}

impl SearchSession {
    pub fn start(root: &Path, options: SearchSessionOptions) -> Result<Self, String> {
        Self::start_roots(&[root.to_path_buf()], options)
    }

    pub fn start_roots(roots: &[PathBuf], options: SearchSessionOptions) -> Result<Self, String> {
        let fingerprints = capture_roots(roots)?;
        Self::start_inner(fingerprints, options, None)
    }

    #[doc(hidden)]
    pub fn start_with_paths(
        root: &Path,
        options: SearchSessionOptions,
        paths: Vec<String>,
    ) -> Result<Self, String> {
        let fingerprints = capture_roots(&[root.to_path_buf()])?;
        Self::start_inner(fingerprints, options, Some(paths))
    }

    fn start_inner(
        fingerprints: Vec<RootFingerprint>,
        options: SearchSessionOptions,
        synthetic_paths: Option<Vec<String>>,
    ) -> Result<Self, String> {
        let roots = fingerprints
            .iter()
            .map(|fingerprint| fingerprint.canonical_root.clone())
            .collect::<Vec<_>>();
        let exclude = ExcludeMatcher::compile(&options.exclude)?;
        let (wake_tx, wake_rx) = bounded::<()>(1);
        let notify_tx = wake_tx.clone();
        let mut nucleo = Nucleo::new(
            Config::DEFAULT.match_paths(),
            Arc::new(move || {
                let _ = notify_tx.try_send(());
            }),
            Some(options.match_threads.get()),
            1,
        );
        // Orca only needs the best 12 results. Letting Nucleo fully sort up to
        // one million matches dominates arbitrary-query latency, so retain the
        // complete match set in input order and select the bounded top-N below.
        nucleo.sort_results(false);
        let injector = nucleo.injector();
        let shared = Arc::new(Shared {
            roots: roots.clone(),
            generation: AtomicU64::new(options.generation.0),
            query: LatestQuery::new(options.generation),
            snapshots: SnapshotSlot::new(options.notify),
            shutdown: Arc::new(AtomicBool::new(false)),
            scanned_paths: Arc::new(AtomicUsize::new(0)),
            walk_complete: Arc::new(AtomicBool::new(false)),
            error_count: Arc::new(AtomicUsize::new(0)),
            wake_tx: wake_tx.clone(),
            result_limit: options.result_limit,
            exclude: exclude.clone(),
            respect_gitignore: options.respect_gitignore,
        });

        let matcher_shared = shared.clone();
        let matcher_handle = std::thread::Builder::new()
            .name("orca-file-matcher".to_string())
            .spawn(move || matcher_worker(matcher_shared, wake_rx, nucleo))
            .map_err(|error| format!("failed to start file matcher: {error}"))?;

        let discovery_control = DiscoveryControl {
            shutdown: shared.shutdown.clone(),
            scanned_paths: shared.scanned_paths.clone(),
            walk_complete: shared.walk_complete.clone(),
            error_count: shared.error_count.clone(),
            wake_tx,
        };
        let discovery_handle = if let Some(paths) = synthetic_paths {
            spawn_synthetic_discovery(paths, injector, discovery_control)
        } else {
            spawn_workspace_discovery(
                roots,
                options.walk_threads.get(),
                options.respect_gitignore,
                exclude,
                injector,
                discovery_control,
            )
        };

        Ok(Self {
            shared,
            handles: vec![matcher_handle, discovery_handle],
            fingerprints,
        })
    }

    pub fn generation(&self) -> SessionGeneration {
        SessionGeneration(self.shared.generation.load(Ordering::Acquire))
    }

    pub fn set_generation(&self, generation: SessionGeneration) {
        self.shared
            .generation
            .store(generation.0, Ordering::Release);
        self.shared.snapshots.clear();
        self.shared.query.update(generation, None);
        self.shared.wake();
    }

    pub fn update(&self, mode: SearchMode) {
        if self.shared.query.update(self.generation(), Some(mode)) {
            self.shared.wake();
        }
    }

    pub fn clear_query(&self) {
        if self.shared.query.update(self.generation(), None) {
            self.shared.wake();
        }
    }

    pub fn take_latest_snapshot(&self) -> Option<SearchSnapshot> {
        self.shared.snapshots.take()
    }

    pub fn tracked_state_changed(&self) -> bool {
        self.fingerprints
            .iter()
            .any(RootFingerprint::tracked_state_changed)
    }

    pub fn cancel(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared.wake();
    }

    pub fn is_finished(&self) -> bool {
        self.handles.iter().all(JoinHandle::is_finished)
    }

    pub fn join(&mut self) {
        self.cancel();
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for SearchSession {
    fn drop(&mut self) {
        self.join();
    }
}

struct Shared {
    roots: Vec<PathBuf>,
    generation: AtomicU64,
    query: LatestQuery,
    snapshots: SnapshotSlot,
    shutdown: Arc<AtomicBool>,
    scanned_paths: Arc<AtomicUsize>,
    walk_complete: Arc<AtomicBool>,
    error_count: Arc<AtomicUsize>,
    wake_tx: Sender<()>,
    result_limit: usize,
    exclude: ExcludeMatcher,
    respect_gitignore: bool,
}

impl Shared {
    fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }

    fn progress(&self) -> SearchProgress {
        SearchProgress {
            scanned_paths: self.scanned_paths.load(Ordering::Acquire),
            walk_complete: self.walk_complete.load(Ordering::Acquire),
        }
    }
}

struct LatestQuery {
    value: Mutex<(u64, SessionGeneration, Option<SearchMode>)>,
}

impl LatestQuery {
    fn new(generation: SessionGeneration) -> Self {
        Self {
            value: Mutex::new((0, generation, None)),
        }
    }

    fn update(&self, generation: SessionGeneration, mode: Option<SearchMode>) -> bool {
        let mut value = lock(&self.value);
        if value.1 == generation && value.2 == mode {
            return false;
        }
        value.0 = value.0.wrapping_add(1);
        value.1 = generation;
        value.2 = mode;
        true
    }

    fn load(&self) -> (u64, SessionGeneration, Option<SearchMode>) {
        lock(&self.value).clone()
    }
}

struct SnapshotSlot {
    value: Mutex<Option<SearchSnapshot>>,
    dirty: AtomicBool,
    notify: Notify,
}

impl SnapshotSlot {
    fn new(notify: Notify) -> Self {
        Self {
            value: Mutex::new(None),
            dirty: AtomicBool::new(false),
            notify,
        }
    }

    fn publish(&self, snapshot: SearchSnapshot) {
        let generation = snapshot.generation;
        let should_notify = {
            let mut value = lock(&self.value);
            if value.as_ref() == Some(&snapshot) {
                return;
            }
            *value = Some(snapshot);
            !self.dirty.swap(true, Ordering::AcqRel)
        };
        if should_notify {
            (self.notify)(generation);
        }
    }

    fn take(&self) -> Option<SearchSnapshot> {
        let mut value = lock(&self.value);
        let snapshot = value.take();
        self.dirty.store(false, Ordering::Release);
        snapshot
    }

    fn clear(&self) {
        lock(&self.value).take();
        self.dirty.store(false, Ordering::Release);
    }
}

fn matcher_worker(shared: Arc<Shared>, wake_rx: Receiver<()>, mut nucleo: Nucleo<Candidate>) {
    let mut active_revision = 0u64;
    let mut active_generation = SessionGeneration::default();
    let mut active_mode: Option<SearchMode> = None;
    let mut last_fuzzy_query = String::new();
    let mut browse_scan = None;
    let mut browse_error = None;
    let mut index_matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut last_publish = Instant::now()
        .checked_sub(SNAPSHOT_INTERVAL)
        .unwrap_or_else(Instant::now);
    let mut last_progress = SearchProgress::default();
    let mut force_publish = false;
    let mut poll_ms = IDLE_POLL_MS;

    while let Ok(()) | Err(RecvTimeoutError::Timeout) =
        wake_rx.recv_timeout(Duration::from_millis(poll_ms))
    {
        if shared.shutdown.load(Ordering::Acquire) {
            break;
        }

        let (revision, requested_generation, requested_mode) = shared.query.load();
        if revision != active_revision {
            active_revision = revision;
            active_generation = requested_generation;
            active_mode = requested_mode;
            browse_scan = None;
            browse_error = None;
            if let Some(mode) = &active_mode {
                match mode {
                    SearchMode::Fuzzy { query } => {
                        let append = query.starts_with(&last_fuzzy_query);
                        nucleo.pattern.reparse(
                            0,
                            query,
                            CaseMatching::Smart,
                            Normalization::Smart,
                            append,
                        );
                        last_fuzzy_query.clone_from(query);
                    }
                    SearchMode::Browse { directory } => {
                        match BrowseScan::start(
                            &shared.roots,
                            directory,
                            shared.result_limit,
                            shared.respect_gitignore,
                            shared.exclude.clone(),
                        ) {
                            Ok(scan) => browse_scan = Some(scan),
                            Err(error) => browse_error = Some(error),
                        }
                    }
                }
                force_publish = true;
            }
        }

        let browse_changed = browse_scan
            .as_mut()
            .is_some_and(|scan| scan.advance(&shared.shutdown));
        if browse_error.is_none() {
            browse_error = browse_scan.as_ref().and_then(BrowseScan::error_message);
        }
        let status = nucleo.tick(MATCH_TICK_BUDGET_MS);
        poll_ms = if status.running {
            ACTIVE_POLL_MS
        } else {
            IDLE_POLL_MS
        };
        let progress = shared.progress();
        let terminal = match &active_mode {
            Some(SearchMode::Browse { .. }) => {
                browse_scan.as_ref().is_none_or(BrowseScan::is_complete)
            }
            Some(SearchMode::Fuzzy { .. }) => progress.walk_complete && !status.running,
            None => false,
        };
        let due = last_publish.elapsed() >= SNAPSHOT_INTERVAL;
        let progress_changed = progress != last_progress;
        if active_mode.is_some()
            && due
            && (force_publish || terminal || status.changed || progress_changed || browse_changed)
        {
            let mode = active_mode.clone().unwrap();
            let matches = match &mode {
                SearchMode::Fuzzy { .. } if force_publish && status.running && !status.changed => {
                    Vec::new()
                }
                SearchMode::Fuzzy { .. } => fuzzy_matches(
                    &nucleo,
                    &mut index_matcher,
                    &shared.roots,
                    shared.result_limit,
                ),
                SearchMode::Browse { .. } => browse_scan
                    .as_ref()
                    .map_or_else(Vec::new, |scan| scan.matches().to_vec()),
            };
            let error_count = shared.error_count.load(Ordering::Acquire);
            let phase = if let Some(message) = browse_error.clone() {
                SearchPhase::Incomplete { message }
            } else if terminal && error_count > 0 {
                SearchPhase::Incomplete {
                    message: format!("search completed with {error_count} traversal errors"),
                }
            } else if terminal {
                SearchPhase::Complete
            } else if progress.scanned_paths == 0 {
                SearchPhase::Searching
            } else {
                SearchPhase::Scanning
            };
            shared.snapshots.publish(SearchSnapshot {
                generation: active_generation,
                mode,
                matches,
                phase,
                progress,
            });
            last_publish = Instant::now();
            last_progress = progress;
            force_publish = false;
        }
    }
}

fn fuzzy_matches(
    nucleo: &Nucleo<Candidate>,
    index_matcher: &mut Matcher,
    roots: &[PathBuf],
    result_limit: usize,
) -> Vec<SearchMatch> {
    let snapshot = nucleo.snapshot();
    let column_pattern = snapshot.pattern().column_pattern(0);
    let mut best = BinaryHeap::with_capacity(result_limit + 1);
    for (matched, item) in snapshot.matches().iter().zip(snapshot.matched_items(..)) {
        let ranked = RankedCandidate {
            score: matched.score,
            index: matched.idx,
            candidate: item.data,
            haystack: &item.matcher_columns[0],
        };
        if best.len() < result_limit {
            best.push(ranked);
        } else if best.peek().is_some_and(|worst| ranked < *worst) {
            best.pop();
            best.push(ranked);
        }
    }

    best.into_sorted_vec()
        .into_iter()
        .map(|ranked| {
            let mut indices = Vec::new();
            let _ = column_pattern.indices(ranked.haystack.slice(..), index_matcher, &mut indices);
            indices.sort_unstable();
            indices.dedup();
            SearchMatch {
                root: roots
                    .get(ranked.candidate.root_index)
                    .cloned()
                    .unwrap_or_default(),
                path: ranked.candidate.path.clone(),
                kind: ranked.candidate.kind,
                score: ranked.score,
                indices,
            }
        })
        .collect()
}

struct RankedCandidate<'a> {
    score: u32,
    index: u32,
    candidate: &'a Candidate,
    haystack: &'a nucleo::Utf32String,
}

impl PartialEq for RankedCandidate<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.score == other.score
            && self.index == other.index
            && self.candidate.kind == other.candidate.kind
            && self.candidate.root_index == other.candidate.root_index
            && self.candidate.path == other.candidate.path
    }
}

impl Eq for RankedCandidate<'_> {}

impl PartialOrd for RankedCandidate<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}

impl Ord for RankedCandidate<'_> {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other
            .score
            .cmp(&self.score)
            .then_with(|| {
                match_kind_order(self.candidate.kind).cmp(&match_kind_order(other.candidate.kind))
            })
            .then_with(|| self.candidate.path.len().cmp(&other.candidate.path.len()))
            .then_with(|| self.candidate.path.cmp(&other.candidate.path))
            .then_with(|| self.candidate.root_index.cmp(&other.candidate.root_index))
            .then_with(|| self.index.cmp(&other.index))
    }
}

fn capture_roots(roots: &[PathBuf]) -> Result<Vec<RootFingerprint>, String> {
    if roots.is_empty() {
        return Err("file search requires at least one root".to_string());
    }
    let mut fingerprints = Vec::new();
    for root in roots {
        let fingerprint = RootFingerprint::capture(root)?;
        if fingerprints
            .iter()
            .any(|existing: &RootFingerprint| existing.canonical_root == fingerprint.canonical_root)
        {
            continue;
        }
        fingerprints.push(fingerprint);
    }
    if fingerprints.is_empty() {
        return Err("file search requires at least one distinct root".to_string());
    }
    Ok(fingerprints)
}

fn match_kind_order(kind: MatchKind) -> u8 {
    match kind {
        MatchKind::File => 0,
        MatchKind::Directory => 1,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{LatestQuery, SearchSessionOptions, SnapshotSlot};
    use crate::types::{
        SearchMode, SearchPhase, SearchProgress, SearchSnapshot, SessionGeneration,
    };

    #[test]
    fn latest_query_replaces_unprocessed_values() {
        let generation = SessionGeneration(1);
        let query = LatestQuery::new(generation);

        assert!(query.update(generation, Some(SearchMode::fuzzy("a"))));
        assert!(query.update(generation, Some(SearchMode::fuzzy("ab"))));
        assert!(query.update(generation, Some(SearchMode::fuzzy("abc"))));

        let (_, installed_generation, mode) = query.load();
        assert_eq!(installed_generation, generation);
        assert_eq!(mode, Some(SearchMode::fuzzy("abc")));
    }

    #[test]
    fn session_options_cap_compute_concurrency() {
        let options = SearchSessionOptions::new(SessionGeneration(1), |_| {}).with_threads(
            NonZeroUsize::new(32).unwrap(),
            NonZeroUsize::new(32).unwrap(),
        );

        assert_eq!(options.walk_threads.get(), 4);
        assert_eq!(options.match_threads.get(), 4);
    }

    #[test]
    fn snapshot_slot_coalesces_notifications_and_returns_latest() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let notify_count = notifications.clone();
        let slot = SnapshotSlot::new(Arc::new(move |_| {
            notify_count.fetch_add(1, Ordering::Relaxed);
        }));
        let snapshot = |query: &str| SearchSnapshot {
            generation: SessionGeneration(4),
            mode: SearchMode::fuzzy(query),
            matches: Vec::new(),
            phase: SearchPhase::Searching,
            progress: SearchProgress::default(),
        };

        slot.publish(snapshot("a"));
        slot.publish(snapshot("ab"));
        slot.publish(snapshot("abc"));

        assert_eq!(notifications.load(Ordering::Relaxed), 1);
        assert_eq!(slot.take(), Some(snapshot("abc")));

        slot.publish(snapshot("abcd"));
        assert_eq!(notifications.load(Ordering::Relaxed), 2);
    }
}
