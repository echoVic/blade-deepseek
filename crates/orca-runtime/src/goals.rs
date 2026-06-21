use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;

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
}
