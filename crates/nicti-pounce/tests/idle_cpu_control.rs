//! THROWAWAY positive control for #511 (never merged): does #407 reproduce on windows-latest with
//! the #408 fix reverted, 16 CPU workers / 8 permits? Always fails so CI prints the numbers.
use std::time::{Duration, Instant};

use nicti_pounce::Pounce;
use sysinfo::{get_current_pid, ProcessesToUpdate, System};

fn cpu_ms(sys: &mut System, pid: sysinfo::Pid) -> u64 {
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    sys.process(pid)
        .map(|p| p.accumulated_cpu_time())
        .unwrap_or(0)
}

#[test]
fn control_16_workers_8_permits_fix_reverted() {
    let pid = get_current_pid().unwrap();
    let mut sys = System::new();
    let pounce = Pounce::new(1 << 30, 16, 8, || {});
    std::thread::sleep(Duration::from_millis(400));
    let (c0, w0, t0) = (cpu_ms(&mut sys, pid), pounce.idle_wakes(), Instant::now());
    std::thread::sleep(Duration::from_secs(10));
    let (c1, w1, t1) = (cpu_ms(&mut sys, pid), pounce.idle_wakes(), Instant::now());
    panic!(
        "CONTROL RESULT: cpu_ms={} wakes={} over {:?}",
        c1 - c0,
        w1 - w0,
        t1 - t0
    );
}
