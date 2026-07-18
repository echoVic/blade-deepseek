use std::collections::HashMap;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;

use orca_core::goal_runtime::{
    GoalGap, GoalId, GoalNextAction, GoalOuterTurnId, GoalRecord, GoalState, GoalTurnOrigin,
    GoalTurnStatus, GoalUpdateAck, GoalUpdateIntent, GoalUsage, GoalVerificationResult,
};
use orca_core::goal_types::ThreadGoal;

use crate::goal_store::{
    BeginGoalRunInput, BeginOuterTurnInput, CreateGoalInput, FinishOuterTurnInput,
    GoalIntentRecord, GoalRecoveryRecord, GoalStore, GoalStoreError, GoalUsageEvent,
};
use crate::goal_tracker::{GoalTracker, GoalTurnResult};

const ACTOR_MAILBOX_CAPACITY: usize = 32;
static GOAL_RUNTIME_LEASES: OnceLock<Mutex<HashMap<PathBuf, Weak<GoalRuntimeLeaseInner>>>> =
    OnceLock::new();

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoalTurnContext {
    pub session_id: String,
    pub goal_id: GoalId,
    pub goal_run_id: orca_core::goal_runtime::GoalRunId,
    pub outer_turn_id: GoalOuterTurnId,
    pub origin: GoalTurnOrigin,
    pub run_started: bool,
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
    OwnerActive { path: String, message: String },
}

impl fmt::Display for GoalActorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => formatter.write_str("goal actor mailbox is closed"),
            Self::Store(error) => write!(formatter, "goal actor store error: {error}"),
            Self::Invalid(error) => formatter.write_str(error),
            Self::OwnerActive { path, message } => {
                write!(
                    formatter,
                    "goal runtime is already owned for {path}: {message}"
                )
            }
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
    pending_recoveries: HashMap<String, Vec<GoalRecoveryRecord>>,
    _runtime_lease: Option<GoalRuntimeLease>,
}

struct GoalRuntimeLease {
    _inner: Arc<GoalRuntimeLeaseInner>,
}

struct GoalRuntimeLeaseInner {
    _file: File,
}

impl GoalRuntimeLease {
    fn acquire(database_path: &Path) -> Result<(Self, bool), GoalActorError> {
        let database_path =
            absolute_path(database_path).map_err(|error| GoalActorError::OwnerActive {
                path: database_path.display().to_string(),
                message: error.to_string(),
            })?;
        let registry = GOAL_RUNTIME_LEASES.get_or_init(|| Mutex::new(HashMap::new()));
        let mut registry = registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(inner) = registry.get(&database_path).and_then(Weak::upgrade) {
            return Ok((Self { _inner: inner }, false));
        }

        let lock_path = database_path.with_extension("runtime.lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|error| GoalActorError::OwnerActive {
                path: lock_path.display().to_string(),
                message: error.to_string(),
            })?;
        try_lock_runtime_file(&file).map_err(|error| GoalActorError::OwnerActive {
            path: lock_path.display().to_string(),
            message: error.to_string(),
        })?;
        let inner = Arc::new(GoalRuntimeLeaseInner { _file: file });
        registry.insert(database_path, Arc::downgrade(&inner));
        Ok((Self { _inner: inner }, true))
    }
}

fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

