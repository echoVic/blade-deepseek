use std::collections::HashMap;
use std::fmt;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;

use orca_core::goal_runtime::{
    GoalGap, GoalId, GoalNextAction, GoalOuterTurnId, GoalRecord, GoalState, GoalTurnOrigin,
    GoalTurnStatus, GoalUpdateAck, GoalUpdateIntent, GoalUsage, GoalVerificationResult,
};
use orca_core::goal_types::ThreadGoal;

use crate::goal_store::{
    BeginGoalRunInput, BeginOuterTurnInput, CreateGoalInput, FinishOuterTurnInput,
    GoalIntentRecord, GoalStore, GoalStoreError, GoalUsageEvent,
};
use crate::goal_tracker::{GoalTracker, GoalTurnResult};

const ACTOR_MAILBOX_CAPACITY: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTurnContext {
    pub session_id: String,
    pub goal_id: GoalId,
    pub goal_run_id: orca_core::goal_runtime::GoalRunId,
    pub outer_turn_id: GoalOuterTurnId,
}

#[derive(Clone, Debug)]
pub struct GoalRuntimeBinding {
    pub handle: GoalRuntimeHandle,
    pub turn: Option<GoalTurnContext>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GoalContinuationStatus {
    Ready,
    OuterTurnInFlight,
    PendingVerification,
    Inactive,
}

#[derive(Clone, Debug)]
pub struct GoalContinuationSnapshot {
    pub record: GoalRecord,
    pub status: GoalContinuationStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GoalActorError {
    Closed,
    Store(String),
    Invalid(String),
}

impl fmt::Display for GoalActorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("goal actor mailbox is closed"),
            Self::Store(error) => write!(formatter, "goal actor store error: {error}"),
            Self::Invalid(error) => formatter.write_str(error),
        }
    }
}

impl std::error::Error for GoalActorError {}

impl From<GoalStoreError> for GoalActorError {
    fn from(error: GoalStoreError) -> Self {
        Self::Store(error.to_string())
    }
}

#[derive(Clone)]
pub struct GoalRuntimeHandle {
    sender: SyncSender<GoalActorCommand>,
}

impl fmt::Debug for GoalRuntimeHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GoalRuntimeHandle(..)")
    }
}

pub struct GoalActor {
    store: GoalStore,
    sender: Receiver<GoalActorCommand>,
    active: HashMap<String, ActiveGoalTurn>,
    trackers: HashMap<String, GoalTracker>,
    pending_verification: HashMap<String, PendingVerification>,
}

struct ActiveGoalTurn {
    context: GoalTurnContext,
    tracker: GoalTracker,
}

struct PendingVerification {
    context: GoalTurnContext,
    tracker: GoalTracker,
}

enum GoalActorCommand {
    Read {
        session_id: String,
        reply: Reply,
    },
    Project {
        session_id: String,
        reply: Reply,
    },
    ContinuationState {
        session_id: String,
        reply: Reply,
    },
    Create {
        input: CreateGoalInput,
        reply: Reply,
    },
    Clear {
        session_id: String,
        reply: Reply,
    },
    BeginOuterTurn {
        session_id: String,
        origin: GoalTurnOrigin,
        provider_turn_id: String,
        started_at: i64,
        reply: Reply,
    },
    SubmitIntent {
        session_id: String,
        intent: GoalUpdateIntent,
        created_at: i64,
        reply: Reply,
    },
    FinishOuterTurn {
        session_id: String,
        status: GoalTurnStatus,
        usage: GoalUsage,
        tool_count: u32,
        model_response_count: u32,
        gap_fingerprint: Option<String>,
        finished_at: i64,
        reply: Reply,
    },
    Verify {
        session_id: String,
        result: GoalVerificationResult,
        at: i64,
        reply: Reply,
    },
    Pause {
        session_id: String,
        reason: orca_core::goal_runtime::GoalPauseReason,
        message: String,
        at: i64,
        reply: Reply,
    },
    Resume {
        session_id: String,
        origin: GoalTurnOrigin,
        at: i64,
        reply: Reply,
    },
    Shutdown,
}

type Reply = SyncSender<Result<GoalActorReply, GoalActorError>>;

