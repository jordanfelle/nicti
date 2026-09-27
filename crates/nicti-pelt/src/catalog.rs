//! Resolves and opens this session's catalog file. `SqliteCatalog::open` creates the file if it's
//! missing (see its own doc comment) -- there's no separate "create" call to make.

use std::path::PathBuf;

use nicti_lair::{CatalogError, SqliteCatalog};

const DEFAULT_CATALOG_FILENAME: &str = "nicti.catalog.sqlite";

/// The catalog path a real user would want: the first CLI argument if one was given (so a
/// double-clicked catalog file or a scripted launch can point at a specific one), otherwise a
/// fixed name in the current directory. A real per-platform app-data default (matching where
/// #62's importer will eventually look) is left to whichever ticket adds catalog-picker UI --
/// this is a placeholder shell's "just open something" default, not a final location policy.
pub fn resolve_path() -> PathBuf {
    std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CATALOG_FILENAME))
}

pub fn open(path: &std::path::Path) -> Result<SqliteCatalog, CatalogError> {
    SqliteCatalog::open(path)
}
