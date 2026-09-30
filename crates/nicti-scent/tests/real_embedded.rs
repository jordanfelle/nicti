//! Cross-checks `nicti_scent::embedded::jpeg` against real LRC-touched JPEGs --
//! the embedded-XMP case, distinct from `real_lrc_sidecars.rs`'s sidecar
//! case, since ~72% of the reference library is JPEG
//! (`docs/research/shed-lrcat-schema.md`).
//!
//! Requires a folder of real JPEGs that LRC has written metadata into --
//! gated on `NICTI_TEST_REAL_EMBEDDED_DIR` (copies only; this test never
//! writes back to the input files, it always works on an in-memory copy).
//! Skips (not fails) when unset, so CI passes cleanly.

use std::fs;
use std::path::PathBuf;

#[test]
fn real_embedded_jpegs_round_trip_a_no_op_write() {
    let Ok(dir) = std::env::var("NICTI_TEST_REAL_EMBEDDED_DIR") else {
        eprintln!(
            "skipping: set NICTI_TEST_REAL_EMBEDDED_DIR to a folder of real LRC-touched JPEGs"
        );
        return;
    };
    let dir = PathBuf::from(dir);

    let mut entries: Vec<_> = fs::read_dir(&dir)
        .expect("read NICTI_TEST_REAL_EMBEDDED_DIR")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg"))
                .unwrap_or(false)
        })
        .collect();
    entries.sort();
    assert!(
        !entries.is_empty(),
        "no .jpg/.jpeg files found in {}",
        dir.display()
    );

    let mut checked = 0;
    let mut with_xmp = 0;

    for jpeg_path in &entries {
        let data =
            fs::read(jpeg_path).unwrap_or_else(|e| panic!("read {}: {e}", jpeg_path.display()));

        let Some(xmp) = nicti_scent::embedded::read_xmp(&data)
            .unwrap_or_else(|e| panic!("read_xmp {}: {e}", jpeg_path.display()))
        else {
            checked += 1;
            continue;
        };
        with_xmp += 1;

        let meta = nicti_scent::lrc_fields::read(&xmp)
            .unwrap_or_else(|e| panic!("parse embedded xmp {}: {e}", jpeg_path.display()));

        // Round-trip: patch the packet with a no-op, splice it back into a
        // *copy* of the file, and confirm the metadata a fresh read sees is
        // unchanged. The original file on disk is never touched.
        let patched = nicti_scent::packet::apply(&xmp, &nicti_scent::packet::Patch::default())
            .unwrap_or_else(|e| panic!("patch {}: {e}", jpeg_path.display()));
        let rewritten = nicti_scent::embedded::write_xmp(&data, &patched)
            .unwrap_or_else(|e| panic!("write_xmp {}: {e}", jpeg_path.display()));
        let reread = nicti_scent::embedded::read_xmp(&rewritten)
            .unwrap_or_else(|e| panic!("re-read {}: {e}", jpeg_path.display()))
            .and_then(|x| nicti_scent::lrc_fields::read(&x).ok());

        assert_eq!(
            Some(meta),
            reread,
            "{}: metadata changed across a no-op embedded write",
            jpeg_path.display()
        );
        checked += 1;
    }

    eprintln!("checked {checked} real JPEGs, {with_xmp} carried an embedded XMP packet");
    assert!(checked > 0, "no files were actually checked");
}
