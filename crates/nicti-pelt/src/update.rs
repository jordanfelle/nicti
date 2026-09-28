//! Background update-check integration (#249/ADR-0249): polls `nicti-shed` on a background
//! thread so a network check never blocks the UI thread, and tracks state for a non-modal
//! "update available" banner. The actual check/download/apply only exists on Windows (Nicti's
//! only shipping platform, ADR-0015) -- on every other platform this always reports "up to
//! date," matching `nicti-shed`'s own `cfg(windows)`-gating of its `net` module.
//!
//! #282 added the Edge channel alongside the original Stable one: same background-thread
//! plumbing, but the "is there something newer" identity differs (semver vs. a build commit
//! SHA, see `nicti_shed::check::edge_update_available`'s own doc comment), so `available` holds
//! an [`AvailableUpdate`] rather than a bare `nicti_shed::LatestRelease`.

use std::sync::{mpsc, Mutex};

use nicti_shed::state::Channel;
use nicti_shed::{EdgeRelease, LatestRelease};

/// Guards every `update.json` read-modify-write round trip (the channel picker's `save_channel`
/// on the UI thread, and a background check's own `mark_checked`/save in `run_check`) against
/// each other -- both do a plain load-then-save with no file lock of their own, so without this
/// a channel switch landing mid-check could lose either write to the other's clobbering `fs::write`
/// (an adversarial review caught this: `state::save` isn't atomic-rename, so the loser's fields
/// are silently dropped, not merged). Process-wide is enough -- only one `nicti.exe` instance
/// touches this file at a time in practice.
static STATE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Either channel's "here's what's available" result, carrying whichever release type that
/// channel's own check produced. Like `UpdateEvent` below, only ever constructed by the
/// `cfg(windows)` `run_check`/`apply_update` -- genuinely dead on non-Windows, not an oversight.
#[allow(dead_code)]
#[derive(Clone)]
enum AvailableUpdate {
    Stable(LatestRelease),
    Edge(EdgeRelease),
}

// `Available`/`Failed` are only ever constructed by the `cfg(windows)` `run_check` below --
// the non-Windows stub always returns `UpToDate` (there's no update channel to check on a
// platform Nicti doesn't ship on yet, ADR-0015/ADR-0249). Genuinely dead on non-Windows, not an
// oversight.
#[allow(dead_code)]
enum UpdateEvent {
    Available(AvailableUpdate),
    UpToDate,
    Failed(String),
}

pub struct UpdateChecker {
    receiver: Option<mpsc::Receiver<UpdateEvent>>,
    apply_receiver: Option<mpsc::Receiver<Result<(), String>>>,
    available: Option<AvailableUpdate>,
    /// A copy of the release currently being applied, kept only so a failed apply can restore
    /// [`Self::available`] for a retry -- the original was moved into the apply thread.
    pending_release: Option<AvailableUpdate>,
    last_error: Option<String>,
    applying: bool,
    channel: Channel,
}

impl UpdateChecker {
    pub fn new() -> Self {
        Self {
            receiver: None,
            apply_receiver: None,
            available: None,
            pending_release: None,
            last_error: None,
            applying: false,
            channel: load_channel(),
        }
    }

    /// Spawns a background check if one isn't already in flight. `force` bypasses the 24h
    /// auto-check throttle (the "Check for updates" button); a startup check should pass
    /// `force: false` so it's a no-op most days.
    pub fn spawn_check(&mut self, current_version: &str, force: bool) {
        if self.receiver.is_some() || self.applying {
            return;
        }
        self.last_error = None;
        let current_version = current_version.to_string();
        let channel = self.channel;
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(run_check(&current_version, channel, force));
        });
    }

    /// The channel the next check will run against.
    pub fn channel(&self) -> Channel {
        self.channel
    }

    /// Switches channel, persists the choice, clears any stale result from the other channel's
    /// last check, and spawns an immediate forced re-check -- so picking Edge surfaces whatever
    /// edge build is currently out right away, not just the next one, and a stale Stable "you're
    /// up to date" (or vice versa) never lingers onscreen answering the wrong question.
    ///
    /// The visible channel is only ever changed *after* `save_channel` succeeds (CodeRabbit
    /// review, PR #283): committing it first and persisting second means a save failure (a full
    /// disk, a permissions problem) would leave the UI showing the new channel while the file on
    /// disk still says the old one -- silently reverting on the next launch with no warning ever
    /// shown. A failed switch instead leaves the channel unchanged and surfaces the error exactly
    /// like a failed check would.
    pub fn set_channel(&mut self, channel: Channel, current_version: &str) {
        if channel == self.channel || self.applying {
            return;
        }
        match save_channel(channel) {
            Ok(()) => {
                self.channel = channel;
                self.available = None;
                self.last_error = None;
                self.receiver = None;
                self.spawn_check(current_version, true);
            }
            Err(e) => {
                self.last_error = Some(format!("failed to switch update channel: {e}"));
            }
        }
    }

    /// Drains any background thread's result that has landed (a check, or an apply). Call once
    /// per frame.
    pub fn poll(&mut self) {
        if let Some(rx) = &self.receiver {
            match rx.try_recv() {
                Ok(UpdateEvent::Available(release)) => {
                    self.available = Some(release);
                    self.receiver = None;
                }
                Ok(UpdateEvent::UpToDate) => {
                    self.receiver = None;
                }
                Ok(UpdateEvent::Failed(msg)) => {
                    self.last_error = Some(msg);
                    self.receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.receiver = None;
                }
            }
        }

        if let Some(rx) = &self.apply_receiver {
            match rx.try_recv() {
                // On success the applying process's own installer relaunch is about to replace
                // this one -- there is deliberately nothing to do here (see `apply`'s own doc
                // comment): a success value never actually arrives before the process exits.
                Ok(Ok(())) => {}
                Ok(Err(msg)) => {
                    self.last_error = Some(msg);
                    self.available = self.pending_release.take();
                    self.applying = false;
                    self.apply_receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.available = self.pending_release.take();
                    self.applying = false;
                    self.apply_receiver = None;
                }
            }
        }
    }

    /// A short display label for whatever update is available -- a semver string on Stable
    /// (`"0.2.0"`), or a short commit SHA on Edge (`"edge (abc1234)"`, matching `release.yml`'s
    /// own release-title convention).
    pub fn available_label(&self) -> Option<String> {
        match self.available.as_ref()? {
            AvailableUpdate::Stable(r) => Some(r.version.to_string()),
            AvailableUpdate::Edge(r) => Some(format!("edge ({})", r.short_sha)),
        }
    }

    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn is_checking(&self) -> bool {
        self.receiver.is_some()
    }

    pub fn is_applying(&self) -> bool {
        self.applying
    }

    /// Spawns a background thread to download, verify, and install the update found by a prior
    /// check, then relaunch. Runs off the UI thread deliberately -- the download itself can take
    /// long enough to freeze the app if run inline from a button-click handler. On success the
    /// spawned process's own installer relaunch replaces this one before any result is ever sent
    /// back; a result only ever arrives on failure, at which point the update is restored to
    /// [`Self::available`] so the user can retry rather than losing it silently.
    pub fn apply(&mut self) {
        if self.applying {
            return;
        }
        let Some(release) = self.available.take() else {
            return;
        };
        self.applying = true;
        self.last_error = None;
        self.pending_release = Some(release.clone());
        let (tx, rx) = mpsc::channel();
        self.apply_receiver = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(apply_update(release));
        });
    }
}

