use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use orca_core::cancel::CancelToken;
use orca_core::config::{AdditionalWorkingDirectory, PermissionProfileNetworkAccess};

use crate::lifecycle::ThreadSteerHandle;
use crate::server_runtime::{ServerThread, ServerThreadRuntime};
use crate::thread_store::ThreadMetadataPatch;

#[derive(Clone)]
pub(super) struct ActiveTurnControl {
    pub(super) thread_id: String,
    pub(super) cancel: CancelToken,
    pub(super) steer_handle: ThreadSteerHandle,
    session_permission_directories: Vec<AdditionalWorkingDirectory>,
    session_network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
}

impl ActiveTurnControl {
    pub(super) fn new(
        thread_id: String,
        cancel: CancelToken,
        steer_handle: ThreadSteerHandle,
    ) -> Self {
        Self {
            thread_id,
            cancel,
            steer_handle,
            session_permission_directories: Vec::new(),
            session_network_domain_permissions: HashMap::new(),
        }
    }
}

pub(super) struct ActiveTurnHandle {
    handle: thread::JoinHandle<(String, String, ServerThread)>,
}

impl ActiveTurnHandle {
    pub(super) fn new(handle: thread::JoinHandle<(String, String, ServerThread)>) -> Self {
        Self { handle }
    }
}

#[derive(Default)]
pub(super) struct ActiveTurnManager {
    controls: HashMap<String, ActiveTurnControl>,
    running: Vec<ActiveTurnHandle>,
}

impl ActiveTurnManager {
    pub(super) fn insert_control(&mut self, turn_id: String, control: ActiveTurnControl) {
        self.controls.insert(turn_id, control);
    }

    pub(super) fn push_running(&mut self, handle: ActiveTurnHandle) {
        self.running.push(handle);
    }

    pub(super) fn get_mut(&mut self, turn_id: &str) -> Option<&mut ActiveTurnControl> {
        self.controls.get_mut(turn_id)
    }

    pub(super) fn has_thread(&self, thread_id: &str) -> bool {
        self.controls
            .values()
            .any(|turn| turn.thread_id == thread_id)
    }

    pub(super) fn apply_session_permission_grant(
        &mut self,
        thread_id: &str,
        additional_working_directories: Vec<AdditionalWorkingDirectory>,
        network_domain_permissions: HashMap<String, PermissionProfileNetworkAccess>,
    ) {
        for control in self.controls.values_mut() {
            if control.thread_id == thread_id {
                control.session_permission_directories = additional_working_directories.clone();
                control.session_network_domain_permissions = network_domain_permissions.clone();
            }
        }
    }

    pub(super) fn join_all(&mut self, threads: &mut ServerThreadRuntime) {
        for active in self.running.drain(..) {
            if let Ok((turn_id, _thread_id, thread)) = active.handle.join() {
                let control = self.controls.remove(&turn_id);
                let thread = merge_completed_turn_metadata(thread, control);
                threads.put_thread(thread);
            }
        }
    }

    pub(super) fn reclaim_finished(&mut self, threads: &mut ServerThreadRuntime) {
        let mut pending = Vec::new();
        for active in self.running.drain(..) {
            if active.handle.is_finished() {
                if let Ok((turn_id, _thread_id, thread)) = active.handle.join() {
                    let control = self.controls.remove(&turn_id);
                    let thread = merge_completed_turn_metadata(thread, control);
                    threads.put_thread(thread);
                }
            } else {
                pending.push(active);
            }
        }
        self.running = pending;
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
    control: Option<ActiveTurnControl>,
) -> ServerThread {
    if let Some(control) = control {
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
    }
    thread
}
