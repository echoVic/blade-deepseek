use crossbeam_channel as mpsc;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::config::RunConfig;
use orca_core::goal_types::ThreadGoal;
use orca_core::task_types::BackgroundTaskSummary;
use orca_runtime::mentions::{MentionBindings, MentionCatalog};
use orca_runtime::runtime_host::HostedTurnRequest;
use orca_runtime::surface::{
    NonEmptyVec, RuntimeSettingsPatch, RuntimeSurfaceThreadHandle, SurfaceSettingsSnapshot,
    SurfaceSnapshot,
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
        crate::surface_client::run(
            &self.thread,
            request,
            config,
            &controller.surface_task_control(),
            event_tx,
        )
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
            &controller.surface_task_control(),
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
            &controller.surface_task_control(),
            event_tx,
        )
    }

    pub(crate) fn manual_compact(
        &self,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::manual_compact(
            &self.thread,
            &controller.surface_task_control(),
            event_tx,
        )
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
        crate::surface_client::set_goal_and_run(
            &self.thread,
            objective,
            &controller.surface_task_control(),
            event_tx,
        )
    }

    pub(crate) fn resume_goal_and_run(
        &self,
        prompt: String,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> io::Result<TuiHostedOperationOutcome> {
        crate::surface_client::resume_goal_and_run(
            &self.thread,
            prompt,
            &controller.surface_task_control(),
            event_tx,
        )
    }

    pub(crate) fn recoverable_background_approval_projection(
        &self,
    ) -> Result<(Vec<BackgroundTaskSummary>, Vec<String>), String> {
        let snapshot = crate::surface_client::read_snapshot(&self.thread)
            .map_err(|error| error.to_string())?;
        let tools = snapshot
            .interactions
            .iter()
            .filter_map(|interaction| {
                let orca_runtime::surface::SurfaceInteractionRequest::BackgroundApproval {
                    tool,
                    ..
                } = &interaction.request
                else {
                    return None;
                };
                matches!(
                    interaction.lifecycle,
                    orca_runtime::surface::SurfaceInteractionLifecycle::Requested
                )
                .then(|| tool.name.as_str().to_string())
            })
            .collect();
        Ok((
            crate::surface_projection::workflow_task_summaries(&snapshot),
            tools,
        ))
    }

    pub(crate) fn stop_task(
        &self,
        task_id: &str,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> Result<Vec<BackgroundTaskSummary>, String> {
        crate::surface_client::stop_task(
            &self.thread,
            task_id,
            &controller.surface_task_control(),
            event_tx,
        )
    }

    pub(crate) fn foreground_task(
        &self,
        task_id: &str,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> Result<Vec<BackgroundTaskSummary>, String> {
        crate::surface_client::foreground_task(
            &self.thread,
            task_id,
            &controller.surface_task_control(),
            event_tx,
        )
    }

    pub(crate) fn resolve_background_approval(
        &self,
        approval_id: &str,
        approved: bool,
        controller: &TuiOperationController,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> Result<(String, Vec<BackgroundTaskSummary>), String> {
        let (task_id, tasks) = crate::surface_client::resolve_background_approval(
            &self.thread,
            approval_id,
            approved,
            &controller.surface_task_control(),
            event_tx,
        )?;
        Ok((task_id, tasks))
    }

    pub(crate) fn launch_workflow(
        &self,
        name: &str,
        args: Option<&str>,
        event_tx: &mpsc::Sender<TuiEvent>,
    ) -> Result<(), String> {
        crate::surface_client::launch_workflow(&self.thread, name, args, event_tx)
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
