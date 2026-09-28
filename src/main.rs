fn main() -> nicti_pelt::Result {
    // `nicti-pelt` is a lib crate (its own `CARGO_PKG_VERSION` would read "0.0.0"), so the real
    // shipped version lives on the root `nicti` binary crate and is threaded in here (#249).
    nicti_pelt::run(env!("CARGO_PKG_VERSION"))
}
