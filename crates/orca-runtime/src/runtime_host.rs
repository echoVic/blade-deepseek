use std::collections::HashMap;
use std::fmt;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

use orca_core::cancel::{CancelToken, OperationId, OperationIdAllocator};
use orca_core::config::RunConfig;
use orca_core::conversation::Message;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::{EventObserver, EventSink};
use orca_core::hook_types::HookEvent;
use orca_mcp::{McpElicitationHandler, McpRegistry};
use tokio::runtime::Builder;
use tokio::sync::mpsc::{self as tokio_mpsc, error::TrySendError};
use tokio::task::JoinHandle;

use crate::background_turn::RuntimeTurnContinuation;
use crate::controller::{ControllerRunOptions, ThreadTurnRequest};
use crate::hooks::HookContext;
use crate::lifecycle::{
    RuntimePermissionRequestHandler, RuntimeTaskKind, RuntimeUserInputHandler, ThreadSteerHandle,
};
use crate::tasks::TaskRegistry;
use crate::thread::RuntimeThread;

pub const HOST_COMMAND_CAPACITY: usize = 16;
pub const THREAD_COMMAND_CAPACITY: usize = 16;

pub trait HostedOperationWriter: io::Write + Send + 'static {
    fn finish_generation(&mut self, commit_terminal: bool) -> io::Result<()>;
}

struct PassthroughHostedOperationWriter<W> {
    writer: W,
}

impl<W> PassthroughHostedOperationWriter<W> {
    fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: io::Write> io::Write for PassthroughHostedOperationWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.writer.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: io::Write + Send + 'static> HostedOperationWriter for PassthroughHostedOperationWriter<W> {
    fn finish_generation(&mut self, _commit_terminal: bool) -> io::Result<()> {
        self.writer.flush()
    }
}

#[derive(Clone, Default)]
pub struct HostedGenerationHandlers {
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    user_input_handler: Option<Arc<dyn RuntimeUserInputHandler + Send + Sync>>,
    mcp_elicitation_handler: Option<Arc<dyn McpElicitationHandler + Send + Sync>>,
}

impl fmt::Debug for HostedGenerationHandlers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HostedGenerationHandlers")
            .field("permission_handler", &self.permission_handler.is_some())
            .field("user_input_handler", &self.user_input_handler.is_some())
            .field(
                "mcp_elicitation_handler",
                &self.mcp_elicitation_handler.is_some(),
            )
            .finish()
    }
}

impl HostedGenerationHandlers {
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
}

type HostedGenerationHandlerFactory =
    dyn Fn(GenerationFence, CancelToken) -> HostedGenerationHandlers + Send + Sync;

