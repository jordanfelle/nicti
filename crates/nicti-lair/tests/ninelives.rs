//! Integration tests for #25's Nine Lives backup (`nicti_lair::ninelives`) against a real,
//! file-backed SQLite catalog -- unit tests in `ninelives.rs` itself already cover the pure
//! filename/scheduling logic; these exercise the parts that need a real file on disk: a
//! concurrent writer during the snapshot, a corrupted live catalog, stale `.partial` cleanup
//! across two separate runs, and `BackupJob` driven through a real `nicti_pounce::Pounce` the way
//! the activity panel actually would.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nicti_lair::ninelives::{run_backup, BackupOutcome, BackupPolicy};
use nicti_lair::pounce_jobs::BackupJob;
use nicti_lair::{CatalogStore, SqliteCatalog};
use nicti_pounce::{JobState, Pounce};

fn wait_until(mut cond: impl FnMut() -> bool, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

/// Stops and joins a background writer thread when dropped, so a panic in the test body can't
/// leave it detached and still inserting rows while other tests run.
struct WriterGuard {
    stop: Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WriterGuard {
    /// Signals the writer to stop and waits for it (propagating a panic inside the writer).
    fn finish(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            handle.join().unwrap();
        }
    }
}

impl Drop for WriterGuard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            // Already unwinding (or `finish` already ran): don't turn a second panic into an abort.
            let _ = handle.join();
        }
    }
}

fn open_catalog(dir: &Path) -> (SqliteCatalog, std::path::PathBuf) {
    let path = dir.join("test.catalog.sqlite");
    let catalog = SqliteCatalog::open(&path).unwrap();
    (catalog, path)
}

#[test]
fn snapshot_completes_without_waiting_for_a_slower_concurrent_writer() {
    let dir = tempfile::tempdir().unwrap();
    let (catalog, catalog_path) = open_catalog(dir.path());
    let catalog = Arc::new(catalog);
    let volume_id = catalog.upsert_volume("vol", None, None, 1000).unwrap();

    let policy = BackupPolicy::for_catalog(&catalog_path);

    // A writer thread that keeps writing, paced by a small sleep, until told to stop. It runs for
    // exactly as long as the backup does, whatever the hardware: an earlier version raced a fixed
    // ~600 ms of paced writes against `run_backup`'s own duration and flaked (#314) whenever a
    // loaded CI runner made the backup slower than the writer's budget.
    const WRITER_DELAY: Duration = Duration::from_millis(3);

    // Give the backup real work: a few dozen MB of filler pages in the same database file (a
    // second connection is fine alongside `catalog`'s own under WAL), so `VACUUM INTO` takes
    // long enough that a writer *blocked* behind the snapshot is unmistakable. Without this the
    // catalog is a few KB, the backup takes ~3 ms, and the assertion below would pass whether or
    // not the snapshot held the shared connection (verified by mutation).
    {
        let filler = rusqlite::Connection::open(&catalog_path).unwrap();
        filler
            .execute("CREATE TABLE filler (b BLOB NOT NULL)", [])
            .unwrap();
        filler.execute("BEGIN", []).unwrap();
        for _ in 0..96 {
            filler
                .execute("INSERT INTO filler VALUES (zeroblob(1048576))", [])
                .unwrap();
        }
        filler.execute("COMMIT", []).unwrap();
    }
    let progress = Arc::new(AtomicUsize::new(0));
    // When each write *completed*. A writer blocked behind the snapshot only completes its write
    // once the backup releases the connection, so its timestamp lands after the backup's end;
    // counting bare progress instead would credit that write to the backup window, because the
    // unblocked writer can win the race to the counter (verified by mutation).
    let stamps = Arc::new(std::sync::Mutex::new(Vec::<Instant>::new()));
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let writer_catalog = catalog.clone();
    let writer_progress = progress.clone();
    let writer_stamps = stamps.clone();
    let writer_stop = stop.clone();
    let writer = std::thread::spawn(move || {
        let mut i = 0usize;
        while !writer_stop.load(Ordering::SeqCst) {
            std::thread::sleep(WRITER_DELAY);
            writer_catalog
                .ensure_root(volume_id, &format!("root-{i}"))
                .unwrap();
            writer_stamps.lock().unwrap().push(Instant::now());
            writer_progress.fetch_add(1, Ordering::SeqCst);
            i += 1;
        }
    });
    let mut writer = WriterGuard {
        stop: stop.clone(),
        handle: Some(writer),
    };

    // Wait until the writer has genuinely started (and is now mid-pace) before snapshotting, so
    // the assertion below can't pass merely because the writer thread hadn't been scheduled yet.
    assert!(
        wait_until(
            || progress.load(Ordering::SeqCst) > 0,
            Duration::from_secs(30)
        ),
        "writer thread never made any progress"
    );

    let started = Instant::now();
    let report = run_backup(&catalog, &policy, 1_000).unwrap();
    let ended = Instant::now();
    let backup_took = ended - started;

    writer.finish();
    // The longest stretch inside the backup window in which the writer completed nothing. The
    // snapshot only holds the shared connection for the `VACUUM INTO` step (not the verification
    // that follows), so a *count* of writes during the backup can't tell the cases apart -- a
    // blocked writer still lands writes before and after the lock. A stall does show up as one
    // long gap between consecutive writes, of about the `VACUUM INTO`'s duration, while a paced
    // 3 ms writer that is never blocked has only scheduler-noise gaps.
    let max_gap = {
        let stamps = stamps.lock().unwrap();
        let mut points = vec![started];
        points.extend(
            stamps
                .iter()
                .copied()
                .filter(|t| *t > started && *t < ended),
        );
        points.push(ended);
        points
            .windows(2)
            .map(|w| w[1] - w[0])
            .max()
            .unwrap_or_default()
    };
    // The property: `run_backup` (specifically its `snapshot_into` step, which opens its own
    // independent read-only connection for a file-backed catalog rather than reusing the shared
    // `Mutex<Connection>` -- see that function's own doc comment) must not hold the connection
    // every `ensure_root` call goes through. If it did, the writer would make *no* progress for
    // for the duration of the `VACUUM INTO` -- one long gap between its paced 3 ms writes.
    assert!(
        backup_took >= Duration::from_millis(50),
        "the backup took only {backup_took:?}: too fast for this test to mean anything -- grow \
         the filler"
    );
    // Measured (mutating `snapshot_into` to use the shared connection): the longest writer stall
    // is ~80% of the backup when the snapshot blocks it, ~15% when it doesn't -- so "under half"
    // separates them with margin on both sides, and scales with the machine instead of racing a
    // fixed budget (#314).
    assert!(
        max_gap * 2 < backup_took,
        "the writer stalled for {max_gap:?} of a {backup_took:?} backup -- something now blocks \
         writers behind the snapshot"
    );

    assert!(matches!(report.outcome, BackupOutcome::Verified(_)));
}

