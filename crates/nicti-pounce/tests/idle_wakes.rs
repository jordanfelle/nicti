//! Idle cost of the worker pool, in a way that holds on every OS and host (#511, from #407/#409).
//!
//! #407: every idle CPU worker woke every other idle worker when it released its permit, a
//! livelock that pinned the pool with an empty queue. #409 showed a pool sized to a 4-core CI
//! runner does not reproduce it (even with the fix reverted it burned 0 ms), so this builds the
//! 16-worker / 8-permit pool #408 was found with, whatever the machine has. It counts worker wakes
//! instead of reading CPU time, so there is no wall-clock CPU threshold to flake on: the bound
//! comes from [`POLL_INTERVAL`] and the window actually elapsed.
//!
//! Its own integration-test binary (a separate process) so no other test's pool shares it.

use std::time::{Duration, Instant};

use nicti_pounce::runtime::POLL_INTERVAL;
use nicti_pounce::Pounce;

const CPU_WORKERS: usize = 16;
const PERMITS: usize = 8;
/// CPU workers plus the GPU lane's one.
const WORKERS: u64 = CPU_WORKERS as u64 + 1;

#[test]
fn an_idle_sized_pool_wakes_only_on_the_poll_backstop() {
    let pounce = Pounce::new(1 << 30, CPU_WORKERS, PERMITS, || {});
    // Let startup settle: every worker's first pass is an immediate (counted) empty one.
    std::thread::sleep(POLL_INTERVAL * 2);
    let (wakes_before, start) = (pounce.idle_wakes(), Instant::now());
    std::thread::sleep(Duration::from_secs(2));
    let wakes = pounce.idle_wakes() - wakes_before;
    let window = start.elapsed();

    // Every worker, permit holder or not, parks a full POLL_INTERVAL between counted passes, so it
    // adds at most one per interval; +2 each for the edges of the window. A wake storm or a spin
    // in either branch is orders of magnitude over this (#407 re-introduced: thousands a second).
    let bound = WORKERS * (window.as_millis() as u64 / POLL_INTERVAL.as_millis() as u64 + 2);
    assert!(
        wakes <= bound,
        "idle pool woke {wakes} times in {window:?} (bound {bound}): a wake storm"
    );
}
