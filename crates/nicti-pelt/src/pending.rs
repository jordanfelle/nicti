//! A result being computed on a worker thread, polled from the UI thread.

use std::sync::mpsc;

/// A result being computed on a worker thread. Replacing the `Pending` drops the receiver, so a
/// superseded worker's answer is discarded when it finishes.
pub(crate) struct Pending<T>(mpsc::Receiver<T>);

impl<T: Send + 'static> Pending<T> {
    pub(crate) fn spawn(ctx: &egui::Context, work: impl FnOnce() -> T + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        // Named so a hung worker (e.g. a drive probe stuck on a dead mount) is identifiable in a
        // thread dump.
        std::thread::Builder::new()
            .name("nicti-pending-worker".into())
            .spawn(move || {
                let _ = tx.send(work());
                crate::repaint_trace::request(&ctx, crate::repaint_trace::Wake::Pending);
            })
            .expect("failed to spawn pending worker");
        Self(rx)
    }

    /// `Ok(Some)` = ready, `Ok(None)` = still running, `Err` = the worker died without a result.
    pub(crate) fn poll(&self) -> Result<Option<T>, mpsc::TryRecvError> {
        match self.0.try_recv() {
            Ok(v) => Ok(Some(v)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(e) => Err(e),
        }
    }
}
