use std::collections::HashMap;

use crate::runtime_host::{GenerationAdmissionResult, GenerationFence, OperationHandle};

pub(super) struct ServerActiveTurn {
    thread_id: String,
    operation: OperationHandle,
}

impl ServerActiveTurn {
    pub(super) fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub(super) fn operation(&self) -> &OperationHandle {
        &self.operation
    }
}

#[derive(Default)]
pub(super) struct ServerActiveTurnRegistry {
    turns: HashMap<String, ServerActiveTurn>,
}

impl ServerActiveTurnRegistry {
    pub(super) fn insert(
        &mut self,
        turn_id: String,
        thread_id: String,
        operation: OperationHandle,
    ) {
        self.turns.insert(
            turn_id,
            ServerActiveTurn {
                thread_id,
                operation,
            },
        );
    }

    pub(super) fn get(&self, turn_id: &str) -> Option<&ServerActiveTurn> {
        self.turns.get(turn_id)
    }

    pub(super) fn has_thread(&self, thread_id: &str) -> bool {
        self.turns.values().any(|turn| turn.thread_id == thread_id)
    }

    pub(super) fn accepts_generation(
        &self,
        turn_id: &str,
        thread_id: &str,
        generation: GenerationFence,
    ) -> bool {
        self.turns.get(turn_id).is_some_and(|turn| {
            turn.thread_id == thread_id
                && matches!(
                    turn.operation.admit_generation(generation),
                    Ok(GenerationAdmissionResult::Accepted { .. })
                )
        })
    }

    pub(super) fn prune_finished(&mut self) {
        self.turns
            .retain(|_, turn| turn.operation.completion().try_terminal().is_none());
    }

    #[cfg(test)]
    pub(super) fn wait_all(&mut self) {
        for turn in self.turns.values() {
            let _ = turn.operation.wait();
        }
        self.prune_finished();
    }

    pub(super) fn clear(&mut self) {
        self.turns.clear();
    }
}
