use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, mpsc};

use serde_json::{Value, json};

use crate::lifecycle::{RuntimeUserInputHandler, RuntimeUserInputRequest};
use crate::protocol::ServerEvent;

use super::{lock_error, write_locked_event};

pub(super) struct PendingUserInputRequest {
    pub(super) sender: mpsc::Sender<Option<String>>,
}

#[derive(Clone, Default)]
pub(super) struct PendingUserInputManager {
    pending: Arc<Mutex<HashMap<String, PendingUserInputRequest>>>,
}

impl PendingUserInputManager {
    pub(super) fn insert(
        &self,
        request_id: String,
        request: PendingUserInputRequest,
    ) -> io::Result<()> {
        let mut pending = self.pending.lock().map_err(lock_error)?;
        if pending.contains_key(&request_id) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("duplicate pending user input request id: {request_id}"),
            ));
        }
        pending.insert(request_id, request);
        Ok(())
    }

    pub(super) fn remove(&self, request_id: &str) -> io::Result<Option<PendingUserInputRequest>> {
        let mut pending = self.pending.lock().map_err(lock_error)?;
        Ok(pending.remove(request_id))
    }
}

pub(super) struct ServerUserInputRequestHandler<W: Write + Send + 'static> {
    writer: Arc<Mutex<W>>,
    pending: PendingUserInputManager,
    event_id: Value,
    thread_id: String,
    turn_id: String,
}

impl<W: Write + Send + 'static> ServerUserInputRequestHandler<W> {
    pub(super) fn new(
        writer: Arc<Mutex<W>>,
        pending: PendingUserInputManager,
        event_id: Value,
        thread_id: String,
        turn_id: String,
    ) -> Self {
        Self {
            writer,
            pending,
            event_id,
            thread_id,
            turn_id,
        }
    }
}

impl<W: Write + Send + 'static> RuntimeUserInputHandler for ServerUserInputRequestHandler<W> {
    fn request_user_input(&self, request: &RuntimeUserInputRequest) -> io::Result<Option<String>> {
        let request_id = format!("user-input-{}-{}", self.turn_id, request.id);
        let (sender, receiver) = mpsc::channel();
        self.pending
            .insert(request_id.clone(), PendingUserInputRequest { sender })?;
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
        receiver
            .recv()
            .map_err(|_| io::Error::other("user input response channel closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                },
            )
            .expect("insert first request");
        assert!(
            manager
                .insert(
                    "user-input-turn-1-ask".to_string(),
                    PendingUserInputRequest {
                        sender: second_sender,
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
}
