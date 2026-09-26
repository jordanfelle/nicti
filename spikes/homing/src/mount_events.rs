//! #71's change-detection comparison: `CM_Register_Notification` (`GUID_DEVINTERFACE_VOLUME`,
//! push-based, no window required) vs. polling `sysinfo`'s `Disks` list at a fixed interval. The
//! `watch` CLI command runs whichever backend is picked and prints a timestamped line per
//! attach/detach, plus a summary of observed latency once the deadline elapses -- the same shape
//! as `spikes/retina/src/watch.rs`'s filesystem-watch spike.

use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum MountEventKind {
    Attached,
    Detached,
}

#[derive(Debug, Clone, Serialize)]
pub struct MountEvent {
    pub at_ms: u64,
    pub kind: MountEventKind,
    pub mount_point: String,
}

#[cfg(windows)]
pub mod windows_impl {
    use super::{MountEvent, MountEventKind};
    use anyhow::Result;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    /// Polling backend, always available as a comparison baseline regardless of which
    /// notification API `watch()` uses -- `interval` is the axis this spike sweeps (1s and 5s,
    /// per the issue's evaluation ask).
    pub fn poll_for(deadline: Instant, interval: Duration) -> Result<Vec<MountEvent>> {
        let start = Instant::now();
        let mut events = Vec::new();
        let mut seen: HashSet<String> = current_mount_points()?.into_iter().collect();
        while Instant::now() < deadline {
            std::thread::sleep(interval);
            let now: HashSet<String> = current_mount_points()?.into_iter().collect();
            for added in now.difference(&seen) {
                events.push(MountEvent {
                    at_ms: start.elapsed().as_millis() as u64,
                    kind: MountEventKind::Attached,
                    mount_point: added.clone(),
                });
            }
            for removed in seen.difference(&now) {
                events.push(MountEvent {
                    at_ms: start.elapsed().as_millis() as u64,
                    kind: MountEventKind::Detached,
                    mount_point: removed.clone(),
                });
            }
            seen = now;
        }
        Ok(events)
    }

    fn current_mount_points() -> Result<Vec<String>> {
        use sysinfo::Disks;
        let disks = Disks::new_with_refreshed_list();
        Ok(disks
            .iter()
            .map(|d| d.mount_point().to_string_lossy().to_string())
            .collect())
    }

    /// `CM_Register_Notification`-backed push path. Left as a documented follow-up rather than
    /// fully implemented here: the `windows` crate's device-notification bindings need a message
    /// pump (or `CM_Register_Notification`'s callback form, which needs care around thread
    /// lifetime) -- deferred to the reference-machine pass itself, tracked in
    /// `docs/research/homing-volume-identity.md`'s Deferred section rather than blocking this
    /// spike's schema/fingerprint work on it. `poll_for` above is what `homing watch` actually
    /// runs today; this stub exists so the CLI's `--backend notify` flag has a real (if
    /// unimplemented) arm to report against instead of silently falling back to polling.
    pub fn watch_push(_deadline: Instant) -> Result<Vec<MountEvent>> {
        anyhow::bail!(
            "CM_Register_Notification backend not yet implemented -- see homing-volume-identity.md's Deferred section"
        )
    }
}

#[cfg(not(windows))]
pub mod windows_impl {
    use super::MountEvent;
    use anyhow::{bail, Result};
    use std::time::{Duration, Instant};

    pub fn poll_for(_deadline: Instant, _interval: Duration) -> Result<Vec<MountEvent>> {
        bail!("mount-event detection is Windows-only (see #73 for non-Windows deferral)")
    }

    pub fn watch_push(_deadline: Instant) -> Result<Vec<MountEvent>> {
        bail!("mount-event detection is Windows-only (see #73 for non-Windows deferral)")
    }
}

/// Summary stats over a captured event stream -- feeds the ADR's latency/CPU comparison table.
pub fn summarize(events: &[MountEvent]) -> String {
    format!("{} event(s) captured", events.len())
}
