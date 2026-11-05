//! Panic containment for worker threads that do not run under Pounce (#706).

/// Runs `f`, turning a panic into its message so a long-lived worker can report the failure to
/// whoever is waiting and keep serving instead of dying silently.
///
/// `AssertUnwindSafe` is deliberate: callers use this around one unit of work (one batch, one
/// connection) and must treat anything that unit half-updated as suspect, as Pounce does for a
/// job that panicked.
pub fn catch_panic<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).map_err(|payload| {
        payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "non-string panic payload".to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_the_value_or_the_panic_message() {
        assert_eq!(catch_panic(|| 7), Ok(7));
        assert_eq!(catch_panic(|| -> u8 { panic!("boom") }), Err("boom".into()));
        let n = 3;
        assert_eq!(catch_panic(|| -> u8 { panic!("n={n}") }), Err("n=3".into()));
    }
}
