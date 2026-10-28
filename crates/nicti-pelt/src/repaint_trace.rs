//! Which wake source drove each frame (#512).
//!
//! `ctx.repaint_causes()` lists every `request_repaint*` call made during the previous pass, not
//! what actually woke the loop, so it cannot say why an idle window keeps repainting (#409's
//! research doc). This is the trigger side: the immediate-repaint sites go through [`request`],
//! which records its [`Wake`] before waking egui, and [`frame`] (once per UI pass) charges the pass
//! to every source that asked since the previous pass, or to "unattributed" when none did (a
//! `request_repaint_after` timer, input, or an uninstrumented request). egui may run `ui()` more
//! than once per frame (a layout that changed asks for a second pass); those extra passes are
//! counted separately as `relayout`, not as frames, since only the last one is painted.
//!
//! Off unless `NICTI_REPAINT_TRACE` is set (any value); then one summary line per second goes to
//! stderr: `repaint-trace: frames=3 relayout=2 elapsed_ms=1004 telemetry=2 pending=1 pounce=0 cull=0 unattributed=0`.
//! Lines are printed from the UI pass, so an idle window prints no faster than it paints: the
//! spacing between lines is at least a second, and `elapsed_ms` is the span the counts cover.
//! A source can count in a frame another source also asked in, so the parts may sum above `frames`.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Wake {
    Telemetry,
    Pending,
    Pounce,
    Cull,
}

const WAKES: [(Wake, &str); 4] = [
    (Wake::Telemetry, "telemetry"),
    (Wake::Pending, "pending"),
    (Wake::Pounce, "pounce"),
    (Wake::Cull, "cull"),
];

struct Trace {
    /// Sources that asked since the last [`frame`], one bit per [`Wake`].
    asked: AtomicU32,
    frames: AtomicU64,
    relayout: AtomicU64,
    unattributed: AtomicU64,
    per_source: [AtomicU64; WAKES.len()],
    window_start: Mutex<Instant>,
    /// When the previous frame began, and the gaps (ms) before this second's unattributed frames:
    /// a fixed gap after a known wake points at a `request_repaint_after` delay.
    last_frame: Mutex<Instant>,
    gaps_ms: Mutex<Vec<u64>>,
    /// Distinct `ctx.repaint_causes()` (requests made during the pass before) seen on this
    /// second's unattributed frames: for a follow-up frame right after a wake, these are the
    /// in-pass requests that asked for it.
    causes: Mutex<Vec<String>>,
}

fn trace() -> Option<&'static Trace> {
    static TRACE: OnceLock<Option<Trace>> = OnceLock::new();
    TRACE
        .get_or_init(|| {
            std::env::var_os("NICTI_REPAINT_TRACE").map(|_| Trace {
                asked: AtomicU32::new(0),
                frames: AtomicU64::new(0),
                relayout: AtomicU64::new(0),
                unattributed: AtomicU64::new(0),
                per_source: Default::default(),
                window_start: Mutex::new(Instant::now()),
                last_frame: Mutex::new(Instant::now()),
                gaps_ms: Mutex::new(Vec::new()),
                causes: Mutex::new(Vec::new()),
            })
        })
        .as_ref()
}

fn bit(wake: Wake) -> u32 {
    1 << WAKES.iter().position(|(w, _)| *w == wake).unwrap()
}

/// How long after a wake the frame runs. Non-zero on purpose, see [`request`].
pub(crate) const WAKE_DELAY: Duration = Duration::from_millis(1);

/// Wakes the UI for one frame, recording who asked when tracing is on.
///
/// This is `request_repaint_after(WAKE_DELAY)`, not `request_repaint()`: egui answers a zero-delay
/// request with **two** passes ("each request results in two repaints, just to give some things
/// time to settle", `egui::context::request_repaint_after`), the second one 1-2 ms after the
/// first and with no source of its own. For these wakes -- a worker thread posting data the next
/// pass reads -- that second pass is pure idle cost, and it was the "unattributed follow-up frame
/// after every wake" of #584 (about 1.2 of the 2.17 idle frames/s #512 left). egui schedules a
/// non-zero delay once, so the wake costs one frame.
pub(crate) fn request(ctx: &egui::Context, wake: Wake) {
    if let Some(t) = trace() {
        t.asked.fetch_or(bit(wake), Ordering::Relaxed);
    }
    ctx.request_repaint_after(WAKE_DELAY);
}

