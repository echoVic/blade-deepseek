use std::error::Error as _;
use std::io;
use std::panic;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once};
use std::thread;
use std::time::Duration;

use crossbeam_channel as mpsc;
use orca_core::config::ThemeName;
use qwertty::{
    Event as QwerttyEvent, KittyKeyboardFlags, MouseMode, RestoreHandle, Rgb, TokioTerminalSession,
};
use tokio::sync::watch;

use crate::input_adapter::InputAdapter;
use crate::terminal_capabilities::{
    TerminalColorLevel, TerminalProfile, system_color_level, terminal_background_from_rgb,
};

const CAPABILITY_PROBE_TIMEOUT: Duration = Duration::from_millis(250);
const INPUT_CHANNEL_CAPACITY: usize = 256;
const STARTUP_CHANNEL_CAPACITY: usize = 1;

static PANIC_HOOK_INIT: Once = Once::new();
static ACTIVE_RESTORE_HANDLE: Mutex<Option<RestoreHandle>> = Mutex::new(None);
static TERMINAL_OWNER_ACTIVE: AtomicBool = AtomicBool::new(false);

enum StartupMessage {
    Ready(TerminalProfile),
    Failed {
        kind: io::ErrorKind,
        message: String,
    },
}

pub(crate) struct InputRuntime {
    profile: TerminalProfile,
    events: mpsc::Receiver<crossterm::event::Event>,
    stop_tx: Option<watch::Sender<bool>>,
    join: Option<thread::JoinHandle<io::Result<()>>>,
}

impl InputRuntime {
    pub(crate) fn start(requested_theme: ThemeName) -> io::Result<Self> {
        if TERMINAL_OWNER_ACTIVE
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "another TUI already owns the terminal",
            ));
        }

        let color_level = system_color_level();
        let (event_tx, events) = mpsc::bounded(INPUT_CHANNEL_CAPACITY);
        let (startup_tx, startup_rx) = mpsc::bounded(STARTUP_CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = watch::channel(false);
        let join = match thread::Builder::new()
            .name("orca-tui-input".to_string())
            .spawn(move || {
                input_thread(requested_theme, color_level, startup_tx, event_tx, stop_rx)
            }) {
            Ok(join) => join,
            Err(error) => {
                TERMINAL_OWNER_ACTIVE.store(false, Ordering::Release);
                return Err(error);
            }
        };

        match startup_rx.recv() {
            Ok(StartupMessage::Ready(profile)) => Ok(Self {
                profile,
                events,
                stop_tx: Some(stop_tx),
                join: Some(join),
            }),
            Ok(StartupMessage::Failed { kind, message }) => {
                let _ = join.join();
                TERMINAL_OWNER_ACTIVE.store(false, Ordering::Release);
                Err(io::Error::new(kind, message))
            }
            Err(_) => {
                let thread_error = join
                    .join()
                    .map_err(|_| io::Error::other("terminal input thread panicked"))?
                    .err();
                TERMINAL_OWNER_ACTIVE.store(false, Ordering::Release);
                Err(thread_error.unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal input thread exited before startup completed",
                    )
                }))
            }
        }
    }

    pub(crate) const fn profile(&self) -> TerminalProfile {
        self.profile
    }

    pub(crate) fn events(&self) -> &mpsc::Receiver<crossterm::event::Event> {
        &self.events
    }

    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if let Some(stop_tx) = self.stop_tx.take() {
            let _ = stop_tx.send(true);
        }
        let result = if let Some(join) = self.join.take() {
            join.join()
                .map_err(|_| io::Error::other("terminal input thread panicked"))?
        } else {
            Ok(())
        };
        TERMINAL_OWNER_ACTIVE.store(false, Ordering::Release);
        result
    }

    #[cfg(test)]
    fn from_parts_for_test(
        profile: TerminalProfile,
        events: mpsc::Receiver<crossterm::event::Event>,
        stop_tx: watch::Sender<bool>,
        join: thread::JoinHandle<io::Result<()>>,
    ) -> Self {
        Self {
            profile,
            events,
            stop_tx: Some(stop_tx),
            join: Some(join),
        }
    }
}

