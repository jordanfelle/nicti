//! #409 spike (not for merge): process CPU burned by idle Pounce alone vs the telemetry sampler.
use std::time::Duration;

use nicti_pounce::telemetry::{default_load_source, default_vram_source, TelemetrySampler};
use nicti_pounce::Pounce;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

fn cpu_ms(sys: &mut System, pid: Pid) -> u64 {
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().with_cpu(),
    );
    sys.process(pid).map_or(0, |p| p.accumulated_cpu_time())
}

fn window(label: &str, sys: &mut System, pid: Pid) {
    std::thread::sleep(Duration::from_secs(2));
    let before = cpu_ms(sys, pid);
    std::thread::sleep(Duration::from_secs(10));
    let burned = cpu_ms(sys, pid) - before;
    println!(
        "{label}: {burned} ms CPU in 10 s ({:.2} cores)",
        burned as f64 / 10_000.0
    );
}

fn main() {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    let pid = Pid::from_u32(std::process::id());
    let mut sys = System::new();
    println!("cores={cores}");
    window("bare process", &mut sys, pid);
    let pounce = Pounce::new(1 << 30, cores, (cores / 2).max(1), || {});
    window("pounce pool only", &mut sys, pid);
    let sampler = TelemetrySampler::spawn(
        default_vram_source(),
        default_load_source(),
        Duration::from_millis(500),
        || {},
    );
    window("pool + telemetry sampler (500ms)", &mut sys, pid);
    drop(sampler);
    window("pool, sampler dropped", &mut sys, pid);
    drop(pounce);
}
