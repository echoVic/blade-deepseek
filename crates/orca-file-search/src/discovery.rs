use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use ignore::{DirEntry, WalkBuilder, WalkState};
use nucleo::{Injector, Utf32String};

use crate::eligibility::{Candidate, candidate_from_path, is_vcs_metadata_name};
use crate::types::MatchKind;

#[derive(Clone)]
pub(crate) struct DiscoveryControl {
    pub shutdown: Arc<AtomicBool>,
    pub scanned_paths: Arc<AtomicUsize>,
    pub walk_complete: Arc<AtomicBool>,
    pub error_count: Arc<AtomicUsize>,
    pub wake_tx: Sender<()>,
}

impl DiscoveryControl {
    fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }
}

pub(crate) fn spawn_workspace_discovery(
    root: PathBuf,
    threads: usize,
    injector: Injector<Candidate>,
    control: DiscoveryControl,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("orca-file-walker".to_string())
        .spawn(move || {
            let mut builder = WalkBuilder::new(&root);
            builder
                .threads(threads)
                .hidden(false)
                .follow_links(false)
                .require_git(true)
                .filter_entry(not_vcs_metadata);
            let walker = builder.build_parallel();
            walker.run(|| {
                let root = root.clone();
                let injector = injector.clone();
                let control = control.clone();
                let mut processed_since_check = 0usize;
                let mut last_check = Instant::now();
                Box::new(move |entry| {
                    if should_stop(&control, &mut processed_since_check, &mut last_check) {
                        return WalkState::Quit;
                    }
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(_) => {
                            control.error_count.fetch_add(1, Ordering::Relaxed);
                            return WalkState::Continue;
                        }
                    };
                    let Some(candidate) = candidate_from_path(&root, entry.path()) else {
                        return WalkState::Continue;
                    };
                    injector.push(candidate, |candidate, columns| {
                        columns[0] = Utf32String::from(candidate.path.as_str());
                    });
                    control.scanned_paths.fetch_add(1, Ordering::Relaxed);
                    control.wake();
                    WalkState::Continue
                })
            });
            control.walk_complete.store(true, Ordering::Release);
            control.wake();
        })
        .expect("file discovery thread should start")
}

pub(crate) fn spawn_synthetic_discovery(
    paths: Vec<String>,
    injector: Injector<Candidate>,
    control: DiscoveryControl,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("orca-file-synthetic-walker".to_string())
        .spawn(move || {
            let mut processed_since_check = 0usize;
            let mut last_check = Instant::now();
            for path in paths {
                if should_stop(&control, &mut processed_since_check, &mut last_check) {
                    break;
                }
                let kind = if path.ends_with('/') {
                    MatchKind::Directory
                } else {
                    MatchKind::File
                };
                let candidate = Candidate { path, kind };
                injector.push(candidate, |candidate, columns| {
                    columns[0] = Utf32String::from(candidate.path.as_str());
                });
                control.scanned_paths.fetch_add(1, Ordering::Relaxed);
                control.wake();
            }
            control.walk_complete.store(true, Ordering::Release);
            control.wake();
        })
        .expect("synthetic file discovery thread should start")
}

fn should_stop(
    control: &DiscoveryControl,
    processed_since_check: &mut usize,
    last_check: &mut Instant,
) -> bool {
    *processed_since_check += 1;
    if *processed_since_check < 256 && last_check.elapsed() < Duration::from_millis(10) {
        return false;
    }
    *processed_since_check = 0;
    *last_check = Instant::now();
    control.shutdown.load(Ordering::Acquire)
}

pub(crate) fn not_vcs_metadata(entry: &DirEntry) -> bool {
    !is_vcs_metadata_name(entry.file_name())
}
