//! Regression for #407: idle CPU workers each woke every other idle worker on every permit
//! release, a livelock that pinned most of the pool with an empty queue.
//!
//! Its own integration-test binary (a separate process) on purpose: the measurement sums every
//! `pounce-cpu-*` thread in the process, so it must not share one with the unit tests, whose own
//! busy pools would count against it.
#![cfg(target_os = "linux")]

use std::time::Duration;

use nicti_pounce::Pounce;

/// Total CPU ticks (utime + stime) this process's `pounce-cpu-*` threads have burned.
fn pounce_cpu_ticks() -> u64 {
    let mut total = 0;
    for task in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let comm = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        if !comm.starts_with("pounce-cpu-") {
            continue;
        }
        let stat = std::fs::read_to_string(task.path().join("stat")).unwrap_or_default();
        // Fields after the parenthesised comm; utime/stime are fields 14/15 overall.
        if let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) {
            let f: Vec<&str> = rest.split_whitespace().collect();
            total += f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
                + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
        }
    }
    total
}

#[test]
fn idle_cpu_pool_does_not_burn_cpu() {
    let _pounce = Pounce::new(1 << 30, 16, 8, || {});
    std::thread::sleep(Duration::from_millis(200));
    let before = pounce_cpu_ticks();
    std::thread::sleep(Duration::from_secs(1));
    let burned = pounce_cpu_ticks() - before;
    // 100 ticks/s per core; a healthy idle pool is a handful of ticks, the livelock was
    // hundreds (80 measured with 16 workers).
    assert!(burned < 20, "idle pool burned {burned} ticks in 1s");
}