#[cfg(unix)]
fn try_lock_runtime_file(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn try_lock_runtime_file(_file: &File) -> std::io::Result<()> {
    Ok(())
}

struct ActiveGoalTurn {
    context: GoalTurnContext,
    tracker: GoalTracker,
    pending_pause: Option<PendingGoalPause>,
}

struct PendingGoalPause {
    reason: orca_core::goal_runtime::GoalPauseReason,
    message: String,
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
    TakeRecoveries {
        session_id: String,
        reply: Reply,
    },
    Create {
        input: CreateGoalInput,
        reply: Reply,
    },
    RecordVerifierUsage {
        outer_turn_id: GoalOuterTurnId,
        event: GoalUsageEvent,
        reply: Reply,
    },
    Edit {
        session_id: String,
        objective: String,
        token_budget: Option<i64>,
        at: i64,
        reply: Reply,
    },
    LatestActive {
        reply: Reply,
    },
    ResumeInto {
        source_session_id: String,
        resumed_session_id: String,
        at: i64,
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
    Recoveries(Vec<GoalRecoveryRecord>),
    Created(GoalRecord),
    Usage(GoalUsage),
    Edited(Option<GoalRecord>),
    Latest(Option<ThreadGoal>),
    Turn(GoalTurnContext),
    Ack(GoalUpdateAck),
    Action(GoalNextAction),
}

impl GoalRuntimeHandle {
    pub fn spawn(store: GoalStore) -> (Self, thread::JoinHandle<()>) {
        Self::spawn_with_lease(store, None, Vec::new())
    }

    fn spawn_with_lease(
        store: GoalStore,
        runtime_lease: Option<GoalRuntimeLease>,
        recoveries: Vec<GoalRecoveryRecord>,
    ) -> (Self, thread::JoinHandle<()>) {
        let (sender, receiver) = mpsc::sync_channel(ACTOR_MAILBOX_CAPACITY);
        let mut pending_recoveries = HashMap::<String, Vec<GoalRecoveryRecord>>::new();
        for recovery in recoveries {
            pending_recoveries
                .entry(recovery.session_id.clone())
                .or_default()
                .push(recovery);
        }
        let actor = GoalActor {
            store,
            sender: receiver,
            active: HashMap::new(),
            trackers: HashMap::new(),
            pending_verification: HashMap::new(),
            pending_recoveries,
            _runtime_lease: runtime_lease,
        };
        let join = thread::Builder::new()
            .name("orca-goal-actor".to_string())
            .spawn(move || actor.run())
            .expect("goal actor thread must start");
        (Self { sender }, join)
    }

    pub fn open_default() -> Result<(Self, thread::JoinHandle<()>), GoalActorError> {
        let store = GoalStore::load_default()?;
        let (lease, first_owner_in_process) = GoalRuntimeLease::acquire(store.path())?;
        let recoveries = if first_owner_in_process {
            store.recover_in_flight_runs()?
        } else {
            Vec::new()
        };
        Ok(Self::spawn_with_lease(store, Some(lease), recoveries))
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

    pub fn take_recoveries(
        &self,
        session_id: &str,
    ) -> Result<Vec<GoalRecoveryRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::TakeRecoveries {
            session_id: session_id.to_string(),
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Recoveries(recoveries) => Ok(recoveries),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong recovery reply".to_string(),
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

    pub fn record_verifier_usage_once(
        &self,
        outer_turn_id: &GoalOuterTurnId,
        event: GoalUsageEvent,
    ) -> Result<GoalUsage, GoalActorError> {
        self.request(|reply| GoalActorCommand::RecordVerifierUsage {
            outer_turn_id: outer_turn_id.clone(),
            event,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Usage(usage) => Ok(usage),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong verifier usage reply".to_string(),
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

    pub fn edit(
        &self,
        session_id: &str,
        objective: impl Into<String>,
        token_budget: Option<i64>,
        at: i64,
    ) -> Result<Option<GoalRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::Edit {
            session_id: session_id.to_string(),
            objective: objective.into(),
            token_budget,
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Edited(goal) => Ok(goal),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong edit reply".to_string(),
            )),
        })
    }

    pub fn latest_active(&self) -> Result<Option<ThreadGoal>, GoalActorError> {
        self.request(|reply| GoalActorCommand::LatestActive { reply })
            .and_then(|reply| match reply {
                GoalActorReply::Latest(goal) => Ok(goal),
                _ => Err(GoalActorError::Invalid(
                    "goal actor returned wrong latest-goal reply".to_string(),
                )),
            })
    }

    pub fn resume_into(
        &self,
        source_session_id: &str,
        resumed_session_id: &str,
        at: i64,
    ) -> Result<Option<GoalRecord>, GoalActorError> {
        self.request(|reply| GoalActorCommand::ResumeInto {
            source_session_id: source_session_id.to_string(),
            resumed_session_id: resumed_session_id.to_string(),
            at,
            reply,
        })
        .and_then(|reply| match reply {
            GoalActorReply::Record(goal) => Ok(goal),
            _ => Err(GoalActorError::Invalid(
                "goal actor returned wrong resume reply".to_string(),
            )),
        })
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
            GoalActorCommand::TakeRecoveries { session_id, reply } => (
                reply,
                Ok(GoalActorReply::Recoveries(
                    self.pending_recoveries
                        .remove(&session_id)
                        .unwrap_or_default(),
                )),
            ),
            GoalActorCommand::Create { input, reply } => (
                reply,
                self.store
                    .create_goal(input)
                    .map(GoalActorReply::Created)
                    .map_err(Into::into),
            ),
            GoalActorCommand::RecordVerifierUsage {
                outer_turn_id,
                event,
                reply,
            } => (
                reply,
                self.store
                    .record_verifier_usage_once(&outer_turn_id, event)
                    .map(GoalActorReply::Usage)
                    .map_err(Into::into),
            ),
            GoalActorCommand::Edit {
                session_id,
                objective,
                token_budget,
                at,
                reply,
            } => (
                reply,
                self.edit(&session_id, &objective, token_budget, at)
                    .map(GoalActorReply::Edited),
            ),
            GoalActorCommand::LatestActive { reply } => (
                reply,
                self.store
                    .latest_active()
                    .map(GoalActorReply::Latest)
                    .map_err(Into::into),
            ),
            GoalActorCommand::ResumeInto {
                source_session_id,
                resumed_session_id,
                at,
                reply,
            } => (
                reply,
                self.resume_into(&source_session_id, &resumed_session_id, at)
                    .map(GoalActorReply::Record),
            ),
            GoalActorCommand::Clear { session_id, reply } => {
                (reply, self.clear(&session_id).map(|_| GoalActorReply::None))
            }
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

    fn clear(&mut self, session_id: &str) -> Result<(), GoalActorError> {
        self.ensure_no_active_turn(session_id, "clear")?;
        self.active.remove(session_id);
        self.trackers.remove(session_id);
        self.pending_verification.remove(session_id);
        self.store.clear_goal(session_id)?;
        Ok(())
    }

    fn edit(
        &mut self,
        session_id: &str,
        objective: &str,
        token_budget: Option<i64>,
        at: i64,
    ) -> Result<Option<GoalRecord>, GoalActorError> {
        self.ensure_no_active_turn(session_id, "edit")?;
        self.active.remove(session_id);
        self.trackers.remove(session_id);
        self.pending_verification.remove(session_id);
        self.store
            .edit_goal(session_id, objective, token_budget, at)
            .map_err(Into::into)
    }

    fn resume_into(
        &mut self,
        source_session_id: &str,
        resumed_session_id: &str,
        at: i64,
    ) -> Result<Option<GoalRecord>, GoalActorError> {
        self.ensure_no_active_turn(source_session_id, "resume")?;
        self.ensure_no_active_turn(resumed_session_id, "resume")?;
        self.active.remove(source_session_id);
        self.trackers.remove(source_session_id);
        self.pending_verification.remove(source_session_id);
        self.active.remove(resumed_session_id);
        self.trackers.remove(resumed_session_id);
        self.pending_verification.remove(resumed_session_id);
        self.store
            .resume_into(source_session_id, resumed_session_id, at)
            .map_err(Into::into)
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

    fn ensure_no_active_turn(&self, session_id: &str, action: &str) -> Result<(), GoalActorError> {
        if self.active.contains_key(session_id) {
            return Err(GoalActorError::Invalid(format!(
                "cannot {action} goal while an outer turn is in flight"
            )));
        }
        Ok(())
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
        let run_started = record.current_run.is_none();
        let run_id = if run_started {
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
            origin,
            run_started,
        };
        self.active.insert(
            session_id.to_string(),
            ActiveGoalTurn {
                context: context.clone(),
                tracker,
                pending_pause: None,
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
        let requested_pause = active.pending_pause.take();
        let tracker_action = active
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
        let action = if let Some(pause) = requested_pause.as_ref() {
            active.tracker.pause(pause.reason, pause.message.clone())
        } else {
            tracker_action
        };
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
        let ActiveGoalTurn {
            context, tracker, ..
        } = active;
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
                if requested_pause.is_none() {
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
        if let Some(active) = self.active.get_mut(session_id) {
            let next = GoalState::Paused {
                reason,
                message: message.clone(),
            };
            self.store.transition_state_while_turn_in_flight(
                &active.context.goal_id,
                next,
                "paused",
                &active.context.outer_turn_id,
                at,
            )?;
            active.pending_pause = Some(PendingGoalPause {
                reason,
                message: message.clone(),
            });
            active.tracker.pause(reason, message.clone());
            self.pending_verification.remove(session_id);
            return Ok(GoalNextAction::Pause { reason, message });
        }
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
        self.ensure_no_active_turn(session_id, "resume")?;
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
    use orca_core::goal_runtime::{EvidenceItem, GoalPauseReason, GoalRequestedState, IntentId};
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
    fn goal_runtime_lease_child_probe() {
        let Ok(path) = std::env::var("ORCA_TEST_GOAL_RUNTIME_LEASE_PATH") else {
            return;
        };
        assert!(matches!(
            GoalRuntimeLease::acquire(Path::new(&path)),
            Err(GoalActorError::OwnerActive { .. })
        ));
    }

    #[test]
    fn goal_runtime_lease_is_shared_in_process_and_exclusive_across_processes() {
        let dir = tempdir().unwrap();
        let database_path = dir.path().join("goals.sqlite3");
        let store = GoalStore::open(&database_path).unwrap();
        let (first, first_in_process) = GoalRuntimeLease::acquire(store.path()).unwrap();
        let (second, second_in_process) = GoalRuntimeLease::acquire(store.path()).unwrap();
        assert!(first_in_process);
        assert!(!second_in_process);

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "goal_actor::tests::goal_runtime_lease_child_probe",
                "--nocapture",
            ])
            .env("ORCA_TEST_GOAL_RUNTIME_LEASE_PATH", &database_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child lease probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        drop(first);
        let (third, third_in_process) = GoalRuntimeLease::acquire(store.path()).unwrap();
        assert!(
            !third_in_process,
            "second in-process lease still owns the lock"
        );
        drop(second);
        drop(third);
        let (_fourth, fourth_in_process) = GoalRuntimeLease::acquire(store.path()).unwrap();
        assert!(fourth_in_process);
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

    #[test]
    fn actor_records_verifier_usage_against_closed_outer_turn_once() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let inspection_store = store.clone();
        let (handle, join) = GoalRuntimeHandle::spawn(store);
        let goal = create(&handle, "verifier-usage-session");
        let turn = handle
            .begin_outer_turn(
                "verifier-usage-session",
                GoalTurnOrigin::User,
                "provider-verifier-usage",
                1,
            )
            .unwrap();
        handle
            .finish_outer_turn(
                "verifier-usage-session",
                GoalTurnStatus::Success,
                GoalUsage::default(),
                1,
                1,
                None,
                2,
            )
            .unwrap();
        let event = GoalUsageEvent {
            usage_event_id: format!("verifier:{}:1", turn.outer_turn_id),
            goal_id: goal.goal_id,
            source: "goal_verifier".to_string(),
            usage: GoalUsage {
                verifier_tokens: 23,
                ..GoalUsage::default()
            },
            created_at: 3,
        };

        let first = handle
            .record_verifier_usage_once(&turn.outer_turn_id, event.clone())
            .unwrap();
        let second = handle
            .record_verifier_usage_once(&turn.outer_turn_id, event)
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(first.verifier_tokens, 23);
        assert_eq!(
            inspection_store
                .outer_turn_verifier_tokens(&turn.outer_turn_id)
                .unwrap(),
            Some(23)
        );
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn pause_waits_for_active_turn_settlement_then_resume_starts_a_fresh_run() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let inspection_store = store.clone();
        let (handle, join) = GoalRuntimeHandle::spawn(store);
        create(&handle, "pause-resume-session");
        let first = handle
            .begin_outer_turn(
                "pause-resume-session",
                GoalTurnOrigin::User,
                "provider-before-pause",
                1,
            )
            .unwrap();

        handle
            .pause(
                "pause-resume-session",
                GoalPauseReason::User,
                "user paused",
                2,
            )
            .unwrap();
        assert_eq!(
            inspection_store
                .outer_turn_status(&first.outer_turn_id)
                .unwrap()
                .as_deref(),
            Some("in_flight")
        );
        assert!(matches!(
            handle
                .finish_outer_turn(
                    "pause-resume-session",
                    GoalTurnStatus::Cancelled,
                    GoalUsage {
                        charged_input_tokens: 5,
                        output_tokens: 2,
                        ..GoalUsage::default()
                    },
                    0,
                    0,
                    None,
                    3,
                )
                .unwrap(),
            GoalNextAction::Pause {
                reason: GoalPauseReason::User,
                ..
            }
        ));
        handle
            .resume("pause-resume-session", GoalTurnOrigin::Resume, 4)
            .unwrap();
        let resumed = handle
            .begin_outer_turn(
                "pause-resume-session",
                GoalTurnOrigin::Resume,
                "provider-after-resume",
                5,
            )
            .unwrap();

        assert_eq!(
            inspection_store
                .outer_turn_status(&first.outer_turn_id)
                .unwrap()
                .as_deref(),
            Some("cancelled")
        );
        assert_ne!(resumed.goal_run_id, first.goal_run_id);
        assert!(resumed.run_started);
        assert_eq!(resumed.origin, GoalTurnOrigin::Resume);
        handle.shutdown().unwrap();
        join.join().unwrap();
    }

    #[test]
    fn rejected_active_controls_do_not_discard_actor_turn_ownership() {
        let dir = tempdir().unwrap();
        let store = GoalStore::open(dir.path().join("goals.sqlite3")).unwrap();
        let (handle, join) = GoalRuntimeHandle::spawn(store);
        create(&handle, "active-control-session");
        handle
            .begin_outer_turn(
                "active-control-session",
                GoalTurnOrigin::User,
                "active-control-provider-turn",
                1,
            )
            .unwrap();

        assert!(matches!(
            handle.edit(
                "active-control-session",
                "must wait for cancellation",
                None,
                2,
            ),
            Err(GoalActorError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            handle.resume("active-control-session", GoalTurnOrigin::Resume, 3),
            Err(GoalActorError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            handle.clear("active-control-session"),
            Err(GoalActorError::Invalid(message)) if message.contains("in flight")
        ));
        assert!(matches!(
            handle
                .continuation_state("active-control-session")
                .unwrap()
                .unwrap()
                .status,
            GoalContinuationStatus::OuterTurnInFlight
        ));
        assert!(matches!(
            handle
                .finish_outer_turn(
                    "active-control-session",
                    GoalTurnStatus::Cancelled,
                    GoalUsage::default(),
                    0,
                    0,
                    None,
                    4,
                )
                .unwrap(),
            GoalNextAction::Pause {
                reason: GoalPauseReason::Infrastructure,
                ..
            }
        ));

        handle.shutdown().unwrap();
        join.join().unwrap();
    }
}
