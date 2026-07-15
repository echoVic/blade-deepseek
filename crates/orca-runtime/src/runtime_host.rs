use std::collections::HashMap;
use std::fmt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use orca_core::cancel::{CancelToken, OperationCancellation, OperationId, OperationScope};
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::{EventObserver, EventSink};
use orca_core::hook_types::HookEvent;
use orca_mcp::McpElicitationHandler;
use tokio::runtime::Builder;
use tokio::sync::mpsc::{self as tokio_mpsc, error::TrySendError};
use tokio::task::JoinHandle;

use crate::background_turn::RuntimeTurnContinuation;
use crate::controller::{ControllerRunOptions, ThreadTurnRequest};
use crate::hooks::HookContext;
use crate::lifecycle::{
    RuntimePermissionRequestHandler, RuntimeUserInputHandler, ThreadSteerHandle,
};
use crate::thread::RuntimeThread;

pub const HOST_COMMAND_CAPACITY: usize = 16;
pub const THREAD_COMMAND_CAPACITY: usize = 16;

pub trait ThreadOperationExecutor: Send + Sync + 'static {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        config: &RunConfig,
        request: &HostedTurnRequest,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<RunStatus>;
}

#[derive(Clone)]
pub struct HostedTurnRequest {
    prompt: String,
    options: ControllerRunOptions,
    emit_session_completed: bool,
    envelope: HostedOperationEnvelope,
    steer_handle: Option<ThreadSteerHandle>,
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    user_input_handler: Option<Arc<dyn RuntimeUserInputHandler + Send + Sync>>,
    mcp_elicitation_handler: Option<Arc<dyn McpElicitationHandler + Send + Sync>>,
    event_observer: Option<Arc<dyn EventObserver>>,
    continuation: Option<RuntimeTurnContinuation>,
    resumes_existing_turn: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostedOperationEnvelope {
    Turn,
    HeadlessSession,
}

impl HostedTurnRequest {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            options: ControllerRunOptions::default(),
            emit_session_completed: true,
            envelope: HostedOperationEnvelope::Turn,
            steer_handle: None,
            permission_handler: None,
            user_input_handler: None,
            mcp_elicitation_handler: None,
            event_observer: None,
            continuation: None,
            resumes_existing_turn: false,
        }
    }

    pub fn headless_session(prompt: impl Into<String>) -> Self {
        Self {
            envelope: HostedOperationEnvelope::HeadlessSession,
            emit_session_completed: false,
            ..Self::new(prompt)
        }
    }

    pub fn prompt(&self) -> &str {
        &self.prompt
    }

    pub fn with_options(mut self, options: ControllerRunOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_wait_for_background_workflows(mut self, wait: bool) -> Self {
        self.options.wait_for_background_workflows = wait;
        self
    }

    pub fn with_session_completed_event(mut self, emit: bool) -> Self {
        self.emit_session_completed = emit;
        self
    }

    pub fn with_steer_handle(mut self, handle: ThreadSteerHandle) -> Self {
        self.steer_handle = Some(handle);
        self
    }

    pub fn with_permission_handler(
        mut self,
        handler: Arc<dyn RuntimePermissionRequestHandler + Send + Sync>,
    ) -> Self {
        self.permission_handler = Some(handler);
        self
    }

    pub fn with_user_input_handler(
        mut self,
        handler: Arc<dyn RuntimeUserInputHandler + Send + Sync>,
    ) -> Self {
        self.user_input_handler = Some(handler);
        self
    }

    pub fn with_mcp_elicitation_handler(
        mut self,
        handler: Arc<dyn McpElicitationHandler + Send + Sync>,
    ) -> Self {
        self.mcp_elicitation_handler = Some(handler);
        self
    }

    pub fn with_event_observer(mut self, observer: Arc<dyn EventObserver>) -> Self {
        self.event_observer = Some(observer);
        self
    }

    pub fn with_continuation(mut self, continuation: RuntimeTurnContinuation) -> Self {
        self.continuation = Some(continuation);
        self
    }

    pub fn with_existing_turn_prompt(mut self) -> Self {
        self.resumes_existing_turn = true;
        self
    }

    fn legacy_request(&self) -> ThreadTurnRequest {
        let mut request = ThreadTurnRequest::new(self.prompt.clone())
            .with_options(self.options)
            .with_session_completed_event(
                self.envelope == HostedOperationEnvelope::Turn && self.emit_session_completed,
            );
        if let Some(handle) = self.steer_handle.clone() {
            request = request.with_steer_handle(handle);
        }
        if let Some(handler) = self.permission_handler.clone() {
            request = request.with_permission_handler(handler);
        }
        if let Some(handler) = self.user_input_handler.clone() {
            request = request.with_threaded_user_input_handler(handler);
        }
        if let Some(handler) = self.mcp_elicitation_handler.clone() {
            request = request.with_mcp_elicitation_handler(handler);
        }
        if let Some(observer) = self.event_observer.clone() {
            request = request.with_event_observer(observer);
        }
        if let Some(continuation) = self.continuation.clone() {
            request = request.with_continuation(continuation);
        }
        if self.resumes_existing_turn {
            request = request.with_existing_turn_prompt();
        }
        request
    }

    fn event_observer(&self) -> Option<Arc<dyn EventObserver>> {
        self.event_observer.clone()
    }
}

