use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel as mpsc;

use crate::diagnostics::KeybindingsLocation;

pub(crate) const MAX_KEYBINDINGS_BYTES: usize = 64 * 1024;
const RELOAD_INTERVAL: Duration = Duration::from_millis(500);
const ORCA_HOME_ENV: &str = "ORCA_HOME";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileObservation {
    Missing,
    Bytes(Vec<u8>),
    Rejected(String),
}

type LoaderFn = Arc<dyn Fn(&Path) -> FileObservation + Send + Sync + 'static>;

pub(crate) fn keybindings_path() -> Option<PathBuf> {
    keybindings_directory_from_sources(std::env::var_os(ORCA_HOME_ENV), dirs::home_dir())
        .map(|(directory, _)| directory)
        .map(|directory| directory.join("keybindings.json"))
}

pub(crate) fn keybindings_location() -> KeybindingsLocation {
    keybindings_location_from_sources(std::env::var_os(ORCA_HOME_ENV), dirs::home_dir())
}

fn keybindings_location_from_sources(
    orca_home: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> KeybindingsLocation {
    keybindings_directory_from_sources(orca_home, home)
        .map(|(_, location)| location)
        .unwrap_or(KeybindingsLocation::Unavailable)
}

fn keybindings_directory_from_sources(
    orca_home: Option<std::ffi::OsString>,
    home: Option<PathBuf>,
) -> Option<(PathBuf, KeybindingsLocation)> {
    orca_home
        .map(|directory| (PathBuf::from(directory), KeybindingsLocation::OrcaHome))
        .or_else(|| {
            home.map(|directory| (directory.join(".orca"), KeybindingsLocation::DefaultHome))
        })
}

pub(crate) fn load_observation(path: &Path) -> FileObservation {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return FileObservation::Missing,
        Err(error) => {
            return FileObservation::Rejected(format!(
                "cannot inspect {}: {error}",
                path.display()
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return FileObservation::Rejected(format!("{} is a symbolic link", path.display()));
    }
    if !metadata.is_file() {
        return FileObservation::Rejected(format!("{} is not a regular file", path.display()));
    }

    let file = match open_regular_file(path) {
        Ok(file) => file,
        Err(error) => {
            return FileObservation::Rejected(format!("cannot open {}: {error}", path.display()));
        }
    };
    let opened_metadata = match file.metadata() {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => {
            return FileObservation::Rejected(format!(
                "{} did not open as a regular file",
                path.display()
            ));
        }
        Err(error) => {
            return FileObservation::Rejected(format!(
                "cannot inspect opened {}: {error}",
                path.display()
            ));
        }
    };
    let mut bytes =
        Vec::with_capacity(opened_metadata.len().min(MAX_KEYBINDINGS_BYTES as u64) as usize);
    if let Err(error) = file
        .take((MAX_KEYBINDINGS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
    {
        return FileObservation::Rejected(format!("cannot read {}: {error}", path.display()));
    }
    if bytes.len() > MAX_KEYBINDINGS_BYTES {
        return FileObservation::Rejected(format!(
            "{} exceeds the 64 KiB keybindings limit",
            path.display()
        ));
    }
    FileObservation::Bytes(bytes)
}

fn open_regular_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    options.open(path)
}

pub(crate) struct KeymapReloader {
    request_tx: mpsc::Sender<()>,
    result_rx: mpsc::Receiver<FileObservation>,
    join: Option<JoinHandle<()>>,
    next_poll_at: Instant,
}

impl KeymapReloader {
    pub(crate) fn start(path: PathBuf, now: Instant) -> Self {
        Self::start_with_loader(path, now, Arc::new(load_observation))
    }

    fn start_with_loader(path: PathBuf, now: Instant, loader: LoaderFn) -> Self {
        let (request_tx, request_rx) = mpsc::bounded(1);
        let (result_tx, result_rx) = mpsc::bounded(1);
        let worker_result_rx = result_rx.clone();
        let join = thread::Builder::new()
            .name("orca-keybindings-reload".to_string())
            .spawn(move || {
                while request_rx.recv().is_ok() {
                    let observation = loader(&path);
                    match result_tx.try_send(observation) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(observation)) => {
                            let _ = worker_result_rx.try_recv();
                            let _ = result_tx.try_send(observation);
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            })
            .expect("keybindings reload worker must spawn");
        Self {
            request_tx,
            result_rx,
            join: Some(join),
            next_poll_at: now,
        }
    }

    pub(crate) fn request_reload(&mut self, now: Instant) -> bool {
        if now < self.next_poll_at {
            return false;
        }
        self.next_poll_at = now + RELOAD_INTERVAL;
        matches!(self.request_tx.try_send(()), Ok(()))
    }

    pub(crate) fn try_recv(&self) -> Option<FileObservation> {
        self.result_rx.try_recv().ok()
    }
}

impl Drop for KeymapReloader {
    fn drop(&mut self) {
        let (replacement, _receiver) = mpsc::bounded(0);
        let request_tx = std::mem::replace(&mut self.request_tx, replacement);
        drop(request_tx);
        if self.join.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(join) = self.join.take()
        {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use crate::diagnostics::KeybindingsLocation;
    use crate::keybindings::runtime::{
        InputOwnerFingerprint, KeymapRuntime, ModalOwner, ReloadOutcome, ShortcutResolution,
    };
    use crate::shortcuts::{IdleShortcut, ShortcutAction, ShortcutContext};
    use crate::types::PanelMode;

    use super::{
        FileObservation, KeymapReloader, MAX_KEYBINDINGS_BYTES, keybindings_location_from_sources,
        keybindings_path, load_observation,
    };

    fn idle_owner() -> InputOwnerFingerprint {
        InputOwnerFingerprint {
            context: ShortcutContext::Idle,
            modal: ModalOwner::None,
            panel: PanelMode::Conversation,
            vim_mode: None,
        }
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
    }

    #[test]
    fn keybindings_path_uses_orca_home() {
        let _env = crate::test_support::lock_process_env();
        let home = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("ORCA_HOME", home.path()) };
        assert_eq!(
            keybindings_path().unwrap(),
            home.path().join("keybindings.json"),
        );
        unsafe { std::env::remove_var("ORCA_HOME") };
    }

    #[test]
    fn keybindings_location_distinguishes_orca_home_default_home_and_unavailable() {
        assert_eq!(
            keybindings_location_from_sources(Some("custom".into()), Some("home".into())),
            KeybindingsLocation::OrcaHome,
        );
        assert_eq!(
            keybindings_location_from_sources(None, Some("home".into())),
            KeybindingsLocation::DefaultHome,
        );
        assert_eq!(
            keybindings_location_from_sources(None, None),
            KeybindingsLocation::Unavailable,
        );
    }

    #[test]
    fn loader_reads_only_limit_plus_sentinel() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keybindings.json");
        fs::write(&path, vec![b'x'; MAX_KEYBINDINGS_BYTES + 1]).unwrap();

        assert!(matches!(
            load_observation(&path),
            FileObservation::Rejected(ref error) if error.contains("64 KiB"),
        ));
    }

    #[test]
    fn missing_directory_and_symlink_observations_are_bounded() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            load_observation(&directory.path().join("missing")),
            FileObservation::Missing,
        );
        assert!(matches!(
            load_observation(directory.path()),
            FileObservation::Rejected(ref error) if error.contains("regular file"),
        ));

        #[cfg(unix)]
        {
            let target = directory.path().join("target.json");
            let link = directory.path().join("keybindings.json");
            fs::write(&target, br#"{"version":1,"bindings":{}}"#).unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert!(matches!(
                load_observation(&link),
                FileObservation::Rejected(ref error) if error.contains("symbolic link"),
            ));
        }
    }

    #[test]
    fn runtime_applies_valid_rejects_invalid_deduplicates_and_restores_defaults() {
        let mut runtime = KeymapRuntime::new(crate::keybindings::config::Keymap::built_in());
        let valid = FileObservation::Bytes(
            br#"{"version":1,"bindings":{"idle.submit":["ctrl+s"]}}"#.to_vec(),
        );
        assert_eq!(
            runtime.apply_observation(valid.clone()),
            ReloadOutcome::Applied
        );
        assert_eq!(
            runtime.resolve(idle_owner(), ctrl('s'), Instant::now()),
            ShortcutResolution::Action(crate::keybindings::runtime::ShortcutInvocation::key(
                ShortcutAction::Idle(IdleShortcut::Submit),
                ctrl('s'),
            ),),
        );
        assert_eq!(
            runtime.apply_observation(FileObservation::Bytes(b"{".to_vec())),
            ReloadOutcome::Rejected(
                "keybindings reload rejected: EOF while parsing an object".to_string()
            ),
        );
        assert_eq!(
            runtime.apply_observation(FileObservation::Bytes(b"{".to_vec())),
            ReloadOutcome::Unchanged,
        );
        assert_eq!(
            runtime.apply_observation(valid),
            ReloadOutcome::Unchanged,
            "last-known-good map remains active after a rejected observation",
        );
        assert_eq!(
            runtime.apply_observation(FileObservation::Missing),
            ReloadOutcome::RestoredDefaults,
        );
    }

    #[test]
    fn applied_reload_clears_pending_chord() {
        let map = crate::keybindings::config::parse_keymap(
            br#"{"version":1,"bindings":{"idle.submit":["ctrl+x ctrl+s"]}}"#,
        )
        .unwrap();
        let mut runtime = KeymapRuntime::new(map);
        assert_eq!(
            runtime.resolve(idle_owner(), ctrl('x'), Instant::now()),
            ShortcutResolution::Pending,
        );

        assert_eq!(
            runtime.apply_observation(FileObservation::Bytes(
                br#"{"version":1,"bindings":{"idle.submit":["ctrl+s"]}}"#.to_vec()
            )),
            ReloadOutcome::Applied,
        );
        assert!(!runtime.has_pending_chord());
    }

    #[test]
    fn reloader_caps_requests_and_delivers_latest_observation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keybindings.json");
        fs::write(&path, br#"{"version":1,"bindings":{}}"#).unwrap();
        let now = Instant::now();
        let mut reloader = KeymapReloader::start(path, now);

        assert!(reloader.request_reload(now));
        assert!(!reloader.request_reload(now + Duration::from_millis(499)));
        let deadline = Instant::now() + Duration::from_secs(2);
        let observation = loop {
            if let Some(observation) = reloader.try_recv() {
                break observation;
            }
            assert!(Instant::now() < deadline, "reload result timed out");
            std::thread::yield_now();
        };
        assert!(matches!(observation, FileObservation::Bytes(_)));
        assert!(reloader.request_reload(now + Duration::from_millis(500)));
    }

    #[test]
    fn blocked_loader_does_not_block_request_or_drop() {
        let (entered_tx, entered_rx) = crossbeam_channel::bounded(1);
        let (_release_tx, release_rx) = crossbeam_channel::bounded::<()>(0);
        let loader = Arc::new(move |_path: &Path| {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            FileObservation::Missing
        });
        let now = Instant::now();
        let mut reloader = KeymapReloader::start_with_loader("blocked".into(), now, loader);
        assert!(reloader.request_reload(now));
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();

        let started = Instant::now();
        drop(reloader);
        assert!(started.elapsed() < Duration::from_millis(100));
    }
}
