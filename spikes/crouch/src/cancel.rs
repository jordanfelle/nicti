//! Cooperative cancellation: a per-job [`CancelToken`] the worker checks between chunks, plus one
//! process-global [`EditingGate`] ("`IS_EDITING`") a slider drag sets so background chunks stop
//! being *started* (not interrupted mid-chunk) while the user's actively dragging -- ADR-0054
//! decision rule #4. Plain atomics, not `tokio_util::sync::CancellationToken`: the scheduler owns
//! exactly one serial GPU/`ort` worker thread (one shared `wgpu::Device`, one `ort::Session` per
//! ADR-0016/0019/0050), so there's no async runtime anywhere else in this design to make a
//! `tokio`-flavored token pay for itself -- see ADR-0054's Options considered.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A per-job cancellation flag. Cloning shares the same underlying flag (like
/// `tokio_util::sync::CancellationToken`, minus the async wake mechanism this design doesn't
/// need).
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Process-global "a slider is currently being dragged" flag. While set, the scheduler refuses to
/// *start* a new background chunk (an already-in-flight chunk still runs to completion, since it
/// can't be interrupted) -- checked once per chunk boundary in `queue::Scheduler::run_next`.
#[derive(Clone, Debug, Default)]
pub struct EditingGate(Arc<AtomicBool>);

impl EditingGate {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    pub fn set_editing(&self, editing: bool) {
        self.0.store(editing, Ordering::SeqCst);
    }

    pub fn is_editing(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_token_starts_uncancelled() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        token.cancel();
        assert!(token.is_cancelled());
    }

    #[test]
    fn cancel_token_clone_shares_state() {
        let a = CancelToken::new();
        let b = a.clone();
        b.cancel();
        assert!(
            a.is_cancelled(),
            "cancelling the clone must cancel the original"
        );
    }

    #[test]
    fn editing_gate_defaults_to_not_editing() {
        let gate = EditingGate::new();
        assert!(!gate.is_editing());
        gate.set_editing(true);
        assert!(gate.is_editing());
        gate.set_editing(false);
        assert!(!gate.is_editing());
    }
}