struct LegacyThreadOperationExecutor;

impl ThreadOperationExecutor for LegacyThreadOperationExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        config: &RunConfig,
        request: &HostedTurnRequest,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<RunStatus> {
        thread.run_request_with_event_factory_and_cancel(
            config,
            &request.legacy_request(),
            writer,
            events,
            cancel.clone(),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeHostError {
    HostUnavailable,
    ThreadUnavailable,
    MailboxFull { owner: &'static str },
    ResponseChannelClosed { owner: &'static str },
    OperationActive { operation_id: OperationId },
    ThreadStartFailed { message: String },
    RuntimeStartFailed { message: String },
    ThreadActorPanicked { thread_id: String, message: String },
    SupervisorPanicked,
}

impl fmt::Display for RuntimeHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostUnavailable => formatter.write_str("runtime host is unavailable"),
            Self::ThreadUnavailable => formatter.write_str("runtime thread is unavailable"),
            Self::MailboxFull { owner } => write!(formatter, "{owner} command mailbox is full"),
            Self::ResponseChannelClosed { owner } => {
                write!(formatter, "{owner} response channel closed")
            }
            Self::OperationActive { operation_id } => {
                write!(formatter, "operation {operation_id:?} is already active")
            }
            Self::ThreadStartFailed { message } => {
                write!(formatter, "failed to start runtime thread: {message}")
            }
            Self::RuntimeStartFailed { message } => {
                write!(formatter, "failed to start runtime host: {message}")
            }
            Self::ThreadActorPanicked { thread_id, message } => {
                write!(
                    formatter,
                    "runtime thread actor {thread_id} panicked: {message}"
                )
            }
            Self::SupervisorPanicked => formatter.write_str("runtime host supervisor panicked"),
        }
    }
}

impl std::error::Error for RuntimeHostError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InterruptOperationResult {
    Requested {
        operation_id: OperationId,
    },
    Stale {
        requested_operation_id: OperationId,
        active_operation_id: OperationId,
    },
    Idle {
        requested_operation_id: OperationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeThreadState {
    Idle,
    Running { operation_id: OperationId },
    Unavailable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationOutcome {
    Completed(RunStatus),
    ExecutionFailed {
        kind: io::ErrorKind,
        message: String,
    },
    Panicked {
        message: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationTerminal {
    operation_id: OperationId,
    outcome: OperationOutcome,
}

impl OperationTerminal {
    pub fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    pub fn outcome(&self) -> &OperationOutcome {
        &self.outcome
    }
}

#[derive(Clone)]
pub struct OperationCompletion {
    state: Arc<OperationCompletionState>,
}

struct OperationCompletionState {
    terminal: Mutex<Option<OperationTerminal>>,
    completed: Condvar,
}

impl OperationCompletion {
    fn new() -> Self {
        Self {
            state: Arc::new(OperationCompletionState {
                terminal: Mutex::new(None),
                completed: Condvar::new(),
            }),
        }
    }

    pub fn try_terminal(&self) -> Option<OperationTerminal> {
        self.lock_terminal().clone()
    }

    pub fn wait(&self) -> OperationTerminal {
        let mut terminal = self.lock_terminal();
        loop {
            if let Some(terminal) = terminal.clone() {
                return terminal;
            }
            terminal = self
                .state
                .completed
                .wait(terminal)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Option<OperationTerminal> {
        let terminal = self.lock_terminal();
        if terminal.is_some() {
            return terminal.clone();
        }
        let (terminal, _) = self
            .state
            .completed
            .wait_timeout_while(terminal, timeout, |terminal| terminal.is_none())
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        terminal.clone()
    }

    fn complete(&self, terminal: OperationTerminal) -> bool {
        let mut current = self.lock_terminal();
        if current.is_some() {
            return false;
        }
        *current = Some(terminal);
        self.state.completed.notify_all();
        true
    }

    fn lock_terminal(&self) -> MutexGuard<'_, Option<OperationTerminal>> {
        self.state
            .terminal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct OperationHandle {
    operation_id: OperationId,
    thread: RuntimeThreadHandle,
    completion: OperationCompletion,
}

impl OperationHandle {
    pub fn id(&self) -> OperationId {
        self.operation_id
    }

    pub fn thread_id(&self) -> &str {
        self.thread.thread_id()
    }

    pub fn completion(&self) -> OperationCompletion {
        self.completion.clone()
    }

    pub fn interrupt(&self) -> Result<InterruptOperationResult, RuntimeHostError> {
        self.thread.interrupt_operation(self.operation_id)
    }

    pub fn wait(&self) -> OperationTerminal {
        self.completion.wait()
    }

    pub fn wait_timeout(&self, timeout: Duration) -> Option<OperationTerminal> {
        self.completion.wait_timeout(timeout)
    }
}

impl fmt::Debug for OperationHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationHandle")
            .field("thread_id", &self.thread_id())
            .field("operation_id", &self.operation_id)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct RuntimeThreadHandle {
    thread_id: String,
    startup_warnings: Arc<Vec<String>>,
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
}

impl RuntimeThreadHandle {
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn startup_warnings(&self) -> &[String] {
        self.startup_warnings.as_slice()
    }

    pub fn start_turn<W>(
        &self,
        request: HostedTurnRequest,
        writer: W,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: io::Write + Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::StartTurn {
            request: Box::new(request),
            writer: Box::new(writer),
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn interrupt_operation(
        &self,
        operation_id: OperationId,
    ) -> Result<InterruptOperationResult, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::InterruptOperation {
            operation_id,
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn state(&self) -> Result<RuntimeThreadState, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::ReadState { reply: reply_tx })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn shutdown(&self) -> Result<(), RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        send_thread_shutdown(
            &self.command_tx,
            ThreadCommand::ShutdownThread {
                reply: Some(reply_tx),
            },
        )?;
        receive_reply(reply_rx, "runtime thread")?
    }

    fn try_send(&self, command: ThreadCommand) -> Result<(), RuntimeHostError> {
        match self.command_tx.try_send(command) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(RuntimeHostError::MailboxFull {
                owner: "runtime thread",
            }),
            Err(TrySendError::Closed(_)) => Err(RuntimeHostError::ThreadUnavailable),
        }
    }
}

impl fmt::Debug for RuntimeThreadHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeThreadHandle")
            .field("thread_id", &self.thread_id)
            .finish_non_exhaustive()
    }
}

pub struct RuntimeHost {
    command_tx: tokio_mpsc::Sender<HostCommand>,
    supervisor: Option<thread::JoinHandle<()>>,
}

impl RuntimeHost {
    pub fn start() -> Result<Self, RuntimeHostError> {
        Self::start_with_executor(Arc::new(LegacyThreadOperationExecutor))
    }

    pub fn start_with_executor(
        executor: Arc<dyn ThreadOperationExecutor>,
    ) -> Result<Self, RuntimeHostError> {
        let (command_tx, command_rx) = tokio_mpsc::channel(HOST_COMMAND_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let supervisor = thread::Builder::new()
            .name("orca-runtime-host".to_string())
            .spawn(move || {
                let runtime = Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("orca-runtime-worker")
                    .build()
                    .map_err(|error| RuntimeHostError::RuntimeStartFailed {
                        message: error.to_string(),
                    });
                match runtime {
                    Ok(runtime) => {
                        let _ = ready_tx.send(Ok(()));
                        runtime.block_on(run_host_supervisor(command_rx, executor));
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error));
                    }
                }
            })
            .map_err(|error| RuntimeHostError::RuntimeStartFailed {
                message: error.to_string(),
            })?;

        match receive_reply(ready_rx, "runtime host") {
            Ok(Ok(())) => Ok(Self {
                command_tx,
                supervisor: Some(supervisor),
            }),
            Ok(Err(error)) | Err(error) => {
                let _ = supervisor.join();
                Err(error)
            }
        }
    }

    pub fn start_thread(
        &self,
        config: RunConfig,
        title: impl Into<String>,
    ) -> Result<RuntimeThreadHandle, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        match self.command_tx.try_send(HostCommand::StartThread {
            config: Box::new(config),
            title: title.into(),
            reply: reply_tx,
        }) {
            Ok(()) => receive_reply(reply_rx, "runtime host")?,
            Err(TrySendError::Full(_)) => Err(RuntimeHostError::MailboxFull {
                owner: "runtime host",
            }),
            Err(TrySendError::Closed(_)) => Err(RuntimeHostError::HostUnavailable),
        }
    }

    pub fn shutdown(mut self) -> Result<(), RuntimeHostError> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&mut self) -> Result<(), RuntimeHostError> {
        let Some(supervisor) = self.supervisor.take() else {
            return Ok(());
        };
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        let send_result =
            send_host_shutdown(&self.command_tx, HostCommand::Shutdown { reply: reply_tx });
        let shutdown_result = match send_result {
            Ok(()) => receive_reply(reply_rx, "runtime host").and_then(|result| result),
            Err(error) => Err(error),
        };
        let join_result = supervisor
            .join()
            .map_err(|_| RuntimeHostError::SupervisorPanicked);
        shutdown_result.and(join_result)
    }
}

impl Drop for RuntimeHost {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

enum HostCommand {
    StartThread {
        config: Box<RunConfig>,
        title: String,
        reply: SyncSender<Result<RuntimeThreadHandle, RuntimeHostError>>,
    },
    Shutdown {
        reply: SyncSender<Result<(), RuntimeHostError>>,
    },
}

enum ThreadCommand {
    StartTurn {
        request: Box<HostedTurnRequest>,
        writer: Box<dyn io::Write + Send>,
        reply: SyncSender<Result<OperationHandle, RuntimeHostError>>,
    },
    InterruptOperation {
        operation_id: OperationId,
        reply: SyncSender<Result<InterruptOperationResult, RuntimeHostError>>,
    },
    ReadState {
        reply: SyncSender<Result<RuntimeThreadState, RuntimeHostError>>,
    },
    ShutdownThread {
        reply: Option<SyncSender<Result<(), RuntimeHostError>>>,
    },
}

struct ThreadActorEntry {
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
    join: JoinHandle<()>,
}

async fn run_host_supervisor(
    mut command_rx: tokio_mpsc::Receiver<HostCommand>,
    executor: Arc<dyn ThreadOperationExecutor>,
) {
    let mut actors = HashMap::<String, ThreadActorEntry>::new();
    while let Some(command) = command_rx.recv().await {
        match command {
            HostCommand::StartThread {
                config,
                title,
                reply,
            } => {
                let actor_config = (*config).clone();
                let started =
                    tokio::task::spawn_blocking(move || RuntimeThread::start(&config, title)).await;
                let thread = match started {
                    Ok(Ok(thread)) => thread,
                    Ok(Err(error)) => {
                        let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                            message: error.to_string(),
                        }));
                        continue;
                    }
                    Err(error) => {
                        let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                            message: error.to_string(),
                        }));
                        continue;
                    }
                };
                let thread_id = thread.thread_id().to_string();
                let startup_warnings = Arc::new(
                    thread
                        .session()
                        .mcp_registry()
                        .errors()
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>(),
                );
                if actors.contains_key(&thread_id) {
                    let _ = reply.send(Err(RuntimeHostError::ThreadStartFailed {
                        message: format!("duplicate runtime thread id: {thread_id}"),
                    }));
                    continue;
                }
                let (command_tx, actor_rx) = tokio_mpsc::channel(THREAD_COMMAND_CAPACITY);
                let handle = RuntimeThreadHandle {
                    thread_id: thread_id.clone(),
                    startup_warnings,
                    command_tx: command_tx.clone(),
                };
                let actor_handle = handle.clone();
                let actor_executor = Arc::clone(&executor);
                let join = tokio::spawn(async move {
                    ThreadActor::new(thread, actor_config, actor_handle, actor_executor)
                        .run(actor_rx)
                        .await;
                });
                actors.insert(thread_id, ThreadActorEntry { command_tx, join });
                let _ = reply.send(Ok(handle));
            }
            HostCommand::Shutdown { reply } => {
                for actor in actors.values() {
                    let _ = actor
                        .command_tx
                        .send(ThreadCommand::ShutdownThread { reply: None })
                        .await;
                }
                let mut actor_error = None;
                for (thread_id, actor) in actors.drain() {
                    if let Err(error) = actor.join.await
                        && actor_error.is_none()
                    {
                        actor_error = Some(RuntimeHostError::ThreadActorPanicked {
                            thread_id,
                            message: error.to_string(),
                        });
                    }
                }
                let _ = reply.send(actor_error.map_or(Ok(()), Err));
                break;
            }
        }
    }
}

