use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use orca_core::cancel::CancelToken;
use orca_core::config::{AdditionalWorkingDirectory, PermissionProfileNetworkAccess};

use crate::lifecycle::ThreadSteerHandle;
use crate::server_runtime::{ServerThread, ServerThreadRuntime};
use crate::thread_store::ThreadMetadataPatch;

#[derive(Debug)]
pub(super) enum ActiveTurnCommand {
    Resume,
}

#[derive(Clone)]
pub(super) struct ActiveTurnControl {
    pub(super) thread_id: String,
    pub(super) steer_handle: ThreadSteerHandle,
    generation: Arc<Mutex<ActiveTurnGeneration>>,
    command_tx: mpsc::SyncSender<ActiveTurnCommand>,
    session_permission_directories: Vec<AdditionalWorkingDirectory>,
    session_network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
}

struct ActiveTurnGeneration {
    id: u64,
    cancel: CancelToken,
    accepts_commands: bool,
}

impl ActiveTurnControl {
    pub(super) fn new(
        thread_id: String,
        cancel: CancelToken,
        steer_handle: ThreadSteerHandle,
        command_tx: mpsc::SyncSender<ActiveTurnCommand>,
    ) -> Self {
        Self {
            thread_id,
            steer_handle,
            generation: Arc::new(Mutex::new(ActiveTurnGeneration {
                id: 0,
                cancel,
                accepts_commands: true,
            })),
            command_tx,
            session_permission_directories: Vec::new(),
            session_network_domain_permissions: HashMap::new(),
        }
    }

    fn generation(&self) -> u64 {
        self.generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .id
    }

    pub(super) fn cancel_token(&self) -> CancelToken {
        self.generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cancel
            .clone()
    }

    pub(super) fn cancel_current(&self) {
        self.generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .cancel
            .cancel();
    }

    pub(super) fn start_generation(&self) -> CancelToken {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        generation.id = generation.id.saturating_add(1);
        generation.cancel = CancelToken::new();
        generation.accepts_commands = true;
        generation.cancel.clone()
    }

    pub(super) fn request_resume(&self) -> bool {
        let generation = self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !generation.accepts_commands || !generation.cancel.is_cancelled() {
            return false;
        }
        matches!(
            self.command_tx.try_send(ActiveTurnCommand::Resume),
            Ok(()) | Err(mpsc::TrySendError::Full(_))
        )
    }

    pub(super) fn close_generation_and_take_resume(
        &self,
        command_rx: &mpsc::Receiver<ActiveTurnCommand>,
    ) -> bool {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        generation.accepts_commands = false;
        command_rx
            .try_iter()
            .any(|command| matches!(command, ActiveTurnCommand::Resume))
    }

    pub(super) fn steer(&self, input: String) -> bool {
        let generation = self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !generation.accepts_commands || generation.cancel.is_cancelled() {
            return false;
        }
        self.steer_handle.push(input);
        true
    }

    #[cfg(test)]
    fn for_test(thread_id: String) -> (Self, mpsc::Receiver<ActiveTurnCommand>) {
        let (command_tx, command_rx) = mpsc::sync_channel(1);
        (
            Self::new(
                thread_id,
                CancelToken::new(),
                ThreadSteerHandle::default(),
                command_tx,
            ),
            command_rx,
        )
    }
}

struct ActiveTurnEntry {
    control: ActiveTurnControl,
    worker: thread::JoinHandle<ServerThread>,
}

#[must_use = "active turn cleanup must be joined before server exit"]
pub(super) struct ActiveTurnReaper {
    turns: HashMap<String, ActiveTurnEntry>,
}

impl ActiveTurnReaper {
    pub(super) fn join(mut self) {
        self.join_all();
    }

    fn join_all(&mut self) {
        for (_turn_id, active) in self.turns.drain() {
            let _ = active.worker.join();
        }
    }
}

impl Drop for ActiveTurnReaper {
    fn drop(&mut self) {
        self.join_all();
    }
}

#[derive(Default)]
pub(super) struct ActiveTurnManager {
    turns: HashMap<String, ActiveTurnEntry>,
}

impl ActiveTurnManager {
    pub(super) fn insert_running(
        &mut self,
        turn_id: String,
        control: ActiveTurnControl,
        worker: thread::JoinHandle<ServerThread>,
    ) {
        self.turns
            .insert(turn_id, ActiveTurnEntry { control, worker });
    }

    pub(super) fn get_mut(&mut self, turn_id: &str) -> Option<&mut ActiveTurnControl> {
        self.turns.get_mut(turn_id).map(|turn| &mut turn.control)
    }

    pub(super) fn has_thread(&self, thread_id: &str) -> bool {
        self.turns
            .values()
            .any(|turn| turn.control.thread_id == thread_id)
    }

    pub(super) fn accepts_generation(
        &self,
        turn_id: &str,
        thread_id: &str,
        generation: u64,
    ) -> bool {
        self.turns.get(turn_id).is_some_and(|turn| {
            let control = &turn.control;
            control.thread_id == thread_id
                && control.generation() == generation
                && !control.cancel_token().is_cancelled()
        })
    }

    pub(super) fn apply_session_permission_grant(
        &mut self,
        thread_id: &str,
        additional_working_directories: Vec<AdditionalWorkingDirectory>,
        network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
    ) {
        for turn in self.turns.values_mut() {
            let control = &mut turn.control;
            if control.thread_id == thread_id {
                control.session_permission_directories = additional_working_directories.clone();
                control.session_network_domain_permissions = network_domain_permissions.clone();
            }
        }
    }

