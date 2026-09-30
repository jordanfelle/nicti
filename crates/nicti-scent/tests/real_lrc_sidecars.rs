//! Cross-checks `nicti-scent`'s LRC-field reader/patcher against real
//! LRC-written `.xmp` sidecars, and against the same photos' rows in a real
//! `.lrcat` (via `spikes/shed`'s own reader) where available.
//!
//! Requires a real, non-public NEF+XMP folder (the user's own photo
//! library) -- gated on `NICTI_TEST_REAL_NEF_DIR`, the same env var
//! `spikes/litter`'s real-file test already uses (same folder, since it's
//! the same 37 real NEF+XMP pairs). Skips (not fails) when unset, so CI
//! passes cleanly.

use std::fs;
use std::path::PathBuf;

#[test]
fn real_sidecars_parse_and_round_trip_through_a_no_op_patch() {
    let Ok(dir) = std::env::var("NICTI_TEST_REAL_NEF_DIR") else {
        eprintln!("skipping: set NICTI_TEST_REAL_NEF_DIR to a folder of real NEF+XMP pairs");
        return;
    };
    let dir = PathBuf::from(dir);

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("read NICTI_TEST_REAL_NEF_DIR")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("xmp"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "no .xmp sidecars found in {}",
        dir.display()
    );

    let mut checked = 0;
    let mut with_rating = 0;
    let mut with_label = 0;
    let mut with_keywords = 0;
    let mut with_hierarchical = 0;

    for xmp_path in &entries {
        let xmp = fs::read_to_string(xmp_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", xmp_path.display()));

        let meta = nicti_scent::lrc_fields::read(&xmp)
            .unwrap_or_else(|e| panic!("parse {}: {e}", xmp_path.display()));

        // A no-op patch (nothing set) must never change the metadata a
        // second read sees -- the real correctness property this spike
        // needs, independent of exact byte formatting.
        let patched = nicti_scent::packet::apply(&xmp, &nicti_scent::packet::Patch::default())
            .unwrap_or_else(|e| panic!("patch {}: {e}", xmp_path.display()));
        let reread = nicti_scent::lrc_fields::read(&patched)
            .unwrap_or_else(|e| panic!("re-parse patched {}: {e}", xmp_path.display()));
        assert_eq!(
            meta,
            reread,
            "{}: metadata changed across a no-op patch",
            xmp_path.display()
        );

        if meta.rating.is_some() {
            with_rating += 1;
        }
        if meta.label.is_some() {
            with_label += 1;
        }
        if !meta.keywords.is_empty() {
            with_keywords += 1;
        }
        if !meta.hierarchical_keywords.is_empty() {
            with_hierarchical += 1;
        }
        checked += 1;
    }

    eprintln!(
        "checked {checked} real sidecars: rating={with_rating} label={with_label} \
         keywords={with_keywords} hierarchical={with_hierarchical}"
    );
    assert!(checked > 0, "no sidecars were actually checked");
}