enum GoalActorReply {
    None,
    Record(Option<GoalRecord>),
    Projected(Option<ThreadGoal>),
    Continuation(Option<GoalContinuationSnapshot>),
    Created(GoalRecord),
    Turn(GoalTurnContext),
    Ack(GoalUpdateAck),
    Action(GoalNextAction),
}

impl GoalRuntimeHandle {
    pub fn spawn(store: GoalStore) -> (Self, thread::JoinHandle<()>) {
        let (sender, receiver) = mpsc::sync_channel(ACTOR_MAILBOX_CAPACITY);
        let actor = GoalActor {
            store,
            sender: receiver,
            active: HashMap::new(),
            trackers: HashMap::new(),
            pending_verification: HashMap::new(),
        };
        let join = thread::Builder::new()
            .name("orca-goal-actor".to_string())
            .spawn(move || actor.run())
            .expect("goal actor thread must start");
        (Self { sender }, join)
    }

    pub fn open_default() -> Result<(Self, thread::JoinHandle<()>), GoalActorError> {
        Ok(Self::spawn(GoalStore::load_default()?))
    }

    pub fn read(&self, session_id: &str) -> Result<Option<GoalRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::Read {
            session_id: session_id.to_string(),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Record(record) => Ok(record),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong reply".to_string(),
            )),
        })
    }

    pub fn project_thread_goal(
        &self,
        session_id: &str,
    ) -> Result<Option<ThreadGoal>, GoalActorError> {
        self.request(|reply| GoalActorCommand::Project {
            session_id: session_id.to_string(),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Projected(goal) => Ok(goal),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong projection reply".to_string(),
            )),
        })
    }

    pub fn continuation_state(
        &self,
        session_id: &str,
    ) -> Result<Option<GoalContinuationSnapshot>, GoalActorError> {
        self.request(|reply| GoalActorCommand::ContinuationState {
            session_id: session_id.to_string(),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Continuation(state) => Ok(state),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong continuation reply".to_string(),
            )),
        })
    }

    pub fn create(&self, input: CreateGoalInput) -> Result<GoalRecord, GoalActorError> {
        self.request(|reply| GoalActorCommand::Create { input, reply })
            .and_then(|reply| match reply {
                GoalActorReply::Created(goal) => Ok(goal),
                _ => Err(GoalActorError::Invalid(
                    "goal actor returned wrong create reply".to_string(),
                )),
            })
    }

    pub fn clear(&self, session_id: &str) -> Result<(), GoalActorError> {
        self.request(|reply| GoalActorCommand::Clear {
            session_id: session_id.to_string(),
            reply,
        })
        .map(|_| ())
    }

    pub fn begin_outer_turn(
        &self,
        session_id: &str,
        origin: GoalTurnOrigin,
        provider_turn_id: impl Into<String>,
        started_at: i64,
    ) -> Result<GoalTurnContext, GoalActorError> {
        self.request(|reply| GoalActorCommand::BeginOuterTurn {
            session_id: session_id.to_string(),
            origin,
            provider_turn_id: provider_turn_id.into(),
            started_at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Turn(context) => Ok(context),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong turn reply".to_string(),
            )),
        })
    }

    pub fn submit_intent(
        &self,
        session_id: &str,
        intent: GoalUpdateIntent,
        created_at: i64,
    ) -> Result<GoalUpdateAck, GoalActorError> {
        self.request(|reply| GoalActorCommand::SubmitIntent {
            session_id: session_id.to_string(),
            intent,
            created_at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Ack(ack) => Ok(ack),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong intent reply".to_string(),
            )),
        })
    }

    pub fn finish_outer_turn(
        &self,
        session_id: &str,
        status: GoalTurnStatus,
        usage: GoalUsage,
        tool_count: u32,
        model_response_count: u32,
        gap_fingerprint: Option<String>,
        finished_at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        self.request(|reply| GoalActorCommand::FinishOuterTurn {
            session_id: session_id.to_string(),
            status,
            usage,
            tool_count,
            model_response_count,
            gap_fingerprint,
            finished_at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Action(action) => Ok(action),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong finish reply".to_string(),
            )),
        })
    }

    pub fn verify(
        &self,
        session_id: &str,
        result: GoalVerificationResult,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        self.request(|reply| GoalActorCommand::Verify {
            session_id: session_id.to_string(),
            result,
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Action(action) => Ok(action),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong verifier reply".to_string(),
            )),
        })
    }

    pub fn pause(
        &self,
        session_id: &str,
        reason: orca_core::goal_runtime::GoalPauseReason,
        message: impl Into<String>,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        self.request(|reply| GoalActorCommand::Pause {
            session_id: session_id.to_string(),
            reason,
            message: message.into(),
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Action(action) => Ok(action),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong pause reply".to_string(),
            )),
        })
    }

    pub fn resume(
        &self,
        session_id: &str,
        origin: GoalTurnOrigin,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        self.request(|reply| GoalActorCommand::Resume {
            session_id: session_id.to_string(),
            origin,
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Action(action) => Ok(action),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong resume reply".to_string(),
            )),
        })
    }

    pub fn shutdown(&self) -> Result<(), GoalActorError> {
        self.sender
            .send(GoalActorCommand::Shutdown)
            .map_err(|_| GoalActorError::Closed)
    }

    fn request(
        &self,
        command: impl FnOnce(Reply) -> GoalActorCommand,
    ) -> Result<GoalActorReply, GoalActorError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.sender
            .send(command(reply_tx))
            .map_err(|_| GoalActorError::Closed)?;
        reply_rx.recv().map_err(|_| GoalActorError::Closed)?
    }
}