#[test]
fn run_backup_reports_live_corruption_without_touching_existing_backups() {
    let dir = tempfile::tempdir().unwrap();
    let (catalog, catalog_path) = open_catalog(dir.path());
    catalog.upsert_volume("vol", None, None, 1000).unwrap();

    let policy = BackupPolicy::for_catalog(&catalog_path);
    let first = run_backup(&catalog, &policy, 1_000).unwrap();
    let BackupOutcome::Verified(first_path) = first.outcome else {
        panic!("expected the first run to succeed, got {:?}", first.outcome);
    };

    // Force WAL-mode's actual page data out of the `-wal` file and into the main database file --
    // otherwise the handful of rows this test writes may still live entirely in the WAL, and
    // corrupting the main file's bytes below would corrupt nothing real. A plain second connection
    // onto the same path can run this; WAL mode allows it alongside `catalog`'s own connection.
    rusqlite::Connection::open(&catalog_path)
        .unwrap()
        .pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .unwrap();

    // Corrupt the *live* catalog file's page data directly on disk underneath `catalog`'s own
    // still-open connection (never closed/reopened here -- this simulates real bit-rot/hardware
    // corruption happening to a file a running app already has open, not a corrupt file being
    // freshly opened) -- crude, but it's the only way to make a real `PRAGMA quick_check` fail
    // short of vendoring a purpose-built corrupt fixture.
    //
    // Smash the header of *every* page after the first (page 1 holds the database header and
    // `sqlite_master`, left intact so the file still opens). An earlier version overwrote 1 KB at
    // the file's midpoint, which only corrupted anything if that spot happened to land inside a
    // page `quick_check` inspects -- schema V7's extra (empty) index pages moved the midpoint into
    // unused space and the test stopped detecting anything. Every non-first page's type byte
    // becoming 0xFF is an invalid page whatever the layout is.
    let mut bytes = std::fs::read(&catalog_path).unwrap();
    let page_size = match u16::from_be_bytes([bytes[16], bytes[17]]) {
        1 => 65_536,
        n => n as usize,
    };
    for page_start in (page_size..bytes.len()).step_by(page_size) {
        let end = (page_start + 64).min(bytes.len());
        bytes[page_start..end].fill(0xFF);
    }
    std::fs::write(&catalog_path, &bytes).unwrap();

    let second = run_backup(&catalog, &policy, 2_000).unwrap();
    assert!(
        matches!(second.outcome, BackupOutcome::LiveCorrupt(_)),
        "expected LiveCorrupt, got {:?}",
        second.outcome
    );
    assert!(
        first_path.exists(),
        "the earlier good backup must survive a live-corruption run"
    );
}

