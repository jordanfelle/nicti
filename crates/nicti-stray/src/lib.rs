//! Lightroom Classic catalog import (#62, ADR-0061/0156/0158) -- a stray cat taken into a new home.
//!
//! Reads a **closed `.lrcat` backup** read-only (`open.rs`, promoted from `spikes/shed`) and brings
//! its library into nicti's catalog: root folders (registered and ingested through the normal
//! Scruff path, so previews/fingerprints are computed fresh -- ADR-0158), ratings/flags/colour
//! labels, hierarchical keywords, manual collections, virtual copies (extra non-master edit
//! variants) and develop settings. Develop translation (`develop/`) maps the keys nicti has a stage
//! for onto `EditDocument` stages; everything else -- and the verbatim Lua -- is kept in the
//! `lrc_provenance` table so a later translator can re-run without the catalog.
//!
//! `.lrcat-data` is never read (ADR-0156). The import runs as one cancellable Pounce job
//! (`job::LrcImportJob`); re-runs are idempotent and never overwrite an edit the user has since made
//! in nicti (`CatalogStore::apply_lrc_chunk`).

pub mod develop;
pub mod job;
pub mod open;
pub mod paths;
pub mod read;
pub mod report;

#[cfg(test)]
pub(crate) mod test_fixture;

pub use job::{ImportConfig, LrcImportJob};
pub use report::LrcImportReport;

/// Everything that can stop an import. A single image's develop text failing to parse is *not* one
/// of these -- it is counted in the report and the image is imported without a translated edit.
#[derive(Debug, thiserror::Error)]
pub enum StrayError {
    #[error("sqlite error reading the LRC catalog: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("catalog error: {0}")]
    Catalog(#[from] nicti_lair::CatalogError),
    #[error("{0}")]
    Io(String),
    /// The `.lrcat` is a live catalog, or isn't a catalog version this importer understands.
    #[error("{0}")]
    Refused(String),
}
