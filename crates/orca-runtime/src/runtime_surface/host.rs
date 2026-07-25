use std::fmt;
use std::path::{Path, PathBuf};

use crate::goal_actor::GoalRuntimeHandle;
use crate::goal_store::CreateGoalInput;
use crate::runtime_host::{
    HostedWorkflowRequest, RuntimeHost, RuntimeHostError, RuntimeHostHandle, RuntimeThreadHandle,
    RuntimeThreadStartRequest,
};
use orca_core::config::RunConfig;
use orca_core::goal_runtime::{GoalNextAction, GoalPauseReason, GoalRecord, GoalTurnOrigin};
use orca_core::goal_types::ThreadGoal;
use orca_core::task_types::{BackgroundTaskSummary, TaskStatus};

use super::{RuntimeSurfaceHandle, RuntimeSurfaceHostHandle};

/// A thread-scoped typed surface entry point.
#[derive(Clone)]
pub struct RuntimeSurfaceThreadHandle {
    runtime: RuntimeThreadHandle,
}

impl fmt::Debug for RuntimeSurfaceThreadHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeSurfaceThreadHandle")
            .field("thread_id", &self.thread_id())
            .finish_non_exhaustive()
    }
}

/// A thread-scoped Goal control facade. The actor handle remains private to
/// `orca-runtime`; clients receive domain values and runtime-owned errors only.
#[derive(Clone)]
pub struct RuntimeSurfaceGoalHandle {
    runtime: GoalRuntimeHandle,
}

impl RuntimeSurfaceGoalHandle {
    fn map_error(error: crate::goal_actor::GoalActorError) -> RuntimeHostError {
        RuntimeHostError::GoalControlFailed {
            message: error.to_string(),
        }
    }

    pub fn read(&self, session_id: &str) -> Result<Option<GoalRecord>, RuntimeHostError> {
        self.runtime.read(session_id).map_err(Self::map_error)
    }

    pub fn project_thread_goal(
        &self,
        session_id: &str,
    ) -> Result<Option<ThreadGoal>, RuntimeHostError> {
        self.runtime
            .project_thread_goal(session_id)
            .map_err(Self::map_error)
    }

    pub fn create(&self, input: CreateGoalInput) -> Result<GoalRecord, RuntimeHostError> {
        self.runtime.create(input).map_err(Self::map_error)
    }

    pub fn edit(
        &self,
        session_id: &str,
        objective: impl Into<String>,
        token_budget: Option<i64>,
        at: i64,
    ) -> Result<Option<GoalRecord>, RuntimeHostError> {
        self.runtime
            .edit(session_id, objective, token_budget, at)
            .map_err(Self::map_error)
    }

    pub fn clear(&self, session_id: &str) -> Result<(), RuntimeHostError> {
        self.runtime.clear(session_id).map_err(Self::map_error)
    }

    pub fn latest_active(&self) -> Result<Option<ThreadGoal>, RuntimeHostError> {
        self.runtime.latest_active().map_err(Self::map_error)
    }

    pub fn pause(
        &self,
        session_id: &str,
        reason: GoalPauseReason,
        message: impl Into<String>,
        at: i64,
    ) -> Result<GoalNextAction, RuntimeHostError> {
        self.runtime
            .pause(session_id, reason, message, at)
            .map_err(Self::map_error)
    }

    pub fn resume(
        &self,
        session_id: &str,
        origin: GoalTurnOrigin,
        at: i64,
    ) -> Result<GoalNextAction, RuntimeHostError> {
        self.runtime
            .resume(session_id, origin, at)
            .map_err(Self::map_error)
    }

    pub fn resume_into(
        &self,
        source_session_id: &str,
        resumed_session_id: &str,
        at: i64,
    ) -> Result<Option<GoalRecord>, RuntimeHostError> {
        self.runtime
            .resume_into(source_session_id, resumed_session_id, at)
            .map_err(Self::map_error)
    }
}