pub trait ThreadOperationExecutor: Send + Sync + 'static {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
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
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    user_input_handler: Option<Arc<dyn RuntimeUserInputHandler + Send + Sync>>,
    mcp_elicitation_handler: Option<Arc<dyn McpElicitationHandler + Send + Sync>>,
    event_observer: Option<Arc<dyn EventObserver>>,
    continuation: Option<RuntimeTurnContinuation>,
    resumes_existing_turn: bool,
    task_id: Option<String>,
    generation_handler_factory: Option<Arc<HostedGenerationHandlerFactory>>,
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
            permission_handler: None,
            user_input_handler: None,
            mcp_elicitation_handler: None,
            event_observer: None,
            continuation: None,
            resumes_existing_turn: false,
            task_id: None,
            generation_handler_factory: None,
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

    pub fn with_task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn with_generation_handlers<F>(mut self, factory: F) -> Self
    where
        F: Fn(GenerationFence, CancelToken) -> HostedGenerationHandlers + Send + Sync + 'static,
    {
        self.generation_handler_factory = Some(Arc::new(factory));
        self
    }

    fn legacy_request(&self, generation: &GenerationContext) -> ThreadTurnRequest {
        let mut request = ThreadTurnRequest::new(self.prompt.clone())
            .with_options(self.options)
            .with_session_completed_event(
                self.envelope == HostedOperationEnvelope::Turn && self.emit_session_completed,
            )
            .with_steer_handle(generation.steer_handle.clone());
        if let Some(handler) = generation
            .handlers
            .permission_handler
            .clone()
            .or_else(|| self.permission_handler.clone())
        {
            request = request.with_permission_handler(handler);
        }
        if let Some(handler) = generation
            .handlers
            .user_input_handler
            .clone()
            .or_else(|| self.user_input_handler.clone())
        {
            request = request.with_threaded_user_input_handler(handler);
        }
        if let Some(handler) = generation
            .handlers
            .mcp_elicitation_handler
            .clone()
            .or_else(|| self.mcp_elicitation_handler.clone())
        {
            request = request.with_mcp_elicitation_handler(handler);
        }
        if let Some(observer) = self.event_observer.clone() {
            request = request.with_event_observer(observer);
        }
        if let Some(continuation) = self.continuation.clone() {
            request = request.with_continuation(continuation);
        }
        if self.resumes_existing_turn || generation.resumes_existing_turn {
            request = request.with_existing_turn_prompt();
        }
        request
    }

    fn event_observer(&self) -> Option<Arc<dyn EventObserver>> {
        self.event_observer.clone()
    }

    fn is_resumable(&self) -> bool {
        self.envelope == HostedOperationEnvelope::Turn
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GenerationId(u64);

impl GenerationId {
    pub fn as_u64(self) -> u64 {
        self.0
    }

    fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct GenerationFence {
    operation_id: OperationId,
    generation_id: GenerationId,
}

impl GenerationFence {
    fn initial(operation_id: OperationId) -> Self {
        Self {
            operation_id,
            generation_id: GenerationId(0),
        }
    }

    fn next(self) -> Self {
        Self {
            operation_id: self.operation_id,
            generation_id: self.generation_id.next(),
        }
    }

    pub fn operation_id(self) -> OperationId {
        self.operation_id
    }

    pub fn generation_id(self) -> GenerationId {
        self.generation_id
    }

    #[cfg(test)]
    pub(crate) fn for_test(generation_id: u64) -> Self {
        Self {
            operation_id: OperationIdAllocator::new().allocate(),
            generation_id: GenerationId(generation_id),
        }
    }
}

#[derive(Clone, Debug)]
pub struct GenerationContext {
    fence: GenerationFence,
    steer_handle: ThreadSteerHandle,
    resumes_existing_turn: bool,
    handlers: HostedGenerationHandlers,
    config: RunConfig,
}

impl GenerationContext {
    fn new(
        fence: GenerationFence,
        steer_handle: ThreadSteerHandle,
        resumes_existing_turn: bool,
        handlers: HostedGenerationHandlers,
        config: RunConfig,
    ) -> Self {
        Self {
            fence,
            steer_handle,
            resumes_existing_turn,
            handlers,
            config,
        }
    }

    pub fn fence(&self) -> GenerationFence {
        self.fence
    }

    pub fn resumes_existing_turn(&self) -> bool {
        self.resumes_existing_turn
    }

    pub fn config(&self) -> &RunConfig {
        &self.config
    }

    pub fn drain_steer_inputs(&self) -> Vec<String> {
        self.steer_handle.drain()
    }
}

struct LegacyThreadOperationExecutor;

impl ThreadOperationExecutor for LegacyThreadOperationExecutor {
    fn run_turn(
        &self,
        thread: &mut RuntimeThread,
        request: &HostedTurnRequest,
        generation: &GenerationContext,
        events: &mut EventFactory,
        writer: &mut (dyn io::Write + Send),
        cancel: &CancelToken,
    ) -> io::Result<RunStatus> {
        thread.run_request_with_event_factory_and_cancel(
            generation.config(),
            &request.legacy_request(generation),
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
        generation: GenerationFence,
    },
    AlreadyRequested {
        generation: GenerationFence,
    },
    Stale {
        requested_operation_id: OperationId,
        active: GenerationFence,
    },
    Idle {
        requested_operation_id: OperationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeOperationResult {
    Queued {
        generation: GenerationFence,
    },
    AlreadyQueued {
        generation: GenerationFence,
    },
    NotInterrupted {
        generation: GenerationFence,
    },
    NotResumable {
        generation: GenerationFence,
    },
    Stale {
        requested_operation_id: OperationId,
        active: GenerationFence,
    },
    Idle {
        requested_operation_id: OperationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SteerOperationResult {
    Accepted {
        generation: GenerationFence,
    },
    Rejected {
        requested_operation_id: OperationId,
        active: Option<GenerationFence>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GenerationAdmissionResult {
    Accepted {
        generation: GenerationFence,
    },
    Rejected {
        requested: GenerationFence,
        active: Option<GenerationFence>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenerationPhase {
    Running,
    Interrupted,
    ResumeQueued,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeThreadState {
    Idle,
    Running {
        generation: GenerationFence,
        phase: GenerationPhase,
    },
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct RuntimeThreadSnapshot {
    thread_id: String,
    messages: Vec<Message>,
    active_task_id: Option<String>,
}

impl RuntimeThreadSnapshot {
    fn from_thread(thread: &RuntimeThread) -> Self {
        Self {
            thread_id: thread.thread_id().to_string(),
            messages: thread.session().conversation().messages.clone(),
            active_task_id: thread
                .lifecycle()
                .active_task()
                .map(|task| task.id().to_string()),
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn active_task_id(&self) -> Option<&str> {
        self.active_task_id.as_deref()
    }
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
    initial_generation: GenerationFence,
    thread: RuntimeThreadHandle,
    completion: OperationCompletion,
}

impl OperationHandle {
    pub fn id(&self) -> OperationId {
        self.operation_id
    }

    pub fn initial_generation(&self) -> GenerationFence {
        self.initial_generation
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

    pub fn resume(&self) -> Result<ResumeOperationResult, RuntimeHostError> {
        self.thread.resume_operation(self.operation_id)
    }

    pub fn steer(
        &self,
        input: impl Into<String>,
    ) -> Result<SteerOperationResult, RuntimeHostError> {
        self.thread.steer_operation(self.operation_id, input)
    }

    pub fn admit_generation(
        &self,
        generation: GenerationFence,
    ) -> Result<GenerationAdmissionResult, RuntimeHostError> {
        self.thread.admit_generation(generation)
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
    task_registry: TaskRegistry,
    mcp_registry: McpRegistry,
    command_tx: tokio_mpsc::Sender<ThreadCommand>,
}

impl RuntimeThreadHandle {
    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn startup_warnings(&self) -> &[String] {
        self.startup_warnings.as_slice()
    }

    pub fn task_registry(&self) -> TaskRegistry {
        self.task_registry.clone()
    }

    pub fn mcp_registry(&self) -> McpRegistry {
        self.mcp_registry.clone()
    }

    pub fn start_turn<W>(
        &self,
        request: HostedTurnRequest,
        writer: W,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: io::Write + Send + 'static,
    {
        self.start_turn_inner(
            request,
            Box::new(PassthroughHostedOperationWriter::new(writer)),
            None,
        )
    }

    pub fn start_turn_with_config<W>(
        &self,
        request: HostedTurnRequest,
        writer: W,
        config: RunConfig,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: io::Write + Send + 'static,
    {
        self.start_turn_inner(
            request,
            Box::new(PassthroughHostedOperationWriter::new(writer)),
            Some(config),
        )
    }

    pub fn start_turn_with_output<W>(
        &self,
        request: HostedTurnRequest,
        writer: W,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: HostedOperationWriter,
    {
        self.start_turn_inner(request, Box::new(writer), None)
    }

    pub fn start_turn_with_config_and_output<W>(
        &self,
        request: HostedTurnRequest,
        writer: W,
        config: RunConfig,
    ) -> Result<OperationHandle, RuntimeHostError>
    where
        W: HostedOperationWriter,
    {
        self.start_turn_inner(request, Box::new(writer), Some(config))
    }

    fn start_turn_inner(
        &self,
        request: HostedTurnRequest,
        writer: Box<dyn HostedOperationWriter>,
        config: Option<RunConfig>,
    ) -> Result<OperationHandle, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::StartTurn {
            request: Box::new(request),
            writer,
            config: config.map(Box::new),
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

    pub fn resume_operation(
        &self,
        operation_id: OperationId,
    ) -> Result<ResumeOperationResult, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::ResumeOperation {
            operation_id,
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn steer_operation(
        &self,
        operation_id: OperationId,
        input: impl Into<String>,
    ) -> Result<SteerOperationResult, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::SteerOperation {
            operation_id,
            input: input.into(),
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn admit_generation(
        &self,
        generation: GenerationFence,
    ) -> Result<GenerationAdmissionResult, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::AdmitGeneration {
            generation,
            reply: reply_tx,
        })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn state(&self) -> Result<RuntimeThreadState, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::ReadState { reply: reply_tx })?;
        receive_reply(reply_rx, "runtime thread")?
    }

    pub fn snapshot(&self) -> Result<RuntimeThreadSnapshot, RuntimeHostError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.try_send(ThreadCommand::ReadSnapshot { reply: reply_tx })?;
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
        writer: Box<dyn HostedOperationWriter>,
        config: Option<Box<RunConfig>>,
        reply: SyncSender<Result<OperationHandle, RuntimeHostError>>,
    },
    InterruptOperation {
        operation_id: OperationId,
        reply: SyncSender<Result<InterruptOperationResult, RuntimeHostError>>,
    },
    ResumeOperation {
        operation_id: OperationId,
        reply: SyncSender<Result<ResumeOperationResult, RuntimeHostError>>,
    },
    SteerOperation {
        operation_id: OperationId,
        input: String,
        reply: SyncSender<Result<SteerOperationResult, RuntimeHostError>>,
    },
    AdmitGeneration {
        generation: GenerationFence,
        reply: SyncSender<Result<GenerationAdmissionResult, RuntimeHostError>>,
    },
    ReadState {
        reply: SyncSender<Result<RuntimeThreadState, RuntimeHostError>>,
    },
    ReadSnapshot {
        reply: SyncSender<Result<RuntimeThreadSnapshot, RuntimeHostError>>,
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
                let task_registry = thread.session().task_registry().clone();
                let mcp_registry = thread.session().mcp_registry().clone();
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
                    task_registry,
                    mcp_registry,
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
    operation_ids: OperationIdAllocator,
    active: Option<ActiveOperation>,
}

struct ThreadActorState {
    thread: RuntimeThread,
    events: EventFactory,
}

struct ActiveOperation {
    operation_id: OperationId,
    task_id: Option<String>,
    completion: OperationCompletion,
    request: HostedTurnRequest,
    config: RunConfig,
    steer_handle: ThreadSteerHandle,
    resume_queued: bool,
    generation: ActiveGeneration,
}

struct ActiveGeneration {
    context: GenerationContext,
    cancel: CancelToken,
    join: JoinHandle<OperationTaskResult>,
}

struct OperationTaskResult {
    state: ThreadActorState,
    writer: Box<dyn HostedOperationWriter>,
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
            operation_ids: OperationIdAllocator::new(),
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
                biased;
                command = command_rx.recv() => {
                    match command {
                        Some(ThreadCommand::ShutdownThread { reply }) => {
                            active.generation.cancel.cancel();
                            let result = (&mut active.generation.join).await;
                            self.finish_generation(active, result, false);
                            if let Some(reply) = reply {
                                let _ = reply.send(Ok(()));
                            }
                            break;
                        }
                        Some(command) => {
                            self.handle_running_command(command, &mut active);
                            self.active = Some(active);
                        }
                        None => {
                            active.generation.cancel.cancel();
                            let result = (&mut active.generation.join).await;
                            self.finish_generation(active, result, false);
                            break;
                        }
                    }
                }
                result = &mut active.generation.join => {
                    self.finish_generation(active, result, true);
                }
            }
        }
    }

    fn handle_idle_command(&mut self, command: ThreadCommand) -> bool {
        match command {
            ThreadCommand::StartTurn {
                request,
                writer,
                config,
                reply,
            } => {
                let Some(mut state) = self.state.take() else {
                    let _ = reply.send(Err(RuntimeHostError::ThreadUnavailable));
                    return false;
                };
                let operation_id = self.operation_ids.allocate();
                let initial_generation = GenerationFence::initial(operation_id);
                let completion = OperationCompletion::new();
                let request = *request;
                let config = config
                    .map(|config| *config)
                    .unwrap_or_else(|| self.config.clone());
                if let Some(task_id) = request.task_id.as_deref() {
                    state
                        .thread
                        .lifecycle_mut()
                        .start_task_with_id(RuntimeTaskKind::Agent, task_id);
                }
                let task_id = state
                    .thread
                    .lifecycle()
                    .active_task()
                    .map(|task| task.id().to_string());
                let steer_handle = ThreadSteerHandle::default();
                let generation = self.spawn_generation(
                    state,
                    &request,
                    writer,
                    GenerationContext::new(
                        initial_generation,
                        steer_handle.clone(),
                        request.resumes_existing_turn,
                        HostedGenerationHandlers::default(),
                        config.clone(),
                    ),
                );
                self.active = Some(ActiveOperation {
                    operation_id,
                    task_id,
                    completion: completion.clone(),
                    request,
                    config,
                    steer_handle,
                    resume_queued: false,
                    generation,
                });
                let _ = reply.send(Ok(OperationHandle {
                    operation_id,
                    initial_generation,
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
            ThreadCommand::ResumeOperation {
                operation_id,
                reply,
            } => {
                let _ = reply.send(Ok(ResumeOperationResult::Idle {
                    requested_operation_id: operation_id,
                }));
                false
            }
            ThreadCommand::SteerOperation {
                operation_id,
                reply,
                ..
            } => {
                let _ = reply.send(Ok(SteerOperationResult::Rejected {
                    requested_operation_id: operation_id,
                    active: None,
                }));
                false
            }
            ThreadCommand::AdmitGeneration { generation, reply } => {
                let _ = reply.send(Ok(GenerationAdmissionResult::Rejected {
                    requested: generation,
                    active: None,
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
            ThreadCommand::ReadSnapshot { reply } => {
                let result = self
                    .state
                    .as_ref()
                    .map(|state| RuntimeThreadSnapshot::from_thread(&state.thread))
                    .ok_or(RuntimeHostError::ThreadUnavailable);
                let _ = reply.send(result);
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

    fn handle_running_command(&self, command: ThreadCommand, active: &mut ActiveOperation) {
        let generation = active.generation.context.fence();
        match command {
            ThreadCommand::StartTurn { reply, .. } => {
                let _ = reply.send(Err(RuntimeHostError::OperationActive {
                    operation_id: active.operation_id,
                }));
            }
            ThreadCommand::InterruptOperation {
                operation_id,
                reply,
            } => {
                let result = if operation_id != active.operation_id {
                    InterruptOperationResult::Stale {
                        requested_operation_id: operation_id,
                        active: generation,
                    }
                } else if active.generation.cancel.is_cancelled() {
                    InterruptOperationResult::AlreadyRequested { generation }
                } else {
                    active.generation.cancel.cancel();
                    InterruptOperationResult::Requested { generation }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::ResumeOperation {
                operation_id,
                reply,
            } => {
                let result = if operation_id != active.operation_id {
                    ResumeOperationResult::Stale {
                        requested_operation_id: operation_id,
                        active: generation,
                    }
                } else if !active.request.is_resumable() {
                    ResumeOperationResult::NotResumable { generation }
                } else if !active.generation.cancel.is_cancelled() {
                    ResumeOperationResult::NotInterrupted { generation }
                } else if active.resume_queued {
                    ResumeOperationResult::AlreadyQueued { generation }
                } else {
                    active.resume_queued = true;
                    ResumeOperationResult::Queued { generation }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::SteerOperation {
                operation_id,
                input,
                reply,
            } => {
                let accepts = operation_id == active.operation_id
                    && !active.generation.join.is_finished()
                    && !active.generation.cancel.is_cancelled()
                    && !active.resume_queued;
                let result = if accepts {
                    active.steer_handle.push(input);
                    SteerOperationResult::Accepted { generation }
                } else {
                    SteerOperationResult::Rejected {
                        requested_operation_id: operation_id,
                        active: Some(generation),
                    }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::AdmitGeneration {
                generation: requested,
                reply,
            } => {
                let accepts = requested == generation
                    && !active.generation.join.is_finished()
                    && !active.generation.cancel.is_cancelled()
                    && !active.resume_queued;
                let result = if accepts {
                    GenerationAdmissionResult::Accepted { generation }
                } else {
                    GenerationAdmissionResult::Rejected {
                        requested,
                        active: Some(generation),
                    }
                };
                let _ = reply.send(Ok(result));
            }
            ThreadCommand::ReadState { reply } => {
                let phase = if active.resume_queued {
                    GenerationPhase::ResumeQueued
                } else if active.generation.cancel.is_cancelled() {
                    GenerationPhase::Interrupted
                } else {
                    GenerationPhase::Running
                };
                let _ = reply.send(Ok(RuntimeThreadState::Running { generation, phase }));
            }
            ThreadCommand::ReadSnapshot { reply } => {
                let _ = reply.send(Err(RuntimeHostError::OperationActive {
                    operation_id: active.operation_id,
                }));
            }
            ThreadCommand::ShutdownThread { .. } => unreachable!("shutdown handled by actor loop"),
        }
    }

    fn spawn_generation(
        &self,
        state: ThreadActorState,
        request: &HostedTurnRequest,
        mut writer: Box<dyn HostedOperationWriter>,
        mut context: GenerationContext,
    ) -> ActiveGeneration {
        let executor = Arc::clone(&self.executor);
        let task_request = request.clone();
        let cancel = CancelToken::new();
        if let Some(factory) = request.generation_handler_factory.as_ref() {
            context.handlers = factory(context.fence(), cancel.clone());
        }
        let task_context = context.clone();
        let task_cancel = cancel.clone();
        let join = tokio::task::spawn_blocking(move || {
            let mut state = state;
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                run_hosted_operation(
                    executor.as_ref(),
                    &mut state.thread,
                    &mut state.events,
                    &task_request,
                    &task_context,
                    writer.as_mut(),
                    &task_cancel,
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
            OperationTaskResult {
                state,
                writer,
                outcome,
            }
        });
        ActiveGeneration {
            context,
            cancel,
            join,
        }
    }

    fn finish_generation(
        &mut self,
        mut active: ActiveOperation,
        result: Result<OperationTaskResult, tokio::task::JoinError>,
        allow_resume: bool,
    ) {
        let outcome = match result {
            Ok(mut result) => {
                let replace_generation = allow_resume
                    && active.resume_queued
                    && active.request.is_resumable()
                    && result.outcome == OperationOutcome::Completed(RunStatus::Cancelled);
                if replace_generation {
                    if let Err(error) = result.writer.finish_generation(false) {
                        self.state = Some(result.state);
                        OperationOutcome::ExecutionFailed {
                            kind: error.kind(),
                            message: error.to_string(),
                        }
                    } else {
                        let _ = active.steer_handle.drain();
                        if let Some(task_id) = active.task_id.as_deref() {
                            result
                                .state
                                .thread
                                .lifecycle_mut()
                                .start_task_with_id(RuntimeTaskKind::Agent, task_id);
                        }
                        let context = GenerationContext::new(
                            active.generation.context.fence().next(),
                            active.steer_handle.clone(),
                            true,
                            HostedGenerationHandlers::default(),
                            active.config.clone(),
                        );
                        active.generation = self.spawn_generation(
                            result.state,
                            &active.request,
                            result.writer,
                            context,
                        );
                        active.resume_queued = false;
                        self.active = Some(active);
                        return;
                    }
                } else {
                    let writer_error = result.writer.finish_generation(true).err();
                    self.state = Some(result.state);
                    writer_error.map_or(result.outcome, |error| OperationOutcome::ExecutionFailed {
                        kind: error.kind(),
                        message: error.to_string(),
                    })
                }
            }
            Err(error) => OperationOutcome::Panicked {
                message: error.to_string(),
            },
        };
        let completed = active.completion.complete(OperationTerminal {
            operation_id: active.operation_id,
            outcome,
        });
        debug_assert!(completed, "operation terminal must complete exactly once");
    }
}

fn run_hosted_operation(
    executor: &dyn ThreadOperationExecutor,
    thread: &mut RuntimeThread,
    events: &mut EventFactory,
    request: &HostedTurnRequest,
    generation: &GenerationContext,
    writer: &mut (dyn io::Write + Send),
    cancel: &CancelToken,
) -> io::Result<RunStatus> {
    match request.envelope {
        HostedOperationEnvelope::Turn => {
            executor.run_turn(thread, request, generation, events, writer, cancel)
        }
        HostedOperationEnvelope::HeadlessSession => run_headless_session(
            executor, thread, events, request, generation, writer, cancel,
        ),
    }
}

fn run_headless_session(
    executor: &dyn ThreadOperationExecutor,
    thread: &mut RuntimeThread,
    events: &mut EventFactory,
    request: &HostedTurnRequest,
    generation: &GenerationContext,
    writer: &mut (dyn io::Write + Send),
    cancel: &CancelToken,
) -> io::Result<RunStatus> {
    let config = generation.config();
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

    let status = executor.run_turn(
        thread,
        request,
        generation,
        events,
        sink.writer_mut(),
        cancel,
    )?;

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
