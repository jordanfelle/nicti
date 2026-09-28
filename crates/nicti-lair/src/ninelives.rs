//! Nine Lives (#25/ADR-0025): continuous, crash-safe catalog backup with no "optimize on exit"
//! step -- `docs/benchmarks.md`'s Maintenance target. A cat has nine lives; a verified backup is a
//! spare life for the catalog, so this module keeps the newest `keep` (default 9) of them.
//!
//! The mechanism is SQLite's own `VACUUM INTO` (ADR-0067's "#25's backup approach" bullet): an
//! online, no-lock snapshot taken from a *second*, independent read-only connection
//! (`SqliteCatalog::open_snapshot_reader`), never the shared `Mutex<Connection>` every other
//! catalog query goes through -- a ~2s snapshot at 2M rows (ADR-0067's own measured figure) would
//! otherwise freeze the whole catalog for its duration. Each snapshot is written to a `.partial`
//! file first, checked (`PRAGMA integrity_check` plus a `user_version` match against the live
//! catalog), and only renamed into its final name once it passes -- a copy that fails verification
//! is deleted, and every existing good backup is left untouched either way.
//!
//! [`NineLives`] is the "when should one run" scheduler half; [`run_backup`] (and its constituent
//! steps `snapshot_into`/`verify`/`rotate`, reused by `pounce_jobs::BackupJob` for cooperative
//! chunking) is the "how a run actually happens" half.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags};

use crate::sqlite::first_check_problem;
use crate::{CatalogError, SqliteCatalog};

/// How often a due check can fire a fresh backup, absent any other reason to skip one -- 15
/// minutes, matching "continuous" without vacuuming on literally every write.
pub const DEFAULT_INTERVAL_SECS: i64 = 15 * 60;
/// How many verified backups to keep before pruning the oldest -- a cat's nine lives.
pub const DEFAULT_KEEP: usize = 9;

/// Where backups live and how often/how many to keep. `stem` names the backup files themselves
/// (`<stem>.<unix-seconds>.sqlite`) -- kept separate from `dir` (rather than derived from it every
/// time) since a caller building a policy by hand (tests, a future "back up to another drive"
/// picker) may want a `dir` that doesn't share the catalog's own file-name-derived default.
#[derive(Debug, Clone)]
pub struct BackupPolicy {
    pub dir: PathBuf,
    pub stem: String,
    pub interval_secs: i64,
    pub keep: usize,
}

impl BackupPolicy {
    pub fn new(dir: impl Into<PathBuf>, stem: impl Into<String>) -> Self {
        BackupPolicy {
            dir: dir.into(),
            stem: stem.into(),
            interval_secs: DEFAULT_INTERVAL_SECS,
            keep: DEFAULT_KEEP,
        }
    }

    /// The default location: `<catalog dir>/<catalog file name>-backups/`, named from the
    /// catalog's own file stem. A picker UI for backing up to a different drive entirely is
    /// future work (ADR-0025's own "Files" list flags this) -- this default is what a caller gets
    /// without one.
    pub fn for_catalog(catalog_path: &Path) -> Self {
        let file_name = catalog_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("nicti.catalog.sqlite");
        let dir = catalog_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{file_name}-backups"));
        let stem = catalog_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("nicti")
            .to_string();
        Self::new(dir, stem)
    }
}

/// One verified backup already on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupEntry {
    pub path: PathBuf,
    pub created_unix: i64,
}

/// Every verified backup (`<stem>.<epoch>.sqlite`, no `.partial` suffix) under `policy.dir`,
/// oldest first. A missing `dir` (nothing has ever backed up here) is `Ok(vec![])`, not an error.
pub fn list_verified(policy: &BackupPolicy) -> Result<Vec<BackupEntry>, CatalogError> {
    let entries = match fs::read_dir(&policy.dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(CatalogError::Io(e.to_string())),
    };
    let prefix = format!("{}.", policy.stem);
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| CatalogError::Io(e.to_string()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        let Some(epoch_str) = rest.strip_suffix(".sqlite") else {
            continue;
        };
        if let Ok(created_unix) = epoch_str.parse::<i64>() {
            out.push(BackupEntry {
                path: entry.path(),
                created_unix,
            });
        }
    }
    out.sort_by_key(|e| e.created_unix);
    Ok(out)
}