impl Drop for InputRuntime {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

fn input_thread(
    requested_theme: ThemeName,
    color_level: TerminalColorLevel,
    startup_tx: mpsc::Sender<StartupMessage>,
    event_tx: mpsc::Sender<crossterm::event::Event>,
    stop_rx: watch::Receiver<bool>,
) -> io::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let (driver, restore_handle) = match QwerttyDriver::open() {
            Ok(driver) => driver,
            Err(error) => {
                let _ = startup_tx.send(StartupMessage::Failed {
                    kind: error.kind(),
                    message: error.to_string(),
                });
                return Err(error);
            }
        };
        let _restore_registration = RestoreRegistration::install(restore_handle)?;
        drive_terminal(
            driver,
            requested_theme,
            color_level,
            startup_tx,
            event_tx,
            stop_rx,
        )
        .await
    })
}

struct RestoreRegistration;

impl RestoreRegistration {
    fn install(handle: RestoreHandle) -> io::Result<Self> {
        PANIC_HOOK_INIT.call_once(|| {
            let previous = panic::take_hook();
            panic::set_hook(Box::new(move |info| {
                if let Ok(slot) = ACTIVE_RESTORE_HANDLE.try_lock()
                    && let Some(handle) = slot.as_ref()
                {
                    let _ = handle.restore();
                }
                previous(info);
            }));
        });
        let mut slot = ACTIVE_RESTORE_HANDLE
            .lock()
            .map_err(|_| io::Error::other("terminal restore registry is poisoned"))?;
        if slot.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "terminal restore handle is already registered",
            ));
        }
        *slot = Some(handle);
        Ok(Self)
    }
}

impl Drop for RestoreRegistration {
    fn drop(&mut self) {
        if let Ok(mut slot) = ACTIVE_RESTORE_HANDLE.lock() {
            *slot = None;
        }
    }
}

struct QwerttyDriver {
    session: TokioTerminalSession,
    #[cfg(unix)]
    resizes: qwertty::ResizeStream,
}

impl QwerttyDriver {
    fn open() -> io::Result<(Self, RestoreHandle)> {
        let session = TokioTerminalSession::open().map_err(qwertty_error)?;
        let restore_handle = session.restore_handle();
        #[cfg(unix)]
        let resizes = session.resize_stream().map_err(qwertty_error)?;
        Ok((
            Self {
                session,
                #[cfg(unix)]
                resizes,
            },
            restore_handle,
        ))
    }
}

trait TerminalDriver: Sized {
    async fn probe_background(&mut self, timeout: Duration) -> io::Result<Option<Rgb>>;
    async fn enter_alternate_screen(&mut self) -> io::Result<()>;
    async fn enable_mouse(&mut self) -> io::Result<()>;
    async fn enable_bracketed_paste(&mut self) -> io::Result<()>;
    async fn push_keyboard_flags(&mut self) -> io::Result<()>;
    async fn next_event(&mut self) -> io::Result<QwerttyEvent>;
    async fn leave(self) -> io::Result<()>;
}

impl TerminalDriver for QwerttyDriver {
    async fn probe_background(&mut self, timeout: Duration) -> io::Result<Option<Rgb>> {
        self.session
            .probe_capabilities(timeout)
            .await
            .map(|capabilities| capabilities.background_color.value_copied())
            .map_err(qwertty_error)
    }

    async fn enter_alternate_screen(&mut self) -> io::Result<()> {
        self.session
            .enter_alternate_screen()
            .await
            .map_err(qwertty_error)
    }

    async fn enable_mouse(&mut self) -> io::Result<()> {
        self.session
            .enable_mouse(MouseMode::ButtonEvent)
            .await
            .map_err(qwertty_error)
    }

    async fn enable_bracketed_paste(&mut self) -> io::Result<()> {
        self.session
            .enable_bracketed_paste()
            .await
            .map_err(qwertty_error)
    }

