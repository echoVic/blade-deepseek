use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use crate::extension::{
    ExtensionData, ExtensionRegistryBuilder, ToolCallOutcome, ToolFinishInput,
    ToolLifecycleContributor,
};
use chrono::Utc;
use orca_core::goal_types::{
    GoalUpdate, ThreadGoal, ThreadGoalStatus, validate_thread_goal_objective,
};
use orca_core::tool_types::ToolInvocationStarted;
use serde::{Deserialize, Serialize};

const ORCA_HOME_ENV: &str = "ORCA_HOME";
const GOALS_DB_FILENAME: &str = "goals_1.json";
static GOAL_DB_MUTATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone)]
pub struct GoalStore {
    path: PathBuf,
}

#[derive(Debug, Default)]
pub struct GoalToolProgressState {
    inner: Mutex<GoalToolProgressInner>,
}

#[derive(Debug, Default)]
struct GoalToolProgressInner {
    completed_tool_attempts: u64,
    last_turn_id: Option<String>,
    last_call_id: Option<String>,
}

impl GoalToolProgressState {
    pub fn completed_tool_attempts(&self) -> u64 {
        self.inner().completed_tool_attempts
    }

    pub fn last_turn_id(&self) -> Option<String> {
        self.inner().last_turn_id.clone()
    }

    pub fn last_call_id(&self) -> Option<String> {
        self.inner().last_call_id.clone()
    }