/// Deletes every `<stem>.*.sqlite.partial` file under `policy.dir` -- a snapshot in progress when
/// the process died (a crash mid-`VACUUM INTO`, or a `BackupJob` cancelled between chunks, neither
/// of which gets a synchronous cleanup callback: `nicti_pounce::ChunkedJob` has no on-cancel hook,
/// only "the scheduler stops calling `step()` again," see its own doc comment). Run at the start
/// of every [`run_backup`]/`BackupJob` so an abandoned copy never lingers past the *next* attempt,
/// even though it can't be cleaned up the instant a cancellation happens. Returns how many were
/// removed; a missing `dir` is `Ok(0)`.
pub fn cleanup_stale_partials(policy: &BackupPolicy) -> Result<u64, CatalogError> {
    let entries = match fs::read_dir(&policy.dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(CatalogError::Io(e.to_string())),
    };
    let prefix = format!("{}.", policy.stem);
    let mut removed = 0u64;
    for entry in entries {
        let entry = entry.map_err(|e| CatalogError::Io(e.to_string()))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.starts_with(&prefix)
            && name.ends_with(".sqlite.partial")
            && fs::remove_file(entry.path()).is_ok()
        {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Picks a `.partial` target name whose epoch collides with neither an existing `.partial` at
/// that epoch nor an existing *final* `<stem>.<epoch>.sqlite` -- checking only the former was a
/// real gap an adversarial review caught: a rerun landing on an epoch that already has a verified
/// backup would still write and verify its `.partial` there without incident, then `rotate`'s
/// `fs::rename` would silently overwrite that already-verified backup (`rename`/`MoveFileExW`
/// both replace an existing destination with no error), destroying it with no record that it
/// happened. Bumps by a second at a time on either collision -- far rarer than
/// `policy.interval_secs` in real scheduled use, but reachable by a caller-chosen `now_unix`
/// (tests, or a future manual "back up now" button ADR-0025 flags as intended future work).
fn unique_partial_path(policy: &BackupPolicy, now_unix: i64) -> PathBuf {
    let mut epoch = now_unix;
    loop {
        let partial = policy
            .dir
            .join(format!("{}.{epoch}.sqlite.partial", policy.stem));
        let final_name = policy.dir.join(format!("{}.{epoch}.sqlite", policy.stem));
        if !partial.exists() && !final_name.exists() {
            return partial;
        }
        epoch += 1;
    }
}

/// Shared retry budget for the two Windows helpers below: 50 attempts, 100ms apart (~5s worst
/// case). **History, corrected**: three consecutive commits chased `cargo test (windows)` failing
/// two `ninelives` tests with an identical `Io("Access is denied. (os error 5)")` under the theory
/// that a freshly-written/renamed file was hitting a *transient* antivirus-scan lock, raising this
/// budget each time it didn't help. It never helped because that theory was wrong: the actual
/// failure was `snapshot_into`'s own now-deleted manual fsync step, which opened its target file
/// read-only via `std::fs::File::open` and then called `.sync_all()` on it -- Windows'
/// `FlushFileBuffers` (what `sync_all` calls there) requires a handle opened with write access, so
/// that call failed with `ERROR_ACCESS_DENIED` *deterministically*, every single time, not
/// transiently. No amount of retrying a permanently-failing call can fix it; the tell, in
/// hindsight, was that the whole test suite kept finishing in well under a second across all three
/// "fixes," meaning these retry loops' own guards were never actually matching and engaging at
/// all. That step is gone now (see `snapshot_into`'s own doc comment). What's left here --
/// `rotate`'s `fs::rename` and `verify`'s `Connection::open_with_flags`, both reopening a file
/// `snapshot_into` (or a prior `rotate`) just finished writing -- was never actually confirmed to
/// need retrying by any real CI failure (both are downstream of the one call that *was* broken and
/// panicking first, so neither one was ever actually exercised on a real failure in these three
/// runs); a genuine antivirus-scan race remains a real, if unconfirmed, possibility for a plain
/// reopen of a fresh file specifically (unlike the deleted fsync step, neither of these opens the
/// file in an access mode any documented Windows API restricts), and retrying is cheap insurance
/// on a background-job-only code path either way, so both retries are kept rather than removed on
/// the strength of "we were wrong once already."
const RETRY_MAX_ATTEMPTS: u32 = 50;
const RETRY_DELAY: Duration = Duration::from_millis(100);

/// See [`RETRY_MAX_ATTEMPTS`]'s own doc comment for this whole retry family's real (and corrected)
/// history. Guards on the typed `io::ErrorKind::PermissionDenied` (`ERROR_ACCESS_DENIED`,
/// `os error 5`), the shape a genuine transient antivirus-scan lock on a freshly-written file
/// would take if `rotate`'s `fs::rename` call site ever actually hits one.
fn retry_on_transient_access_denied<T>(
    mut f: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut last_err = None;
    for _ in 0..RETRY_MAX_ATTEMPTS {
        match f() {
            Ok(value) => return Ok(value),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                last_err = Some(e);
                std::thread::sleep(RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("loop only exits via return, or after at least one PermissionDenied"))
}

/// The `rusqlite`-flavored twin of [`retry_on_transient_access_denied`] -- `verify`'s own
/// `Connection::open_with_flags` on a just-written `.partial` can hit the identical Windows
/// antivirus-scan race, surfaced as a `rusqlite::Error` rather than a bare `std::io::Error`.
/// Matches on the typed `ErrorCode` (`PermissionDenied`, or `CannotOpen` -- SQLite's Windows VFS
/// maps a `CreateFile` failure while opening a database file to `SQLITE_CANTOPEN`, not always
/// `SQLITE_IOERR`/`PermissionDenied`), never on the error's rendered message text: an adversarial
/// review of this fix caught that a string-`contains` check on "Access is denied" would be both
/// locale-dependent and, if SQLite's actual message text turned out not to include that exact
/// substring, silently never retry at all -- this typed check has neither problem.
fn retry_rusqlite_open_on_transient_access_denied(
    mut f: impl FnMut() -> rusqlite::Result<Connection>,
) -> rusqlite::Result<Connection> {
    let mut last_err = None;
    for _ in 0..RETRY_MAX_ATTEMPTS {
        match f() {
            Ok(conn) => return Ok(conn),
            Err(rusqlite::Error::SqliteFailure(ffi_err, msg))
                if matches!(
                    ffi_err.code,
                    rusqlite::ErrorCode::PermissionDenied | rusqlite::ErrorCode::CannotOpen
                ) =>
            {
                last_err = Some(rusqlite::Error::SqliteFailure(ffi_err, msg));
                std::thread::sleep(RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.expect("loop only exits via return, or after at least one matching error"))
}

/// Writes a fresh, unverified snapshot of `catalog` to a new `.partial` file under `policy.dir`,
/// returning its path (see the function body's own comment for why this doesn't also do its own
/// separate fsync). Uses `catalog`'s own independent read-only reader connection
/// when it has a backing file (`SqliteCatalog::open_snapshot_reader`) so this never holds the
/// catalog's shared connection mutex for the run's ~2s duration; an in-memory catalog (tests only,
/// `catalog.path()` is `None`) has no file to reopen, so it falls back to
/// `SqliteCatalog::vacuum_into_locked`, which does briefly hold that lock -- acceptable there since
/// nothing else is contending for an in-memory test catalog's connection.
pub fn snapshot_into(
    catalog: &SqliteCatalog,
    policy: &BackupPolicy,
    now_unix: i64,
) -> Result<PathBuf, CatalogError> {
    fs::create_dir_all(&policy.dir).map_err(|e| CatalogError::Io(e.to_string()))?;
    let partial = unique_partial_path(policy, now_unix);

    let target = partial
        .to_str()
        .ok_or_else(|| CatalogError::Io("backup target path is not valid UTF-8".into()))?;

    if catalog.path().is_some() {
        let reader = catalog.open_snapshot_reader()?;
        reader.execute("VACUUM INTO ?1", params![target])?;
    } else {
        catalog.vacuum_into_locked(&partial)?;
    }

    // No separate manual fsync here (an earlier version of this function had one, opening the
    // freshly-written `.partial` via `std::fs::File::open` and calling `.sync_all()` on it) --
    // deleted, not retried, once its real root cause was found: `File::open` opens read-only, and
    // Windows' `FlushFileBuffers` (what `sync_all` calls there) requires a handle opened with
    // write access, so that call failed with `ERROR_ACCESS_DENIED` *deterministically*, every
    // single time, on every Windows CI run -- not the transient antivirus-scan race three
    // consecutive commits assumed while chasing this the wrong way (raising a retry budget can't
    // fix a permanent, unconditional failure; the giveaway, in hindsight, was that the test suite
    // kept finishing in well under a second even with a 5-second retry budget in place, meaning
    // the retry loop's guard was never even matching). `VACUUM INTO` already fsyncs the target
    // internally per SQLite's own documented behavior, which is what this module actually relies
    // on for durability -- the deleted step was redundant defense-in-depth that turned out to
    // actively break Windows instead of adding anything real.
    Ok(partial)
}

/// Checks a freshly written `.partial` snapshot: a full `PRAGMA integrity_check` on the copy
/// itself (never the live catalog -- that's `SqliteCatalog::quick_check`'s job, run *before* the
/// snapshot even starts, see [`run_backup`]), plus a `PRAGMA user_version` match against the live
/// catalog (a `VACUUM INTO` copy always carries the source's schema version; a mismatch means this
/// read a different/stale file, not a real migration race). `Ok(None)` means the copy is good;
/// `Ok(Some(msg))` names the first problem found.
pub fn verify(catalog: &SqliteCatalog, partial: &Path) -> Result<Option<String>, CatalogError> {
    // Retried: the same Windows antivirus-scan race `retry_on_transient_access_denied` documents,
    // hitting this open instead of the fsync in `snapshot_into` -- opens the same just-written
    // file, just moments later.
    let reader = retry_rusqlite_open_on_transient_access_denied(|| {
        Connection::open_with_flags(partial, OpenFlags::SQLITE_OPEN_READ_ONLY)
    })?;
    if let Some(problem) = first_check_problem(&reader, "integrity_check")? {
        return Ok(Some(problem));
    }
    let backup_version: i64 = reader.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let live_version = catalog.user_version()?;
    if backup_version != live_version {
        return Ok(Some(format!(
            "backup user_version {backup_version} does not match live catalog's {live_version}"
        )));
    }
    Ok(None)
}

/// Renames a verified `.partial` to its final name (atomic on the same volume -- `policy.dir` is
/// always alongside the catalog by default, see [`BackupPolicy::for_catalog`]), then prunes the
/// oldest verified backups down to `policy.keep`. Pruning only ever removes backups *older* than
/// the one just rotated in, and only after it's already verified and renamed -- a run that fails
/// verification never reaches this function, so a bad copy can never push out a good one.
pub fn rotate(policy: &BackupPolicy, partial: &Path) -> Result<(PathBuf, u64), CatalogError> {
    let final_name = partial
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".partial"))
        .ok_or_else(|| CatalogError::Io("malformed backup partial filename".into()))?
        .to_string();
    let final_path = policy.dir.join(final_name);
    // Retried for the same reason as `snapshot_into`'s fsync and `verify`'s open -- see
    // `retry_on_transient_access_denied`'s own doc comment.
    retry_on_transient_access_denied(|| fs::rename(partial, &final_path))
        .map_err(|e| CatalogError::Io(e.to_string()))?;

    let verified = list_verified(policy)?; // ascending by created_unix
    let mut pruned = 0u64;
    if verified.len() > policy.keep {
        for oldest in &verified[..verified.len() - policy.keep] {
            if fs::remove_file(&oldest.path).is_ok() {
                pruned += 1;
            }
        }
    }
    Ok((final_path, pruned))
}

/// What one [`run_backup`] (or `BackupJob`) attempt did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupOutcome {
    /// A new snapshot was written, verified, and rotated in.
    Verified(PathBuf),
    /// The live catalog's own `quick_check` reported a problem before any snapshot was attempted
    /// -- every existing verified backup is untouched.
    LiveCorrupt(String),
    /// The written copy failed `verify` (integrity_check or a `user_version` mismatch) and was
    /// deleted -- every existing verified backup is untouched.
    VerifyFailed(String),
    /// A step failed for a reason unrelated to the catalog's or the copy's own integrity (a disk
    /// I/O error, a permission failure, a transient file lock) -- every existing verified backup
    /// is untouched. `pounce_jobs::BackupJob` maps any such error to this variant rather than
    /// letting its own `step()` return `Err`, specifically so the job always reaches `Done` and
    /// its `ReportSlot` always resolves -- an adversarial review caught that a `step()` failure
    /// used to leave the slot permanently unresolved and silently suppress every later scheduled
    /// attempt (see `NineLives::due`'s and `pounce_jobs::BackupJob`'s own doc comments).
    Failed(String),
}

/// The full outcome of one backup attempt, whether run via [`run_backup`] or step-by-step through
/// `pounce_jobs::BackupJob`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupReport {
    pub outcome: BackupOutcome,
    /// Stale `.partial` files this run swept up before doing anything else -- see
    /// [`cleanup_stale_partials`]'s own doc comment for why this can't happen synchronously at
    /// cancellation time instead.
    pub stale_partials_removed: u64,
    /// Old verified backups pruned this run (`0` unless `outcome` is `Verified`).
    pub pruned: u64,
}

/// Runs one full backup attempt end to end: sweep stale partials, check the live catalog, snapshot
/// it, verify the snapshot, and rotate it in. `pounce_jobs::BackupJob` reuses the same constituent
/// functions (`cleanup_stale_partials`/`snapshot_into`/`verify`/`rotate`) split across its own
/// cooperatively-cancellable chunks instead of calling this directly, so a real caller (Pounce's
/// activity panel) sees per-step progress; this function is for callers that don't need that
/// (tests, a hypothetical CLI).
pub fn run_backup(
    catalog: &SqliteCatalog,
    policy: &BackupPolicy,
    now_unix: i64,
) -> Result<BackupReport, CatalogError> {
    let stale_partials_removed = cleanup_stale_partials(policy)?;

    if let Some(problem) = catalog.quick_check()? {
        return Ok(BackupReport {
            outcome: BackupOutcome::LiveCorrupt(problem),
            stale_partials_removed,
            pruned: 0,
        });
    }

    let partial = snapshot_into(catalog, policy, now_unix)?;

    if let Some(problem) = verify(catalog, &partial)? {
        let _ = fs::remove_file(&partial);
        return Ok(BackupReport {
            outcome: BackupOutcome::VerifyFailed(problem),
            stale_partials_removed,
            pruned: 0,
        });
    }

    let (final_path, pruned) = rotate(policy, &partial)?;
    Ok(BackupReport {
        outcome: BackupOutcome::Verified(final_path),
        stale_partials_removed,
        pruned,
    })
}

/// Decides *when* a backup should run -- deliberately separate from [`run_backup`] itself, since
/// `nicti-pelt`'s app loop calls [`NineLives::due`] on a cheap poll (roughly every 30s) and only
/// submits a real `BackupJob` when it says yes.
///
/// Nothing runs on exit, by design (`docs/benchmarks.md`'s "no optimize catalog" target) -- a
/// crash right after the last real change simply means that change gets picked up by the next
/// due check, either later in the same session or the first check of the next one.
pub struct NineLives {
    policy: BackupPolicy,
    /// Whether `due` has been called yet this process. The *first* call after a restart is
    /// special: `SqliteCatalog::change_counter()` is a per-connection counter that always reads 0
    /// right after `open()`, the same as this struct's own freshly-constructed `baseline_changes`
    /// -- so without this flag, a catalog that already has real, never-backed-up changes from a
    /// previous session that crashed before its next scheduled backup would look indistinguishable
    /// from "nothing changed" and `due` would wrongly skip it. Once this flag has fired once, only
    /// a genuine change in `current_changes` since the last completed run matters.
    checked_since_startup: bool,
    baseline_changes: u64,
}

impl NineLives {
    pub fn new(policy: BackupPolicy) -> Self {
        NineLives {
            policy,
            checked_since_startup: false,
            baseline_changes: 0,
        }
    }

    pub fn policy(&self) -> &BackupPolicy {
        &self.policy
    }

    /// `true` if a backup should run right now. Reads `policy.dir` to find the newest verified
    /// backup (cheap: one directory listing, no file content read) -- due whenever:
    /// - no verified backup exists yet, or
    /// - the newest one is at least `policy.interval_secs` old, **and** either this is the first
    ///   check since this `NineLives` was constructed (covers a previous session's changes that
    ///   were never backed up before it crashed/closed) or `current_changes` has moved since
    ///   [`NineLives::record_ran`] was last called (a real change happened this session and
    ///   enough time has passed).
    ///
    /// Mutates this instance's own startup-tracking as a side effect -- call at most once per poll
    /// tick, and call [`NineLives::record_ran`] after actually attempting a run so the next call's
    /// comparison has a fresh baseline.
    pub fn due(&mut self, now_unix: i64, current_changes: u64) -> Result<bool, CatalogError> {
        let newest = list_verified(&self.policy)?.last().map(|e| e.created_unix);
        let is_startup_check = !self.checked_since_startup;
        self.checked_since_startup = true;

        Ok(match newest {
            None => true,
            Some(t) => {
                let interval_elapsed = now_unix.saturating_sub(t) >= self.policy.interval_secs;
                interval_elapsed && (is_startup_check || current_changes != self.baseline_changes)
            }
        })
    }

    /// Records that a backup attempt just ran (whatever its outcome), resetting the "changed since
    /// last backup" baseline so `due` doesn't immediately re-fire on the very next tick.
    pub fn record_ran(&mut self, changes_at_run: u64) {
        self.baseline_changes = changes_at_run;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CatalogStore;

    fn policy(dir: &Path) -> BackupPolicy {
        BackupPolicy::new(dir.to_path_buf(), "cat".to_string())
    }

    #[test]
    fn list_verified_ignores_partials_and_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("cat.100.sqlite"), b"x").unwrap();
        std::fs::write(dir.path().join("cat.200.sqlite.partial"), b"x").unwrap();
        std::fs::write(dir.path().join("other.100.sqlite"), b"x").unwrap();
        std::fs::write(dir.path().join("readme.txt"), b"x").unwrap();

        let entries = list_verified(&policy(dir.path())).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].created_unix, 100);
    }

    #[test]
    fn list_verified_on_missing_dir_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let entries = list_verified(&policy(&missing)).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn cleanup_stale_partials_removes_only_partials_for_this_stem() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("cat.100.sqlite.partial"), b"x").unwrap();
        std::fs::write(dir.path().join("cat.200.sqlite"), b"x").unwrap();
        std::fs::write(dir.path().join("other.100.sqlite.partial"), b"x").unwrap();

        let removed = cleanup_stale_partials(&policy(dir.path())).unwrap();
        assert_eq!(removed, 1);
        assert!(!dir.path().join("cat.100.sqlite.partial").exists());
        assert!(dir.path().join("cat.200.sqlite").exists());
        assert!(dir.path().join("other.100.sqlite.partial").exists());
    }

    #[test]
    fn due_is_true_with_no_backups_yet() {
        let dir = tempfile::tempdir().unwrap();
        let mut nine = NineLives::new(policy(dir.path()));
        assert!(nine.due(1_000, 0).unwrap());
    }

    #[test]
    fn due_is_false_before_the_interval_elapses() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = policy(dir.path());
        p.interval_secs = 900;
        std::fs::create_dir_all(&p.dir).unwrap();
        std::fs::write(p.dir.join("cat.1000.sqlite"), b"x").unwrap();

        let mut nine = NineLives::new(p);
        assert!(!nine.due(1_100, 5).unwrap());
    }

    #[test]
    fn due_fires_on_startup_once_interval_has_elapsed_even_with_no_local_changes() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = policy(dir.path());
        p.interval_secs = 900;
        std::fs::create_dir_all(&p.dir).unwrap();
        std::fs::write(p.dir.join("cat.1000.sqlite"), b"x").unwrap();

        // A previous session made changes and then crashed before its next scheduled backup --
        // this session's `current_changes` starts at 0, same as `baseline_changes`, but the first
        // check after the interval has elapsed must still fire.
        let mut nine = NineLives::new(p);
        assert!(nine.due(1_000 + 900, 0).unwrap());
    }

    #[test]
    fn due_is_false_after_startup_check_if_nothing_changed_since() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = policy(dir.path());
        p.interval_secs = 900;
        std::fs::create_dir_all(&p.dir).unwrap();
        std::fs::write(p.dir.join("cat.1000.sqlite"), b"x").unwrap();

        let mut nine = NineLives::new(p);
        assert!(nine.due(1_000 + 900, 0).unwrap()); // startup check fires
        nine.record_ran(0);
        assert!(!nine.due(1_000 + 1800, 0).unwrap()); // interval elapsed again, but no changes
    }

    #[test]
    fn due_fires_once_interval_elapsed_and_changes_moved() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = policy(dir.path());
        p.interval_secs = 900;
        std::fs::create_dir_all(&p.dir).unwrap();
        std::fs::write(p.dir.join("cat.1000.sqlite"), b"x").unwrap();

        let mut nine = NineLives::new(p);
        // Changed already (current_changes=5 differs from the default baseline of 0), but the
        // interval hasn't elapsed yet -- not due. This call also consumes the one-time "startup"
        // check, so the next call below is exercising the plain changed+elapsed path, not the
        // startup path `due_fires_on_startup_once_interval_has_elapsed_even_with_no_local_changes`
        // already covers.
        assert!(!nine.due(1_000 + 500, 5).unwrap());
        // Now the interval has elapsed too, with the same unrecorded change.
        assert!(nine.due(1_000 + 900, 5).unwrap());
    }

    #[test]
    fn snapshot_verify_rotate_round_trip_on_a_file_backed_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = dir.path().join("test.catalog.sqlite");
        let catalog = SqliteCatalog::open(&catalog_path).unwrap();
        catalog
            .upsert_volume("vol", None, None, 1000)
            .expect("seed a row so the snapshot has real content");

        let mut policy = BackupPolicy::for_catalog(&catalog_path);
        policy.keep = 2;

        let report = run_backup(&catalog, &policy, 1_000).unwrap();
        let BackupOutcome::Verified(path) = report.outcome else {
            panic!("expected Verified, got {:?}", report.outcome);
        };
        assert!(path.exists());
        assert!(path.to_str().unwrap().ends_with(".sqlite"));
        assert_eq!(report.pruned, 0);

        // The backup is a real, independently-openable catalog with the seeded row in it.
        // Reopening a file this soon after `rotate` renamed it into place can hit the same
        // transient Windows antivirus-scan race `retry_rusqlite_open_on_transient_access_denied`
        // documents (this test's own reopen isn't inside that helper's coverage, since
        // `SqliteCatalog::open` returns `CatalogError`, not a bare `rusqlite::Result`) -- retried
        // here rather than in library code, since no real production caller reopens a backup file
        // this immediately today. Scoped to the same typed `ErrorCode` shape as the production
        // helpers, not "any `CatalogError`" -- an adversarial review caught that retrying on
        // literally any error here would mask a genuine regression (corruption, a real logic bug)
        // behind pointless retries instead of failing on the very first attempt. Same
        // `RETRY_MAX_ATTEMPTS`/`RETRY_DELAY` budget as the production helpers.
        let mut attempts_left = RETRY_MAX_ATTEMPTS;
        let reopened = loop {
            match SqliteCatalog::open(&path) {
                Ok(catalog) => break catalog,
                Err(CatalogError::Sqlite(rusqlite::Error::SqliteFailure(ref ffi_err, _)))
                    if attempts_left > 1
                        && matches!(
                            ffi_err.code,
                            rusqlite::ErrorCode::PermissionDenied | rusqlite::ErrorCode::CannotOpen
                        ) =>
                {
                    attempts_left -= 1;
                    std::thread::sleep(RETRY_DELAY);
                }
                Err(e) => panic!("failed to reopen the backup after retrying: {e}"),
            }
        };
        assert_eq!(reopened.asset_count().unwrap(), 0); // no assets, but this proves it opens
    }

    #[test]
    fn unique_partial_path_skips_an_epoch_whose_final_name_already_exists() {
        // An adversarial review caught this: the original version only checked for a colliding
        // `.partial`, never the *final* name a same-epoch rerun would eventually `rotate` into --
        // which would silently overwrite an already-verified backup via `fs::rename`.
        let dir = tempfile::tempdir().unwrap();
        let p = policy(dir.path());
        std::fs::create_dir_all(&p.dir).unwrap();
        std::fs::write(p.dir.join("cat.1000.sqlite"), b"already verified").unwrap();

        let candidate = unique_partial_path(&p, 1_000);
        assert_eq!(candidate, p.dir.join("cat.1001.sqlite.partial"));
    }

    #[test]
    fn rerunning_snapshot_at_the_same_epoch_never_overwrites_the_earlier_verified_backup() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = dir.path().join("test.catalog.sqlite");
        let catalog = SqliteCatalog::open(&catalog_path).unwrap();
        catalog.upsert_volume("vol", None, None, 1000).unwrap();

        let policy = BackupPolicy::for_catalog(&catalog_path);
        let first = run_backup(&catalog, &policy, 1_000).unwrap();
        let BackupOutcome::Verified(first_path) = first.outcome else {
            panic!("expected the first run to succeed, got {:?}", first.outcome);
        };
        // See `retry_on_transient_access_denied`'s own doc comment -- reading a file this soon
        // after `rotate` renamed it into place can hit the same transient Windows race.
        let first_contents = retry_on_transient_access_denied(|| fs::read(&first_path)).unwrap();

        // Same `now_unix` as the first run -- this used to overwrite `first_path` via `rotate`'s
        // `fs::rename`.
        let second = run_backup(&catalog, &policy, 1_000).unwrap();
        let BackupOutcome::Verified(second_path) = second.outcome else {
            panic!(
                "expected the second run to succeed, got {:?}",
                second.outcome
            );
        };

        assert_ne!(first_path, second_path);
        assert!(first_path.exists(), "the first run's backup must survive");
        assert_eq!(
            retry_on_transient_access_denied(|| fs::read(&first_path)).unwrap(),
            first_contents,
            "the first run's backup content must be untouched"
        );
    }

    #[test]
    fn rotate_prunes_down_to_keep_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut p = policy(dir.path());
        p.keep = 2;
        std::fs::create_dir_all(&p.dir).unwrap();
        for epoch in [100, 200, 300] {
            std::fs::write(p.dir.join(format!("cat.{epoch}.sqlite")), b"x").unwrap();
        }
        let partial = p.dir.join("cat.400.sqlite.partial");
        std::fs::write(&partial, b"x").unwrap();

        let (final_path, pruned) = rotate(&p, &partial).unwrap();
        assert_eq!(final_path, p.dir.join("cat.400.sqlite"));
        assert_eq!(pruned, 2);
        let remaining = list_verified(&p).unwrap();
        assert_eq!(
            remaining.iter().map(|e| e.created_unix).collect::<Vec<_>>(),
            vec![300, 400]
        );
    }

    #[test]
    fn verify_rejects_a_corrupted_partial_without_touching_good_backups() {
        let dir = tempfile::tempdir().unwrap();
        let catalog_path = dir.path().join("test.catalog.sqlite");
        let catalog = SqliteCatalog::open(&catalog_path).unwrap();

        let policy = BackupPolicy::for_catalog(&catalog_path);
        std::fs::create_dir_all(&policy.dir).unwrap();
        let good = policy.dir.join(format!("{}.100.sqlite", policy.stem));
        std::fs::write(
            &good,
            b"not a real sqlite file either, but it's the 'existing good backup'",
        )
        .unwrap();

        let partial = policy
            .dir
            .join(format!("{}.200.sqlite.partial", policy.stem));
        std::fs::write(&partial, b"definitely not a valid sqlite database").unwrap();

        let problem = verify(&catalog, &partial);
        assert!(problem.is_err() || matches!(problem, Ok(Some(_))));
        assert!(
            good.exists(),
            "an existing backup must survive a failed verify of a new one"
        );
    }
}
