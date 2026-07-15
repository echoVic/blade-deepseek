use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use orca_core::cancel::CancelToken;
use serde_json::{Value, json};

use crate::lifecycle::{RuntimeUserInputHandler, RuntimeUserInputRequest};
use crate::protocol::ServerEvent;
use crate::runtime_host::GenerationFence;

use super::{lock_error, write_locked_event};

pub(super) struct PendingUserInputRequest {
    pub(super) sender: mpsc::Sender<Option<String>>,
    pub(super) thread_id: String,
    pub(super) turn_id: String,
    pub(super) generation: GenerationFence,
}

impl PendingUserInputRequest {
    pub(super) fn generation_scope(&self) -> (&str, &str, GenerationFence) {
        (&self.thread_id, &self.turn_id, self.generation)
    }
}

#[derive(Default)]
struct PendingUserInputState {
    closed: bool,
    pending: HashMap<String, PendingUserInputRequest>,
}

#[derive(Clone, Default)]
pub(super) struct PendingUserInputManager {
    state: Arc<Mutex<PendingUserInputState>>,
}

impl PendingUserInputManager {
    pub(super) fn insert(
        &self,
        request_id: String,
        request: PendingUserInputRequest,
    ) -> io::Result<()> {
        let mut state = self.state.lock().map_err(lock_error)?;
        if state.closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "server user input request manager is closed",
            ));
        }
        if state.pending.contains_key(&request_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending user input request id: {request_id}"),
            ));
        }
        state.pending.insert(request_id, request);
        Ok(())
    }

    pub(super) fn remove(&self, request_id: &str) -> io::Result<Option<PendingUserInputRequest>> {
        let mut state = self.state.lock().map_err(lock_error)?;
        Ok(state.pending.remove(request_id))
    }

    pub(super) fn close(&self) -> io::Result<()> {
        let pending = {
            let mut state = self.state.lock().map_err(lock_error)?;
            state.closed = true;
            std::mem::take(&mut state.pending)
        };
        for request in pending.into_values() {
            let _ = request.sender.send(None);
        }
        Ok(())
    }
}

pub(super) struct ServerUserInputRequestHandler<W: Write + Send + 'static> {
    writer: Arc<Mutex<W>>,
    pending: PendingUserInputManager,
    event_id: Value,
    thread_id: String,
    turn_id: String,
    generation: GenerationFence,
    cancel: CancelToken,
}

impl<W: Write + Send + 'static> ServerUserInputRequestHandler<W> {
    pub(super) fn new(
        writer: Arc<Mutex<W>>,
        pending: PendingUserInputManager,
        event_id: Value,
        thread_id: String,
        turn_id: String,
        generation: GenerationFence,
        cancel: CancelToken,
    ) -> Self {
        Self {
            writer,
            pending,
            event_id,
            thread_id,
            turn_id,
            generation,
            cancel,
        }
    }
}

impl<W: Write + Send + 'static> RuntimeUserInputHandler for ServerUserInputRequestHandler<W> {
    fn request_user_input(&self, request: &RuntimeUserInputRequest) -> io::Result<Option<String>> {
        let request_id = super::generation_scoped_id(
            format!("user-input-{}-{}", self.turn_id, request.id),
            self.generation,
        );
        let (sender, receiver) = mpsc::channel();
        self.pending.insert(
            request_id.clone(),
            PendingUserInputRequest {
                sender,
                thread_id: self.thread_id.clone(),
                turn_id: self.turn_id.clone(),
                generation: self.generation,
            },
        )?;
        if let Err(error) = write_locked_event(
            &self.writer,
            &self.event_id,
            ServerEvent::UserInputRequest {
                request_id: json!(request_id.clone()),
                thread_id: json!(self.thread_id),
                turn_id: json!(self.turn_id),
                question: json!(request.question),
                choices: json!(request.choices),
            },
        ) {
            let _ = self.pending.remove(&request_id);
            return Err(error);
        }
        loop {
            if self.cancel.is_cancelled() {
                let _ = self.pending.remove(&request_id);
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "user input request cancelled",
                ));
            }
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(answer) => return Ok(answer),
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::other("user input response channel closed"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn generation(id: u64) -> GenerationFence {
        GenerationFence::for_test(id)
    }

    #[test]
    fn pending_user_input_manager_rejects_duplicate_request_id_without_overwriting() {
        let manager = PendingUserInputManager::default();
        let (first_sender, first_receiver) = mpsc::channel();
        let (second_sender, _second_receiver) = mpsc::channel();

        manager
            .insert(
                "user-input-turn-1-ask".to_string(),
                PendingUserInputRequest {
                    sender: first_sender,
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    generation: generation(0),
                },
            )
            .expect("insert first request");
        assert!(
            manager
                .insert(
                    "user-input-turn-1-ask".to_string(),
                    PendingUserInputRequest {
                        sender: second_sender,
                        thread_id: "thread-2".to_string(),
                        turn_id: "turn-2".to_string(),
                        generation: generation(0),
                    },
                )
                .is_err(),
            "duplicate pending request ids must not replace the original waiter"
        );

        let pending = manager
            .remove("user-input-turn-1-ask")
            .expect("remove pending")
            .expect("original request still pending");
        pending
            .sender
            .send(Some("first".to_string()))
            .expect("original sender still active");
        assert_eq!(
            first_receiver.recv().expect("first receiver"),
            Some("first".to_string())
        );
    }

    #[test]
    fn closing_user_input_manager_cancels_waiters_and_rejects_late_requests() {
        let manager = PendingUserInputManager::default();
        let (sender, receiver) = mpsc::channel();
        manager
            .insert(
                "user-input-turn-1-ask".to_string(),
                PendingUserInputRequest {
                    sender,
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-1".to_string(),
                    generation: generation(0),
                },
            )
            .expect("insert pending request");

        manager.close().expect("close manager");

        assert_eq!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(100))
                .expect("cancelled response"),
            None
        );
        let (late_sender, _late_receiver) = mpsc::channel();
        let error = manager
            .insert(
                "user-input-turn-2-ask".to_string(),
                PendingUserInputRequest {
                    sender: late_sender,
                    thread_id: "thread-1".to_string(),
                    turn_id: "turn-2".to_string(),
                    generation: generation(0),
                },
            )
            .expect_err("closed manager must reject late requests");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn cancelled_generation_releases_user_input_waiter_and_removes_request() {
        let writer = Arc::new(Mutex::new(Vec::new()));
        let manager = PendingUserInputManager::default();
        let cancel = CancelToken::new();
        let handler = ServerUserInputRequestHandler::new(
            Arc::clone(&writer),
            manager.clone(),
            json!("turn"),
            "thread-1".to_string(),
            "turn-1".to_string(),
            generation(1),
            cancel.clone(),
        );
        let worker = std::thread::spawn(move || {
            handler.request_user_input(&RuntimeUserInputRequest {
                id: "ask".to_string(),
                question: "Continue?".to_string(),
                choices: vec!["yes".to_string(), "no".to_string()],
            })
        });

        wait_for_output(&writer);
        cancel.cancel();

        let error = worker
            .join()
            .expect("user input worker")
            .expect_err("cancelled generation must release waiter");
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(
            manager
                .remove("user-input-turn-1-ask-generation-1")
                .expect("remove pending")
                .is_none()
        );
    }

    fn wait_for_output(writer: &Arc<Mutex<Vec<u8>>>) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !writer.lock().expect("writer").is_empty() {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for event");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
