//! #23's filter/query model: `Filter`/`Sort`/`Page` describe a query, and
//! `CatalogStore::hunt`/`hunt_count`/`facets` (implemented in `sqlite.rs`) run it. `hunt` is the
//! query #242's filter bar drives; a smart collection's own rule (`clowder.rs`) is just a
//! serialized `Filter` tagged with a version, so resolving one means calling `hunt` with it.
//!
//! Filename search here is a plain `GLOB '*text*'` scan (ADR-0067's flagged, never-closed gap) --
//! an FTS5 index is the documented follow-up once this ships, not attempted in this pass.

use serde::{Deserialize, Serialize};

/// A query over the asset library. Every field is optional and additive (AND-combined) — the
/// absence of a field means "don't filter on this dimension," not "match nothing."
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Filter {
    /// Match assets tagged with this keyword.
    pub keyword_id: Option<i64>,
    /// If `true`, also match assets tagged with any keyword under `keyword_id`'s subtree
    /// (a `path GLOB` prefix scan, ADR-0067's measured keyword-subtree query). Ignored if
    /// `keyword_id` is `None`.
    pub include_subtree: bool,
    /// Inclusive lower bound on `rating` (`-1` = reject, `0..=5` = stars). `None` = no lower
    /// bound.
    pub rating_min: Option<i64>,
    /// Inclusive upper bound on `rating`.
    pub rating_max: Option<i64>,
    /// If `true`, an unrated asset (`rating IS NULL`) also matches, regardless of
    /// `rating_min`/`rating_max` (a `NULL` fails any real `>=`/`<=` comparison, so this is the
    /// explicit opt-in to include it alongside a star-rating range).
    pub include_unrated: bool,
    /// Match assets whose `flag` equals this (only real value is `1`, pick).
    pub flag: Option<i64>,
    /// Match assets whose `label` equals this, exactly.
    pub label: Option<String>,
    /// Match assets whose EXIF `make` equals this, exactly.
    pub make: Option<String>,
    /// Match assets whose EXIF `model` equals this, exactly.
    pub model: Option<String>,
    /// Inclusive lower bound on `captured_at` (ISO-8601 text, compared lexicographically —
    /// correct for same-format timestamps).
    pub captured_after: Option<String>,
    /// Inclusive upper bound on `captured_at`.
    pub captured_before: Option<String>,
    /// Match assets under this root, by `rel_path` prefix (a folder subtree, not just the root's
    /// direct children).
    pub root_id: Option<i64>,
    pub rel_path_prefix: Option<String>,
    /// Case-insensitive substring match against `rel_path_fold`.
    pub filename_contains: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortField {
    Captured,
    Imported,
    Filename,
    Rating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sort {
    pub field: SortField,
    pub direction: SortDirection,
}

/// A keyset-pagination cursor -- the last row's own sort-key value and id, echoed back to
/// `hunt` as `Page::after` to fetch the next page. Deliberately not an opaque offset: `OFFSET`
/// is O(n) at 2M rows (ADR-0067's own measured library scale), a keyset `WHERE (key, id) > (?, ?)`
/// isn't. One variant per [`SortField`], since the key's own type (and its NULL-handling
/// sentinel) differs per field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Cursor {
    /// `captured_at`, with `None` sorting as if it were `""` (before any real timestamp
    /// ascending, after every real one descending) -- an "unknown date" bucket at one end,
    /// not interleaved with real dates.
    Captured {
        captured_at: Option<String>,
        id: i64,
    },
    Imported {
        imported_at: i64,
        id: i64,
    },
    Filename {
        rel_path_fold: String,
        id: i64,
    },
    /// `rating`, with `None` sorting via a sentinel well outside the real `-1..=5` range (see
    /// `sqlite.rs`'s `RATING_SORT_SENTINEL`) so unrated assets group at one end rather than
    /// interleaving with real ratings.
    Rating {
        rating: Option<i64>,
        id: i64,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Page {
    pub after: Option<Cursor>,
    pub limit: u32,
}

/// Facet counts for a query -- by model, by rating, and by flag, plus the total. Each map's keys
/// are the raw column values (`None` collapsing to `""`/unrated the same way `facet_count` does).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FacetCounts {
    pub by_model: Vec<(Option<String>, u64)>,
    pub by_rating: Vec<(Option<i64>, u64)>,
    pub by_flag: Vec<(Option<i64>, u64)>,
    pub total: u64,
}
