//! Persists the updater's own small per-user state: whether background auto-check is on, and
//! when it last ran, so it runs at most once per 24h rather than on every launch. No `dirs`
//! crate -- `%LOCALAPPDATA%` is read directly, matching `crates/nicti-pelt`'s own convention of
//! not pulling in a per-platform-paths dependency until a second location is actually needed.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ShedError;

const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Which release `nicti-shed` checks against -- see `check::RELEASES_LATEST_URL`/
/// `check::EDGE_RELEASE_URL`. Defaults to `Stable` on any state file that predates this field
/// (`#[serde(default)]` on `UpdateState::channel`), so an existing install never silently opts
/// itself into unsigned, unreviewed edge builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Channel {
    #[default]
    Stable,
    Edge,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateState {
    #[serde(default = "default_true")]
    pub auto_check: bool,
    #[serde(default)]
    pub last_check_unix: Option<u64>,
    #[serde(default)]
    pub channel: Channel,
}

fn default_true() -> bool {
    true
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            auto_check: true,
            last_check_unix: None,
            channel: Channel::default(),
        }
    }
}

impl UpdateState {
    /// Whether a background check should run now, given the current unix time. Never checks
    /// more than once per [`CHECK_INTERVAL_SECS`], and never at all with auto-check off.
    pub fn should_check_now(&self, now_unix: u64) -> bool {
        self.auto_check
            && self
                .last_check_unix
                .is_none_or(|last| now_unix.saturating_sub(last) >= CHECK_INTERVAL_SECS)
    }

    pub fn mark_checked(&mut self, now_unix: u64) {
        self.last_check_unix = Some(now_unix);
    }
}

/// Loads state from `path`, falling back to [`UpdateState::default`] if the file is missing,
/// unreadable, or corrupt -- a broken state file should never block the app from starting or
/// from checking for updates, it should just reset to "never checked."
pub fn load(path: &Path) -> UpdateState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

/// Writes `state` to `path` by writing a temp file in the same directory then renaming it over
/// the destination -- a plain `fs::write` truncates the destination in place first, so a reader
/// (or a crash mid-write) could observe a partially-written file; `state::load` treats that as
/// unparseable and silently resets to defaults, losing every saved preference. A same-directory
/// rename is atomic on both Windows (`MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`, what Rust's
/// `fs::rename` uses there) and POSIX, so a reader only ever sees the old complete file or the
/// new complete file, never a partial one. The temp filename embeds this process's PID so two
/// processes racing a save (there's no cross-process lock, only the in-process
/// `nicti-pelt::update::STATE_WRITE_LOCK`) can't collide on the same temp path. (CodeRabbit
/// review, PR #283.)
pub fn save(path: &Path, state: &UpdateState) -> Result<(), ShedError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ShedError::Io)?;
    }
    let json = serde_json::to_string_pretty(state).map_err(|e| ShedError::Parse(e.to_string()))?;
    let tmp_path = path.with_extension(format!("json.tmp-{}", std::process::id()));
    std::fs::write(&tmp_path, json).map_err(ShedError::Io)?;
    std::fs::rename(&tmp_path, path).map_err(ShedError::Io)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `%LOCALAPPDATA%\Nicti\update.json`, or `None` if the platform doesn't have that variable set
/// (or isn't Windows at all -- there's no update channel to track state for anywhere else yet).
#[cfg(windows)]
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Nicti").join("update.json"))
}

#[cfg(not(windows))]
pub fn default_path() -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_state_checks_immediately() {
        let state = UpdateState::default();
        assert!(state.should_check_now(1_000_000));
    }

    #[test]
    fn recently_checked_state_does_not_recheck() {
        let mut state = UpdateState::default();
        state.mark_checked(1_000_000);
        assert!(!state.should_check_now(1_000_000 + 60));
    }

    #[test]
    fn stale_check_triggers_recheck() {
        let mut state = UpdateState::default();
        state.mark_checked(1_000_000);
        assert!(state.should_check_now(1_000_000 + CHECK_INTERVAL_SECS));
    }

    #[test]
    fn auto_check_off_never_checks() {
        let state = UpdateState {
            auto_check: false,
            last_check_unix: None,
            channel: Channel::default(),
        };
        assert!(!state.should_check_now(1_000_000));
    }

    #[test]
    fn state_predating_channel_field_defaults_to_stable() {
        let state: UpdateState = serde_json::from_str(r#"{"auto_check":true}"#).unwrap();
        assert_eq!(state.channel, Channel::Stable);
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("nicti-shed-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("update.json");

        let mut state = UpdateState::default();
        state.mark_checked(42);
        state.auto_check = false;
        save(&path, &state).unwrap();

        let loaded = load(&path);
        assert_eq!(loaded, state);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_loads_as_default() {
        let path = std::env::temp_dir().join("nicti-shed-test-does-not-exist.json");
        assert_eq!(load(&path), UpdateState::default());
    }
}