struct ThreadActor {
    state: Option<ThreadActorState>,
    config: RunConfig,
    handle: RuntimeThreadHandle,
    executor: Arc<dyn ThreadOperationExecutor>,
    cancellation: OperationCancellation,
    active: Option<ActiveOperation>,
}

struct ThreadActorState {
    thread: RuntimeThread,
    events: EventFactory,
}

struct ActiveOperation {
    scope: OperationScope,
    completion: OperationCompletion,
    join: JoinHandle<OperationTaskResult>,
}

struct OperationTaskResult {
    state: ThreadActorState,
    outcome: OperationOutcome,
}

impl ThreadActor {
    fn new(
        thread: RuntimeThread,
        config: RunConfig,
        handle: RuntimeThreadHandle,
        executor: Arc<dyn ThreadOperationExecutor>,
    ) -> Self {
        let events = EventFactory::new(thread.thread_id().to_string());
        Self {
            state: Some(ThreadActorState { thread, events }),
            config,
            handle,
            executor,
            cancellation: OperationCancellation::new(),
            active: None,
        }
    }

    async fn run(mut self, mut command_rx: tokio_mpsc::Receiver<ThreadCommand>) {
        loop {
            let Some(mut active) = self.active.take() else {
                let Some(command) = command_rx.recv().await else {
                    break;
                };
                if self.handle_idle_command(command) {
                    break;
                }
                continue;
            };

            tokio::select! {
                result = &mut active.join => {
                    self.finish_operation(active, result);
                }
                command = command_rx.recv() => {
                    match command {
                        Some(ThreadCommand::ShutdownThread { reply }) => {
                            active.scope.cancel();
                            let result = (&mut active.join).await;
                            self.finish_operation(active, result);
                            if let Some(reply) = reply {
                                let _ = reply.send(Ok(()));
                            }
                            break;
                        }
                        Some(command) => {
                            self.handle_running_command(command, &active);
                            self.active = Some(active);
                        }
                        None => {
                            active.scope.cancel();
                            let result = (&mut active.join).await;
                            self.finish_operation(active, result);
                            break;
                        }
                    }
                }
            }
        }
    }

