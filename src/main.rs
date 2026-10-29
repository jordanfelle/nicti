// Release builds use the Windows GUI subsystem so launching doesn't open a stray console window
// behind the app (#298). Debug builds keep the console for logs/panics.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() -> nicti_pelt::Result {
    // `nicti-pelt` is a lib crate (its own `CARGO_PKG_VERSION` would read "0.0.0"), so the real
    // shipped version lives on the root `nicti` binary crate and is threaded in here (#249).
    // The agent control channel is opt-in (`--control[=PORT]`, `NICTI_CONTROL_PORT`; ADR-0434).
    let env_port = std::env::var("NICTI_CONTROL_PORT").ok();
    let control =
        match nicti_pelt::ControlOptions::parse(std::env::args().skip(1), env_port.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("nicti: {e}");
                std::process::exit(2);
            }
        };
    nicti_pelt::run_with(env!("CARGO_PKG_VERSION"), control)
}
