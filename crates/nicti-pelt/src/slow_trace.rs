//! Names the UI-thread section that made a frame slow (#768).
//!
//! The operator reported nicti locking up when a photo opens; without a profiler on every
//! machine the first question is *which* synchronous step blocked the UI. [`section`] wraps a
//! named step and, when `NICTI_SLOW_TRACE` is set (any value), prints one line to stderr for each
//! run that takes longer than [`THRESHOLD`]: `slow-trace: paint_loupe_viewport 1640 ms`. Off, it is
//! one cached boolean check per call.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// A section slower than this is reported.
pub(crate) const THRESHOLD: Duration = Duration::from_millis(250);

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("NICTI_SLOW_TRACE").is_some())
}

/// The line for a section that took `elapsed`, or `None` when it was fast enough.
pub(crate) fn message(name: &str, elapsed: Duration) -> Option<String> {
    (elapsed >= THRESHOLD).then(|| format!("slow-trace: {name} {} ms", elapsed.as_millis()))
}

/// Runs `f`, reporting it when tracing is on and it was slow.
pub(crate) fn section<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    if !enabled() {
        return f();
    }
    let start = Instant::now();
    let out = f();
    if let Some(line) = message(name, start.elapsed()) {
        eprintln!("{line}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_sections_at_or_over_the_threshold_are_reported() {
        assert_eq!(message("a", Duration::from_millis(249)), None);
        assert_eq!(
            message("paint_loupe_viewport", Duration::from_millis(1640)).as_deref(),
            Some("slow-trace: paint_loupe_viewport 1640 ms")
        );
        assert!(message("a", THRESHOLD).is_some());
    }

    #[test]
    fn a_section_returns_its_value_whether_or_not_tracing_is_on() {
        assert_eq!(section("x", || 7), 7);
    }
}
