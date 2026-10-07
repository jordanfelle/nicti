//! What an import did, for the UI summary and the tests.

use std::collections::BTreeMap;

use crate::develop::{FilterCounts, Stats};

/// First N unmatched paths kept per root (the count is always exact).
pub const MAX_MISSING_EXAMPLES: usize = 100;

/// One LRC root folder's outcome.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RootReport {
    pub lrc_root_id: i64,
    pub lrc_path: String,
    /// Where this machine looked for the folder (after remaps and drive-letter mapping).
    pub local_path: String,
    /// The folder exists here. A root that doesn't is skipped entirely: every one of its images
    /// counts as missing.
    pub exists: bool,
    pub lrc_images: u64,
    /// Files ingested (added or updated) by the normal Scruff pass.
    pub ingested: u64,
    pub ingest_failed: u64,
    /// LRC images matched to a cataloged file by `(root, rel_path)`.
    pub matched: u64,
    /// LRC images with no file on disk / in the catalog: skipped, never invented.
    pub missing: u64,
    pub missing_examples: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct LrcImportReport {
    /// The job was cancelled (or dropped) before finishing; what was applied stays applied.
    pub cancelled: bool,
    /// A failure that stopped the run (catalog refused to open, an SQL error ...).
    pub error: Option<String>,
    pub roots: Vec<RootReport>,
    pub keywords_created: u64,
    /// Distinct assets tagged with at least one imported keyword.
    pub assets_tagged: u64,
    pub collections_created: u64,
    pub collection_members: u64,
    pub smart_collections_skipped: u64,
    /// Images whose develop text did not parse (imported without a translated edit).
    pub develop_parse_failures: u64,
    /// Virtual copies imported as extra non-master variants.
    pub virtual_copies: u64,
    pub docs_written: u64,
    pub kept_local_docs: u64,
    pub meta_applied: u64,
    pub kept_local_meta: u64,
    /// Assets marked catalog-dirty so the XMP sidecar sync does not revert imported markers.
    pub sidecars_marked: u64,
    /// Develop keys with real values that nicti has no stage for, by number of images using them.
    pub untranslated: BTreeMap<String, u64>,
    /// Photos whose LRC camera profile (#381) resolved to an installed `.dcp`/Look and was written.
    pub profiles_resolved: u64,
    /// Camera profiles LRC uses that nicti could not resolve (not installed for that camera, no
    /// resolver configured, or camera unknown), by profile name -- what the user should install.
    pub profiles_missing: BTreeMap<String, u64>,
    pub filters: FilterCounts,
    pub stats: Stats,
}

impl LrcImportReport {
    pub fn matched(&self) -> u64 {
        self.roots.iter().map(|r| r.matched).sum()
    }

    pub fn missing(&self) -> u64 {
        self.roots.iter().map(|r| r.missing).sum()
    }

    /// One-line summary for the activity panel / status bar.
    pub fn summary(&self) -> String {
        if let Some(e) = &self.error {
            return format!("LRC import failed: {e}");
        }
        format!(
            "LRC import{}: {} photos matched, {} missing, {} keywords, {} collections, {} edits written, {} left alone",
            if self.cancelled { " (cancelled)" } else { "" },
            self.matched(),
            self.missing(),
            self.keywords_created,
            self.collections_created,
            self.docs_written,
            self.kept_local_docs + self.kept_local_meta,
        )
    }
}
