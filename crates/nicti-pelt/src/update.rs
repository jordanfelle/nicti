//! Background update-check integration (#249/ADR-0249): polls `nicti-shed` on a background
//! thread so a network check never blocks the UI thread, and tracks state for a non-modal
//! "update available" banner. The actual check/download/apply only exists on Windows (Nicti's
//! only shipping platform, ADR-0015) -- on every other platform this always reports "up to
//! date," matching `nicti-shed`'s own `cfg(windows)`-gating of its `net` module.

use std::sync::mpsc;

use nicti_shed::LatestRelease;

// `Available`/`Failed` are only ever constructed by the `cfg(windows)` `run_check` below --
// the non-Windows stub always returns `UpToDate` (there's no update channel to check on a
// platform Nicti doesn't ship on yet, ADR-0015/ADR-0249). Genuinely dead on non-Windows, not an
// oversight.
#[allow(dead_code)]
enum UpdateEvent {
    Available(LatestRelease),
    UpToDate,
    Failed(String),
}

pub struct UpdateChecker {
    receiver: Option<mpsc::Receiver<UpdateEvent>>,
    apply_receiver: Option<mpsc::Receiver<Result<(), String>>>,
    available: Option<LatestRelease>,
    /// A copy of the release currently being applied, kept only so a failed apply can restore
    /// [`Self::available`] for a retry -- the original was moved into the apply thread.
    pending_release: Option<LatestRelease>,
    last_error: Option<String>,
    applying: bool,
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
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(run_check(&current_version, force));
        });
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

    pub fn available_version(&self) -> Option<&semver::Version> {
        self.available.as_ref().map(|r| &r.version)
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

#[cfg(windows)]
fn run_check(current_version: &str, force: bool) -> UpdateEvent {
    let path = nicti_shed::state::default_path();
    let now = nicti_shed::state::now_unix();
    let mut state = path
        .as_deref()
        .map(nicti_shed::state::load)
        .unwrap_or_default();

    if !force && !state.should_check_now(now) {
        return UpdateEvent::UpToDate;
    }

    let result = match semver::Version::parse(current_version) {
        Ok(current) => nicti_shed::net::check_for_update(&current),
        Err(e) => Err(nicti_shed::ShedError::InvalidVersion(e.to_string())),
    };

    state.mark_checked(now);
    if let Some(path) = &path {
        let _ = nicti_shed::state::save(path, &state);
    }

    match result {
        Ok(Some(release)) => UpdateEvent::Available(release),
        Ok(None) => UpdateEvent::UpToDate,
        Err(e) => UpdateEvent::Failed(e.to_string()),
    }
}

#[cfg(not(windows))]
fn run_check(_current_version: &str, _force: bool) -> UpdateEvent {
    UpdateEvent::UpToDate
}

#[cfg(windows)]
fn apply_update(release: LatestRelease) -> Result<(), String> {
    nicti_shed::net::download_and_apply(&release).map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn apply_update(_release: LatestRelease) -> Result<(), String> {
    Err("updates are only supported on Windows".to_string())
}