#[test]
fn a_partial_left_by_an_abandoned_run_is_swept_up_by_the_next_one() {
    let dir = tempfile::tempdir().unwrap();
    let (catalog, catalog_path) = open_catalog(dir.path());
    catalog.upsert_volume("vol", None, None, 1000).unwrap();

    let policy = BackupPolicy::for_catalog(&catalog_path);
    std::fs::create_dir_all(&policy.dir).unwrap();
    let abandoned = policy
        .dir
        .join(format!("{}.500.sqlite.partial", policy.stem));
    std::fs::write(&abandoned, b"leftover from a run that never finished").unwrap();

    let report = run_backup(&catalog, &policy, 1_000).unwrap();
    assert_eq!(report.stale_partials_removed, 1);
    assert!(!abandoned.exists());
    assert!(matches!(report.outcome, BackupOutcome::Verified(_)));
}

#[test]
fn backup_job_driven_through_pounce_produces_a_verified_backup() {
    let dir = tempfile::tempdir().unwrap();
    let (catalog, catalog_path) = open_catalog(dir.path());
    let catalog = Arc::new(catalog);
    catalog.upsert_volume("vol", None, None, 1000).unwrap();

    let policy = BackupPolicy::for_catalog(&catalog_path);
    let pounce = Pounce::new(u64::MAX, 2, 2, || {});
    let (job, result) = BackupJob::new(catalog.clone(), policy.clone(), 1_000);
    let id = pounce.submit(Box::new(job));

    assert!(
        wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| s.id == id && s.state == JobState::Done),
            Duration::from_secs(5)
        ),
        "BackupJob never reached Done"
    );
    pounce.shutdown();

    let report = result
        .lock()
        .unwrap()
        .take()
        .expect("a Done job leaves its report");
    assert!(matches!(report.outcome, BackupOutcome::Verified(_)));
}

#[test]
fn backup_job_resolves_its_report_slot_even_when_a_step_fails() {
    // An adversarial review caught that `BackupJob::step` used to return `Err` on a genuine I/O
    // failure -- `nicti_pounce::Pounce` marks a job `JobState::Failed` on that and never calls
    // back into it, so its `ReportSlot` was left permanently unresolved (see this test's own name
    // and `docs/adr/0025-continuous-catalog-backup.md`'s "Review findings" section). Forces a
    // failure in the very first (`QuickCheck`) chunk by making `policy.dir` a plain file rather
    // than a directory, so `ninelives::cleanup_stale_partials`'s own `fs::read_dir` call errors.
    let dir = tempfile::tempdir().unwrap();
    let (catalog, catalog_path) = open_catalog(dir.path());
    let catalog = Arc::new(catalog);
    catalog.upsert_volume("vol", None, None, 1000).unwrap();

    let mut policy = BackupPolicy::for_catalog(&catalog_path);
    policy.dir = dir.path().join("not-a-directory");
    std::fs::write(&policy.dir, b"occupying the path a directory should be at").unwrap();

    let pounce = Pounce::new(u64::MAX, 2, 2, || {});
    let (job, result) = BackupJob::new(catalog.clone(), policy, 1_000);
    let id = pounce.submit(Box::new(job));

    assert!(
        wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| s.id == id && s.state == JobState::Done),
            Duration::from_secs(5)
        ),
        "BackupJob must reach Done (not Failed) even when a step errors internally"
    );
    pounce.shutdown();

    let report = result
        .lock()
        .unwrap()
        .take()
        .expect("a Done job leaves its report, even on an internal failure");
    assert!(
        matches!(report.outcome, BackupOutcome::Failed(_)),
        "expected BackupOutcome::Failed, got {:?}",
        report.outcome
    );
}