    async fn push_keyboard_flags(&mut self) -> io::Result<()> {
        let flags = KittyKeyboardFlags::DISAMBIGUATE_ESCAPE_CODES
            .union(KittyKeyboardFlags::REPORT_EVENT_TYPES)
            .union(KittyKeyboardFlags::REPORT_ALTERNATE_KEYS);
        self.session
            .push_kitty_keyboard(flags)
            .await
            .map_err(qwertty_error)
    }

    async fn next_event(&mut self) -> io::Result<QwerttyEvent> {
        #[cfg(unix)]
        {
            tokio::select! {
                event = self.session.next_event() => event.map_err(qwertty_error),
                resize = self.resizes.next_resize() => {
                    resize.map(QwerttyEvent::Resize).map_err(qwertty_error)
                }
            }
        }
        #[cfg(windows)]
        {
            self.session.next_event().await.map_err(qwertty_error)
        }
    }

    async fn leave(self) -> io::Result<()> {
        self.session.leave().await.map_err(qwertty_error)
    }
}

fn qwertty_error(error: qwertty::Error) -> io::Error {
    let kind = error
        .source()
        .and_then(|source| source.downcast_ref::<io::Error>())
        .map_or(io::ErrorKind::Other, io::Error::kind);
    io::Error::new(kind, error)
}

async fn drive_terminal<D: TerminalDriver>(
    mut driver: D,
    requested_theme: ThemeName,
    color_level: TerminalColorLevel,
    startup_tx: mpsc::Sender<StartupMessage>,
    event_tx: mpsc::Sender<crossterm::event::Event>,
    mut stop_rx: watch::Receiver<bool>,
) -> io::Result<()> {
    let background = if requested_theme == ThemeName::Auto {
        match driver.probe_background(CAPABILITY_PROBE_TIMEOUT).await {
            Ok(background) => background,
            Err(error) => {
                return fail_startup(driver, startup_tx, error).await;
            }
        }
    } else {
        None
    };

    if let Err(error) = driver.enter_alternate_screen().await {
        return fail_startup(driver, startup_tx, error).await;
    }
    if let Err(error) = driver.enable_mouse().await {
        return fail_startup(driver, startup_tx, error).await;
    }
    if let Err(error) = driver.enable_bracketed_paste().await {
        return fail_startup(driver, startup_tx, error).await;
    }
    if let Err(error) = driver.push_keyboard_flags().await {
        return fail_startup(driver, startup_tx, error).await;
    }

    let profile = TerminalProfile {
        background: terminal_background_from_rgb(requested_theme, background),
        color_level,
    };
    startup_tx
        .send(StartupMessage::Ready(profile))
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "TUI startup receiver closed"))?;

    let mut adapter = InputAdapter::default();
    let operation = loop {
        tokio::select! {
            biased;
            changed = stop_rx.changed() => {
                match changed {
                    Ok(()) if *stop_rx.borrow() => break Ok(()),
                    Ok(()) => {}
                    Err(_) => break Ok(()),
                }
            }
            event = driver.next_event() => {
                let event = match event {
                    Ok(event) => event,
                    Err(error) => break Err(error),
                };
                if let Some(event) = adapter.adapt(event)
                    && !send_event_until_stopped(&event_tx, event, &mut stop_rx).await
                {
                    break Ok(());
                }
            }
        }
    };

    finish_driver(driver, operation).await
}

async fn fail_startup<D: TerminalDriver>(
    driver: D,
    startup_tx: mpsc::Sender<StartupMessage>,
    error: io::Error,
) -> io::Result<()> {
    let kind = error.kind();
    let message = error.to_string();
    let result = finish_driver(driver, Err(error)).await;
    let _ = startup_tx.send(StartupMessage::Failed { kind, message });
    result
}

async fn finish_driver<D: TerminalDriver>(driver: D, operation: io::Result<()>) -> io::Result<()> {
    let leave = driver.leave().await;
    operation.and(leave)
}

