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

    // A writer thread deliberately paced with a small sleep between writes, so it's guaranteed to
    // take a real, bounded wall-clock time (WRITER_COUNT * WRITER_DELAY, here ~600ms) regardless
    // of hardware speed -- unlike an unpaced loop, whose real duration on a tiny in-test catalog
    // would be too close to `run_backup`'s own duration for a timing comparison to mean anything
    // either way.
    const WRITER_COUNT: usize = 200;
    const WRITER_DELAY: Duration = Duration::from_millis(3);
    let progress = Arc::new(AtomicUsize::new(0));

    let writer_catalog = catalog.clone();
    let writer_progress = progress.clone();
    let writer = std::thread::spawn(move || {
        for i in 0..WRITER_COUNT {
            std::thread::sleep(WRITER_DELAY);
            writer_catalog
                .ensure_root(volume_id, &format!("root-{i}"))
                .unwrap();
            writer_progress.fetch_add(1, Ordering::SeqCst);
        }
    });

    // Wait until the writer has genuinely started (and is now mid-pace) before snapshotting, so
    // the assertion below can't pass merely because the writer thread hadn't been scheduled yet.
    assert!(
        wait_until(
            || progress.load(Ordering::SeqCst) > 0,
            Duration::from_secs(2)
        ),
        "writer thread never made any progress"
    );

    let report = run_backup(&catalog, &policy, 1_000).unwrap();

    // The real property this test demonstrates: `run_backup` (specifically its `snapshot_into`
    // step, which opens its own independent read-only connection for a file-backed catalog rather
    // than reusing the shared `Mutex<Connection>` -- see that function's own doc comment) returns
    // long before the writer's own ~600ms of deliberately-paced work is done. If `snapshot_into`
    // instead contended for the same connection every `ensure_root` call goes through, the writer
    // would make no further progress while the snapshot ran, and -- since the snapshot itself
    // completes quickly regardless of which connection performs it, at this small a row count --
    // this specific assertion wouldn't reliably distinguish the two cases at 2M-row scale the way
    // it does here; ADR-0067 is where that scale's own timing (2.0s/2.5s p50/p95) was measured.
    let progress_at_return = progress.load(Ordering::SeqCst);
    assert!(
        progress_at_return < WRITER_COUNT,
        "expected the writer to still be mid-pace ({progress_at_return}/{WRITER_COUNT} done) \
         when run_backup returned -- if this fails, either the backup unexpectedly took as long \
         as the writer's own ~600ms, or something now blocks the writer behind the snapshot"
    );

    writer.join().unwrap();
    assert_eq!(progress.load(Ordering::SeqCst), WRITER_COUNT);

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
    let mut bytes = std::fs::read(&catalog_path).unwrap();
    let corrupt_from = bytes.len() / 2;
    for b in bytes.iter_mut().skip(corrupt_from).take(1024) {
        *b = 0xFF;
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