impl RuntimeSurfaceHostHandle {
    /// Read saved thread projections without acquiring an owner lease. Opening
    /// a selected session still happens inside `RuntimeHost::start_thread`.
    pub fn list_saved_sessions(
        limit: usize,
    ) -> std::io::Result<Vec<crate::history::SessionSummary>> {
        crate::history::list_sessions(limit)
    }

    pub fn load_saved_session(
        selector: &str,
    ) -> std::io::Result<crate::history::SessionTranscript> {
        crate::history::load_session(selector)
    }

    pub fn folder_is_trusted(path: &Path) -> bool {
        orca_core::config::folder_trust::is_trusted(path)
    }

    pub fn set_folder_trust(path: &Path, trusted: bool) -> Result<(), String> {
        let level = if trusted {
            orca_core::config::folder_trust::TrustLevel::Trusted
        } else {
            orca_core::config::folder_trust::TrustLevel::Untrusted
        };
        orca_core::config::folder_trust::set_trust(path, level)
    }

    pub fn save_api_key(api_key: &str) -> Result<PathBuf, String> {
        orca_core::config::file::save_api_key_checked(api_key).map_err(|error| error.to_string())
    }

    pub fn project_saved_goal(session_id: &str) -> Result<Option<ThreadGoal>, RuntimeHostError> {
        with_saved_goal_runtime(|runtime| runtime.project_thread_goal(session_id))
    }

    pub fn latest_active_saved_goal() -> Result<Option<ThreadGoal>, RuntimeHostError> {
        with_saved_goal_runtime(GoalRuntimeHandle::latest_active)
    }

    pub fn pause_saved_goal(session_id: &str, at: i64) -> Result<GoalNextAction, RuntimeHostError> {
        with_saved_goal_runtime(|runtime| {
            runtime.pause(session_id, GoalPauseReason::User, "paused by user", at)
        })
    }

    pub fn resume_saved_goal(
        session_id: &str,
        at: i64,
    ) -> Result<GoalNextAction, RuntimeHostError> {
        with_saved_goal_runtime(|runtime| runtime.resume(session_id, GoalTurnOrigin::Resume, at))
    }

    pub fn start_thread(
        &self,
        config: RunConfig,
        title: impl Into<String>,
    ) -> Result<RuntimeSurfaceThreadHandle, RuntimeHostError> {
        self.start_thread_with_request(RuntimeThreadStartRequest::new(config, title))
    }

    pub fn start_thread_with_request(
        &self,
        request: RuntimeThreadStartRequest,
    ) -> Result<RuntimeSurfaceThreadHandle, RuntimeHostError> {
        self.runtime
            .as_ref()
            .ok_or(RuntimeHostError::HostUnavailable)?
            .start_thread_with_request(request)
            .map(RuntimeSurfaceThreadHandle::from_runtime)
    }
}

fn with_saved_goal_runtime<T>(
    run: impl FnOnce(&GoalRuntimeHandle) -> Result<T, crate::goal_actor::GoalActorError>,
) -> Result<T, RuntimeHostError> {
    let (runtime, join) =
        GoalRuntimeHandle::open_default().map_err(RuntimeSurfaceGoalHandle::map_error)?;
    let result = run(&runtime).map_err(RuntimeSurfaceGoalHandle::map_error);
    drop(runtime);
    if join.join().is_err() {
        return Err(RuntimeHostError::GoalControlFailed {
            message: "saved Goal actor panicked during shutdown".to_string(),
        });
    }
    result
}

impl RuntimeSurfaceThreadHandle {
    fn from_runtime(runtime: RuntimeThreadHandle) -> Self {
        Self { runtime }
    }

    pub fn thread_id(&self) -> &str {
        self.runtime.thread_id()
    }

    pub fn surface(&self) -> RuntimeSurfaceHandle {
        self.runtime.surface()
    }

    pub fn acp_surface(&self) -> Option<RuntimeSurfaceHandle> {
        self.runtime.acp_surface()
    }

    pub fn read_history(&self) -> Result<Vec<super::SurfaceHistoryMessage>, RuntimeHostError> {
        self.runtime.read_surface_history()
    }

