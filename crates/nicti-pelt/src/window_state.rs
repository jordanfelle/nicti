//! Remembered window size, position and maximized state (#772), saved beside the catalog
//! (`<catalog>.window.json`, temp file + rename like `section_state`). A missing, corrupt or
//! absurd file just means the default window.
//!
//! The position is the risky part: a window restored onto a monitor that is no longer there is
//! invisible. [`needs_rescue`] catches the common case (the restored corner is beyond the monitor
//! the window opened on) so the app can pull it back.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The window's minimum inner size (the launcher's `with_min_inner_size`).
pub const MIN_SIZE: [f32; 2] = [900.0, 600.0];
const MAX_SIZE: f32 = 16_384.0;
const MAX_POS: f32 = 32_768.0;
/// A changed geometry is written once it has been stable this long (a drag emits one per frame).
const SETTLE: Duration = Duration::from_millis(800);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowState {
    /// Inner size in points.
    pub size: [f32; 2],
    /// Outer top-left in points; `None` when the platform did not report one.
    pub pos: Option<[f32; 2]>,
    pub maximized: bool,
}

pub fn file_for(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| "nicti".into());
    name.push(".window.json");
    catalog_path.with_file_name(name)
}

/// Drops values no real window has (NaN, tiny, enormous); `None` when the size itself is unusable.
pub fn sanitize(mut s: WindowState) -> Option<WindowState> {
    let ok = |v: f32, lo: f32, hi: f32| v.is_finite() && v >= lo && v <= hi;
    if !ok(s.size[0], MIN_SIZE[0], MAX_SIZE) || !ok(s.size[1], MIN_SIZE[1], MAX_SIZE) {
        return None;
    }
    if let Some(p) = s.pos {
        if !ok(p[0], -MAX_POS, MAX_POS) || !ok(p[1], -MAX_POS, MAX_POS) {
            s.pos = None;
        }
    }
    Some(s)
}

pub fn load(catalog_path: &Path) -> Option<WindowState> {
    let text = std::fs::read_to_string(file_for(catalog_path)).ok()?;
    sanitize(serde_json::from_str(&text).ok()?)
}

pub fn save(catalog_path: &Path, state: &WindowState) -> std::io::Result<()> {
    let path = file_for(catalog_path);
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string(state).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)
}

/// The launcher's window builder with the saved geometry applied.
pub fn apply(builder: egui::ViewportBuilder, saved: Option<WindowState>) -> egui::ViewportBuilder {
    let Some(s) = saved else { return builder };
    let mut b = builder.with_inner_size(s.size).with_maximized(s.maximized);
    if let Some(p) = s.pos {
        b = b.with_position(p);
    }
    b
}

/// Whether a window whose top-left is `pos` is (almost) entirely off a monitor of `monitor` size
/// -- the saved position came from a monitor that is gone. Only the far edges can be checked
/// (the monitor's own origin is unknown); a window whose title bar is within 80 points of the
/// right/bottom edge cannot be grabbed.
pub fn needs_rescue(pos: [f32; 2], monitor: [f32; 2]) -> bool {
    monitor[0] > 0.0
        && monitor[1] > 0.0
        && (pos[0] > monitor[0] - 80.0 || pos[1] > monitor[1] - 80.0)
}

/// Debounces the geometry the app observes each frame into "write this now" moments.
#[derive(Default)]
pub struct Tracker {
    saved: Option<WindowState>,
    pending: Option<(WindowState, Instant)>,
}

impl Tracker {
    /// Seeds with what was loaded at startup so an unchanged window is never rewritten.
    pub fn new(loaded: Option<WindowState>) -> Self {
        Self {
            saved: loaded,
            pending: None,
        }
    }

    /// Feeds this frame's geometry. Returns a state to write once it has stopped changing.
    pub fn observe(&mut self, now: Instant, current: WindowState) -> Option<WindowState> {
        let current = sanitize(current)?;
        if self.saved == Some(current) {
            self.pending = None;
            return None;
        }
        match self.pending {
            Some((p, since)) if p == current => {
                if now.duration_since(since) >= SETTLE {
                    self.pending = None;
                    self.saved = Some(current);
                    return Some(current);
                }
            }
            _ => self.pending = Some((current, now)),
        }
        None
    }

    /// Whether a change is waiting out the settle time (the app asks for a repaint then).
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(w: f32, h: f32) -> WindowState {
        WindowState {
            size: [w, h],
            pos: Some([10.0, 20.0]),
            maximized: false,
        }
    }

    #[test]
    fn a_saved_state_round_trips_and_a_missing_or_corrupt_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("library.db");
        assert_eq!(load(&cat), None);
        save(&cat, &st(1200.0, 800.0)).unwrap();
        assert_eq!(load(&cat), Some(st(1200.0, 800.0)));
        std::fs::write(file_for(&cat), "{not json").unwrap();
        assert_eq!(load(&cat), None);
    }

    #[test]
    fn absurd_values_are_dropped_not_applied() {
        assert_eq!(sanitize(st(f32::NAN, 800.0)), None);
        assert_eq!(sanitize(st(100.0, 100.0)), None, "below the minimum");
        assert_eq!(sanitize(st(1e9, 800.0)), None);
        let off = WindowState {
            pos: Some([1e9, 0.0]),
            ..st(1000.0, 700.0)
        };
        assert_eq!(
            sanitize(off).unwrap().pos,
            None,
            "size kept, position dropped"
        );
    }

    #[test]
    fn a_window_beyond_the_monitor_is_rescued_and_one_inside_is_not() {
        assert!(needs_rescue([2500.0, 100.0], [1920.0, 1080.0]));
        assert!(needs_rescue([100.0, 1050.0], [1920.0, 1080.0]));
        assert!(!needs_rescue([100.0, 100.0], [1920.0, 1080.0]));
        assert!(
            !needs_rescue([5000.0, 5000.0], [0.0, 0.0]),
            "unknown monitor: never guess"
        );
    }

    #[test]
    fn a_change_is_written_once_it_settles_and_an_unchanged_window_never_is() {
        let t0 = Instant::now();
        let mut t = Tracker::new(Some(st(1000.0, 700.0)));
        assert_eq!(t.observe(t0, st(1000.0, 700.0)), None, "same as saved");
        assert!(!t.is_pending());
        // A drag: every frame differs, nothing is written until it stops.
        assert_eq!(t.observe(t0, st(1100.0, 700.0)), None);
        assert_eq!(
            t.observe(t0 + Duration::from_millis(300), st(1150.0, 700.0)),
            None
        );
        assert!(t.is_pending());
        assert_eq!(
            t.observe(t0 + Duration::from_millis(600), st(1150.0, 700.0)),
            None
        );
        let done = t.observe(t0 + Duration::from_millis(1500), st(1150.0, 700.0));
        assert_eq!(done, Some(st(1150.0, 700.0)));
        // Written once: the same state again is not rewritten.
        assert_eq!(
            t.observe(t0 + Duration::from_millis(2500), st(1150.0, 700.0)),
            None
        );
    }

    #[test]
    fn a_minimised_window_is_never_recorded() {
        let t0 = Instant::now();
        let mut t = Tracker::new(None);
        // A minimised window reports a zero size: sanitize refuses it.
        assert_eq!(t.observe(t0, st(0.0, 0.0)), None);
        assert!(!t.is_pending());
    }
}