impl GoalActor {
    fn run(mut self) {
        while let Ok(command) = self.sender.recv() {
            if matches!(command, GoalActorCommand::Shutdown) {
                break;
            }
            self.handle(command);
        }
    }

    fn handle(&mut self, command: GoalActorCommand) {
        let (reply, result) = match command {
            GoalActorCommand::Read { session_id, reply } => {
                (reply, self.read(&session_id).map(GoalActorReply::Record))
            }
            GoalActorCommand::Project { session_id, reply } => (
                reply,
                self.store
                    .project_thread_goal(&session_id)
                    .map(GoalActorReply::Projected)
                    .map_err(Into::into),
            ),
            GoalActorCommand::ContinuationState { session_id, reply } => (
                reply,
                self.continuation_state(&session_id)
                    .map(GoalActorReply::Continuation),
            ),
            GoalActorCommand::Create { input, reply } => (
                reply,
                self.store
                    .create_goal(input)
                    .map(GoalActorReply::Created)
                    .map_err(Into::into),
            ),
            GoalActorCommand::Clear { session_id, reply } => (
                reply,
                self.store
                    .clear_goal(&session_id)
                    .map(|_| GoalActorReply::None)
                    .map_err(Into::into),
            ),
            GoalActorCommand::BeginOuterTurn {
                session_id,
                origin,
                provider_turn_id,
                started_at,
                reply,
            } => (
                reply,
                self.begin_outer_turn(&session_id, origin, provider_turn_id, started_at)
                    .map(GoalActorReply::Turn),
            ),
            GoalActorCommand::SubmitIntent {
                session_id,
                intent,
                created_at,
                reply,
            } => (
                reply,
                self.submit_intent(&session_id, intent, created_at)
                    .map(GoalActorReply::Ack),
            ),
            GoalActorCommand::FinishOuterTurn {
                session_id,
                status,
                usage,
                tool_count,
                model_response_count,
                gap_fingerprint,
                finished_at,
                reply,
            } => (
                reply,
                self.finish_outer_turn(
                    &session_id,
                    status,
                    usage,
                    tool_count,
                    model_response_count,
                    gap_fingerprint,
                    finished_at,
                )
                .map(GoalActorReply::Action),
            ),
            GoalActorCommand::Verify {
                session_id,
                result,
                at,
                reply,
            } => (
                reply,
                self.verify(&session_id, result, at)
                    .map(GoalActorReply::Action),
            ),
            GoalActorCommand::Pause {
                session_id,
                reason,
                message,
                at,
                reply,
            } => (
                reply,
                self.pause(&session_id, reason, message, at)
                    .map(GoalActorReply::Action),
            ),
            GoalActorCommand::Resume {
                session_id,
                origin,
                at,
                reply,
            } => (
                reply,
                self.resume(&session_id, origin, at)
                    .map(GoalActorReply::Action),
            ),
            GoalActorCommand::Shutdown => unreachable!(),
        };
        let _ = reply.send(result);
    }