    fn handle_idle_command(&mut self, command: ThreadCommand) -> bool {
        match command {
            ThreadCommand::StartTurn {
                request,
                writer,
                reply,
            } => {
                let Some(state) = self.state.take() else {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                    return false;
                };
                let scope = self.cancellation.start();
                let operation_id = scope.id();
                let completion = OperationCompletion::new();
                let executor = Arc::clone(&self.executor);
                let config = self.config.clone();
                let task_scope = scope.clone();
                let mut writer = writer;
                let join = tokio::task::spawn_blocking(move || {
                    let mut state = state;
                    let outcome = catch_unwind(AssertUnwindSafe(|| {
                        run_hosted_operation(
                            executor.as_ref(),
                            &mut state.thread,
                            &mut state.events,
                            &config,
                            request.as_ref(),
                            writer.as_mut(),
                            task_scope.token(),
                        )
                    }));
                    let outcome = match outcome {
                        Ok(Ok(status)) => OperationOutcome::Completed(status),
                        Ok(Err(error)) => OperationOutcome::ExecutionFailed {
                            kind: error.kind(),
                            message: error.to_string(),
                        },
                        Err(payload) => OperationOutcome::Panicked {
                            message: panic_message(payload),
                        },
                    };
                    OperationTaskResult { state, outcome }
                });
                self.active = Some(ActiveOperation {
                    scope,
                    completion: completion.clone(),
                    join,
                });
                let _ = reply.send(Ok(OperationHandle {
                    operation_id,
                    thread: self.handle.clone(),
                    completion,
                }));
                false
            }
            ThreadCommand::InterruptOperation {
                operation_id,
                reply,
            } => {
                let _ = reply.send(Ok(InterruptOperationResult::Idle {
                    requested_operation_id: operation_id,
                }));
                false
            }
            ThreadCommand::ReadState { reply } => {
                let state = if self.state.is_some() {
                    RuntimeThreadState::Idle
                } else {
                    RuntimeThreadState::Unavailable
                };
                let _ = reply.send(Ok(state));
                false
            }
            ThreadCommand::ShutdownThread { reply } => {
                if let Some(reply) = reply {
                    let _ = reply.send(Ok(()));
                }
                true
            }
        }
    }

