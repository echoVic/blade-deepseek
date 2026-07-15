use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

#[derive(Clone, Debug)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct OperationId(u64);

#[derive(Clone, Debug)]
pub struct OperationScope {
    id: OperationId,
    token: CancelToken,
}

impl OperationScope {
    pub fn id(&self) -> OperationId {
        self.id
    }

    pub fn token(&self) -> &CancelToken {
        &self.token
    }

    pub fn cancel(&self) {
        self.token.cancel();
    }
}

#[derive(Clone, Debug)]
pub struct OperationCancellation {
    state: Arc<OperationCancellationState>,
}

#[derive(Debug)]
struct OperationCancellationState {
    next_id: std::sync::atomic::AtomicU64,
    current: Mutex<Option<OperationScope>>,
}

impl OperationCancellation {
    pub fn new() -> Self {
        Self {
            state: Arc::new(OperationCancellationState {
                next_id: std::sync::atomic::AtomicU64::new(1),
                current: Mutex::new(None),
            }),
        }
    }

    pub fn start(&self) -> OperationScope {
        let id = OperationId(self.state.next_id.fetch_add(1, Ordering::Relaxed));
        let scope = OperationScope {
            id,
            token: CancelToken::new(),
        };
        *self.lock_current() = Some(scope.clone());
        scope
    }

    pub fn current_id(&self) -> Option<OperationId> {
        self.lock_current().as_ref().map(OperationScope::id)
    }

    pub fn cancel_current(&self) -> Option<OperationId> {
        let current = self.lock_current();
        let scope = current.as_ref()?;
        scope.cancel();
        Some(scope.id())
    }

    pub fn cancel(&self, id: OperationId) -> bool {
        let current = self.lock_current();
        let Some(scope) = current.as_ref().filter(|scope| scope.id() == id) else {
            return false;
        };
        scope.cancel();
        true
    }

    fn lock_current(&self) -> MutexGuard<'_, Option<OperationScope>> {
        self.state
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Default for OperationCancellation {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_scopes_are_one_shot_and_replaceable() {
        let controller = OperationCancellation::new();
        let first = controller.start();
        let first_id = first.id();

        first.cancel();
        assert!(first.token().is_cancelled());

        let second = controller.start();
        assert_ne!(second.id(), first_id);
        assert!(!second.token().is_cancelled());
        assert!(!controller.cancel(first_id));
        assert_eq!(controller.current_id(), Some(second.id()));

        assert_eq!(controller.cancel_current(), Some(second.id()));
        assert!(second.token().is_cancelled());
    }

    #[test]
    fn cancel_token_lifecycle() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());

        token.cancel();
        assert!(token.is_cancelled());

        token.reset();
        assert!(!token.is_cancelled());
    }

    #[test]
    fn clone_shares_state() {
        let token = CancelToken::new();
        let clone = token.clone();

        token.cancel();
        assert!(clone.is_cancelled());
    }
}