async fn send_event_until_stopped(
    sender: &mpsc::Sender<crossterm::event::Event>,
    mut event: crossterm::event::Event,
    stop_rx: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        match sender.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(returned)) => event = returned,
        }

        tokio::select! {
            biased;
            changed = stop_rx.changed() => {
                match changed {
                    Ok(()) if *stop_rx.borrow() => return false,
                    Ok(()) => {}
                    Err(_) => return false,
                }
            }
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crossbeam_channel as mpsc;
    use orca_core::config::ThemeName;
    use qwertty::{Event, Key, KeyEvent, Rgb};
    use tokio::sync::watch;

    use super::{StartupMessage, TerminalDriver, drive_terminal};
    use crate::terminal_capabilities::{TerminalBackground, TerminalColorLevel};

    struct FakeDriver {
        calls: Arc<Mutex<Vec<&'static str>>>,
        background: Option<Rgb>,
        events: VecDeque<Event>,
        fail_at: Option<&'static str>,
    }

    impl FakeDriver {
        fn new(
            calls: Arc<Mutex<Vec<&'static str>>>,
            background: Option<Rgb>,
            events: impl IntoIterator<Item = Event>,
        ) -> Self {
            Self {
                calls,
                background,
                events: events.into_iter().collect(),
                fail_at: None,
            }
        }

        fn failing(mut self, operation: &'static str) -> Self {
            self.fail_at = Some(operation);
            self
        }

        fn record(&self, operation: &'static str) -> io::Result<()> {
            self.calls.lock().expect("calls lock").push(operation);
            if self.fail_at == Some(operation) {
                Err(io::Error::other(format!("{operation} failed")))
            } else {
                Ok(())
            }
        }
    }

    impl TerminalDriver for FakeDriver {
        async fn probe_background(&mut self, _timeout: Duration) -> io::Result<Option<Rgb>> {
            self.record("probe")?;
            Ok(self.background)
        }

        async fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.record("alternate")
        }

        async fn enable_mouse(&mut self) -> io::Result<()> {
            self.record("mouse")
        }

        async fn enable_bracketed_paste(&mut self) -> io::Result<()> {
            self.record("paste")
        }

        async fn push_keyboard_flags(&mut self) -> io::Result<()> {
            self.record("keyboard")
        }

        async fn next_event(&mut self) -> io::Result<Event> {
            self.record("read")?;
            if let Some(event) = self.events.pop_front() {
                Ok(event)
            } else {
                std::future::pending().await
            }
        }

        async fn leave(self) -> io::Result<()> {
            self.record("leave")
        }
    }

    fn light_background() -> Option<Rgb> {
        Some(Rgb::new(255, 255, 255))
    }

    async fn wait_for_startup(receiver: &mpsc::Receiver<StartupMessage>) -> StartupMessage {
        for _ in 0..100 {
            if let Ok(message) = receiver.try_recv() {
                return message;
            }
            tokio::task::yield_now().await;
        }
        panic!("startup message was not sent");
    }

    #[test]
    fn auto_probe_precedes_modes_ready_reads_and_leave() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let driver = FakeDriver::new(Arc::clone(&calls), light_background(), []);
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, _event_rx) = mpsc::bounded(4);
                let (stop_tx, stop_rx) = watch::channel(false);

                let task = tokio::spawn(drive_terminal(
                    driver,
                    ThemeName::Auto,
                    TerminalColorLevel::Ansi256,
                    startup_tx,
                    event_tx,
                    stop_rx,
                ));
                let StartupMessage::Ready(profile) = wait_for_startup(&startup_rx).await else {
                    panic!("expected ready startup message");
                };
                assert_eq!(profile.background, TerminalBackground::Light);
                assert_eq!(profile.color_level, TerminalColorLevel::Ansi256);
                stop_tx.send(true).expect("stop receiver alive");
                task.await.expect("driver task").expect("clean stop");

                assert_eq!(
                    *calls.lock().expect("calls lock"),
                    [
                        "probe",
                        "alternate",
                        "mouse",
                        "paste",
                        "keyboard",
                        "read",
                        "leave"
                    ]
                );
            });
    }

    #[test]
    fn explicit_theme_skips_probe_but_keeps_mode_order() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let driver = FakeDriver::new(Arc::clone(&calls), light_background(), []);
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, _event_rx) = mpsc::bounded(4);
                let (stop_tx, stop_rx) = watch::channel(false);

                let task = tokio::spawn(drive_terminal(
                    driver,
                    ThemeName::Light,
                    TerminalColorLevel::TrueColor,
                    startup_tx,
                    event_tx,
                    stop_rx,
                ));
                assert!(matches!(
                    wait_for_startup(&startup_rx).await,
                    StartupMessage::Ready(_)
                ));
                stop_tx.send(true).expect("stop receiver alive");
                task.await.expect("driver task").expect("clean stop");

                assert_eq!(
                    *calls.lock().expect("calls lock"),
                    ["alternate", "mouse", "paste", "keyboard", "read", "leave"]
                );
            });
    }

    #[test]
    fn full_mailbox_never_blocks_stop_or_leave() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let events = [
                    Event::Key(KeyEvent::new(Key::Char('a'))),
                    Event::Key(KeyEvent::new(Key::Char('b'))),
                ];
                let driver = FakeDriver::new(Arc::clone(&calls), None, events);
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, event_rx) = mpsc::bounded(1);
                let (stop_tx, stop_rx) = watch::channel(false);

                let task = tokio::spawn(drive_terminal(
                    driver,
                    ThemeName::Dark,
                    TerminalColorLevel::TrueColor,
                    startup_tx,
                    event_tx,
                    stop_rx,
                ));
                let _ = wait_for_startup(&startup_rx).await;
                for _ in 0..100 {
                    if event_rx.len() == 1
                        && calls
                            .lock()
                            .expect("calls lock")
                            .iter()
                            .filter(|call| **call == "read")
                            .count()
                            >= 2
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
                stop_tx.send(true).expect("stop receiver alive");
                tokio::time::timeout(Duration::from_secs(1), task)
                    .await
                    .expect("stop must not deadlock")
                    .expect("driver task")
                    .expect("clean stop");

                assert_eq!(calls.lock().expect("calls lock").last(), Some(&"leave"));
            });
    }

    #[test]
    fn startup_failure_is_reported_and_still_leaves() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(async {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let driver = FakeDriver::new(Arc::clone(&calls), None, []).failing("mouse");
                let (startup_tx, startup_rx) = mpsc::bounded(1);
                let (event_tx, _event_rx) = mpsc::bounded(1);
                let (_stop_tx, stop_rx) = watch::channel(false);

                let error = drive_terminal(
                    driver,
                    ThemeName::Dark,
                    TerminalColorLevel::TrueColor,
                    startup_tx,
                    event_tx,
                    stop_rx,
                )
                .await
                .expect_err("mode failure should fail startup");
                assert_eq!(error.to_string(), "mouse failed");
                assert!(matches!(
                    startup_rx.recv().expect("startup error"),
                    StartupMessage::Failed { .. }
                ));
                assert_eq!(
                    *calls.lock().expect("calls lock"),
                    ["alternate", "mouse", "leave"]
                );
            });
    }

    #[test]
    fn input_runtime_finish_and_drop_signal_and_join_once() {
        for explicit_finish in [true, false] {
            let (stop_tx, mut stop_rx) = watch::channel(false);
            let joined = Arc::new(Mutex::new(0));
            let thread_joined = Arc::clone(&joined);
            let join = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("test runtime");
                runtime.block_on(async {
                    stop_rx.changed().await.expect("stop sender alive");
                    assert!(*stop_rx.borrow());
                });
                *thread_joined.lock().expect("join count lock") += 1;
                Ok(())
            });
            let (_event_tx, event_rx) = mpsc::bounded(1);
            let mut runtime = super::InputRuntime::from_parts_for_test(
                super::TerminalProfile {
                    background: TerminalBackground::Unknown,
                    color_level: TerminalColorLevel::TrueColor,
                },
                event_rx,
                stop_tx,
                join,
            );

            if explicit_finish {
                runtime.finish().expect("finish should join");
                runtime.finish().expect("second finish is idempotent");
            } else {
                drop(runtime);
            }
            assert_eq!(*joined.lock().expect("join count lock"), 1);
        }
    }
}
