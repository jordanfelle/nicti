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
//! stderr: `repaint-trace: frames=3 relayout=2 telemetry=2 pending=1 pounce=0 cull=0 unattributed=0`.
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
            })
        })
        .as_ref()
}

fn bit(wake: Wake) -> u32 {
    1 << WAKES.iter().position(|(w, _)| *w == wake).unwrap()
}

/// `ctx.request_repaint()`, recording who asked when tracing is on.
pub(crate) fn request(ctx: &egui::Context, wake: Wake) {
    if let Some(t) = trace() {
        t.asked.fetch_or(bit(wake), Ordering::Relaxed);
    }
    ctx.request_repaint();
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
    }
    for (i, (wake, _)) in WAKES.iter().enumerate() {
        if asked & bit(*wake) != 0 {
            t.per_source[i].fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut start = t.window_start.lock().unwrap();
    if start.elapsed() >= Duration::from_secs(1) {
        let mut line = format!(
            "repaint-trace: frames={} relayout={}",
            t.frames.swap(0, Ordering::Relaxed),
            t.relayout.swap(0, Ordering::Relaxed)
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
        eprintln!("{line}");
        *start = Instant::now();
    }
}