    fn read(&self, session_id: &str) -> Result<Option<GoalRecord>, GoalActorError> {
        self.store.get_by_session(session_id).map_err(Into::into)
    }

    fn continuation_state(
        &self,
        session_id: &str,
    ) -> Result<Option<GoalContinuationSnapshot>, GoalActorError> {
        let Some(record) = self.store.get_by_session(session_id)? else {
            return Ok(None);
        };
        let status = if self.pending_verification.contains_key(session_id) {
            GoalContinuationStatus::PendingVerification
        } else if self.active.contains_key(session_id) {
            GoalContinuationStatus::OuterTurnInFlight
        } else if record.state.should_continue() {
            GoalContinuationStatus::Ready
        } else {
            GoalContinuationStatus::Inactive
        };
        Ok(Some(GoalContinuationSnapshot { record, status }))
    }

    fn begin_outer_turn(
        &mut self,
        session_id: &str,
        origin: GoalTurnOrigin,
        provider_turn_id: String,
        started_at: i64,
    ) -> Result<GoalTurnContext, GoalActorError> {
        if self.active.contains_key(session_id) {
            return Err(GoalActorError::Invalid(
                "goal already has an active outer turn".to_string(),
            ));
        }
        if self.pending_verification.contains_key(session_id) {
            return Err(GoalActorError::Invalid(
                "goal has a terminal intent pending verification".to_string(),
            ));
        }
        let record = self
            .store
            .get_by_session(session_id)?
            .ok_or_else(|| GoalActorError::Invalid("goal does not exist".to_string()))?;
        if !record.state.should_continue() {
            return Err(GoalActorError::Invalid(format!(
                "goal is not active: {:?}",
                record.state
            )));
        }
        let run_id = record
            .current_run
            .as_ref()
            .filter(|run| !run.in_flight)
            .map(|run| run.goal_run_id.clone())
            .unwrap_or_default();
        let run_id = if record.current_run.is_none() {
            let run_id = run_id;
            self.store.begin_run(BeginGoalRunInput {
                goal_id: record.goal_id.clone(),
                goal_run_id: run_id.clone(),
                origin,
                started_at,
            })?;
            run_id
        } else {
            run_id
        };
        let mut tracker = self
            .trackers
            .remove(session_id)
            .unwrap_or_else(|| GoalTracker::from_record(&record));
        let outer_turn_id = tracker
            .begin_outer_turn(origin)
            .map_err(|error| GoalActorError::Invalid(error.to_string()))?;
        self.store.begin_outer_turn(BeginOuterTurnInput {
            goal_id: record.goal_id.clone(),
            goal_run_id: run_id.clone(),
            outer_turn_id: outer_turn_id.clone(),
            origin,
            provider_turn_id,
            started_at,
        })?;
        let context = GoalTurnContext {
            session_id: session_id.to_string(),
            goal_id: record.goal_id,
            goal_run_id: run_id,
            outer_turn_id,
        };
        self.active.insert(
            session_id.to_string(),
            ActiveGoalTurn {
                context: context.clone(),
                tracker,
            },
        );
        Ok(context)
    }

    fn submit_intent(
        &mut self,
        session_id: &str,
        intent: GoalUpdateIntent,
        created_at: i64,
    ) -> Result<GoalUpdateAck, GoalActorError> {
        let active = self
            .active
            .get_mut(session_id)
            .ok_or_else(|| GoalActorError::Invalid("no active goal outer turn".to_string()))?;
        let ack = active.tracker.submit_terminal_intent(intent.clone());
        if matches!(ack, GoalUpdateAck::DeferredToTurnEnd { .. }) {
            self.store.record_intent(GoalIntentRecord {
                outer_turn_id: active.context.outer_turn_id.clone(),
                intent,
                ack: ack.clone(),
                created_at,
            })?;
        }
        Ok(ack)
    }

