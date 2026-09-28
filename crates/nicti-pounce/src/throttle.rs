//! User-set rate limiting for background jobs (CPU threads, disk I/O) -- ADR-0054 evaluated
//! `governor` (a real, maintained, MIT-licensed token-bucket crate) against a hand-rolled bucket
//! and picked hand-rolled: `governor`'s `RateLimiter` is built around "cells per unit time" for
//! request-rate limiting, not "N of these can run concurrently" (a semaphore-shaped problem, which
//! is what "user-set thread/I-O limits" actually means here); reaching for it would mean bending
//! its API to a shape it isn't for, for a data structure that's a dozen lines on its own. See
//! `docs/decisions/jobs.md`'s Options considered for the full comparison.
//!
//! One change from `spikes/crouch::throttle`: `limit` is now an `AtomicUsize`, not a plain field
//! behind `&mut self` -- the CPU lane's worker threads all hold a shared `Arc<Throttle>`, and the
//! activity panel needs to adjust the concurrency cap live (`Pounce::set_cpu_limit`) without
//! taking `&mut` through that shared handle.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// A counting-semaphore-style concurrency limiter: at most `limit` permits may be checked out at
/// once. Used to cap how many background chunks (CPU-bound tile work, disk-I/O-bound import
/// scans) run concurrently, independent of the GPU/`ort` worker's own one-serial-job model --
/// this throttles CPU/disk-side background work, e.g. Scruff/Patrol's import scan
/// (`nicti-lair::pounce_jobs`).
pub struct Throttle {
    in_use: Mutex<usize>,
    limit: AtomicUsize,
    cond: Condvar,
}

pub struct Permit<'a> {
    throttle: &'a Throttle,
}

impl Throttle {
    pub fn new(limit: usize) -> Self {
        assert!(limit > 0, "a zero-permit throttle can never make progress");
        Throttle {
            in_use: Mutex::new(0),
            limit: AtomicUsize::new(limit),
            cond: Condvar::new(),
        }
    }

    /// Blocks until a permit is available. Callers that must never block (a worker loop that also
    /// needs to notice shutdown) should use [`Throttle::try_acquire`] instead.
    pub fn acquire(&self) -> Permit<'_> {
        let mut in_use = self.in_use.lock().unwrap();
        while *in_use >= self.limit.load(Ordering::SeqCst) {
            in_use = self.cond.wait(in_use).unwrap();
        }
        *in_use += 1;
        Permit { throttle: self }
    }

    pub fn try_acquire(&self) -> Option<Permit<'_>> {
        let mut in_use = self.in_use.lock().unwrap();
        if *in_use >= self.limit.load(Ordering::SeqCst) {
            return None;
        }
        *in_use += 1;
        Some(Permit { throttle: self })
    }

    pub fn in_use(&self) -> usize {
        *self.in_use.lock().unwrap()
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::SeqCst)
    }

    /// Adjusts the live concurrency cap. Raising it wakes every thread currently blocked in
    /// [`Throttle::acquire`]; lowering it only takes effect as permits are naturally released --
    /// an already-running chunk is never interrupted, matching this crate's cooperative-
    /// cancellation stance everywhere else.
    pub fn set_limit(&self, limit: usize) {
        assert!(limit > 0, "a zero-permit throttle can never make progress");
        self.limit.store(limit, Ordering::SeqCst);
        self.cond.notify_all();
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut in_use = self.throttle.in_use.lock().unwrap();
        *in_use = in_use.saturating_sub(1);
        self.throttle.cond.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn try_acquire_respects_limit() {
        let throttle = Throttle::new(2);
        let a = throttle.try_acquire();
        let b = throttle.try_acquire();
        let c = throttle.try_acquire();
        assert!(a.is_some());
        assert!(b.is_some());
        assert!(c.is_none(), "third permit should be refused at limit 2");
    }

    #[test]
    fn dropping_a_permit_frees_a_slot() {
        let throttle = Throttle::new(1);
        {
            let _permit = throttle.try_acquire().unwrap();
            assert!(throttle.try_acquire().is_none());
        }
        assert!(throttle.try_acquire().is_some());
    }

    #[test]
    fn acquire_blocks_until_a_permit_frees() {
        let throttle = Arc::new(Throttle::new(1));
        let first = throttle.try_acquire().unwrap();

        let waiter_throttle = throttle.clone();
        let handle = thread::spawn(move || {
            let _permit = waiter_throttle.acquire();
        });

        thread::sleep(Duration::from_millis(20));
        assert!(!handle.is_finished(), "waiter should still be blocked");
        drop(first);
        handle.join().unwrap();
    }

    #[test]
    fn raising_the_limit_live_wakes_a_blocked_acquirer() {
        let throttle = Arc::new(Throttle::new(1));
        let _first = throttle.try_acquire().unwrap();

        let waiter_throttle = throttle.clone();
        let handle = thread::spawn(move || {
            let _permit = waiter_throttle.acquire();
        });

        thread::sleep(Duration::from_millis(20));
        assert!(
            !handle.is_finished(),
            "waiter should still be blocked at limit 1 with the only permit held"
        );
        throttle.set_limit(2);
        handle.join().unwrap();
    }
}
