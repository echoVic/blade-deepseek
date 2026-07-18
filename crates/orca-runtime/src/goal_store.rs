use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use orca_core::goal_runtime::{
    BlockerKind, BlockerSummary, GoalId, GoalOuterTurnId, GoalPauseReason, GoalRecord,
    GoalRequestedState, GoalRunId, GoalRunSnapshot, GoalState, GoalTransitionSummary,
    GoalTurnOrigin, GoalTurnStatus, GoalUpdateAck, GoalUpdateIntent, GoalUsage,
};
use orca_core::goal_types::{ThreadGoal, ThreadGoalStatus, validate_thread_goal_objective};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

const SCHEMA_VERSION: i64 = 1;
const DATABASE_FILENAME: &str = "goals.sqlite3";
const LEGACY_FILENAME: &str = "goals_1.json";
const LEGACY_MIGRATION_KEY: &str = "legacy_goals_1_migrated";

#[derive(Clone, Debug)]
pub struct GoalStore {
    path: PathBuf,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GoalAuditSnapshot {
    pub outer_turns: i64,
    pub intents: i64,
    pub usage_events: i64,
    pub verifier_tokens: i64,
    pub transitions: i64,
    pub in_flight_runs: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalRecoveryRecord {
    pub session_id: String,
    pub goal_id: GoalId,
    pub stale_goal_run_id: GoalRunId,
    pub outer_turn_id: Option<GoalOuterTurnId>,
    pub recovered_state: GoalState,
}

#[derive(Debug)]
pub enum GoalStoreError {
    Sqlite(rusqlite::Error),
    Io(io::Error),
    Json(serde_json::Error),
    Invalid(String),
    Migration(String),
}

impl fmt::Display for GoalStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "goal database error: {error}"),
            Self::Io(error) => write!(formatter, "goal database I/O error: {error}"),
            Self::Json(error) => write!(formatter, "goal database JSON error: {error}"),
            Self::Invalid(message) => formatter.write_str(message),
            Self::Migration(message) => {
                write!(formatter, "legacy goal migration failed: {message}")
            }
        }
    }
}

impl std::error::Error for GoalStoreError {}

