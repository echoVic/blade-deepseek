use crossbeam_channel as mpsc;
use std::io;
use std::path::{Path, PathBuf};

use orca_core::config::RunConfig;
use orca_core::goal_runtime::{GoalPauseReason, GoalRecord, GoalTurnOrigin};
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
        self.thread
            .goal()
            .and_then(|goal| goal.project_thread_goal(session_id))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn goal_record(&self, session_id: &str) -> Result<Option<GoalRecord>, String> {
        self.thread
            .goal()
            .and_then(|goal| goal.read(session_id))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn set_goal(
        &self,
        session_id: &str,
        objective: String,
        at: i64,
    ) -> Result<ThreadGoal, String> {
        let goal = self.thread.goal().map_err(|error| error.to_string())?;
        if goal
            .read(session_id)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            goal.edit(session_id, objective, None, at)
                .map_err(|error| error.to_string())?;
        } else {
            goal.create(orca_runtime::goal_store::CreateGoalInput {
                session_id: session_id.to_string(),
                objective,
                token_budget: None,
                now: at,
            })
            .map_err(|error| error.to_string())?;
        }
        goal.project_thread_goal(session_id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "goal disappeared after the committed update".to_string())
    }

    pub(crate) fn edit_goal(
        &self,
        session_id: &str,
        objective: String,
        at: i64,
    ) -> Result<Option<ThreadGoal>, String> {
        let goal = self.thread.goal().map_err(|error| error.to_string())?;
        let updated = goal
            .edit(session_id, objective, None, at)
            .map_err(|error| error.to_string())?;
        if updated.is_none() {
            return Ok(None);
        }
        goal.project_thread_goal(session_id)
            .map_err(|error| error.to_string())
    }

    pub(crate) fn clear_goal(&self, session_id: &str) -> Result<(), String> {
        self.thread
            .goal()
            .and_then(|goal| goal.clear(session_id))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn pause_goal(&self, session_id: &str, at: i64) -> Result<(), String> {
        self.thread
            .goal()
            .and_then(|goal| goal.pause(session_id, GoalPauseReason::User, "paused by user", at))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn resume_goal(&self, session_id: &str, at: i64) -> Result<(), String> {
        self.thread
            .goal()
            .and_then(|goal| goal.resume(session_id, GoalTurnOrigin::Resume, at))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(crate) fn resume_goal_into(
        &self,
        source_session_id: &str,
        resumed_session_id: &str,
        at: i64,
    ) -> Result<Option<GoalRecord>, String> {
        self.thread
            .goal()
            .and_then(|goal| goal.resume_into(source_session_id, resumed_session_id, at))
            .map_err(|error| error.to_string())
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
