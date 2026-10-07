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
use nicti_lair::pounce_jobs::{BaselineJob, IngestJob, SyncJob, VerifyJob};
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

/// #307: a `committed` journal row's source cleanup (the part that BLAKE3-hashes the whole tree)
/// runs as a `ResumeMovesJob` on Pounce's CPU lane, and ends the same place the synchronous
/// `resume_open_moves` does.
#[test]
fn resume_moves_job_driven_through_pounce_finishes_a_committed_cleanup() {
    use nicti_lair::carry::{Carry, CarryOptions, Resumed};
    use nicti_lair::pounce_jobs::ResumeMovesJob;

    let concrete = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let store: Arc<dyn CatalogStore + Send + Sync> = concrete.clone();
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("ssd").join("event");
    let dest_parent = tmp.path().join("archive");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(&dest_parent).unwrap();
    write_fake_nef(&src, "a.nef");
    write_fake_nef(&src, "b.nef");

    let volume_id = concrete
        .upsert_volume("test-volume", None, None, 0)
        .unwrap();
    let root_id = concrete
        .ensure_root(volume_id, &src.to_string_lossy())
        .unwrap();

    // Nothing journaled yet: nothing to submit.
    assert!(ResumeMovesJob::new(store.clone()).is_none());

    // Crash right after the catalog commit, before the source cleanup.
    let mut carry = Carry::new(
        store.clone(),
        root_id,
        &dest_parent,
        CarryOptions {
            force_copy: true,
            ..Default::default()
        },
        0,
    );
    while !carry.is_committed() {
        assert!(carry.step().is_none());
    }
    std::mem::forget(carry);
    assert!(src.join("a.nef").is_file());

    let (job, result) = ResumeMovesJob::new(store).expect("an open journal row");
    let pounce = Pounce::new(u64::MAX, 2, 2, || {});
    let id = pounce.submit(Box::new(job));
    assert!(
        wait_until(
            || pounce
                .snapshot()
                .into_iter()
                .any(|s| s.id == id && s.state == JobState::Done),
            Duration::from_secs(5)
        ),
        "ResumeMovesJob never reached Done"
    );
    pounce.shutdown();

    let report = result
        .lock()
        .unwrap()
        .take()
        .expect("a Done job leaves its report");
    assert_eq!(
        report,
        vec![Resumed::CleanedUp {
            root_id,
            leftover_count: 0
        }]
    );
    assert!(!src.exists());
    assert!(dest_parent.join("event").join("b.nef").is_file());
    assert!(concrete.open_root_moves().unwrap().is_empty());
}

/// #386: ingest -> Verify (everything unhashed) -> Record baseline -> Verify (clean, all checked),
/// each driven through a real Pounce runtime.
#[test]
fn baseline_then_verify_jobs_driven_through_pounce_end_clean() {
    let concrete = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let store: Arc<dyn CatalogStore + Send + Sync> = concrete.clone();
    let dir = tempfile::tempdir().unwrap();
    write_fake_nef(dir.path(), "a.nef");
    write_fake_nef(dir.path(), "b.nef");
    let volume_id = concrete
        .upsert_volume("test-volume", None, None, 1000)
        .unwrap();
    let root_id = concrete.ensure_root(volume_id, "").unwrap();
    nicti_lair::scruff::ingest_root(store.as_ref(), root_id, dir.path()).unwrap();

    let pounce = Pounce::new(u64::MAX, 2, 2, || {});
    let done = |id| {
        wait_until(
            || {
                pounce
                    .snapshot()
                    .into_iter()
                    .any(|s| s.id == id && s.state == JobState::Done)
            },
            Duration::from_secs(5),
        )
    };

    let (job, slot) = VerifyJob::new(store.clone(), root_id, dir.path());
    assert!(done(pounce.submit(Box::new(job))), "VerifyJob never Done");
    let before = slot.lock().unwrap().take().unwrap();
    assert_eq!((before.checked, before.unhashed), (0, 2));

    let (job, slot) = BaselineJob::new(store.clone(), root_id, dir.path());
    assert!(done(pounce.submit(Box::new(job))), "BaselineJob never Done");
    let base = slot.lock().unwrap().take().unwrap();
    assert_eq!((base.recorded, base.skipped_changed), (2, 0));

    let (job, slot) = VerifyJob::new(store, root_id, dir.path());
    assert!(done(pounce.submit(Box::new(job))), "VerifyJob never Done");
    let after = slot.lock().unwrap().take().unwrap();
    pounce.shutdown();
    assert_eq!((after.checked, after.matched, after.unhashed), (2, 2, 0));
    assert!(after.is_clean());
}

/// #386: a job dropped before it finishes (cancelled) still resolves its slot, flagged cancelled.
/// The unchecked-list and keep-what-was-hashed behaviour are unit-tested in `verify.rs`.
#[test]
fn dropping_verify_and_baseline_jobs_reports_cancelled() {
    let concrete = Arc::new(SqliteCatalog::open_in_memory().unwrap());
    let store: Arc<dyn CatalogStore + Send + Sync> = concrete.clone();
    let dir = tempfile::tempdir().unwrap();
    write_fake_nef(dir.path(), "a.nef");
    let volume_id = concrete.upsert_volume("v", None, None, 1000).unwrap();
    let root_id = concrete.ensure_root(volume_id, "").unwrap();
    nicti_lair::scruff::ingest_root(store.as_ref(), root_id, dir.path()).unwrap();
    let (job, slot) = BaselineJob::new(store.clone(), root_id, dir.path());
    drop(job); // never stepped: cancelled before it listed anything
    let report = slot.lock().unwrap().take().unwrap();
    assert!(report.cancelled);
    assert_eq!(report.recorded, 0);

    let (job, slot) = VerifyJob::new(store, root_id, dir.path());
    drop(job);
    assert!(slot.lock().unwrap().take().unwrap().cancelled);
}
