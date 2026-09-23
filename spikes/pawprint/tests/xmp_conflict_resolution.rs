//! Proves the ADR's "Recovery from sidecars" conflict rule: content-hash
//! equality means no conflict, otherwise newer mtime wins, and mtimes within
//! the ambiguity window get flagged instead of silently resolved.

use pawprint::xmp::{resolve_conflict, Resolution, Side};
use pawprint::{EditDocument, StageEntry};
use serde_json::json;

fn doc(exposure: f64) -> EditDocument {
    let mut d = EditDocument::default();
    d.stages.insert(
        "global".to_string(),
        StageEntry {
            schema_version: 1,
            params: json!({"exposure": exposure}),
        },
    );
    d
}

#[test]
fn identical_content_is_never_a_conflict_regardless_of_mtime() {
    let a = doc(0.3);
    let b = doc(0.3);
    let resolution = resolve_conflict(
        Side {
            document: &a,
            mtime_ms: 1_000,
        },
        Side {
            document: &b,
            mtime_ms: 999_999,
        },
        50,
    );
    assert_eq!(resolution, Resolution::NoConflict);
}

#[test]
fn differing_content_prefers_the_later_mtime_on_either_side() {
    let catalog = doc(0.3);
    let sidecar = doc(0.5);

    let catalog_newer = resolve_conflict(
        Side {
            document: &catalog,
            mtime_ms: 2_000,
        },
        Side {
            document: &sidecar,
            mtime_ms: 1_000,
        },
        50,
    );
    assert_eq!(catalog_newer, Resolution::PreferCatalog);

    let sidecar_newer = resolve_conflict(
        Side {
            document: &catalog,
            mtime_ms: 1_000,
        },
        Side {
            document: &sidecar,
            mtime_ms: 2_000,
        },
        50,
    );
    assert_eq!(sidecar_newer, Resolution::PreferSidecar);
}

#[test]
fn differing_content_with_ambiguous_mtimes_is_flagged_not_guessed() {
    let catalog = doc(0.3);
    let sidecar = doc(0.5);
    let resolution = resolve_conflict(
        Side {
            document: &catalog,
            mtime_ms: 1_000,
        },
        Side {
            document: &sidecar,
            mtime_ms: 1_010,
        },
        50,
    );
    assert_eq!(resolution, Resolution::FlagForManualReview);
}