impl From<rusqlite::Error> for GoalStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<io::Error> for GoalStoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for GoalStoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateGoalInput {
    pub session_id: String,
    pub objective: String,
    pub token_budget: Option<i64>,
    pub now: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeginGoalRunInput {
    pub goal_id: GoalId,
    pub goal_run_id: GoalRunId,
    pub origin: GoalTurnOrigin,
    pub started_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BeginOuterTurnInput {
    pub goal_id: GoalId,
    pub goal_run_id: GoalRunId,
    pub outer_turn_id: GoalOuterTurnId,
    pub origin: GoalTurnOrigin,
    pub provider_turn_id: String,
    pub started_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalUsageEvent {
    pub usage_event_id: String,
    pub goal_id: GoalId,
    pub source: String,
    pub usage: GoalUsage,
    pub created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalIntentRecord {
    pub outer_turn_id: GoalOuterTurnId,
    pub intent: GoalUpdateIntent,
    pub ack: GoalUpdateAck,
    pub created_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishOuterTurnInput {
    pub goal_id: GoalId,
    pub goal_run_id: GoalRunId,
    pub outer_turn_id: GoalOuterTurnId,
    pub status: orca_core::goal_runtime::GoalTurnStatus,
    pub tool_count: u32,
    pub model_response_count: u32,
    pub gap_fingerprint: Option<String>,
    pub usage_event: Option<GoalUsageEvent>,
    pub finished_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishOuterTurnOutcome {
    pub already_finished: bool,
    pub usage: GoalUsage,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct LegacyGoalDb {
    goals: BTreeMap<String, ThreadGoal>,
}

struct StoredGoal {
    record: GoalRecord,
    created_at: i64,
    updated_at: i64,
}

impl GoalStore {
    pub fn load_default() -> Result<Self, GoalStoreError> {
        let home = orca_home();
        Self::open_with_legacy(home.join(DATABASE_FILENAME), home.join(LEGACY_FILENAME))
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, GoalStoreError> {
        Self::open_internal(path.as_ref().to_path_buf(), None)
    }

    pub fn open_with_legacy(
        path: impl AsRef<Path>,
        legacy_path: impl AsRef<Path>,
    ) -> Result<Self, GoalStoreError> {
        Self::open_internal(
            path.as_ref().to_path_buf(),
            Some(legacy_path.as_ref().to_path_buf()),
        )
    }

    fn open_internal(path: PathBuf, legacy_path: Option<PathBuf>) -> Result<Self, GoalStoreError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let store = Self { path };
        store.initialize_schema()?;
        if let Some(legacy_path) = legacy_path.as_deref() {
            store.migrate_legacy_once(legacy_path)?;
        }
        Ok(store)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn schema_version(&self) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        let version: String = connection.query_row(
            "SELECT value FROM goal_meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )?;
        version.parse().map_err(|error| {
            GoalStoreError::Invalid(format!("invalid goal schema version '{version}': {error}"))
        })
    }

    pub fn create_goal(&self, input: CreateGoalInput) -> Result<GoalRecord, GoalStoreError> {
        validate_thread_goal_objective(&input.objective).map_err(GoalStoreError::Invalid)?;
        if input.session_id.trim().is_empty() {
            return Err(GoalStoreError::Invalid(
                "goal session id must not be empty".to_string(),
            ));
        }
        if input.token_budget.is_some_and(|budget| budget <= 0) {
            return Err(GoalStoreError::Invalid(
                "goal token budget must be positive".to_string(),
            ));
        }

        let goal_id = GoalId::new();
        let state = GoalState::Active;
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO goals (
                goal_id, session_id, objective, objective_revision, state,
                token_budget, created_at, updated_at
             ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?6)",
            params![
                goal_id.as_str(),
                input.session_id.trim(),
                input.objective.trim(),
                state_json(&state)?,
                input.token_budget,
                input.now,
            ],
        )?;
        insert_transition(
            &transaction,
            &goal_id,
            None,
            &state,
            &state,
            "created",
            input.now,
        )?;
        transaction.commit()?;
        Ok(self
            .get_by_session(input.session_id.trim())?
            .expect("created goal must be readable"))
    }

    pub fn get_by_session(&self, session_id: &str) -> Result<Option<GoalRecord>, GoalStoreError> {
        let connection = self.connection()?;
        Ok(load_stored_goal(&connection, session_id)?.map(|stored| stored.record))
    }

    pub fn project_thread_goal(
        &self,
        session_id: &str,
    ) -> Result<Option<ThreadGoal>, GoalStoreError> {
        let connection = self.connection()?;
        let Some(stored) = load_stored_goal(&connection, session_id)? else {
            return Ok(None);
        };
        Ok(Some(ThreadGoal {
            session_id: stored.record.session_id,
            objective: stored.record.objective,
            status: ThreadGoalStatus::from_runtime_state(&stored.record.state),
            token_budget: stored.record.token_budget,
            tokens_used: stored.record.usage.charged_tokens(),
            time_used_seconds: stored.record.usage.elapsed_seconds,
            created_at: stored.created_at,
            updated_at: stored.updated_at,
        }))
    }

    pub fn latest_active(&self) -> Result<Option<ThreadGoal>, GoalStoreError> {
        let connection = self.connection()?;
        let session_id: Option<String> = connection
            .query_row(
                "SELECT session_id FROM goals
                 WHERE state LIKE '{\"status\":\"active\"%'
                 ORDER BY updated_at DESC, created_at DESC, session_id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        match session_id.as_deref() {
            Some(session_id) => self.project_thread_goal(session_id),
            None => Ok(None),
        }
    }

    pub fn edit_goal(
        &self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
        updated_at: i64,
    ) -> Result<Option<GoalRecord>, GoalStoreError> {
        validate_thread_goal_objective(objective).map_err(GoalStoreError::Invalid)?;
        if token_budget.is_some_and(|budget| budget <= 0) {
            return Err(GoalStoreError::Invalid(
                "goal token budget must be positive".to_string(),
            ));
        }
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String)> = transaction
            .query_row(
                "SELECT goal_id, state FROM goals WHERE session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((goal_id, previous_json)) = row else {
            transaction.commit()?;
            return Ok(None);
        };
        let previous = parse_state(&previous_json)?;
        let goal_id = GoalId::parse(goal_id).map_err(GoalStoreError::Invalid)?;
        ensure_goal_not_in_flight(&transaction, goal_id.as_str(), "edit")?;
        let next = GoalState::Active;
        transaction.execute(
            "UPDATE goals SET objective = ?1, objective_revision = objective_revision + 1,
                state = ?2, token_budget = ?3, updated_at = ?4 WHERE session_id = ?5",
            params![
                objective.trim(),
                state_json(&next)?,
                token_budget,
                updated_at,
                session_id
            ],
        )?;
        transaction.execute(
            "UPDATE goal_runs
             SET status = 'edited', in_flight = 0, finished_at = COALESCE(finished_at, ?1)
             WHERE goal_id = ?2 AND finished_at IS NULL",
            params![updated_at, goal_id.as_str()],
        )?;
        insert_transition(
            &transaction,
            &goal_id,
            None,
            &previous,
            &next,
            "edited",
            updated_at,
        )?;
        transaction.commit()?;
        self.get_by_session(session_id)
    }

    pub fn resume_into(
        &self,
        source_session_id: &str,
        resumed_session_id: &str,
        now: i64,
    ) -> Result<Option<GoalRecord>, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source = transaction
            .query_row(
                "SELECT goal_id, objective, token_budget, created_at, state
                 FROM goals WHERE session_id = ?1",
                [source_session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((source_goal_id, objective, token_budget, created_at, source_state_json)) = source
        else {
            transaction.commit()?;
            return Ok(None);
        };
        ensure_goal_not_in_flight(&transaction, &source_goal_id, "resume")?;
        let source_state = parse_state(&source_state_json)?;
        if source_session_id != resumed_session_id
            && transaction
                .query_row(
                    "SELECT 1 FROM goals WHERE session_id = ?1",
                    [resumed_session_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .is_some()
        {
            return Err(GoalStoreError::Invalid(format!(
                "goal already exists for resume target session '{resumed_session_id}'"
            )));
        }
        if source_session_id == resumed_session_id {
            let goal_id = GoalId::parse(source_goal_id).map_err(GoalStoreError::Invalid)?;
            let next = GoalState::Active;
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE session_id = ?3",
                params![state_json(&next)?, now, resumed_session_id],
            )?;
            insert_transition(
                &transaction,
                &goal_id,
                None,
                &source_state,
                &next,
                "resumed",
                now,
            )?;
        } else {
            let source_goal_id = GoalId::parse(source_goal_id).map_err(GoalStoreError::Invalid)?;
            let usage = usage_totals(&transaction, &source_goal_id)?;
            let next_goal_id = GoalId::new();
            let next = GoalState::Active;
            let paused = GoalState::Paused {
                reason: GoalPauseReason::User,
                message: format!("paused while resuming into session {resumed_session_id}"),
            };
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&paused)?, now, source_goal_id.as_str()],
            )?;
            transaction.execute(
                "UPDATE goal_runs
                 SET status = 'resumed_elsewhere', in_flight = 0,
                     finished_at = COALESCE(finished_at, ?1)
                 WHERE goal_id = ?2 AND finished_at IS NULL",
                params![now, source_goal_id.as_str()],
            )?;
            insert_transition(
                &transaction,
                &source_goal_id,
                None,
                &source_state,
                &paused,
                "resume_fork_source_paused",
                now,
            )?;
            transaction.execute(
                "INSERT INTO goals (
                    goal_id, session_id, objective, objective_revision, state,
                    token_budget, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
                params![
                    next_goal_id.as_str(),
                    resumed_session_id,
                    objective,
                    state_json(&next)?,
                    token_budget,
                    created_at,
                    now
                ],
            )?;
            insert_transition(
                &transaction,
                &next_goal_id,
                None,
                &next,
                &next,
                "resumed",
                now,
            )?;
            if usage.charged_tokens() > 0 || usage.elapsed_seconds > 0 {
                transaction.execute(
                    "INSERT INTO goal_usage_events (
                        usage_event_id, goal_id, source, charged_input_tokens,
                        output_tokens, cache_tokens, verifier_tokens, cost_micros,
                        elapsed_seconds, created_at
                     ) VALUES (?1, ?2, 'resume_copy', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        format!("resume:{source_goal_id}:{resumed_session_id}"),
                        next_goal_id.as_str(),
                        usage.charged_input_tokens,
                        usage.output_tokens,
                        usage.cache_tokens,
                        usage.verifier_tokens,
                        usage.cost_micros,
                        usage.elapsed_seconds,
                        now
                    ],
                )?;
            }
        }
        transaction.commit()?;
        self.get_by_session(resumed_session_id)
    }

    pub fn begin_run(&self, input: BeginGoalRunInput) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let state = goal_state_by_id(&transaction, &input.goal_id)?;
        if !state.should_continue() {
            return Err(GoalStoreError::Invalid(format!(
                "cannot begin goal run while state is {state:?}"
            )));
        }
        transaction.execute(
            "INSERT INTO goal_runs (
                goal_run_id, goal_id, status, origin, current_outer_turn_id,
                continuation_count, in_flight, started_at, finished_at
             ) VALUES (?1, ?2, 'active', ?3, NULL, 0, 0, ?4, NULL)",
            params![
                input.goal_run_id.as_str(),
                input.goal_id.as_str(),
                origin_name(input.origin),
                input.started_at,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn begin_outer_turn(&self, input: BeginOuterTurnInput) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE goal_runs
             SET current_outer_turn_id = ?1,
                 continuation_count = continuation_count + 1,
                 in_flight = 1
             WHERE goal_run_id = ?2 AND goal_id = ?3 AND in_flight = 0 AND finished_at IS NULL",
            params![
                input.outer_turn_id.as_str(),
                input.goal_run_id.as_str(),
                input.goal_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(GoalStoreError::Invalid(
                "goal run is missing, stale, or already has an in-flight outer turn".to_string(),
            ));
        }
        transaction.execute(
            "INSERT INTO goal_turns (
                outer_turn_id, goal_run_id, origin, provider_turn_id, status,
                tool_count, model_response_count, charged_input_tokens,
                output_tokens, verifier_tokens, gap_fingerprint, started_at, finished_at
             ) VALUES (?1, ?2, ?3, ?4, 'in_flight', 0, 0, 0, 0, 0, NULL, ?5, NULL)",
            params![
                input.outer_turn_id.as_str(),
                input.goal_run_id.as_str(),
                origin_name(input.origin),
                input.provider_turn_id,
                input.started_at,
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_usage_once(&self, event: GoalUsageEvent) -> Result<GoalUsage, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT OR IGNORE INTO goal_usage_events (
                usage_event_id, goal_id, source, charged_input_tokens,
                output_tokens, cache_tokens, verifier_tokens, cost_micros,
                elapsed_seconds, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.usage_event_id,
                event.goal_id.as_str(),
                event.source,
                event.usage.charged_input_tokens.max(0),
                event.usage.output_tokens.max(0),
                event.usage.cache_tokens.max(0),
                event.usage.verifier_tokens.max(0),
                event.usage.cost_micros.max(0),
                event.usage.elapsed_seconds.max(0),
                event.created_at,
            ],
        )?;
        let usage = usage_totals(&transaction, &event.goal_id)?;
        let (state, token_budget) = transaction.query_row(
            "SELECT state, token_budget FROM goals WHERE goal_id = ?1",
            [event.goal_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        let state = parse_state(&state)?;
        if state.should_continue()
            && token_budget.is_some_and(|budget| usage.charged_tokens() >= budget)
        {
            let next = GoalState::BudgetLimited;
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&next)?, event.created_at, event.goal_id.as_str()],
            )?;
            insert_transition(
                &transaction,
                &event.goal_id,
                None,
                &state,
                &next,
                "budget_limited",
                event.created_at,
            )?;
        }
        transaction.commit()?;
        Ok(usage)
    }

    pub fn record_verifier_usage_once(
        &self,
        outer_turn_id: &GoalOuterTurnId,
        event: GoalUsageEvent,
    ) -> Result<GoalUsage, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO goal_usage_events (
                usage_event_id, goal_id, source, charged_input_tokens,
                output_tokens, cache_tokens, verifier_tokens, cost_micros,
                elapsed_seconds, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                event.usage_event_id,
                event.goal_id.as_str(),
                event.source,
                event.usage.charged_input_tokens.max(0),
                event.usage.output_tokens.max(0),
                event.usage.cache_tokens.max(0),
                event.usage.verifier_tokens.max(0),
                event.usage.cost_micros.max(0),
                event.usage.elapsed_seconds.max(0),
                event.created_at,
            ],
        )?;
        if inserted == 1 {
            let changed = transaction.execute(
                "UPDATE goal_turns
                 SET verifier_tokens = verifier_tokens + ?1
                 WHERE outer_turn_id = ?2
                   AND finished_at IS NOT NULL
                   AND goal_run_id IN (
                       SELECT goal_run_id FROM goal_runs WHERE goal_id = ?3
                   )",
                params![
                    event.usage.verifier_tokens.max(0),
                    outer_turn_id.as_str(),
                    event.goal_id.as_str(),
                ],
            )?;
            if changed != 1 {
                return Err(GoalStoreError::Invalid(
                    "verifier usage references a missing, in-flight, or unrelated outer turn"
                        .to_string(),
                ));
            }
        }
        let usage = usage_totals(&transaction, &event.goal_id)?;
        let (state, token_budget) = transaction.query_row(
            "SELECT state, token_budget FROM goals WHERE goal_id = ?1",
            [event.goal_id.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        let state = parse_state(&state)?;
        if state.should_continue()
            && token_budget.is_some_and(|budget| usage.charged_tokens() >= budget)
        {
            let next = GoalState::BudgetLimited;
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&next)?, event.created_at, event.goal_id.as_str()],
            )?;
            insert_transition(
                &transaction,
                &event.goal_id,
                Some(outer_turn_id.as_str()),
                &state,
                &next,
                "budget_limited",
                event.created_at,
            )?;
        }
        transaction.commit()?;
        Ok(usage)
    }

    pub fn record_intent(&self, record: GoalIntentRecord) -> Result<GoalUpdateAck, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO goal_intents (
                intent_id, outer_turn_id, requested_state, payload_json,
                ack_code, ack_json, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                record.intent.intent_id.as_str(),
                record.outer_turn_id.as_str(),
                requested_state_name(record.intent.requested_state),
                serde_json::to_string(&record.intent)?,
                ack_code(&record.ack),
                serde_json::to_string(&record.ack)?,
                record.created_at,
            ],
        )?;
        let ack_json: String = if inserted == 1 {
            serde_json::to_string(&record.ack)?
        } else {
            transaction.query_row(
                "SELECT ack_json FROM goal_intents WHERE intent_id = ?1",
                [record.intent.intent_id.as_str()],
                |row| row.get(0),
            )?
        };
        transaction.commit()?;
        Ok(serde_json::from_str(&ack_json)?)
    }

    pub fn intent_count(&self) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row("SELECT COUNT(*) FROM goal_intents", [], |row| row.get(0))?)
    }

    pub fn finish_outer_turn(
        &self,
        input: FinishOuterTurnInput,
    ) -> Result<FinishOuterTurnOutcome, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let status: Option<String> = transaction
            .query_row(
                "SELECT status FROM goal_turns
                 WHERE outer_turn_id = ?1 AND goal_run_id = ?2",
                params![input.outer_turn_id.as_str(), input.goal_run_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(status) = status else {
            return Err(GoalStoreError::Invalid(
                "goal outer turn does not exist".to_string(),
            ));
        };
        if status != "in_flight" {
            let usage = usage_totals(&transaction, &input.goal_id)?;
            transaction.commit()?;
            return Ok(FinishOuterTurnOutcome {
                already_finished: true,
                usage,
            });
        }
        let turn_usage = input
            .usage_event
            .as_ref()
            .map(|event| event.usage.clone())
            .unwrap_or_default();
        if let Some(event) = input.usage_event {
            insert_usage_event(&transaction, &event)?;
        }
        let changed = transaction.execute(
            "UPDATE goal_turns SET status = ?1, tool_count = ?2,
                model_response_count = ?3, charged_input_tokens = ?4,
                output_tokens = ?5, verifier_tokens = ?6,
                gap_fingerprint = ?7, finished_at = ?8
             WHERE outer_turn_id = ?9 AND goal_run_id = ?10 AND status = 'in_flight'",
            params![
                turn_status_name(input.status),
                input.tool_count,
                input.model_response_count,
                turn_usage.charged_input_tokens.max(0),
                turn_usage.output_tokens.max(0),
                turn_usage.verifier_tokens.max(0),
                input.gap_fingerprint,
                input.finished_at,
                input.outer_turn_id.as_str(),
                input.goal_run_id.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(GoalStoreError::Invalid(
                "goal outer turn was concurrently finalized".to_string(),
            ));
        }
        transaction.execute(
            "UPDATE goal_runs SET current_outer_turn_id = NULL, in_flight = 0
             WHERE goal_run_id = ?1 AND goal_id = ?2",
            params![input.goal_run_id.as_str(), input.goal_id.as_str()],
        )?;
        let usage = usage_totals(&transaction, &input.goal_id)?;
        let (state_json_value, token_budget): (String, Option<i64>) = transaction.query_row(
            "SELECT state, token_budget FROM goals WHERE goal_id = ?1",
            [input.goal_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let state = parse_state(&state_json_value)?;
        if state.should_continue()
            && token_budget.is_some_and(|budget| usage.charged_tokens() >= budget)
        {
            let next = GoalState::BudgetLimited;
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![
                    state_json(&next)?,
                    input.finished_at,
                    input.goal_id.as_str()
                ],
            )?;
            insert_transition(
                &transaction,
                &input.goal_id,
                Some(input.outer_turn_id.as_str()),
                &state,
                &next,
                "budget_limited",
                input.finished_at,
            )?;
        }
        let final_state = goal_state_by_id(&transaction, &input.goal_id)?;
        if let Some(run_status) = closed_run_status(&final_state) {
            transaction.execute(
                "UPDATE goal_runs
                 SET status = ?1, in_flight = 0, finished_at = COALESCE(finished_at, ?2)
                 WHERE goal_run_id = ?3 AND goal_id = ?4",
                params![
                    run_status,
                    input.finished_at,
                    input.goal_run_id.as_str(),
                    input.goal_id.as_str()
                ],
            )?;
        }
        transaction.commit()?;
        Ok(FinishOuterTurnOutcome {
            already_finished: false,
            usage,
        })
    }

    pub fn outer_turn_status(
        &self,
        outer_turn_id: &GoalOuterTurnId,
    ) -> Result<Option<String>, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                "SELECT status FROM goal_turns WHERE outer_turn_id = ?1",
                [outer_turn_id.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn outer_turn_verifier_tokens(
        &self,
        outer_turn_id: &GoalOuterTurnId,
    ) -> Result<Option<i64>, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                "SELECT verifier_tokens FROM goal_turns WHERE outer_turn_id = ?1",
                [outer_turn_id.as_str()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn transition_state(
        &self,
        goal_id: &GoalId,
        next: GoalState,
        reason_code: &str,
        outer_turn_id: Option<&GoalOuterTurnId>,
        updated_at: i64,
    ) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = goal_state_by_id(&transaction, goal_id).map_err(|error| match error {
            GoalStoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                GoalStoreError::Invalid("goal does not exist".to_string())
            }
            error => error,
        })?;
        if matches!(previous, GoalState::Complete { .. }) && previous != next {
            return Err(GoalStoreError::Invalid(
                "complete goal cannot be downgraded by a runtime transition".to_string(),
            ));
        }
        if previous != next {
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&next)?, updated_at, goal_id.as_str()],
            )?;
        }
        if let Some(run_status) = closed_run_status(&next) {
            transaction.execute(
                "UPDATE goal_runs
                 SET status = ?1, in_flight = 0, finished_at = COALESCE(finished_at, ?2)
                 WHERE goal_id = ?3 AND finished_at IS NULL",
                params![run_status, updated_at, goal_id.as_str()],
            )?;
        }
        if previous != next {
            insert_transition(
                &transaction,
                goal_id,
                outer_turn_id.map(GoalOuterTurnId::as_str),
                &previous,
                &next,
                reason_code,
                updated_at,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn transition_state_while_turn_in_flight(
        &self,
        goal_id: &GoalId,
        next: GoalState,
        reason_code: &str,
        outer_turn_id: &GoalOuterTurnId,
        updated_at: i64,
    ) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = goal_state_by_id(&transaction, goal_id).map_err(|error| match error {
            GoalStoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                GoalStoreError::Invalid("goal does not exist".to_string())
            }
            error => error,
        })?;
        if matches!(previous, GoalState::Complete { .. }) && previous != next {
            return Err(GoalStoreError::Invalid(
                "complete goal cannot be downgraded by a runtime transition".to_string(),
            ));
        }
        let in_flight: bool = transaction.query_row(
            "SELECT EXISTS(
                    SELECT 1 FROM goal_turns AS turns
                    JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
                    WHERE turns.outer_turn_id = ?1 AND runs.goal_id = ?2
                      AND turns.status = 'in_flight' AND runs.in_flight = 1
                )",
            params![outer_turn_id.as_str(), goal_id.as_str()],
            |row| row.get(0),
        )?;
        if !in_flight {
            return Err(GoalStoreError::Invalid(
                "goal pause request requires the active outer turn".to_string(),
            ));
        }
        if previous != next {
            transaction.execute(
                "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                params![state_json(&next)?, updated_at, goal_id.as_str()],
            )?;
            insert_transition(
                &transaction,
                goal_id,
                Some(outer_turn_id.as_str()),
                &previous,
                &next,
                reason_code,
                updated_at,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn usage_event_count(&self, goal_id: &GoalId) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM goal_usage_events WHERE goal_id = ?1",
            [goal_id.as_str()],
            |row| row.get(0),
        )?)
    }

    pub fn audit_snapshot(&self, goal_id: &GoalId) -> Result<GoalAuditSnapshot, GoalStoreError> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT
                    (SELECT COUNT(*) FROM goal_turns AS turns
                     JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
                     WHERE runs.goal_id = ?1),
                    (SELECT COUNT(*) FROM goal_intents AS intents
                     JOIN goal_turns AS turns ON turns.outer_turn_id = intents.outer_turn_id
                     JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
                     WHERE runs.goal_id = ?1),
                    (SELECT COUNT(*) FROM goal_usage_events WHERE goal_id = ?1),
                    (SELECT COALESCE(SUM(turns.verifier_tokens), 0)
                     FROM goal_turns AS turns
                     JOIN goal_runs AS runs ON runs.goal_run_id = turns.goal_run_id
                     WHERE runs.goal_id = ?1),
                    (SELECT COUNT(*) FROM goal_transitions WHERE goal_id = ?1),
                    (SELECT COUNT(*) FROM goal_runs
                     WHERE goal_id = ?1 AND in_flight = 1)",
                [goal_id.as_str()],
                |row| {
                    Ok(GoalAuditSnapshot {
                        outer_turns: row.get(0)?,
                        intents: row.get(1)?,
                        usage_events: row.get(2)?,
                        verifier_tokens: row.get(3)?,
                        transitions: row.get(4)?,
                        in_flight_runs: row.get(5)?,
                    })
                },
            )
            .map_err(Into::into)
    }

    pub fn in_flight_run_count(&self) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM goal_runs WHERE in_flight = 1",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn transition_count(&self, goal_id: &GoalId) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row(
            "SELECT COUNT(*) FROM goal_transitions WHERE goal_id = ?1",
            [goal_id.as_str()],
            |row| row.get(0),
        )?)
    }

    pub fn goal_count(&self) -> Result<i64, GoalStoreError> {
        let connection = self.connection()?;
        Ok(connection.query_row("SELECT COUNT(*) FROM goals", [], |row| row.get(0))?)
    }

    pub fn clear_goal(&self, session_id: &str) -> Result<bool, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let goal_id = transaction
            .query_row(
                "SELECT goal_id FROM goals WHERE session_id = ?1",
                [session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(goal_id) = goal_id.as_deref() {
            ensure_goal_not_in_flight(&transaction, goal_id, "clear")?;
        }
        let changed =
            transaction.execute("DELETE FROM goals WHERE session_id = ?1", [session_id])?;
        transaction.commit()?;
        Ok(changed == 1)
    }

    fn initialize_schema(&self) -> Result<(), GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS goal_meta (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goals (
                goal_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL UNIQUE,
                objective TEXT NOT NULL,
                objective_revision INTEGER NOT NULL,
                state TEXT NOT NULL,
                token_budget INTEGER,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goal_runs (
                goal_run_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL REFERENCES goals(goal_id) ON DELETE CASCADE,
                status TEXT NOT NULL,
                origin TEXT NOT NULL,
                current_outer_turn_id TEXT,
                continuation_count INTEGER NOT NULL,
                in_flight INTEGER NOT NULL,
                started_at INTEGER NOT NULL,
                finished_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS goal_turns (
                outer_turn_id TEXT PRIMARY KEY,
                goal_run_id TEXT NOT NULL REFERENCES goal_runs(goal_run_id) ON DELETE CASCADE,
                origin TEXT NOT NULL,
                provider_turn_id TEXT NOT NULL,
                status TEXT NOT NULL,
                tool_count INTEGER NOT NULL,
                model_response_count INTEGER NOT NULL,
                charged_input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                verifier_tokens INTEGER NOT NULL,
                gap_fingerprint TEXT,
                started_at INTEGER NOT NULL,
                finished_at INTEGER
             );
             CREATE TABLE IF NOT EXISTS goal_intents (
                intent_id TEXT PRIMARY KEY,
                outer_turn_id TEXT NOT NULL REFERENCES goal_turns(outer_turn_id) ON DELETE CASCADE,
                requested_state TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                ack_code TEXT NOT NULL,
                ack_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goal_usage_events (
                usage_event_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL REFERENCES goals(goal_id) ON DELETE CASCADE,
                source TEXT NOT NULL,
                charged_input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                cache_tokens INTEGER NOT NULL,
                verifier_tokens INTEGER NOT NULL,
                cost_micros INTEGER NOT NULL,
                elapsed_seconds INTEGER NOT NULL,
                created_at INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS goal_transitions (
                transition_id TEXT PRIMARY KEY,
                goal_id TEXT NOT NULL REFERENCES goals(goal_id) ON DELETE CASCADE,
                outer_turn_id TEXT,
                previous_state TEXT NOT NULL,
                next_state TEXT NOT NULL,
                reason_code TEXT NOT NULL,
                evidence_json TEXT,
                created_at INTEGER NOT NULL
             );",
        )?;
        transaction.execute(
            "INSERT INTO goal_meta (key, value) VALUES ('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn migrate_legacy_once(&self, legacy_path: &Path) -> Result<(), GoalStoreError> {
        let connection = self.connection()?;
        let migrated = connection
            .query_row(
                "SELECT value FROM goal_meta WHERE key = ?1",
                [LEGACY_MIGRATION_KEY],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .is_some();
        drop(connection);
        if migrated {
            return Ok(());
        }
        if !legacy_path.exists() {
            let connection = self.connection()?;
            connection.execute(
                "INSERT OR REPLACE INTO goal_meta (key, value) VALUES (?1, 'absent')",
                [LEGACY_MIGRATION_KEY],
            )?;
            return Ok(());
        }

        let contents = fs::read_to_string(legacy_path).map_err(|error| {
            GoalStoreError::Migration(format!("cannot read {}: {error}", legacy_path.display()))
        })?;
        let legacy: LegacyGoalDb = serde_json::from_str(&contents).map_err(|error| {
            GoalStoreError::Migration(format!("cannot parse {}: {error}", legacy_path.display()))
        })?;
        validate_legacy_goals(&legacy)?;

        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (session_id, goal) in legacy.goals {
            let goal_id = GoalId::new();
            let state = legacy_state(&goal);
            transaction.execute(
                "INSERT INTO goals (
                    goal_id, session_id, objective, objective_revision, state,
                    token_budget, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7)",
                params![
                    goal_id.as_str(),
                    session_id,
                    goal.objective,
                    state_json(&state)?,
                    goal.token_budget,
                    goal.created_at,
                    goal.updated_at,
                ],
            )?;
            insert_transition(
                &transaction,
                &goal_id,
                None,
                &state,
                &state,
                "legacy_migrated",
                goal.updated_at,
            )?;
            if goal.tokens_used > 0 || goal.time_used_seconds > 0 {
                transaction.execute(
                    "INSERT INTO goal_usage_events (
                        usage_event_id, goal_id, source, charged_input_tokens,
                        output_tokens, cache_tokens, verifier_tokens, cost_micros,
                        elapsed_seconds, created_at
                     ) VALUES (?1, ?2, 'legacy_migration', ?3, 0, 0, 0, 0, ?4, ?5)",
                    params![
                        format!("legacy:{}", goal.session_id),
                        goal_id.as_str(),
                        goal.tokens_used.max(0),
                        goal.time_used_seconds.max(0),
                        goal.updated_at,
                    ],
                )?;
            }
        }
        transaction.execute(
            "INSERT INTO goal_meta (key, value) VALUES (?1, 'complete')",
            [LEGACY_MIGRATION_KEY],
        )?;
        transaction.commit()?;

        let timestamp = Utc::now().timestamp();
        let stem = legacy_path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("goals_1");
        let backup = legacy_path.with_file_name(format!("{stem}.migrated.{timestamp}.json"));
        fs::rename(legacy_path, &backup).map_err(|error| {
            GoalStoreError::Migration(format!(
                "database commit succeeded but cannot back up {} to {}: {error}",
                legacy_path.display(),
                backup.display()
            ))
        })?;
        Ok(())
    }

    pub(crate) fn recover_in_flight_runs(&self) -> Result<Vec<GoalRecoveryRecord>, GoalStoreError> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recoveries = {
            let mut statement = transaction.prepare(
                "SELECT runs.goal_run_id, runs.goal_id, runs.current_outer_turn_id,
                        goals.session_id
                 FROM goal_runs AS runs
                 JOIN goals ON goals.goal_id = runs.goal_id
                 WHERE runs.in_flight = 1",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let now = Utc::now().timestamp();
        let mut records = Vec::with_capacity(recoveries.len());
        for (run_id, goal_id, outer_turn_id, session_id) in recoveries {
            let goal_id = GoalId::parse(goal_id).map_err(GoalStoreError::Invalid)?;
            let previous = goal_state_by_id(&transaction, &goal_id)?;
            let mut recovered_state = previous.clone();
            if !matches!(previous, GoalState::Complete { .. }) {
                let next = GoalState::Paused {
                    reason: GoalPauseReason::Recovery,
                    message: format!("recovered interrupted goal run {run_id}"),
                };
                transaction.execute(
                    "UPDATE goals SET state = ?1, updated_at = ?2 WHERE goal_id = ?3",
                    params![state_json(&next)?, now, goal_id.as_str()],
                )?;
                insert_transition(
                    &transaction,
                    &goal_id,
                    outer_turn_id.as_deref(),
                    &previous,
                    &next,
                    "recovered",
                    now,
                )?;
                recovered_state = next;
            }
            transaction.execute(
                "UPDATE goal_runs
                 SET status = 'recovered', in_flight = 0, finished_at = ?1
                 WHERE goal_run_id = ?2",
                params![now, run_id],
            )?;
            if let Some(ref outer_turn_id) = outer_turn_id {
                transaction.execute(
                    "UPDATE goal_turns
                     SET status = 'cancelled', finished_at = ?1
                     WHERE outer_turn_id = ?2 AND finished_at IS NULL",
                    params![now, outer_turn_id],
                )?;
            }
            records.push(GoalRecoveryRecord {
                session_id,
                goal_id,
                stale_goal_run_id: GoalRunId::parse(run_id).map_err(GoalStoreError::Invalid)?,
                outer_turn_id: outer_turn_id
                    .map(GoalOuterTurnId::parse)
                    .transpose()
                    .map_err(GoalStoreError::Invalid)?,
                recovered_state,
            });
        }
        transaction.commit()?;
        Ok(records)
    }

    fn connection(&self) -> Result<Connection, GoalStoreError> {
        let connection = Connection::open(&self.path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        Ok(connection)
    }
}

fn load_stored_goal(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<StoredGoal>, GoalStoreError> {
    let row = connection
        .query_row(
            "SELECT goal_id, session_id, objective, objective_revision, state,
                    token_budget, created_at, updated_at
             FROM goals WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?;
    let Some((
        goal_id,
        session_id,
        objective,
        revision,
        state,
        token_budget,
        created_at,
        updated_at,
    )) = row
    else {
        return Ok(None);
    };
    let goal_id = GoalId::parse(goal_id).map_err(GoalStoreError::Invalid)?;
    let current_run = load_current_run(connection, &goal_id)?;
    let last_transition = load_last_transition(connection, &goal_id)?;
    Ok(Some(StoredGoal {
        record: GoalRecord {
            goal_id: goal_id.clone(),
            session_id,
            objective,
            objective_revision: revision,
            state: parse_state(&state)?,
            token_budget,
            usage: usage_totals(connection, &goal_id)?,
            current_run,
            last_transition,
        },
        created_at,
        updated_at,
    }))
}

fn load_current_run(
    connection: &Connection,
    goal_id: &GoalId,
) -> Result<Option<GoalRunSnapshot>, GoalStoreError> {
    let row = connection
        .query_row(
            "SELECT goal_run_id, current_outer_turn_id, origin,
                    continuation_count, in_flight
             FROM goal_runs
             WHERE goal_id = ?1 AND finished_at IS NULL
             ORDER BY started_at DESC LIMIT 1",
            [goal_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            },
        )
        .optional()?;
    row.map(|(run_id, turn_id, origin, continuation_count, in_flight)| {
        Ok(GoalRunSnapshot {
            goal_run_id: GoalRunId::parse(run_id).map_err(GoalStoreError::Invalid)?,
            outer_turn_id: turn_id
                .map(GoalOuterTurnId::parse)
                .transpose()
                .map_err(GoalStoreError::Invalid)?,
            origin: parse_origin(&origin)?,
            continuation_count,
            in_flight,
        })
    })
    .transpose()
}

fn load_last_transition(
    connection: &Connection,
    goal_id: &GoalId,
) -> Result<Option<GoalTransitionSummary>, GoalStoreError> {
    let row = connection
        .query_row(
            "SELECT previous_state, next_state, reason_code
             FROM goal_transitions
             WHERE goal_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1",
            [goal_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    row.map(|(previous, next, reason_code)| {
        Ok(GoalTransitionSummary {
            previous_state: parse_state(&previous)?,
            next_state: parse_state(&next)?,
            reason_code,
        })
    })
    .transpose()
}

fn usage_totals(connection: &Connection, goal_id: &GoalId) -> Result<GoalUsage, GoalStoreError> {
    Ok(connection.query_row(
        "SELECT
            COALESCE(SUM(charged_input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cache_tokens), 0),
            COALESCE(SUM(verifier_tokens), 0),
            COALESCE(SUM(cost_micros), 0),
            COALESCE(SUM(elapsed_seconds), 0)
         FROM goal_usage_events WHERE goal_id = ?1",
        [goal_id.as_str()],
        |row| {
            Ok(GoalUsage {
                charged_input_tokens: row.get(0)?,
                output_tokens: row.get(1)?,
                cache_tokens: row.get(2)?,
                verifier_tokens: row.get(3)?,
                cost_micros: row.get(4)?,
                elapsed_seconds: row.get(5)?,
            })
        },
    )?)
}

fn insert_usage_event(
    transaction: &Transaction<'_>,
    event: &GoalUsageEvent,
) -> Result<(), GoalStoreError> {
    transaction.execute(
        "INSERT OR IGNORE INTO goal_usage_events (
            usage_event_id, goal_id, source, charged_input_tokens,
            output_tokens, cache_tokens, verifier_tokens, cost_micros,
            elapsed_seconds, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            event.usage_event_id,
            event.goal_id.as_str(),
            event.source,
            event.usage.charged_input_tokens.max(0),
            event.usage.output_tokens.max(0),
            event.usage.cache_tokens.max(0),
            event.usage.verifier_tokens.max(0),
            event.usage.cost_micros.max(0),
            event.usage.elapsed_seconds.max(0),
            event.created_at,
        ],
    )?;
    Ok(())
}

fn requested_state_name(state: GoalRequestedState) -> &'static str {
    match state {
        GoalRequestedState::Complete => "complete",
        GoalRequestedState::Blocked => "blocked",
    }
}

fn ack_code(ack: &GoalUpdateAck) -> &'static str {
    match ack {
        GoalUpdateAck::DeferredToTurnEnd { .. } => "deferred_to_turn_end",
        GoalUpdateAck::Rejected { .. } => "rejected",
        GoalUpdateAck::AlreadyPending { .. } => "already_pending",
        GoalUpdateAck::BlockedAgainstInactive { .. } => "blocked_against_inactive",
    }
}

fn turn_status_name(status: GoalTurnStatus) -> &'static str {
    match status {
        GoalTurnStatus::Success => "success",
        GoalTurnStatus::Failed => "failed",
        GoalTurnStatus::Cancelled => "cancelled",
        GoalTurnStatus::ApprovalRequired => "approval_required",
        GoalTurnStatus::BudgetExhausted => "budget_exhausted",
    }
}

fn closed_run_status(state: &GoalState) -> Option<&'static str> {
    match state {
        GoalState::Active => None,
        GoalState::Paused { .. } => Some("paused"),
        GoalState::Blocked { .. } => Some("blocked"),
        GoalState::BudgetLimited => Some("budget_limited"),
        GoalState::Complete { .. } => Some("complete"),
    }
}

fn goal_state_by_id(
    connection: &Connection,
    goal_id: &GoalId,
) -> Result<GoalState, GoalStoreError> {
    let state: String = connection.query_row(
        "SELECT state FROM goals WHERE goal_id = ?1",
        [goal_id.as_str()],
        |row| row.get(0),
    )?;
    parse_state(&state)
}

fn ensure_goal_not_in_flight(
    transaction: &Transaction<'_>,
    goal_id: &str,
    action: &str,
) -> Result<(), GoalStoreError> {
    let in_flight: bool = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM goal_runs WHERE goal_id = ?1 AND in_flight = 1
        )",
        [goal_id],
        |row| row.get(0),
    )?;
    if in_flight {
        return Err(GoalStoreError::Invalid(format!(
            "cannot {action} goal while an outer turn is in flight"
        )));
    }
    Ok(())
}

fn insert_transition(
    transaction: &Transaction<'_>,
    goal_id: &GoalId,
    outer_turn_id: Option<&str>,
    previous: &GoalState,
    next: &GoalState,
    reason_code: &str,
    created_at: i64,
) -> Result<(), GoalStoreError> {
    transaction.execute(
        "INSERT INTO goal_transitions (
            transition_id, goal_id, outer_turn_id, previous_state,
            next_state, reason_code, evidence_json, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7)",
        params![
            format!("transition_{}", uuid::Uuid::now_v7()),
            goal_id.as_str(),
            outer_turn_id,
            state_json(previous)?,
            state_json(next)?,
            reason_code,
            created_at,
        ],
    )?;
    Ok(())
}

fn state_json(state: &GoalState) -> Result<String, GoalStoreError> {
    Ok(serde_json::to_string(state)?)
}

fn parse_state(value: &str) -> Result<GoalState, GoalStoreError> {
    Ok(serde_json::from_str(value)?)
}

fn origin_name(origin: GoalTurnOrigin) -> &'static str {
    match origin {
        GoalTurnOrigin::User => "user",
        GoalTurnOrigin::Resume => "resume",
        GoalTurnOrigin::Continuation => "continuation",
        GoalTurnOrigin::WorkflowNotification => "workflow_notification",
    }
}