    fn record_completed_attempt(&self, turn_id: &str, call_id: &str) {
        let mut inner = self.inner();
        inner.completed_tool_attempts = inner.completed_tool_attempts.saturating_add(1);
        inner.last_turn_id = Some(turn_id.to_string());
        inner.last_call_id = Some(call_id.to_string());
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, GoalToolProgressInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct GoalToolLifecycleContributor;

impl ToolLifecycleContributor for GoalToolLifecycleContributor {
    fn on_tool_finish(&self, input: ToolFinishInput<'_>) {
        record_goal_tool_finish(
            input.thread_store,
            input.turn_store,
            input.tool_name,
            input.call_id,
            input.outcome,
        );
    }
}

pub fn install_goal_tool_lifecycle(builder: &mut ExtensionRegistryBuilder) {
    builder.tool_lifecycle_contributor(Arc::new(GoalToolLifecycleContributor));
}

fn goal_tool_attempt_counts(outcome: ToolCallOutcome) -> bool {
    matches!(
        outcome,
        ToolCallOutcome::Completed
            | ToolCallOutcome::Failed {
                started: ToolInvocationStarted::Yes | ToolInvocationStarted::Unknown
            }
            | ToolCallOutcome::Indeterminate {
                started: ToolInvocationStarted::Yes | ToolInvocationStarted::Unknown
            }
    )
}

pub fn record_goal_tool_finish(
    thread_store: &ExtensionData,
    turn_store: &ExtensionData,
    tool_name: &str,
    call_id: &str,
    outcome: ToolCallOutcome,
) {
    if !goal_tool_attempt_counts(outcome) || tool_name == "update_goal" {
        return;
    }

    thread_store
        .get_or_init(GoalToolProgressState::default)
        .record_completed_attempt(turn_store.level_id(), call_id);
}

pub fn validate_goal_terminal_update_against_extensions(
    update: &GoalUpdate,
    thread_store: &ExtensionData,
) -> Result<(), String> {
    if !matches!(
        update.status,
        Some(ThreadGoalStatus::Complete | ThreadGoalStatus::Blocked)
    ) {
        return Ok(());
    }

    let completed_attempts = thread_store
        .get::<GoalToolProgressState>()
        .map(|progress| progress.completed_tool_attempts())
        .unwrap_or_default();

    if completed_attempts == 0 {
        return Err(
            "terminal update_goal status requires at least one completed non-goal tool attempt in live runtime thread state"
                .to_string(),
        );
    }

    Ok(())
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct GoalDb {
    goals: BTreeMap<String, ThreadGoal>,
}

impl GoalStore {
    pub fn load_default() -> Self {
        Self {
            path: goals_db_path(),
        }
    }

    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn get(&self, session_id: &str) -> io::Result<Option<ThreadGoal>> {
        Ok(self.load()?.goals.get(session_id).cloned())
    }

    pub fn latest_active(&self) -> io::Result<Option<ThreadGoal>> {
        Ok(self
            .load()?
            .goals
            .values()
            .filter(|goal| goal.status == ThreadGoalStatus::Active)
            .max_by(|left, right| {
                left.updated_at
                    .cmp(&right.updated_at)
                    .then_with(|| left.created_at.cmp(&right.created_at))
                    .then_with(|| left.session_id.cmp(&right.session_id))
            })
            .cloned())
    }

    pub fn resume_into(
        &mut self,
        source_session_id: &str,
        resumed_session_id: &str,
    ) -> io::Result<Option<ThreadGoal>> {
        let _guard = goal_db_mutation_lock();
        let mut db = self.load()?;
        let Some(source) = db.goals.get(source_session_id).cloned() else {
            return Ok(None);
        };
        if source_session_id != resumed_session_id && db.goals.contains_key(resumed_session_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("goal already exists for resume target session '{resumed_session_id}'"),
            ));
        }
        let now = now_timestamp();
        let mut resumed = source;
        resumed.session_id = resumed_session_id.to_string();
        resumed.status = ThreadGoalStatus::Active;
        resumed.updated_at = now;

        if source_session_id != resumed_session_id
            && let Some(source) = db.goals.get_mut(source_session_id)
        {
            source.status = ThreadGoalStatus::Paused;
            source.updated_at = now;
        }

        db.goals
            .insert(resumed_session_id.to_string(), resumed.clone());
        self.save(&db)?;
        Ok(Some(resumed))
    }

    pub fn replace(
        &mut self,
        session_id: &str,
        objective: &str,
        status: ThreadGoalStatus,
        token_budget: Option<i64>,
    ) -> io::Result<ThreadGoal> {
        let _guard = goal_db_mutation_lock();
        validate_thread_goal_objective(objective).map_err(invalid_input)?;
        let mut db = self.load()?;
        let now = now_timestamp();
        let goal = ThreadGoal {
            session_id: session_id.to_string(),
            objective: objective.trim().to_string(),
            status,
            token_budget,
            tokens_used: 0,
            time_used_seconds: 0,
            created_at: now,
            updated_at: now,
        };
        db.goals.insert(session_id.to_string(), goal.clone());
        self.save(&db)?;
        Ok(goal)
    }

    pub fn update(
        &mut self,
        session_id: &str,
        update: GoalUpdate,
    ) -> io::Result<Option<ThreadGoal>> {
        let _guard = goal_db_mutation_lock();
        let mut db = self.load()?;
        let Some(goal) = db.goals.get_mut(session_id) else {
            return Ok(None);
        };

        if let Some(objective) = update.objective {
            validate_thread_goal_objective(&objective).map_err(invalid_input)?;
            goal.objective = objective.trim().to_string();
        }
        if let Some(token_budget) = update.token_budget {
            goal.token_budget = token_budget;
        }
        if let Some(status) = update.status {
            if !goal.status.is_terminal() || status == ThreadGoalStatus::Complete {
                goal.status = status;
            }
        }
        goal.updated_at = now_timestamp();
        let goal = goal.clone();
        self.save(&db)?;
        Ok(Some(goal))
    }

    pub fn clear(&mut self, session_id: &str) -> io::Result<bool> {
        let _guard = goal_db_mutation_lock();
        let mut db = self.load()?;
        let removed = db.goals.remove(session_id).is_some();
        if removed {
            self.save(&db)?;
        }
        Ok(removed)
    }

    pub fn account_usage(
        &mut self,
        session_id: &str,
        tokens_delta: i64,
        elapsed_delta: i64,
    ) -> io::Result<Option<ThreadGoal>> {
        let _guard = goal_db_mutation_lock();
        let mut db = self.load()?;
        let Some(goal) = db.goals.get_mut(session_id) else {
            return Ok(None);
        };
        goal.tokens_used = goal.tokens_used.saturating_add(tokens_delta.max(0));
        goal.time_used_seconds = goal.time_used_seconds.saturating_add(elapsed_delta.max(0));
        if let Some(budget) = goal.token_budget
            && goal.tokens_used >= budget
            && goal.status == ThreadGoalStatus::Active
        {
            goal.status = ThreadGoalStatus::BudgetLimited;
        }
        goal.updated_at = now_timestamp();
        let goal = goal.clone();
        self.save(&db)?;
        Ok(Some(goal))
    }

    fn load(&self) -> io::Result<GoalDb> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str(&contents).map_err(io::Error::other),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(GoalDb::default()),
            Err(error) => Err(error),
        }
    }

    fn save(&self, db: &GoalDb) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let contents = serde_json::to_string_pretty(db).map_err(io::Error::other)?;
        fs::write(&tmp, contents)?;
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
}