impl Default for UpdateChecker {
    fn default() -> Self {
        Self::new()
    }
}

fn load_channel() -> Channel {
    nicti_shed::state::default_path()
        .as_deref()
        .map(nicti_shed::state::load)
        .unwrap_or_default()
        .channel
}

/// Persists just the channel choice -- reloads the current state first (rather than writing a
/// fresh default) so this never clobbers `auto_check`/`last_check_unix` a background check
/// already saved. Holds [`STATE_WRITE_LOCK`] across the whole load-mutate-save round trip so a
/// concurrent `run_check` save can't interleave with it.
///
/// `Ok(())` on a platform with no state path at all (non-Windows, `default_path()` is always
/// `None`) -- there's genuinely nothing to persist to there, which isn't the same failure as a
/// real write erroring out on a platform that does have one, and `set_channel` (the only caller)
/// must not refuse an in-memory-only channel switch just because this build has no update
/// mechanism to begin with.
fn save_channel(channel: Channel) -> Result<(), nicti_shed::ShedError> {
    let Some(path) = nicti_shed::state::default_path() else {
        return Ok(());
    };
    let _guard = STATE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut state = nicti_shed::state::load(&path);
    state.channel = channel;
    nicti_shed::state::save(&path, &state)
}

#[cfg(windows)]
fn run_check(current_version: &str, channel: Channel, force: bool) -> UpdateEvent {
    let path = nicti_shed::state::default_path();
    let now = nicti_shed::state::now_unix();
    // A racy read against a concurrent `save_channel` write is fine here -- worst case this
    // check runs once more or less than the 24h throttle intends, never a corrupted read (each
    // writer always writes a complete valid JSON document, see `state::save`'s doc comment).
    let should_check = path
        .as_deref()
        .map(nicti_shed::state::load)
        .unwrap_or_default()
        .should_check_now(now)
        || force;

    if !should_check {
        return UpdateEvent::UpToDate;
    }

    let result = match channel {
        Channel::Stable => match semver::Version::parse(current_version) {
            Ok(current) => nicti_shed::net::check_for_update(&current)
                .map(|opt| opt.map(AvailableUpdate::Stable)),
            Err(e) => Err(nicti_shed::ShedError::InvalidVersion(e.to_string())),
        },
        Channel::Edge => {
            nicti_shed::net::check_for_edge_update().map(|opt| opt.map(AvailableUpdate::Edge))
        }
    };

    // Reload fresh (rather than reusing the pre-network-call read above) and hold
    // `STATE_WRITE_LOCK` only across this short load-mutate-save round trip, not the whole
    // network check -- a concurrent `save_channel` from the channel picker must never be
    // blocked for the duration of an HTTP request, and reloading here means this save only ever
    // touches `last_check_unix`, never clobbers a channel switch that landed mid-check.
    if let Some(path) = &path {
        let _guard = STATE_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = nicti_shed::state::load(path);
        state.mark_checked(now);
        let _ = nicti_shed::state::save(path, &state);
    }

    match result {
        Ok(Some(release)) => UpdateEvent::Available(release),
        Ok(None) => UpdateEvent::UpToDate,
        Err(e) => UpdateEvent::Failed(e.to_string()),
    }
}

#[cfg(not(windows))]
fn run_check(_current_version: &str, _channel: Channel, _force: bool) -> UpdateEvent {
    UpdateEvent::UpToDate
}

#[cfg(windows)]
fn apply_update(release: AvailableUpdate) -> Result<(), String> {
    match release {
        AvailableUpdate::Stable(r) => {
            nicti_shed::net::download_and_apply(&r).map_err(|e| e.to_string())
        }
        AvailableUpdate::Edge(r) => {
            nicti_shed::net::download_and_apply_edge(&r).map_err(|e| e.to_string())
        }
    }
}

#[cfg(not(windows))]
fn apply_update(_release: AvailableUpdate) -> Result<(), String> {
    Err("updates are only supported on Windows".to_string())
}
