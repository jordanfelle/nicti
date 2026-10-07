//! `#[ignore]`d check against a real closed `.lrcat` backup (never the live catalog -- `open_backup`
//! refuses that). Run with the path in `NICTI_TEST_LRCAT`:
//!
//! ```text
//! NICTI_TEST_LRCAT=/path/to/catalog.lrcat \
//!   cargo test -p nicti-stray --release --test real_catalog -- --ignored --nocapture
//! ```
//!
//! It reads and translates every image's develop settings (no nicti catalog involved) and prints
//! only counts -- never a keyword, collection or path string (`spikes/shed`'s privacy rule).

use std::collections::BTreeMap;
use std::time::Instant;

use nicti_stray::develop::{translate, Context, FilterCounts, Stats};
use nicti_stray::open::open_validated;
use nicti_stray::read::Reader;

#[test]
#[ignore = "needs NICTI_TEST_LRCAT=<closed .lrcat backup>"]
fn every_real_develop_row_parses_and_translates() {
    let path = std::env::var("NICTI_TEST_LRCAT").expect("set NICTI_TEST_LRCAT");
    let conn = open_validated(std::path::Path::new(&path)).expect("catalog opens");
    let reader = Reader::new(&conn).unwrap();
    let started = Instant::now();

    let (mut images, mut with_develop, mut parse_failures, mut with_doc) = (0u64, 0u64, 0u64, 0u64);
    let (mut copies, mut stats, mut filters) = (0u64, Stats::default(), FilterCounts::default());
    let mut untranslated: BTreeMap<String, u64> = BTreeMap::new();
    let mut after = 0;
    loop {
        let page = reader.images_page(&[], after, 2000, true).unwrap();
        let Some(last) = page.last().map(|i| i.id_local) else {
            break;
        };
        for img in &page {
            images += 1;
            copies += u64::from(img.master_image.is_some());
            let Some(dev) = &img.develop else { continue };
            with_develop += 1;
            let ctx = Context {
                width: img.file_width.map(|w| w as f32),
                height: img.file_height.map(|h| h as f32),
                orientation: img.orientation.clone(),
                process_version: dev.process_version.clone(),
            };
            match translate(&dev.text, &ctx) {
                Ok(t) => {
                    with_doc += u64::from(!t.document.stages.is_empty());
                    stats.add(&t.stats);
                    filters.add(&t.filters);
                    for k in t.untranslated {
                        *untranslated.entry(k).or_default() += 1;
                    }
                }
                Err(_) => parse_failures += 1,
            }
        }
        after = last;
    }
    println!(
        "{images} images ({copies} virtual copies), {with_develop} with develop settings, \
         {with_doc} produced a translated edit, {parse_failures} parse failures, in {:?}",
        started.elapsed()
    );
    println!("stats: {stats:?}\nfilters: {filters:?}");
    let mut top: Vec<_> = untranslated.into_iter().collect();
    top.sort_by_key(|a| std::cmp::Reverse(a.1));
    println!("top untranslated keys (images using each):");
    for (k, n) in top.iter().take(25) {
        println!("  {n:>7}  {k}");
    }
    assert_eq!(
        parse_failures, 0,
        "ADR-0061: 0 parse failures across 380,307 rows"
    );
}

/// Runs the whole import job for one real root into a *throwaway* nicti catalog (a temp dir; never
/// the user's own) and prints counts. Reads the real photos (ingest hashes them) but writes nothing
/// next to them. `NICTI_TEST_LRCAT_ROOT` = `AgLibraryRootFolder.id_local`.
#[test]
#[ignore = "needs NICTI_TEST_LRCAT and NICTI_TEST_LRCAT_ROOT"]
fn imports_one_real_root_into_a_throwaway_catalog() {
    use nicti_lair::{CatalogStore, SqliteCatalog};
    use nicti_pounce::{ChunkedJob, Step};
    use nicti_stray::{ImportConfig, LrcImportJob};
    use std::sync::Arc;

    let lrcat = std::env::var("NICTI_TEST_LRCAT").expect("set NICTI_TEST_LRCAT");
    let root: i64 = std::env::var("NICTI_TEST_LRCAT_ROOT")
        .expect("set NICTI_TEST_LRCAT_ROOT")
        .parse()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(SqliteCatalog::open(&dir.path().join("nicti.db")).unwrap());
    let dynamic: Arc<dyn CatalogStore + Send + Sync> = store.clone();
    let (mut job, slot) = LrcImportJob::new(
        dynamic,
        ImportConfig {
            catalog_path: lrcat.into(),
            remaps: vec![],
            only_roots: vec![root],
            profile_resolver: None,
        },
    );
    let started = Instant::now();
    while matches!(job.step().unwrap(), Step::Yield) {}
    drop(job);
    let report = slot.lock().unwrap().take().unwrap();
    println!("{} in {:?}", report.summary(), started.elapsed());
    for r in &report.roots {
        println!(
            "root: exists={} images={} ingested={} ingest_failed={} matched={} missing={}",
            r.exists, r.lrc_images, r.ingested, r.ingest_failed, r.matched, r.missing
        );
    }
    println!(
        "applied: docs={} kept_docs={} meta={} variants={} sidecars_marked={} parse_failures={}",
        report.docs_written,
        report.kept_local_docs,
        report.meta_applied,
        report.virtual_copies,
        report.sidecars_marked,
        report.develop_parse_failures
    );
    println!("stats: {:?}", report.stats);
    assert_eq!(report.error, None);
    assert!(store.asset_count().unwrap() > 0, "something was ingested");
}
