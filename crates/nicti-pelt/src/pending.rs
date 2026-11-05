//! A result being computed on a worker thread, polled from the UI thread.

use std::sync::mpsc;

/// A result being computed on a worker thread. Replacing the `Pending` drops the receiver, so a
/// superseded worker's answer is discarded when it finishes.
pub(crate) struct Pending<T>(mpsc::Receiver<T>);

impl<T: Send + 'static> Pending<T> {
    pub(crate) fn spawn(ctx: &egui::Context, work: impl FnOnce() -> T + Send + 'static) -> Self {
        Self::spawn_waking(ctx, work, |_| true)
    }

    /// Like [`Self::spawn`], but the worker only wakes the UI when `wake(&result)` says the result
    /// is worth a frame (#512). A periodic re-read that came back unchanged has nothing to draw;
    /// the result still sits in the channel for whichever pass polls next, so only use this where
    /// something else (a timer) guarantees that pass.
    pub(crate) fn spawn_waking(
        ctx: &egui::Context,
        work: impl FnOnce() -> T + Send + 'static,
        wake: impl FnOnce(&T) -> bool + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        // Named so a hung worker (e.g. a drive probe stuck on a dead mount) is identifiable in a
        // thread dump.
        // If the thread can't start, the closure (and its sender) is dropped, so the consumer's
        // `poll` reports the worker as gone instead of waiting forever.
        let _ = std::thread::Builder::new()
            .name("nicti-pending-worker".into())
            .spawn(move || {
                // A panic in `work` drops `tx` (the consumer sees `Disconnected`); wake the UI so it
                // notices on this frame rather than whenever the next input event arrives (#740).
                let wake = match nicti_pounce::guard::catch_panic(work) {
                    Ok(value) => {
                        let wake = wake(&value);
                        let _ = tx.send(value);
                        wake
                    }
                    Err(_) => true,
                };
                if wake {
                    crate::repaint_trace::request(&ctx, crate::repaint_trace::Wake::Pending);
                }
            });
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
