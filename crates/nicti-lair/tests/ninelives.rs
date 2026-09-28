//! Integration tests for #25's Nine Lives backup (`nicti_lair::ninelives`) against a real,
//! file-backed SQLite catalog -- unit tests in `ninelives.rs` itself already cover the pure
//! filename/scheduling logic; these exercise the parts that need a real file on disk: a
//! concurrent writer during the snapshot (proving ADR-0067's own "no lock on the live store"
//! claim, not just restating it), a corrupted live catalog, stale `.partial` cleanup across two
//! separate runs, and `BackupJob` driven through a real `nicti_pounce::Pounce` the way the
//! activity panel actually would.

use std::path::Path;
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
fn snapshot_never_blocks_a_concurrent_writer() {
    let dir = tempfile::tempdir().unwrap();
    let (catalog, catalog_path) = open_catalog(dir.path());
    let catalog = Arc::new(catalog);
    let volume_id = catalog.upsert_volume("vol", None, None, 1000).unwrap();
    for i in 0..2000 {
        catalog
            .ensure_root(volume_id, &format!("root-{i}"))
            .unwrap();
    }

    let policy = BackupPolicy::for_catalog(&catalog_path);

    let writer_catalog = catalog.clone();
    let writer = std::thread::spawn(move || {
        // Keeps writing for as long as the main thread's snapshot below is running -- if
        // `snapshot_into` held the shared connection mutex (rather than a second, independent
        // read-only connection), every one of these inserts would stall until the snapshot
        // finished instead of interleaving with it.
        for i in 2000..4000 {
            writer_catalog
                .ensure_root(volume_id, &format!("root-{i}"))
                .unwrap();
        }
    });

    let report = run_backup(&catalog, &policy, 1_000).unwrap();
    writer.join().unwrap();

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
