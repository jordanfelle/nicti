//! Integration test for #55: driving `IngestJob`/`SyncJob` through a real `nicti_pounce::Pounce`
//! runtime, not just calling `ingest_root`/`sync_root` directly (already covered by
//! `tests/ingest.rs`/`tests/sync.rs`) -- this is the actual wiring a real caller (the activity
//! panel) exercises. File *content* correctness (EXIF/preview extraction, move detection, etc.)
//! is that other suite's job; these files are trivial placeholders, since Scruff already treats
//! an unparseable RAW file as "cataloged with no preview" rather than an error (`scruff.rs`'s own
//! doc comment).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nicti_lair::patrol::SyncOptions;
use nicti_lair::pounce_jobs::{IngestJob, SyncJob};
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

fn write_fake_nef(dir: &Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(
        &path,
        b"not a real raw file, just needs the right extension",
    )
    .unwrap();
    path
}

#[test]
fn ingest_job_driven_through_pounce_matches_a_direct_ingest_root_report() {
    let concrete = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let store: Arc<dyn CatalogStore + Send + Sync> = concrete.clone();
    let dir = tempfile::tempdir().unwrap();
    write_fake_nef(dir.path(), "a.nef");
    write_fake_nef(dir.path(), "b.nef");
    write_fake_nef(dir.path(), "c.txt"); // not a RAW extension, must be ignored

    let volume_id = concrete
        .upsert_volume("test-volume", None, None, 1000)
        .unwrap();
    let root_id = concrete.ensure_root(volume_id, "").unwrap();

    let pounce = Pounce::new(u64::MAX, 2, 2, || {});
    let (job, result) = IngestJob::new(store, root_id, dir.path());
    let id = pounce.submit(Box::new(job));

    assert!(
        wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| s.id == id && s.state == JobState::Done),
            Duration::from_secs(5)
        ),
        "IngestJob never reached Done"
    );
    pounce.shutdown();

    let report = result
        .lock()
        .unwrap()
        .take()
        .expect("a Done job leaves its report");
    assert_eq!(
        report.added, 2,
        "only the two .nef files should be ingested"
    );
    assert!(report.failed.is_empty());
    assert_eq!(concrete.asset_count().unwrap(), 2);
}

#[test]
fn sync_job_driven_through_pounce_flags_a_deleted_file_as_missing() {
    let concrete = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let store: Arc<dyn CatalogStore + Send + Sync> = concrete.clone();
    let dir = tempfile::tempdir().unwrap();
    let doomed = write_fake_nef(dir.path(), "doomed.nef");
    write_fake_nef(dir.path(), "survivor.nef");

    let volume_id = concrete
        .upsert_volume("test-volume", None, None, 1000)
        .unwrap();
    let root_id = concrete.ensure_root(volume_id, "").unwrap();

    // Seed the catalog first (a plain ingest, not through Pounce -- this test is about SyncJob).
    nicti_lair::scruff::ingest_root(store.as_ref(), root_id, dir.path()).unwrap();
    std::fs::remove_file(&doomed).unwrap();

    let pounce = Pounce::new(u64::MAX, 2, 2, || {});
    let (job, result) = SyncJob::new(store, root_id, dir.path(), SyncOptions::default());
    let id = pounce.submit(Box::new(job));

    assert!(
        wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| s.id == id && s.state == JobState::Done),
            Duration::from_secs(5)
        ),
        "SyncJob never reached Done"
    );
    pounce.shutdown();

    let report = result
        .lock()
        .unwrap()
        .take()
        .expect("a Done job leaves its report");
    assert!(!report.root_unreachable);
    assert_eq!(report.newly_missing, 1);
}