fn goal_db_mutation_lock() -> std::sync::MutexGuard<'static, ()> {
    GOAL_DB_MUTATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

pub fn goals_db_path() -> PathBuf {
    orca_home().join(GOALS_DB_FILENAME)
}

fn orca_home() -> PathBuf {
    if let Ok(value) = std::env::var(ORCA_HOME_ENV)
        && !value.trim().is_empty()
    {
        return PathBuf::from(value);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".orca")
}

fn now_timestamp() -> i64 {
    Utc::now().timestamp()
}

fn invalid_input(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::{
        ExtensionData, ExtensionRegistryBuilder, ToolCallOutcome, ToolFinishInput,
    };
    use orca_core::goal_types::{GoalUpdate, ThreadGoalStatus};
    use tempfile::tempdir;

    #[test]
    fn store_replaces_gets_reloads_and_clears_goal() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals_1.json");
        let mut store = GoalStore::with_path(path.clone());

        let goal = store
            .replace(
                "session-1",
                "ship persistent goals",
                ThreadGoalStatus::Active,
                Some(50_000),
            )
            .unwrap();
        assert_eq!(goal.session_id, "session-1");
        assert_eq!(goal.objective, "ship persistent goals");
        assert_eq!(goal.status, ThreadGoalStatus::Active);
        assert_eq!(goal.token_budget, Some(50_000));

        let reloaded = GoalStore::with_path(path)
            .get("session-1")
            .unwrap()
            .expect("goal should reload");
        assert_eq!(reloaded.objective, "ship persistent goals");

        assert!(store.clear("session-1").unwrap());
        assert!(!store.clear("session-1").unwrap());
        assert!(store.get("session-1").unwrap().is_none());
    }

    #[test]
    fn store_updates_goal_and_accounts_usage() {
        let dir = tempdir().unwrap();
        let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
        store
            .replace("session-1", "old", ThreadGoalStatus::Active, None)
            .unwrap();

        let updated = store
            .update(
                "session-1",
                GoalUpdate {
                    objective: Some("new objective".to_string()),
                    status: Some(ThreadGoalStatus::Paused),
                    token_budget: Some(Some(1234)),
                },
            )
            .unwrap()
            .expect("goal exists");

        assert_eq!(updated.objective, "new objective");
        assert_eq!(updated.status, ThreadGoalStatus::Paused);
        assert_eq!(updated.token_budget, Some(1234));

        let accounted = store
            .account_usage("session-1", 300, 12)
            .unwrap()
            .expect("goal exists");
        assert_eq!(accounted.tokens_used, 300);
        assert_eq!(accounted.time_used_seconds, 12);
    }

    #[test]
    fn latest_active_returns_most_recent_active_goal() {
        let dir = tempdir().unwrap();
        let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
        store
            .replace("active-1", "old", ThreadGoalStatus::Active, None)
            .unwrap();
        store
            .replace("paused", "paused", ThreadGoalStatus::Paused, None)
            .unwrap();
        store
            .replace("active-2", "new", ThreadGoalStatus::Active, None)
            .unwrap();

        let latest = store.latest_active().unwrap().expect("active goal");

        assert_eq!(latest.session_id, "active-2");
        assert_eq!(latest.objective, "new");
    }

    #[test]
    fn resume_into_same_session_preserves_goal_usage_and_identity() {
        let dir = tempdir().unwrap();
        let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
        let created = store
            .replace(
                "session-1",
                "finish the release",
                ThreadGoalStatus::Paused,
                Some(50_000),
            )
            .unwrap();
        let accounted = store
            .account_usage("session-1", 12_345, 780)
            .unwrap()
            .expect("goal exists");
        let old_updated_at = created.created_at.saturating_sub(60);
        let mut db = store.load().unwrap();
        db.goals
            .get_mut("session-1")
            .expect("goal exists")
            .updated_at = old_updated_at;
        store.save(&db).unwrap();

        let resumed = store
            .resume_into("session-1", "session-1")
            .unwrap()
            .expect("goal resumes");

        assert_eq!(resumed.session_id, "session-1");
        assert_eq!(resumed.objective, "finish the release");
        assert_eq!(resumed.status, ThreadGoalStatus::Active);
        assert_eq!(resumed.token_budget, Some(50_000));
        assert_eq!(resumed.tokens_used, 12_345);
        assert_eq!(resumed.time_used_seconds, 780);
        assert_eq!(resumed.created_at, created.created_at);
        assert_eq!(resumed.created_at, accounted.created_at);
        assert!(resumed.updated_at > old_updated_at);
    }

    #[test]
    fn resume_into_different_session_migrates_goal_and_pauses_source() {
        let dir = tempdir().unwrap();
        let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
        let created = store
            .replace(
                "old-session",
                "migrate the full goal",
                ThreadGoalStatus::Active,
                Some(99_000),
            )
            .unwrap();
        let accounted = store
            .account_usage("old-session", 45_678, 321)
            .unwrap()
            .expect("goal exists");
        let old_updated_at = created.created_at.saturating_sub(60);
        let mut db = store.load().unwrap();
        db.goals
            .get_mut("old-session")
            .expect("goal exists")
            .updated_at = old_updated_at;
        store.save(&db).unwrap();

        let resumed = store
            .resume_into("old-session", "new-session")
            .unwrap()
            .expect("goal resumes");

        assert_eq!(resumed.session_id, "new-session");
        assert_eq!(resumed.objective, "migrate the full goal");
        assert_eq!(resumed.status, ThreadGoalStatus::Active);
        assert_eq!(resumed.token_budget, Some(99_000));
        assert_eq!(resumed.tokens_used, 45_678);
        assert_eq!(resumed.time_used_seconds, 321);
        assert_eq!(resumed.created_at, created.created_at);
        assert_eq!(resumed.created_at, accounted.created_at);
        assert!(resumed.updated_at > old_updated_at);

        let source = store
            .get("old-session")
            .unwrap()
            .expect("source goal remains");
        assert_eq!(source.session_id, "old-session");
        assert_eq!(source.objective, resumed.objective);
        assert_eq!(source.status, ThreadGoalStatus::Paused);
        assert_eq!(source.token_budget, resumed.token_budget);
        assert_eq!(source.tokens_used, resumed.tokens_used);
        assert_eq!(source.time_used_seconds, resumed.time_used_seconds);
        assert_eq!(source.created_at, resumed.created_at);
        assert_eq!(source.updated_at, resumed.updated_at);
    }

    #[test]
    fn resume_into_missing_source_creates_no_target() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals_1.json");
        let mut store = GoalStore::with_path(path.clone());

        let resumed = store.resume_into("missing", "new-session").unwrap();

        assert!(resumed.is_none());
        assert!(store.get("new-session").unwrap().is_none());
        assert!(!path.exists());
    }

    #[test]
    fn resume_into_rejects_occupied_target_without_mutating_either_goal() {
        let dir = tempdir().unwrap();
        let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
        store
            .replace(
                "source-session",
                "preserve the source",
                ThreadGoalStatus::Active,
                Some(100_000),
            )
            .unwrap();
        store
            .account_usage("source-session", 12_345, 456)
            .unwrap()
            .expect("source goal exists");
        store
            .replace(
                "target-session",
                "preserve the target",
                ThreadGoalStatus::Paused,
                Some(200_000),
            )
            .unwrap();
        store
            .account_usage("target-session", 67_890, 987)
            .unwrap()
            .expect("target goal exists");
        let source_before = store
            .get("source-session")
            .unwrap()
            .expect("source goal exists");
        let target_before = store
            .get("target-session")
            .unwrap()
            .expect("target goal exists");

        let result = store.resume_into("source-session", "target-session");
        let source_after = store
            .get("source-session")
            .unwrap()
            .expect("source goal remains");
        let target_after = store
            .get("target-session")
            .unwrap()
            .expect("target goal remains");

        assert!(
            result.is_err(),
            "occupied target was overwritten: source before={source_before:?}, source after={source_after:?}, target before={target_before:?}, target after={target_after:?}"
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(source_after, source_before);
        assert_eq!(target_after, target_before);
    }

    #[test]
    fn replace_still_resets_usage_after_prior_accounting() {
        let dir = tempdir().unwrap();
        let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
        store
            .replace(
                "session-1",
                "old objective",
                ThreadGoalStatus::Active,
                Some(50_000),
            )
            .unwrap();
        store
            .account_usage("session-1", 12_345, 780)
            .unwrap()
            .expect("goal exists");

        let replaced = store
            .replace(
                "session-1",
                "new objective",
                ThreadGoalStatus::Active,
                Some(60_000),
            )
            .unwrap();

        assert_eq!(replaced.objective, "new objective");
        assert_eq!(replaced.token_budget, Some(60_000));
        assert_eq!(replaced.tokens_used, 0);
        assert_eq!(replaced.time_used_seconds, 0);
    }

    #[test]
    fn concurrent_usage_deltas_are_not_lost() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals_1.json");
        let mut store = GoalStore::with_path(path.clone());
        store
            .replace(
                "session-1",
                "concurrent usage",
                ThreadGoalStatus::Active,
                None,
            )
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut workers = Vec::new();

        for _ in 0..8 {
            let barrier = Arc::clone(&barrier);
            let path = path.clone();
            workers.push(std::thread::spawn(move || {
                let mut store = GoalStore::with_path(path);
                barrier.wait();
                for _ in 0..25 {
                    store
                        .account_usage("session-1", 7, 1)
                        .unwrap()
                        .expect("goal exists");
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let goal = store.get("session-1").unwrap().unwrap();
        assert_eq!(goal.tokens_used, 8 * 25 * 7);
        assert_eq!(goal.time_used_seconds, 8 * 25);
    }

    #[test]
    fn terminal_status_is_not_downgraded_by_pause_or_block() {
        let dir = tempdir().unwrap();
        let mut store = GoalStore::with_path(dir.path().join("goals_1.json"));
        store
            .replace("session-1", "finish", ThreadGoalStatus::Complete, None)
            .unwrap();

        let updated = store
            .update(
                "session-1",
                GoalUpdate {
                    objective: None,
                    status: Some(ThreadGoalStatus::Paused),
                    token_budget: None,
                },
            )
            .unwrap()
            .expect("goal exists");

        assert_eq!(updated.status, ThreadGoalStatus::Complete);
    }

    #[test]
    fn goal_extension_records_completed_non_goal_tool_attempts() {
        let mut builder = ExtensionRegistryBuilder::new();
        install_goal_tool_lifecycle(&mut builder);
        let registry = builder.build();
        let thread_store = ExtensionData::new("session-1");
        let turn_store = ExtensionData::new("turn-1");

        registry.on_tool_finish(ToolFinishInput {
            thread_store: &thread_store,
            turn_store: &turn_store,
            tool_name: "bash",
            call_id: "call-1",
            outcome: ToolCallOutcome::Completed,
        });
        registry.on_tool_finish(ToolFinishInput {
            thread_store: &thread_store,
            turn_store: &turn_store,
            tool_name: "update_goal",
            call_id: "call-2",
            outcome: ToolCallOutcome::Completed,
        });
        registry.on_tool_finish(ToolFinishInput {
            thread_store: &thread_store,
            turn_store: &turn_store,
            tool_name: "read_file",
            call_id: "call-3",
            outcome: ToolCallOutcome::Blocked,
        });

        let progress = thread_store
            .get::<GoalToolProgressState>()
            .expect("goal progress state");
        assert_eq!(progress.completed_tool_attempts(), 1);
        assert_eq!(progress.last_turn_id().as_deref(), Some("turn-1"));
        assert_eq!(progress.last_call_id().as_deref(), Some("call-1"));
    }

    #[test]
    fn goal_extension_counts_possible_started_work_but_not_cancelled_or_unstarted_work() {
        let thread_store = ExtensionData::new("session-1");
        let turn_store = ExtensionData::new("turn-1");

        record_goal_tool_finish(
            &thread_store,
            &turn_store,
            "bash",
            "cancelled-call",
            ToolCallOutcome::Cancelled {
                started: ToolInvocationStarted::Yes,
            },
        );
        record_goal_tool_finish(
            &thread_store,
            &turn_store,
            "bash",
            "unknown-call",
            ToolCallOutcome::Indeterminate {
                started: ToolInvocationStarted::Yes,
            },
        );
        record_goal_tool_finish(
            &thread_store,
            &turn_store,
            "bash",
            "unknown-call",
            ToolCallOutcome::Indeterminate {
                started: ToolInvocationStarted::Unknown,
            },
        );
        record_goal_tool_finish(
            &thread_store,
            &turn_store,
            "bash",
            "unstarted-call",
            ToolCallOutcome::Failed {
                started: ToolInvocationStarted::No,
            },
        );

        let progress = thread_store
            .get::<GoalToolProgressState>()
            .expect("goal progress state");
        assert_eq!(progress.completed_tool_attempts(), 2);
        assert_eq!(progress.last_call_id().as_deref(), Some("unknown-call"));
    }

    #[test]
    fn terminal_goal_update_requires_live_thread_extension_progress() {
        let thread_store = ExtensionData::new("session-1");
        let update = GoalUpdate {
            objective: None,
            status: Some(ThreadGoalStatus::Complete),
            token_budget: None,
        };

        let error =
            validate_goal_terminal_update_against_extensions(&update, &thread_store).unwrap_err();

        assert!(
            error.contains("requires at least one completed non-goal tool attempt"),
            "unexpected error: {error}"
        );

        let mut builder = ExtensionRegistryBuilder::new();
        install_goal_tool_lifecycle(&mut builder);
        let registry = builder.build();
        let turn_store = ExtensionData::new("turn-1");
        registry.on_tool_finish(ToolFinishInput {
            thread_store: &thread_store,
            turn_store: &turn_store,
            tool_name: "bash",
            call_id: "call-1",
            outcome: ToolCallOutcome::Completed,
        });

        validate_goal_terminal_update_against_extensions(&update, &thread_store).unwrap();
    }

    #[test]
    fn goal_progress_record_helper_reuses_lifecycle_rules() {
        let thread_store = ExtensionData::new("session-1");
        let turn_store = ExtensionData::new("turn-1");

        record_goal_tool_finish(
            &thread_store,
            &turn_store,
            "update_goal",
            "call-1",
            ToolCallOutcome::Completed,
        );
        record_goal_tool_finish(
            &thread_store,
            &turn_store,
            "bash",
            "call-2",
            ToolCallOutcome::Completed,
        );

        let progress = thread_store
            .get::<GoalToolProgressState>()
            .expect("goal progress state");
        assert_eq!(progress.completed_tool_attempts(), 1);
        assert_eq!(progress.last_call_id().as_deref(), Some("call-2"));
    }
}