    fn handle_running_command(&self, command: ThreadCommand, active: &ActiveOperation) {
        match command {
            ThreadCommand::StartTurn { reply, .. } => {
                let _ = reply.send(Err(RuntimeHostError::OperationActive {
                    operation_id: active.scope.id(),
                }));
            }
            ThreadCommand::InterruptOperation {
                operation_id,
                reply,
            } => {
                let result = if operation_id == active.scope.id() {
                    active.scope.cancel();
                    InterruptOperationResult::Requested { operation_id }
                } else {
                    InterruptOperationResult::Stale {
                        requested_operation_id: operation_id,
                        active_operation_id: active.scope.id(),
                    }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::ReadState { reply } => {
                let _ = reply.send(Ok(RuntimeThreadState::Running {
                    operation_id: active.scope.id(),
                }));
            }
            ThreadCommand::ShutdownThread { .. } => unreachable!("shutdown handled by actor loop"),
        }
    }

    fn finish_operation(
        &mut self,
        active: ActiveOperation,
        result: Result<OperationTaskResult, tokio::task::JoinError>,
    ) {
        let operation_id = active.scope.id();
        let outcome = match result {
            Ok(result) => {
                self.state = Some(result.state);
                result.outcome
            }
            Err(error) => OperationOutcome::Panicked {
                message: error.to_string(),
            },
        };
        let completed = active.completion.complete(OperationTerminal {
            operation_id,
            outcome,
        });
        debug_assert!(completed, "operation terminal must complete exactly once");
    }
}

fn run_hosted_operation(
    executor: &dyn ThreadOperationExecutor,
    thread: &mut RuntimeThread,
    events: &mut EventFactory,
    config: &RunConfig,
    request: &HostedTurnRequest,
    writer: &mut (dyn io::Write + Send),
    cancel: &CancelToken,
) -> io::Result<RunStatus> {
    match request.envelope {
        HostedOperationEnvelope::Turn => {
            executor.run_turn(thread, config, request, events, writer, cancel)
        }
        HostedOperationEnvelope::HeadlessSession => {
            run_headless_session(executor, thread, events, config, request, writer, cancel)
        }
    }
}

fn run_headless_session(
    executor: &dyn ThreadOperationExecutor,
    thread: &mut RuntimeThread,
    events: &mut EventFactory,
    config: &RunConfig,
    request: &HostedTurnRequest,
    writer: &mut (dyn io::Write + Send),
    cancel: &CancelToken,
) -> io::Result<RunStatus> {
    let cwd_path = config.cwd.clone().unwrap_or(std::env::current_dir()?);
    let cwd = cwd_path.display().to_string();
    let mut sink = EventSink::new(writer, config.output_format)
        .with_optional_observer(request.event_observer());
    sink.emit(&events.session_started(
        &cwd,
        config.approval_mode.as_str(),
        config.provider.as_str(),
        config.verifier.as_deref(),
    ))?;
    if let Err(error) = thread.session().hooks().run(
        HookEvent::SessionStart,
        HookContext {
            cwd: &cwd,
            session_status: None,
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        },
    ) {
        sink.emit(&events.error(&format!("session_start hook failed: {error}")))?;
    }

    let status = executor.run_turn(thread, config, request, events, sink.writer_mut(), cancel)?;

    if let Err(error) = thread.session().hooks().run(
        HookEvent::SessionEnd,
        HookContext {
            cwd: &cwd,
            session_status: Some(status.as_str()),
            tool_request: None,
            tool_result: None,
            before_messages: None,
            after_messages: None,
            usage: None,
        },
    ) {
        sink.emit(&events.error(&format!("session_end hook failed: {error}")))?;
    }
    sink.emit(&events.session_completed(status))?;
    Ok(status)
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "operation executor panicked".to_string()
}

fn receive_reply<T>(
    receiver: mpsc::Receiver<T>,
    owner: &'static str,
) -> Result<T, RuntimeHostError> {
    receiver
        .recv()
        .map_err(|_| RuntimeHostError::ResponseChannelClosed { owner })
}

fn send_host_shutdown(
    sender: &tokio_mpsc::Sender<HostCommand>,
    mut command: HostCommand,
) -> Result<(), RuntimeHostError> {
    loop {
        match sender.try_send(command) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                command = returned;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Closed(_)) => return Err(RuntimeHostError::HostUnavailable),
        }
    }
}

fn send_thread_shutdown(
    sender: &tokio_mpsc::Sender<ThreadCommand>,
    mut command: ThreadCommand,
) -> Result<(), RuntimeHostError> {
    loop {
        match sender.try_send(command) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(returned)) => {
                command = returned;
                thread::sleep(Duration::from_millis(1));
            }
            Err(TrySendError::Closed(_)) => return Err(RuntimeHostError::ThreadUnavailable),
        }
    }
}