    /// Expand an input's immutable mention bindings using the registry owned by
    /// the runtime thread. TUI and other clients do not receive the registry.
    pub fn expand_mentions(
        &self,
        input: &str,
        bindings: &crate::mentions::MentionBindings,
        cwd: &Path,
        workspace_roots: &[PathBuf],
    ) -> Result<String, String> {
        crate::mentions::expand_mentions(
            input,
            bindings,
            cwd,
            workspace_roots,
            &self.runtime.mcp_registry(),
        )
    }

    /// Discover immutable mention candidates with the runtime-owned MCP
    /// registry. Surface clients receive the result, never the registry.
    pub fn discover_mention_catalog(&self, roots: &[PathBuf]) -> crate::mentions::MentionCatalog {
        crate::mentions::MentionCatalog::discover(roots, &self.runtime.mcp_registry())
    }

    pub fn backtrack_last_user(&self) -> Result<Option<String>, RuntimeHostError> {
        self.runtime.backtrack_last_user()
    }

    pub fn goal(&self) -> Result<RuntimeSurfaceGoalHandle, RuntimeHostError> {
        self.runtime
            .goal_runtime()
            .map(|runtime| RuntimeSurfaceGoalHandle { runtime })
    }

    pub fn task_summaries(&self) -> Vec<BackgroundTaskSummary> {
        self.runtime.task_registry().list()
    }

    pub fn stop_task(&self, task_id: &str) -> Result<Vec<BackgroundTaskSummary>, String> {
        let registry = self.runtime.task_registry();
        let task = registry
            .get(task_id)
            .ok_or_else(|| format!("task '{task_id}' not found"))?;
        if matches!(
            task.status,
            TaskStatus::Completed
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Stopped
        ) {
            return Err(format!(
                "task '{task_id}' is already {}",
                task_status_label(task.status)
            ));
        }
        if task.status == TaskStatus::ApprovalRequired {
            registry.stop(task_id, "Task stopped".to_string())?;
        } else {
            registry.request_stop(task_id)?;
        }
        Ok(registry.list())
    }

    pub fn foreground_task(&self, task_id: &str) -> Result<Vec<BackgroundTaskSummary>, String> {
        let registry = self.runtime.task_registry();
        registry.mark_foregrounded(task_id)?;
        Ok(registry.list())
    }

    pub fn resolve_background_approval(
        &self,
        approval_id: &str,
        approved: bool,
    ) -> Result<(String, Vec<BackgroundTaskSummary>), String> {
        let registry = self.runtime.task_registry();
        let task_id =
            registry.submit_pending_tool_approval_response_by_request_id(approval_id, approved)?;
        if !approved {
            registry.finish_denied_pending_tool_approval(&task_id)?;
        }
        Ok((task_id, registry.list()))
    }

    pub fn launch_workflow(&self, request: HostedWorkflowRequest) -> Result<(), RuntimeHostError> {
        self.runtime.launch_workflow(request).map(|_| ())
    }

    pub fn remember_user(&self, note: &str) -> Result<PathBuf, String> {
        crate::memory::remember_user(note)
    }

    pub fn remember_project(&self, root: &Path, note: &str) -> Result<PathBuf, String> {
        crate::memory::remember_project(root, note)
    }

    pub(crate) fn legacy(&self) -> RuntimeThreadHandle {
        self.runtime.clone()
    }
}

impl RuntimeThreadHandle {
    pub fn typed_surface(&self) -> RuntimeSurfaceThreadHandle {
        RuntimeSurfaceThreadHandle::from_runtime(self.clone())
    }
}

impl RuntimeHost {
    pub fn surface_handle(&self) -> RuntimeSurfaceHostHandle {
        RuntimeSurfaceHostHandle::from_runtime(self.handle())
    }
}

impl RuntimeHostHandle {
    pub fn surface_handle(&self) -> RuntimeSurfaceHostHandle {
        RuntimeSurfaceHostHandle::from_runtime(self.clone())
    }
}

fn task_status_label(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "running",
        TaskStatus::ApprovalRequired => "approval_required",
        TaskStatus::Paused => "paused",
        TaskStatus::Stopping => "stopping",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
        TaskStatus::Stopped => "stopped",
    }
}