    fn finish_outer_turn(
        &mut self,
        session_id: &str,
        status: GoalTurnStatus,
        usage: GoalUsage,
        tool_count: u32,
        model_response_count: u32,
        gap_fingerprint: Option<String>,
        finished_at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        let gap_fingerprint =
            gap_fingerprint.unwrap_or_else(|| "outer_turn:no_structured_progress".to_string());
        let mut active = self
            .active
            .remove(session_id)
            .ok_or_else(|| GoalActorError::Invalid("no active goal outer turn".to_string()))?;
        let action = active
            .tracker
            .finish_outer_turn(GoalTurnResult {
                status,
                usage: usage.clone(),
                gaps: vec![GoalGap {
                    summary: "outer turn ended without structured progress evidence".to_string(),
                    fingerprint: gap_fingerprint.clone(),
                    model_fixable: true,
                }],
                evidence_count: 0,
            })
            .map_err(|error| GoalActorError::Invalid(error.to_string()))?;
        self.store.finish_outer_turn(FinishOuterTurnInput {
            goal_id: active.context.goal_id.clone(),
            goal_run_id: active.context.goal_run_id.clone(),
            outer_turn_id: active.context.outer_turn_id.clone(),
            status,
            tool_count,
            model_response_count,
            gap_fingerprint: Some(gap_fingerprint),
            usage_event: Some(GoalUsageEvent {
                usage_event_id: format!("{}:turn", active.context.outer_turn_id),
                goal_id: active.context.goal_id.clone(),
                source: "goal_outer_turn".to_string(),
                usage,
                created_at: finished_at,
            }),
            finished_at,
        })?;
        let ActiveGoalTurn { context, tracker } = active;
        match action.clone() {
            GoalNextAction::Verify { intent: _ } => {
                self.pending_verification.insert(
                    session_id.to_string(),
                    PendingVerification { context, tracker },
                );
            }
            GoalNextAction::Pause {
                reason,
                ref message,
            } => {
                self.store.transition_state(
                    &context.goal_id,
                    GoalState::Paused {
                        reason,
                        message: message.clone(),
                    },
                    "turn_paused",
                    Some(&context.outer_turn_id),
                    finished_at,
                )?;
            }
            GoalNextAction::BudgetLimited => {
                self.store.transition_state(
                    &context.goal_id,
                    GoalState::BudgetLimited,
                    "budget_limited",
                    Some(&context.outer_turn_id),
                    finished_at,
                )?;
            }
            _ => {
                self.trackers.insert(session_id.to_string(), tracker);
            }
        }
        Ok(action)
    }

    fn verify(
        &mut self,
        session_id: &str,
        result: GoalVerificationResult,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        let pending = self
            .pending_verification
            .remove(session_id)
            .ok_or_else(|| {
                GoalActorError::Invalid(
                    "no terminal goal intent is pending verification".to_string(),
                )
            })?;
        let PendingVerification {
            context,
            mut tracker,
        } = pending;
        let action = tracker.apply_verification(result).clone();
        match action.clone() {
            GoalNextAction::Complete { ref evidence } => self.store.transition_state(
                &context.goal_id,
                GoalState::Complete {
                    evidence: evidence.clone(),
                },
                "verified_complete",
                Some(&context.outer_turn_id),
                at,
            )?,
            GoalNextAction::Blocked { ref blocker } => self.store.transition_state(
                &context.goal_id,
                GoalState::Blocked {
                    blocker: blocker.clone(),
                },
                "verified_blocked",
                Some(&context.outer_turn_id),
                at,
            )?,
            GoalNextAction::Pause {
                reason,
                ref message,
            } => self.store.transition_state(
                &context.goal_id,
                GoalState::Paused {
                    reason,
                    message: message.clone(),
                },
                "verification_paused",
                Some(&context.outer_turn_id),
                at,
            )?,
            GoalNextAction::BudgetLimited => self.store.transition_state(
                &context.goal_id,
                GoalState::BudgetLimited,
                "budget_limited",
                Some(&context.outer_turn_id),
                at,
            )?,
            _ => {}
        }
        if !matches!(
            action,
            GoalNextAction::Complete { .. } | GoalNextAction::Blocked { .. }
        ) {
            self.trackers.insert(session_id.to_string(), tracker);
        }
        Ok(action)
    }

    fn pause(
        &mut self,
        session_id: &str,
        reason: orca_core::goal_runtime::GoalPauseReason,
        message: String,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        let record = self
            .store
            .get_by_session(session_id)?
            .ok_or_else(|| GoalActorError::Invalid("goal does not exist".to_string()))?;
        self.store.transition_state(
            &record.goal_id,
            GoalState::Paused {
                reason,
                message: message.clone(),
            },
            "paused",
            None,
            at,
        )?;
        self.active.remove(session_id);
        self.trackers.remove(session_id);
        self.pending_verification.remove(session_id);
        Ok(GoalNextAction::Pause { reason, message })
    }