/// Call once per UI pass.
pub(crate) fn frame(ctx: &egui::Context) {
    let Some(t) = trace() else { return };
    if ctx.current_pass_index() > 0 {
        t.relayout.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let asked = t.asked.swap(0, Ordering::Relaxed);
    t.frames.fetch_add(1, Ordering::Relaxed);
    let gap = std::mem::replace(&mut *t.last_frame.lock().unwrap(), Instant::now()).elapsed();
    if asked == 0 {
        t.unattributed.fetch_add(1, Ordering::Relaxed);
        let mut gaps = t.gaps_ms.lock().unwrap();
        if gaps.len() < 8 {
            gaps.push(gap.as_millis() as u64);
        }
        let mut causes = t.causes.lock().unwrap();
        for cause in ctx.repaint_causes() {
            let text = cause.to_string();
            if causes.len() < 12 && !causes.contains(&text) {
                causes.push(text);
            }
        }
    }
    for (i, (wake, _)) in WAKES.iter().enumerate() {
        if asked & bit(*wake) != 0 {
            t.per_source[i].fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut start = t.window_start.lock().unwrap();
    if start.elapsed() >= Duration::from_secs(1) {
        let mut line = format!(
            "repaint-trace: frames={} relayout={} elapsed_ms={}",
            t.frames.swap(0, Ordering::Relaxed),
            t.relayout.swap(0, Ordering::Relaxed),
            start.elapsed().as_millis()
        );
        for (i, (_, name)) in WAKES.iter().enumerate() {
            line += &format!(" {name}={}", t.per_source[i].swap(0, Ordering::Relaxed));
        }
        line += &format!(
            " unattributed={}",
            t.unattributed.swap(0, Ordering::Relaxed)
        );
        line += &format!(
            " unattributed_gaps_ms={:?}",
            std::mem::take(&mut *t.gaps_ms.lock().unwrap())
        );
        for cause in std::mem::take(&mut *t.causes.lock().unwrap()) {
            line += &format!("\n  cause: {cause}");
        }
        eprintln!("{line}");
        *start = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Runs the passes eframe would run for a wake made from outside a pass: `egui`'s repaint
    /// callback is eframe's trigger, so "a callback is pending" means "eframe would paint again".
    /// Returns how many passes the wake cost once the context was otherwise idle.
    fn passes_for_wake(wake: impl Fn(&egui::Context)) -> usize {
        let ctx = egui::Context::default();
        let pending = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&pending);
        ctx.set_request_repaint_callback(move |_| *flag.lock().unwrap() = true);
        let pass = |ctx: &egui::Context| {
            ctx.begin_pass(egui::RawInput::default());
            ctx.end_pass().textures_delta.clear();
        };

        // egui runs a couple of passes at start-up; let it settle to idle first.
        pass(&ctx);
        for _ in 0..8 {
            if !std::mem::take(&mut *pending.lock().unwrap()) {
                break;
            }
            pass(&ctx);
        }
        assert!(!*pending.lock().unwrap(), "the context did not go idle");

        wake(&ctx);
        let mut passes = 0;
        while std::mem::take(&mut *pending.lock().unwrap()) {
            pass(&ctx);
            passes += 1;
            assert!(passes < 8, "a single wake kept repainting");
        }
        passes
    }

    #[test]
    fn egui_answers_a_zero_delay_request_with_two_passes() {
        // The control: this is the behaviour #584 traced. If egui ever stops doing this, the
        // reason for `WAKE_DELAY` is gone and this test says so.
        assert_eq!(passes_for_wake(|ctx| ctx.request_repaint()), 2);
    }

    #[test]
    fn a_wake_from_a_worker_thread_costs_one_pass_not_two() {
        for wake in [Wake::Telemetry, Wake::Pending, Wake::Pounce, Wake::Cull] {
            assert_eq!(passes_for_wake(|ctx| request(ctx, wake)), 1, "{wake:?}");
        }
    }

    #[test]
    fn a_wake_that_lands_in_the_middle_of_a_pass_still_gets_exactly_one_more() {
        // A worker posts data while the UI thread is partway through a pass: the pass in flight may
        // already have read its state, so one further pass must follow, and no more than one.
        let ctx = egui::Context::default();
        let pending = Arc::new(Mutex::new(false));
        let flag = Arc::clone(&pending);
        ctx.set_request_repaint_callback(move |_| *flag.lock().unwrap() = true);
        let pass = |during: &dyn Fn()| {
            ctx.begin_pass(egui::RawInput::default());
            during();
            ctx.end_pass().textures_delta.clear();
        };
        for _ in 0..4 {
            pass(&|| {}); // settle past start-up
        }
        *pending.lock().unwrap() = false;

        pass(&|| request(&ctx, Wake::Pounce));
        let mut extra = 0;
        while std::mem::take(&mut *pending.lock().unwrap()) {
            pass(&|| {});
            extra += 1;
            assert!(extra < 8, "a single wake kept repainting");
        }
        assert_eq!(extra, 1, "one pass after the one it landed in");
    }

    #[test]
    fn the_wake_is_still_prompt_and_still_reaches_egui() {
        // A wake must still ask for a repaint (no silent drop), and soon.
        assert!(WAKE_DELAY > Duration::ZERO && WAKE_DELAY <= Duration::from_millis(5));
        let ctx = egui::Context::default();
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&asked);
        ctx.set_request_repaint_callback(move |info| seen.lock().unwrap().push(info.delay));
        // egui runs a couple of passes at start-up; settle past them before the wake.
        for _ in 0..3 {
            ctx.begin_pass(egui::RawInput::default());
            ctx.end_pass().textures_delta.clear();
        }
        asked.lock().unwrap().clear();
        request(&ctx, Wake::Pounce);
        assert_eq!(*asked.lock().unwrap(), vec![WAKE_DELAY]);
    }
}