    #[cfg(test)]
    pub(super) fn join_all(&mut self, threads: &mut ServerThreadRuntime) {
        for (_turn_id, active) in self.turns.drain() {
            if let Ok(thread) = active.worker.join() {
                let thread = merge_completed_turn_metadata(thread, active.control);
                threads.put_thread(thread);
            }
        }
    }

    pub(super) fn cancel_all(&self) {
        for turn in self.turns.values() {
            turn.control.cancel_current();
        }
    }

    pub(super) fn wait_all_bounded(
        &mut self,
        threads: &mut ServerThreadRuntime,
        timeout: Duration,
    ) -> bool {
        const POLL: Duration = Duration::from_millis(10);
        let deadline = Instant::now()
            .checked_add(timeout)
            .unwrap_or_else(Instant::now);
        loop {
            self.reclaim_finished(threads);
            if self.turns.is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(POLL);
        }
    }

    pub(super) fn handoff_remaining_to_reaper(&mut self) -> Option<ActiveTurnReaper> {
        let turns = std::mem::take(&mut self.turns);
        if turns.is_empty() {
            return None;
        }
        Some(ActiveTurnReaper { turns })
    }

    pub(super) fn reclaim_finished(&mut self, threads: &mut ServerThreadRuntime) {
        let finished = self
            .turns
            .iter()
            .filter_map(|(turn_id, turn)| turn.worker.is_finished().then_some(turn_id.clone()))
            .collect::<Vec<_>>();
        for turn_id in finished {
            if let Some(active) = self.turns.remove(&turn_id)
                && let Ok(thread) = active.worker.join()
            {
                let thread = merge_completed_turn_metadata(thread, active.control);
                threads.put_thread(thread);
            }
        }
    }

    pub(super) fn reclaim_finished_thread(
        &mut self,
        threads: &mut ServerThreadRuntime,
        thread_id: &str,
    ) {
        const MAX_WAIT: Duration = Duration::from_millis(100);
        const POLL: Duration = Duration::from_millis(5);
        let deadline = Instant::now() + MAX_WAIT;
        loop {
            self.reclaim_finished(threads);
            if threads.has_thread(thread_id)
                || !self.has_thread(thread_id)
                || Instant::now() >= deadline
            {
                break;
            }
            thread::sleep(POLL);
        }
        self.reclaim_finished(threads);
    }
}

fn merge_completed_turn_metadata(
    mut thread: ServerThread,
    control: ActiveTurnControl,
) -> ServerThread {
    let additional_working_directories = (!control.session_permission_directories.is_empty())
        .then_some(control.session_permission_directories);
    let network_domain_permissions = (!control.session_network_domain_permissions.is_empty())
        .then_some(control.session_network_domain_permissions);
    if additional_working_directories.is_some() || network_domain_permissions.is_some() {
        thread.update_metadata(ThreadMetadataPatch {
            title: None,
            active_permission_profile: None,
            approval_mode: None,
            runtime_workspace_roots: None,
            permission_rules: None,
            additional_working_directories,
            network_domain_permissions,
        });
    }
    thread
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn resumed_generation_uses_a_fresh_permanent_cancellation_scope() {
        let (control, command_rx) = ActiveTurnControl::for_test("thread-1".to_string());
        let first_generation = control.generation();
        let first_cancel = control.cancel_token();

        assert!(!control.request_resume());
        control.cancel_current();
        assert!(control.request_resume());
        assert!(control.close_generation_and_take_resume(&command_rx));
        assert!(!control.request_resume());
        let second_cancel = control.start_generation();

        assert_eq!(first_generation, 0);
        assert_eq!(control.generation(), 1);
        assert!(first_cancel.is_cancelled());
        assert!(!second_cancel.is_cancelled());
    }

    #[test]
    fn duplicate_resume_commands_coalesce_to_one_generation_restart() {
        let (control, command_rx) = ActiveTurnControl::for_test("thread-1".to_string());

        control.cancel_current();
        assert!(control.request_resume());
        assert!(control.request_resume());

        assert!(control.close_generation_and_take_resume(&command_rx));
        assert!(!control.close_generation_and_take_resume(&command_rx));
    }

    #[test]
    fn handed_off_turn_reaper_remains_joinable_until_cleanup_finishes() {
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let handle = thread::spawn(move || -> ServerThread {
            release_rx.recv().expect("release turn");
            finished_tx.send(()).expect("report completion");
            panic!("test turn exits without a ServerThread");
        });
        let (control, _command_rx) = ActiveTurnControl::for_test("thread-1".to_string());
        let mut manager = ActiveTurnManager::default();
        manager.insert_running("turn-1".to_string(), control, handle);
        let reaper = manager
            .handoff_remaining_to_reaper()
            .expect("active turn reaper");

        assert!(finished_rx.try_recv().is_err());
        release_tx.send(()).expect("release turn");
        reaper.join();
        assert_eq!(finished_rx.try_recv(), Ok(()));
    }

    #[test]
    fn empty_turn_manager_does_not_spawn_a_reaper() {
        let mut manager = ActiveTurnManager::default();
        assert!(manager.handoff_remaining_to_reaper().is_none());
    }
}