    fn resume(
        &mut self,
        session_id: &str,
        origin: GoalTurnOrigin,
        at: i64,
    ) -> Result<GoalNextAction, GoalActorError> {
        let record = self
            .store
            .get_by_session(session_id)?
            .ok_or_else(|| GoalActorError::Invalid("goal does not exist".to_string()))?;
        let mut tracker = self
            .trackers
            .remove(session_id)
            .unwrap_or_else(|| GoalTracker::from_record(&record));
        let action = tracker.resume(origin).clone();
        if matches!(action, GoalNextAction::Continue { .. }) {
            self.store
                .transition_state(&record.goal_id, GoalState::Active, "resumed", None, at)?;
            self.trackers.insert(session_id.to_string(), tracker);
        }
        Ok(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::goal_runtime::{EvidenceItem, GoalRequestedState, IntentId};
    use tempfile::tempdir;

    fn create(handle: &GoalRuntimeHandle, session_id: &str) -> GoalRecord {
        handle
            .create(CreateGoalInput {
                session_id: session_id.to_string(),
                objective: "actor-owned goal".to_string(),
                token_budget: None,
                now: 1,
            })
            .unwrap()
    }

    #[test]
    fn mailbox_returns_one_reply_and_owns_goal_lifecycle() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let (handle, join) = GoalRuntimeHandle::spawn(store);
        let goal = create(&handle, "actor-session");
        let _turn = handle
            .begin_outer_turn("actor-session", GoalTurnOrigin::User, "provider-1", 2)
            .unwrap();
        let intent = GoalUpdateIntent {
            intent_id: IntentId::new(),
            requested_state: GoalRequestedState::Complete,
            reason: "verified by tests".to_string(),
            evidence: vec![EvidenceItem::observation("test passed")],
            blocker: None,
        };
        let ack = handle.submit_intent("actor-session", intent, 3).unwrap();
        assert!(matches!(ack, GoalUpdateAck::DeferredToTurnEnd { .. }));
        let action = handle
            .finish_outer_turn(
                "actor-session",
                GoalTurnStatus::Success,
                GoalUsage::default(),
                1,
                1,
                None,
                4,
            )
            .unwrap();
        assert!(matches!(action, GoalNextAction::Verify { .. }));
        let action = handle
            .verify(
                "actor-session",
                GoalVerificationResult::Achieved {
                    evidence: vec![EvidenceItem::observation("verified")],
                },
                5,
            )
            .unwrap();
        assert!(matches!(action, GoalNextAction::Complete { .. }));
        let record = handle.read("actor-session").unwrap().unwrap();
        assert_eq!(record.goal_id, goal.goal_id);
        assert!(matches!(record.state, GoalState::Complete { .. }));
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn duplicate_intent_is_idempotent_and_stale_turn_is_rejected() {
        let dir = tempdir().unwrap();
        let (handle, join) =
            GoalRuntimeHandle::spawn(GoalStore::open(dir.path().join("goals.sqlite3")).unwrap());
        create(&handle, "duplicate-session");
        handle
            .begin_outer_turn("duplicate-session", GoalTurnOrigin::User, "provider-1", 1)
            .unwrap();
        let intent = GoalUpdateIntent {
            intent_id: IntentId::new(),
            requested_state: GoalRequestedState::Complete,
            reason: "done".to_string(),
            evidence: vec![EvidenceItem::observation("proof")],
            blocker: None,
        };
        let first = handle
            .submit_intent("duplicate-session", intent.clone(), 2)
            .unwrap();
        let second = handle
            .submit_intent("duplicate-session", intent, 2)
            .unwrap();
        assert!(matches!(first, GoalUpdateAck::DeferredToTurnEnd { .. }));
        assert!(matches!(second, GoalUpdateAck::AlreadyPending { .. }));
        let error = handle
            .begin_outer_turn(
                "duplicate-session",
                GoalTurnOrigin::Continuation,
                "provider-2",
                3,
            )
            .unwrap_err();
        assert!(error.to_string().contains("active outer turn"));
        handle.shutdown().unwrap();
        join.join().unwrap();
    }
}
