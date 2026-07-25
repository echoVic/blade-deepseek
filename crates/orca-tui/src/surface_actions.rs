use crossbeam_channel as mpsc;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::config::RunConfig;
use orca_core::goal_types::ThreadGoal;
use orca_core::task_types::BackgroundTaskSummary;
use orca_runtime::mentions::{MentionBindings, MentionCatalog};
use orca_runtime::runtime_host::{HostedTurnRequest, HostedWorkflowRequest};
use orca_runtime::surface::{
    NonEmptyVec, RuntimeSettingsPatch, RuntimeSurfaceThreadHandle, SurfaceHistoryMessage,
    SurfaceSettingsSnapshot, SurfaceSnapshot,
};

use crate::hosted_runtime::TuiHostedOperationOutcome;
use crate::operation_controller::TuiOperationController;
use crate::types::{TuiEvent, TuiMemoryScope};

/// The only TUI-facing entry point for thread-scoped runtime commands and
/// authoritative reads. Presentation modules receive this facade instead of a
/// runtime thread handle and cannot reach runtime-owned registries or stores.
#[derive(Clone, Debug)]
pub(crate) struct TuiSurfaceActions {
    thread: RuntimeSurfaceThreadHandle,
}

pub(crate) struct TuiHostActions;

impl TuiHostActions {
    pub(crate) fn folder_is_trusted(path: &Path) -> bool {
        orca_runtime::surface::RuntimeSurfaceHostHandle::folder_is_trusted(path)
    }

    pub(crate) fn set_folder_trust(path: &Path, trusted: bool) -> Result<(), String> {
        orca_runtime::surface::RuntimeSurfaceHostHandle::set_folder_trust(path, trusted)
    }

    pub(crate) fn save_api_key(api_key: &str) -> Result<PathBuf, String> {
        orca_runtime::surface::RuntimeSurfaceHostHandle::save_api_key(api_key)
    }
}

impl TuiSurfaceActions {
    pub(crate) fn new(thread: RuntimeSurfaceThreadHandle) -> Self {
        Self { thread }
    }

    pub(crate) fn run_turn(
        &self,
        request: HostedTurnRequest,
        config: RunConfig,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::run(&self.thread, request, config, controller, event_tx)
    }

    pub(crate) fn resume_operation(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::resume_recovered_operation(
            &self.thread,
            operation_id,
            controller,
            event_tx,
        )
    }

    pub(crate) fn cancel_operation(
        &self,
        operation_id: &orca_runtime::surface::SurfaceOperationId,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::cancel_recovered_operation(
            &self.thread,
            operation_id,
            controller,
            event_tx,
        )
    }

    pub(crate) fn manual_compact(
        &self,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::manual_compact(&self.thread, controller, event_tx)
    }

    pub(crate) fn update_settings(
        &self,
        patches: NonEmptyVec<RuntimeSettingsPatch>,
    ) -> io::Result<SurfaceSettingsSnapshot> {
        crate::surface_client::update_settings(&self.thread, patches)
    }

    pub(crate) fn read_snapshot(&self) -> io::Result<SurfaceSnapshot> {
        crate::surface_client::read_snapshot(&self.thread)
    }

    pub(crate) fn read_history(&self) -> io::Result<Vec<SurfaceHistoryMessage>> {
        crate::surface_client::read_history(&self.thread)
    }

    pub(crate) fn add_pinned_context(&self, note: &str) -> io::Result<()> {
        crate::surface_client::add_pinned_context(&self.thread, note)
    }

    pub(crate) fn expand_mentions(
        &self,
        input: &str,
        bindings: &MentionBindings,
        cwd: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<String, String> {
        self.thread
            .expand_mentions(input, bindings, cwd, workspace_roots)
    }

    pub(crate) fn discover_mention_catalog(&self, roots: &[PathBuf]) -> MentionCatalog {
        self.thread.discover_mention_catalog(roots)
    }

    pub(crate) fn backtrack_last_user(&self) -> Result<Option<String>, String> {
        self.thread
            .backtrack_last_user()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn goal(&self, session_id: &str) -> Result<Option<ThreadGoal>, String> {
        let _ = session_id;
        crate::surface_client::read_goal(&self.thread).map_err(|error| error.to_string())
    }

    pub(crate) fn edit_goal(
        &self,
        session_id: &str,
        objective: String,
        at: i64,
    ) -> Result<Option<ThreadGoal>, String> {
        let _ = (session_id, at);
        crate::surface_client::edit_goal(&self.thread, objective).map_err(|error| error.to_string())
    }

    pub(crate) fn clear_goal(&self, session_id: &str) -> Result<(), String> {
        let _ = session_id;
        crate::surface_client::clear_goal(&self.thread).map_err(|error| error.to_string())
    }

    pub(crate) fn pause_goal(&self) -> Result<ThreadGoal, String> {
        crate::surface_client::pause_goal(&self.thread).map_err(|error| error.to_string())
    }

    pub(crate) fn set_goal_and_run(
        &self,
        objective: String,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::set_goal_and_run(&self.thread, objective, controller, event_tx)
    }

    pub(crate) fn resume_goal_and_run(
        &self,
        prompt: String,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::resume_goal_and_run(&self.thread, prompt, controller, event_tx)
    }

    pub(crate) fn task_summaries(&self) -> Vec<BackgroundTaskSummary> {
        self.thread.task_summaries()
    }

    pub(crate) fn stop_task(&self, task_id: &str) -> Result<Vec<BackgroundTaskSummary>, String> {
        self.thread.stop_task(task_id)
    }

    pub(crate) fn foreground_task(
        &self,
        task_id: &str,
    ) -> Result<Vec<BackgroundTaskSummary>, String> {
        self.thread.foreground_task(task_id)
    }

    pub(crate) fn resolve_background_approval(
        &self,
        approval_id: &str,
        approved: bool,
    ) -> Result<(String, Vec<BackgroundTaskSummary>), String> {
        self.thread
            .resolve_background_approval(approval_id, approved)
    }

    pub(crate) fn launch_workflow(&self, request: HostedWorkflowRequest) -> Result<(), String> {
        self.thread
            .launch_workflow(request)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn remember(
        &self,
        scope: TuiMemoryScope,
        cwd: &Path,
        note: &str,
    ) -> Result<PathBuf, String> {
        match scope {
            TuiMemoryScope::User => self.thread.remember_user(note),
            TuiMemoryScope::Project => self.thread.remember_project(cwd, note),
        }
    }
}
