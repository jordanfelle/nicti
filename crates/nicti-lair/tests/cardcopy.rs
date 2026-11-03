//! #602: the card-copy run (verified copy, then registration) against a real temp-file catalog and
//! a fake card. The engine's own behaviour (hashing, no-overwrite, symlinks) is covered by
//! `offering`'s tests; this checks the run around it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use nicti_lair::cardcopy::{CardCopy, PlannedCopy};
use nicti_lair::offering::list_card_files;
use nicti_lair::pounce_jobs::CardCopyJob;
use nicti_lair::{CatalogStore, SqliteCatalog};
use nicti_pounce::{ChunkedJob, Step};

fn write(path: &Path, fill: u8, len: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut bytes = b"\xFF\xD8FAKE\xFF\xD9".to_vec();
    bytes.extend(std::iter::repeat_n(fill, len));
    std::fs::write(path, bytes).unwrap();
}

struct Fx {
    _tmp: tempfile::TempDir,
    card: PathBuf,
    dest: PathBuf,
    store: Arc<SqliteCatalog>,
    root_id: i64,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let card = tmp.path().join("card");
        let dest = tmp.path().join("photos");
        std::fs::create_dir_all(&dest).unwrap();
        write(&card.join("DCIM/100NIKON/DSC_0001.NEF"), 1, 5000);
        write(&card.join("DCIM/100NIKON/DSC_0002.JPG"), 2, 3000);
        let store = Arc::new(SqliteCatalog::open_in_memory().unwrap());
        let vol = store.upsert_volume("vol", None, None, 0).unwrap();
        let root_id = store.ensure_root(vol, &dest.to_string_lossy()).unwrap();
        Fx {
            _tmp: tmp,
            card,
            dest,
            store,
            root_id,
        }
    }

    fn plan(&self) -> (Vec<PlannedCopy>, Vec<String>) {
        let (files, problems) = list_card_files(&self.card);
        let plan = files
            .into_iter()
            .map(|file| PlannedCopy {
                dest_rel: Path::new("2026-11-03").join(file.path.file_name().unwrap()),
                file,
            })
            .collect();
        (plan, problems)
    }

    fn run(
        &self,
        plan: Vec<PlannedCopy>,
        problems: Vec<String>,
    ) -> nicti_lair::cardcopy::CardCopyReport {
        let copy = CardCopy::new(plan, problems, &self.dest, self.root_id);
        let store: Arc<dyn CatalogStore + Send + Sync> = self.store.clone();
        let (mut job, slot) = CardCopyJob::new(store, copy, "test".into());
        for _ in 0..200 {
            if matches!(job.step().unwrap(), Step::Done) {
                return slot.lock().unwrap().take().unwrap();
            }
        }
        panic!("job did not finish");
    }

    fn asset_count(&self) -> usize {
        self.store.list_assets_by_root(self.root_id).unwrap().len()
    }
}

#[test]
fn a_run_copies_verifies_registers_and_leaves_the_card_alone() {
    let fx = Fx::new();
    let before: Vec<_> = list_card_files(&fx.card).0;
    let (plan, problems) = fx.plan();
    let report = fx.run(plan, problems);

    assert!(report.is_clean(), "{report:?}");
    assert_eq!((report.copied, report.already_there), (2, 0));
    assert_eq!(report.destinations.len(), 2);
    // The engine reports canonical paths (verbatim `\\?\` ones on Windows).
    let dest_dir = fx.dest.join("2026-11-03").canonicalize().unwrap();
    for d in &report.destinations {
        assert!(d.canonicalize().unwrap().starts_with(&dest_dir), "{d:?}");
    }
    assert_eq!(fx.asset_count(), 2);
    // The card is byte-for-byte what it was.
    assert_eq!(list_card_files(&fx.card).0, before);
    let nef = std::fs::read(fx.card.join("DCIM/100NIKON/DSC_0001.NEF")).unwrap();
    let copy = std::fs::read(fx.dest.join("2026-11-03/DSC_0001.NEF")).unwrap();
    assert_eq!(nef, copy);
}

#[test]
fn the_copies_hash_becomes_the_assets_trusted_baseline_without_rehashing() {
    let fx = Fx::new();
    let (plan, problems) = fx.plan();
    let report = fx.run(plan, problems);
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.baseline_recorded, 2, "{report:?}");
    assert!(report.baseline_error.is_none());
    // Compare through the catalog's own paths (not by stripping the reported destinations, whose
    // spelling differs from `fx.dest` on Windows: verbatim prefix, long vs 8.3 names).
    let assets = fx.store.list_assets_by_root(fx.root_id).unwrap();
    assert_eq!(assets.len(), 2);
    for asset in assets {
        let file = fx.dest.join(&asset.rel_path);
        let want = blake3::hash(&std::fs::read(&file).unwrap())
            .to_hex()
            .to_string();
        assert_eq!(
            fx.store.content_hash(asset.id).unwrap().as_deref(),
            Some(want.as_str()),
            "{}",
            asset.rel_path
        );
    }
    // A second run finds the files already there and must not overwrite the baselines.
    let (plan, problems) = fx.plan();
    let again = fx.run(plan, problems);
    assert_eq!(again.already_there, 2);
    assert_eq!(again.baseline_recorded, 0, "existing baselines are kept");
}

#[test]
fn running_it_again_reports_already_there_and_adds_no_duplicates() {
    let fx = Fx::new();
    let (plan, problems) = fx.plan();
    fx.run(plan, problems);
    let (plan, problems) = fx.plan();
    let report = fx.run(plan, problems);
    assert_eq!((report.copied, report.already_there), (0, 2), "{report:?}");
    assert_eq!(fx.asset_count(), 2);
    let names: HashSet<_> = std::fs::read_dir(fx.dest.join("2026-11-03"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(names.len(), 2);
}

#[test]
fn one_bad_destination_is_reported_and_the_rest_still_copy() {
    let fx = Fx::new();
    let (mut plan, problems) = fx.plan();
    plan[0].dest_rel = PathBuf::from("../escape.NEF");
    let report = fx.run(plan, problems);
    assert!(!report.is_clean());
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.copied, 1);
    assert!(!fx.dest.parent().unwrap().join("escape.NEF").exists());
}

#[test]
fn listing_problems_make_the_report_unclean_even_when_every_file_copied() {
    let fx = Fx::new();
    let (plan, _) = fx.plan();
    let report = fx.run(plan, vec!["DCIM/101: permission denied".into()]);
    assert_eq!(report.copied, 2);
    assert!(!report.is_clean());
    assert_eq!(report.listing_problems.len(), 1);
}

#[test]
fn a_job_dropped_part_way_counts_the_files_it_never_attempted() {
    let fx = Fx::new();
    let (plan, problems) = fx.plan();
    let copy = CardCopy::new(plan, problems, &fx.dest, fx.root_id);
    let store: Arc<dyn CatalogStore + Send + Sync> = fx.store.clone();
    let (mut job, slot) = CardCopyJob::new(store, copy, "t".into());
    let _ = job.step().unwrap(); // one file
    drop(job);
    let report = slot.lock().unwrap().take().unwrap();
    assert_eq!(report.copied, 1);
    assert_eq!(report.not_attempted, 1);
    assert!(!report.is_clean());
    assert!(report.ingest.is_none());
}
