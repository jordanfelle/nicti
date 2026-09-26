//! #37's bundled filesystem-watch sub-question (feeds #24): watches `dir` with `notify` 8.x for
//! `seconds`, printing every event with a timestamp plus a running count, and flags a
//! `notify::EventKind::Other` "rescan" event distinctly -- that's `ReadDirectoryChangesW`'s 16KB
//! buffer overflowing on Windows (see notify's `windows.rs`), the thing a burst NEF-import test
//! needs to watch for. No debouncer crate here -- not because of a version conflict
//! (notify-debouncer-full 0.7.0 actually requires notify ^8.2.0, matching Cargo.toml's version
//! exactly; an earlier draft of this comment wrongly claimed a 7.x-only pairing), just because
//! this hand-rolled quiet-period loop is enough for a spike measuring "does an event arrive, and
//! how fast." A real ingest watcher should reach for notify-debouncer-full instead.

use std::path::Path;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use notify::{RecursiveMode, Watcher};

pub fn run(dir: &Path, seconds: u64) -> anyhow::Result<()> {
    let (tx, rx) = channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send((Instant::now(), res));
    })?;
    watcher.watch(dir, RecursiveMode::Recursive)?;

    println!("watching {} for {seconds}s ...", dir.display());
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut count = 0usize;
    let mut rescans = 0usize;
    let start = Instant::now();
    let mut last_event_at = start;

    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok((at, Ok(event))) => {
                count += 1;
                last_event_at = at;
                if matches!(event.kind, notify::EventKind::Other) {
                    rescans += 1;
                    println!(
                        "[{:>7.1}ms] RESCAN FLAG (buffer overflow) -- {:?}",
                        at.duration_since(start).as_secs_f64() * 1000.0,
                        event
                    );
                } else {
                    println!(
                        "[{:>7.1}ms] {:?} {:?}",
                        at.duration_since(start).as_secs_f64() * 1000.0,
                        event.kind,
                        event.paths
                    );
                }
            }
            Ok((_, Err(e))) => eprintln!("watch error: {e}"),
            Err(_) => {} // timeout, loop and re-check deadline
        }
    }

    println!(
        "done: {count} events ({rescans} rescan-flag events), last event at {:.1}ms",
        last_event_at.duration_since(start).as_secs_f64() * 1000.0
    );
    Ok(())
}
