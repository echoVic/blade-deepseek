use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use crate::extension::{
    ExtensionRegistryBuilder, ToolCallOutcome, ToolFinishInput, ToolLifecycleContributor,
};
use chrono::Utc;
use orca_core::goal_types::{
    GoalUpdate, ThreadGoal, ThreadGoalStatus, validate_thread_goal_objective,
};
use serde::{Deserialize, Serialize};

const ORCA_HOME_ENV: &str = "ORCA_HOME";
const GOALS_DB_FILENAME: &str = "goals_1.json";

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
        if !goal_tool_attempt_counts(input.outcome) || input.tool_name == "update_goal" {
            return;
        }

        input
            .thread_store
            .get_or_init(GoalToolProgressState::default)
            .record_completed_attempt(input.turn_store.level_id(), input.call_id);
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
                handler_executed: true
            }
    )
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

    pub fn replace(
        &mut self,
        session_id: &str,
        objective: &str,
        status: ThreadGoalStatus,
        token_budget: Option<i64>,
    ) -> io::Result<ThreadGoal> {
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
}