fn parse_origin(value: &str) -> Result<GoalTurnOrigin, GoalStoreError> {
    match value {
        "user" => Ok(GoalTurnOrigin::User),
        "resume" => Ok(GoalTurnOrigin::Resume),
        "continuation" => Ok(GoalTurnOrigin::Continuation),
        "workflow_notification" => Ok(GoalTurnOrigin::WorkflowNotification),
        _ => Err(GoalStoreError::Invalid(format!(
            "unknown goal turn origin '{value}'"
        ))),
    }
}

fn validate_legacy_goals(legacy: &LegacyGoalDb) -> Result<(), GoalStoreError> {
    let mut sessions = HashSet::new();
    for (key, goal) in &legacy.goals {
        if key != &goal.session_id {
            return Err(GoalStoreError::Migration(format!(
                "goal key '{key}' does not match session id '{}'",
                goal.session_id
            )));
        }
        if !sessions.insert(goal.session_id.as_str()) {
            return Err(GoalStoreError::Migration(format!(
                "duplicate legacy session id '{}'",
                goal.session_id
            )));
        }
        validate_thread_goal_objective(&goal.objective).map_err(GoalStoreError::Migration)?;
    }
    Ok(())
}

fn legacy_state(goal: &ThreadGoal) -> GoalState {
    match goal.status {
        ThreadGoalStatus::Active => GoalState::Active,
        ThreadGoalStatus::Paused => GoalState::Paused {
            reason: GoalPauseReason::User,
            message: "migrated legacy paused goal".to_string(),
        },
        ThreadGoalStatus::Blocked => GoalState::Blocked {
            blocker: BlockerSummary {
                kind: BlockerKind::UnverifiableRequirement,
                summary: "migrated legacy blocked goal without structured evidence".to_string(),
                fingerprint: format!("legacy-blocked:{}", goal.session_id),
                evidence: Vec::new(),
            },
        },
        ThreadGoalStatus::Stalled => GoalState::Paused {
            reason: GoalPauseReason::NoProgress,
            message: "migrated legacy stalled goal".to_string(),
        },
        ThreadGoalStatus::UsageLimited => GoalState::Paused {
            reason: GoalPauseReason::UsageLimit,
            message: "migrated legacy usage-limited goal".to_string(),
        },
        ThreadGoalStatus::BudgetLimited => GoalState::BudgetLimited,
        ThreadGoalStatus::Complete => GoalState::Complete {
            evidence: Vec::new(),
        },
    }
}

