//! Benchmark runner: prints a markdown results table (throughput + dispatch overhead + interop
//! cost, per backend) plus hardware identity, for pasting into `docs/adr/0005-gpu-compute-api.md`.
//! See `CLAUDE.md`'s package-map note -- this is spike tooling, not a production binary.

use glint::gpu::GpuContext;

fn main() {
    let contexts = GpuContext::enumerate();
    if contexts.is_empty() {
        eprintln!("glint: no wgpu adapter available on this machine");
        std::process::exit(1);
    }

    println!("# glint results\n");
    println!("Run on: {}\n", hardware_identity());
    println!("| Backend | Adapter | Timestamps | f16 |");
    println!("|---|---|---|---|");
    for ctx in &contexts {
        println!(
            "| {:?} | {} | {} | {} |",
            ctx.backend,
            ctx.adapter_name,
            ctx.supports_timestamps(),
            ctx.supports_f16()
        );
    }
}

fn hardware_identity() -> String {
    format!(
        "{} ({})",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}