fn orca_home() -> PathBuf {
    if let Ok(value) = std::env::var("ORCA_HOME")
        && !value.trim().is_empty()
    {
        return PathBuf::from(value);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".orca")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::Arc;
    use std::thread;

    use orca_core::goal_runtime::{
        EvidenceItem, GoalPauseReason, GoalRequestedState, GoalRunId, GoalState, GoalTurnOrigin,
        GoalTurnStatus, GoalUpdateAck, GoalUpdateIntent, GoalUsage, IntentId,
    };
    use orca_core::goal_types::{ThreadGoal, ThreadGoalStatus};
    use tempfile::tempdir;

    use super::*;

    fn create_goal(store: &GoalStore, session_id: &str) -> GoalRecord {
        store
            .create_goal(CreateGoalInput {
                session_id: session_id.to_string(),
                objective: "ship runtime-owned goals".to_string(),
                token_budget: Some(100_000),
                now: 100,
            })
            .unwrap()
    }

    #[test]
    fn sqlite_store_creates_and_projects_goal_state() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-1");

        let record = store.get_by_session("session-1").unwrap().unwrap();
        let projection = store.project_thread_goal("session-1").unwrap().unwrap();

        assert_eq!(record.goal_id, goal.goal_id);
        assert_eq!(record.state, GoalState::Active);
        assert_eq!(projection.status, ThreadGoalStatus::Active);
        assert_eq!(projection.tokens_used, 0);
        assert_eq!(store.schema_version().unwrap(), 1);
    }

    #[test]
    fn usage_event_is_idempotent_and_does_not_double_count_cache_tokens() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-usage");
        let event = GoalUsageEvent {
            usage_event_id: "generation-1:model".to_string(),
            goal_id: goal.goal_id,
            source: "model".to_string(),
            usage: GoalUsage {
                charged_input_tokens: 100,
                output_tokens: 20,
                cache_tokens: 80,
                verifier_tokens: 0,
                cost_micros: 12,
                elapsed_seconds: 3,
            },
            created_at: 101,
        };

        let first = store.record_usage_once(event.clone()).unwrap();
        let second = store.record_usage_once(event).unwrap();
        let projected = store.project_thread_goal("session-usage").unwrap().unwrap();

        assert_eq!(first, second);
        assert_eq!(first.charged_tokens(), 120);
        assert_eq!(projected.tokens_used, 120);
        assert_eq!(projected.time_used_seconds, 3);
    }

    #[test]
    fn concurrent_usage_writers_preserve_every_unique_event() {
        let dir = tempdir().unwrap();
        let store = Arc::new(GoalStore::open(dir.path().join("goals.sqlite3")).unwrap());
        let goal = create_goal(&store, "session-concurrent");
        let mut workers = Vec::new();

        for index in 0..8 {
            let store = Arc::clone(&store);
            let goal_id = goal.goal_id.clone();
            workers.push(thread::spawn(move || {
                store
                    .record_usage_once(GoalUsageEvent {
                        usage_event_id: format!("generation-{index}:model"),
                        goal_id,
                        source: "model".to_string(),
                        usage: GoalUsage {
                            charged_input_tokens: 10,
                            output_tokens: 1,
                            ..GoalUsage::default()
                        },
                        created_at: 200 + index,
                    })
                    .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let projection = store
            .project_thread_goal("session-concurrent")
            .unwrap()
            .unwrap();
        assert_eq!(projection.tokens_used, 88);
        assert_eq!(store.usage_event_count(&goal.goal_id).unwrap(), 8);
    }

    #[test]
    fn reopening_recovers_in_flight_run_to_paused_recovery() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals.sqlite3");
        let store = GoalStore::open(&path).unwrap();
        let goal = create_goal(&store, "session-recovery");
        let run_id = GoalRunId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 300,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: orca_core::goal_runtime::GoalOuterTurnId::new(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "turn-provider-1".to_string(),
                started_at: 301,
            })
            .unwrap();
        drop(store);

        let reopened = GoalStore::open(path).unwrap();
        reopened.recover_in_flight_runs().unwrap();
        let recovered = reopened
            .get_by_session("session-recovery")
            .unwrap()
            .unwrap();

        assert!(matches!(
            recovered.state,
            GoalState::Paused {
                reason: GoalPauseReason::Recovery,
                ..
            }
        ));
        assert_eq!(reopened.in_flight_run_count().unwrap(), 0);
        assert!(reopened.transition_count(&goal.goal_id).unwrap() >= 2);
    }

    #[test]
    fn opening_a_reader_does_not_recover_a_live_in_flight_run() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("goals.sqlite3");
        let owner = GoalStore::open(&path).unwrap();
        let goal = create_goal(&owner, "session-live-owner");
        let run_id = GoalRunId::new();
        owner
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 400,
            })
            .unwrap();
        owner
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id,
                goal_run_id: run_id,
                outer_turn_id: GoalOuterTurnId::new(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "live-provider-turn".to_string(),
                started_at: 401,
            })
            .unwrap();

        let reader = GoalStore::open(&path).unwrap();
        let record = reader
            .get_by_session("session-live-owner")
            .unwrap()
            .unwrap();

        assert_eq!(record.state, GoalState::Active);
        assert!(record.current_run.as_ref().is_some_and(|run| run.in_flight));
        assert_eq!(reader.in_flight_run_count().unwrap(), 1);
    }

    #[test]
    fn detached_controls_cannot_mutate_an_in_flight_goal() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-detached-control");
        let run_id = GoalRunId::new();
        let outer_turn_id = GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 500,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id,
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "detached-control-turn".to_string(),
                started_at: 501,
            })
            .unwrap();

        assert!(matches!(
            store.edit_goal(
                "session-detached-control",
                "unsafe detached edit",
                None,
                502,
            ),
            Err(GoalStoreError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            store.resume_into(
                "session-detached-control",
                "session-detached-resume",
                503,
            ),
            Err(GoalStoreError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            store.clear_goal("session-detached-control"),
            Err(GoalStoreError::Invalid(message)) if message.contains("in flight")
        ));
        assert_eq!(
            store.outer_turn_status(&outer_turn_id).unwrap().as_deref(),
            Some("in_flight")
        );
        assert_eq!(store.in_flight_run_count().unwrap(), 1);
        assert_eq!(
            store
                .get_by_session("session-detached-control")
                .unwrap()
                .unwrap()
                .objective,
            "ship runtime-owned goals"
        );
    }

    #[test]
    fn legacy_json_migrates_once_and_is_backed_up_after_commit() {
        let dir = tempdir().unwrap();
        let legacy_path = dir.path().join("goals_1.json");
        let db_path = dir.path().join("goals.sqlite3");
        let legacy_goal = ThreadGoal {
            session_id: "legacy-session".to_string(),
            objective: "preserve legacy goal".to_string(),
            status: ThreadGoalStatus::Stalled,
            token_budget: Some(50_000),
            tokens_used: 123,
            time_used_seconds: 45,
            created_at: 10,
            updated_at: 20,
        };
        let mut goals = BTreeMap::new();
        goals.insert(legacy_goal.session_id.clone(), legacy_goal);
        fs::write(
            &legacy_path,
            serde_json::to_vec_pretty(&serde_json::json!({"goals": goals})).unwrap(),
        )
        .unwrap();

        let store = GoalStore::open_with_legacy(&db_path, &legacy_path).unwrap();
        let migrated = store.get_by_session("legacy-session").unwrap().unwrap();

        assert!(matches!(
            migrated.state,
            GoalState::Paused {
                reason: GoalPauseReason::NoProgress,
                ..
            }
        ));
        assert_eq!(migrated.usage.charged_tokens(), 123);
        assert!(!legacy_path.exists());
        let backups = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains("goals_1.migrated")
            })
            .count();
        assert_eq!(backups, 1);

        drop(store);
        let reopened = GoalStore::open_with_legacy(db_path, legacy_path).unwrap();
        assert_eq!(reopened.goal_count().unwrap(), 1);
    }

    #[test]
    fn malformed_legacy_json_is_preserved_and_migration_fails_closed() {
        let dir = tempdir().unwrap();
        let legacy_path = dir.path().join("goals_1.json");
        let db_path = dir.path().join("goals.sqlite3");
        fs::write(&legacy_path, "{not valid JSON").unwrap();

        let error = GoalStore::open_with_legacy(db_path, &legacy_path).unwrap_err();

        assert!(error.to_string().contains("legacy goal migration"));
        assert_eq!(fs::read_to_string(legacy_path).unwrap(), "{not valid JSON");
    }

    #[test]
    fn intent_record_is_idempotent_and_preserves_typed_ack() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-intent");
        let run_id = GoalRunId::new();
        let outer_turn_id = orca_core::goal_runtime::GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 400,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "turn-provider-intent".to_string(),
                started_at: 401,
            })
            .unwrap();
        let intent_id = IntentId::new();
        let intent = GoalUpdateIntent {
            intent_id: intent_id.clone(),
            requested_state: GoalRequestedState::Complete,
            reason: "verified".to_string(),
            evidence: vec![EvidenceItem::observation("focused tests passed")],
            blocker: None,
        };
        let ack = GoalUpdateAck::DeferredToTurnEnd {
            intent_id,
            pending_depth: 1,
        };
        let record = GoalIntentRecord {
            outer_turn_id,
            intent,
            ack: ack.clone(),
            created_at: 402,
        };

        assert_eq!(store.record_intent(record.clone()).unwrap(), ack);
        assert_eq!(store.record_intent(record).unwrap(), ack);
        assert_eq!(store.intent_count().unwrap(), 1);
    }

    #[test]
    fn finishing_outer_turn_commits_usage_and_releases_in_flight_run() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-finish");
        let run_id = GoalRunId::new();
        let outer_turn_id = orca_core::goal_runtime::GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 500,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "turn-provider-finish".to_string(),
                started_at: 501,
            })
            .unwrap();

        let outcome = store
            .finish_outer_turn(FinishOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id.clone(),
                status: GoalTurnStatus::Success,
                tool_count: 4,
                model_response_count: 3,
                gap_fingerprint: Some("roadmap:next-slice".to_string()),
                usage_event: Some(GoalUsageEvent {
                    usage_event_id: "generation-finish:model".to_string(),
                    goal_id: goal.goal_id.clone(),
                    source: "model".to_string(),
                    usage: GoalUsage {
                        charged_input_tokens: 25,
                        output_tokens: 5,
                        elapsed_seconds: 2,
                        ..GoalUsage::default()
                    },
                    created_at: 502,
                }),
                finished_at: 503,
            })
            .unwrap();

        assert!(!outcome.already_finished);
        assert_eq!(outcome.usage.charged_tokens(), 30);
        assert_eq!(store.in_flight_run_count().unwrap(), 0);
        assert_eq!(
            store.outer_turn_status(&outer_turn_id).unwrap().as_deref(),
            Some("success")
        );
    }

    #[test]
    fn verifier_usage_is_charged_once_to_goal_and_outer_turn() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-verifier-usage");
        let run_id = GoalRunId::new();
        let outer_turn_id = GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 510,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "turn-provider-verifier".to_string(),
                started_at: 511,
            })
            .unwrap();
        store
            .finish_outer_turn(FinishOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id.clone(),
                status: GoalTurnStatus::Success,
                tool_count: 1,
                model_response_count: 1,
                gap_fingerprint: None,
                usage_event: None,
                finished_at: 512,
            })
            .unwrap();
        let event = GoalUsageEvent {
            usage_event_id: format!("verifier:{outer_turn_id}:1"),
            goal_id: goal.goal_id.clone(),
            source: "goal_verifier".to_string(),
            usage: GoalUsage {
                verifier_tokens: 17,
                cost_micros: 4,
                elapsed_seconds: 1,
                ..GoalUsage::default()
            },
            created_at: 513,
        };

        let first = store
            .record_verifier_usage_once(&outer_turn_id, event.clone())
            .unwrap();
        let second = store
            .record_verifier_usage_once(&outer_turn_id, event)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(first.verifier_tokens, 17);
        assert_eq!(store.usage_event_count(&goal.goal_id).unwrap(), 1);
        assert_eq!(
            store.outer_turn_verifier_tokens(&outer_turn_id).unwrap(),
            Some(17)
        );
        assert_eq!(
            store.audit_snapshot(&goal.goal_id).unwrap(),
            GoalAuditSnapshot {
                outer_turns: 1,
                intents: 0,
                usage_events: 1,
                verifier_tokens: 17,
                transitions: 1,
                in_flight_runs: 0,
            }
        );
    }

    #[test]
    fn finishing_outer_turn_at_budget_boundary_pauses_continuation() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = store
            .create_goal(CreateGoalInput {
                session_id: "session-budget-boundary".to_string(),
                objective: "stop exactly at the budget".to_string(),
                token_budget: Some(100),
                now: 600,
            })
            .unwrap();
        let run_id = GoalRunId::new();
        let outer_turn_id = orca_core::goal_runtime::GoalOuterTurnId::new();
        store
            .begin_run(BeginGoalRunInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin: GoalTurnOrigin::User,
                started_at: 601,
            })
            .unwrap();
        store
            .begin_outer_turn(BeginOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id.clone(),
                outer_turn_id: outer_turn_id.clone(),
                origin: GoalTurnOrigin::User,
                provider_turn_id: "provider-budget-boundary".to_string(),
                started_at: 602,
            })
            .unwrap();

        store
            .finish_outer_turn(FinishOuterTurnInput {
                goal_id: goal.goal_id.clone(),
                goal_run_id: run_id,
                outer_turn_id: outer_turn_id,
                status: GoalTurnStatus::Success,
                tool_count: 1,
                model_response_count: 1,
                gap_fingerprint: None,
                usage_event: Some(GoalUsageEvent {
                    usage_event_id: "budget-boundary:model".to_string(),
                    goal_id: goal.goal_id.clone(),
                    source: "model".to_string(),
                    usage: GoalUsage {
                        charged_input_tokens: 70,
                        output_tokens: 30,
                        ..GoalUsage::default()
                    },
                    created_at: 603,
                }),
                finished_at: 603,
            })
            .unwrap();

        let record = store
            .get_by_session("session-budget-boundary")
            .unwrap()
            .unwrap();
        assert_eq!(record.state, GoalState::BudgetLimited);
        assert_eq!(record.usage.charged_tokens(), 100);
        assert_eq!(store.in_flight_run_count().unwrap(), 0);
    }

    #[test]
    fn failed_state_transition_rolls_back_without_extra_history() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let goal = create_goal(&store, "session-rollback");
        let before = store.transition_count(&goal.goal_id).unwrap();

        let error = store
            .transition_state(
                &GoalId::new(),
                GoalState::Paused {
                    reason: GoalPauseReason::User,
                    message: "pause".to_string(),
                },
                "user_paused",
                None,
                600,
            )
            .unwrap_err();

        assert!(error.to_string().contains("goal"));
        assert_eq!(store.transition_count(&goal.goal_id).unwrap(), before);
    }
}
