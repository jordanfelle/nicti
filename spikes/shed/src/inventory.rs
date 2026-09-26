//! Structural + aggregate inventory of a `.lrcat` catalog: table/column shapes and count-only
//! summaries (never names, paths, or keyword text) -- what #61's write-up cites as measured
//! numbers, and what #62's importer needs to know exists before it can plan a mapping.

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Inventory {
    pub asset_count: i64,
    pub file_format_counts: Vec<(String, i64)>,
    pub virtual_copy_count: i64,
    pub root_folder_count: i64,
    /// Roots whose `absolutePath` starts with a drive letter (`C:/...`) -- the LRC folder model
    /// ADR-0020 already assumes, confirmed here rather than only inferred.
    pub drive_letter_root_count: i64,
    /// Roots that also carry a non-empty `relativePathFromCatalog` -- LRC's own portable-catalog
    /// fallback path, present alongside the absolute one on some roots but not all (a real,
    /// measured split, not an assumption).
    pub relative_path_root_count: i64,
    pub folder_count: i64,
    pub file_count: i64,
    pub pick_counts: Vec<(String, i64)>,
    pub rating_counts: Vec<(String, i64)>,
    pub color_label_counts: Vec<(String, i64)>,
    pub keyword_count: i64,
    /// Max nesting depth inferred from `AgLibraryKeyword.genealogy`'s `/`-separated id chain.
    pub keyword_max_depth: i64,
    pub keyword_synonym_count: i64,
    pub keyword_image_link_count: i64,
    /// `(creationId, count)` -- LRC's own collection-kind identifier. A real user-created
    /// collection is `com.adobe.ag.library.collection`; the `*.unsaved` kinds are LRC's own
    /// scratch state for the print/book/slideshow/web-gallery modules, not user data.
    pub collection_kind_counts: Vec<(String, i64)>,
    pub collection_image_count: i64,
    pub iptc_row_count: i64,
    pub folder_stack_count: i64,
}

pub fn inspect(conn: &Connection) -> Result<Inventory> {
    let asset_count: i64 = conn.query_row("SELECT COUNT(*) FROM Adobe_images", [], |r| r.get(0))?;
    let file_format_counts = group_counts(
        conn,
        "SELECT fileFormat, COUNT(*) FROM Adobe_images GROUP BY fileFormat ORDER BY 2 DESC",
    )?;
    let virtual_copy_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM Adobe_images WHERE masterImage IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let root_folder_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM AgLibraryRootFolder", [], |r| r.get(0))?;
    let drive_letter_root_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM AgLibraryRootFolder \
         WHERE absolutePath GLOB '[A-Za-z]:*'",
        [],
        |r| r.get(0),
    )?;
    let relative_path_root_count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM AgLibraryRootFolder \
         WHERE relativePathFromCatalog IS NOT NULL AND relativePathFromCatalog != ''",
        [],
        |r| r.get(0),
    )?;
    let folder_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM AgLibraryFolder", [], |r| r.get(0))?;
    let file_count: i64 = conn.query_row("SELECT COUNT(*) FROM AgLibraryFile", [], |r| r.get(0))?;
    let pick_counts = group_counts(
        conn,
        "SELECT CAST(pick AS TEXT), COUNT(*) FROM Adobe_images GROUP BY pick",
    )?;
    let rating_counts = group_counts(
        conn,
        "SELECT COALESCE(CAST(rating AS TEXT), '(none)'), COUNT(*) FROM Adobe_images GROUP BY rating",
    )?;
    let color_label_counts = group_counts(
        conn,
        "SELECT CASE WHEN colorLabels = '' THEN '(none)' ELSE colorLabels END, COUNT(*) \
         FROM Adobe_images GROUP BY colorLabels",
    )?;
    let keyword_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM AgLibraryKeyword", [], |r| r.get(0))?;
    // genealogy is a `/`-separated chain of ancestor ids, e.g. `/540430/826707370`; the number of
    // separators is the nesting depth (root-level keywords have exactly one leading `/`, depth 0).
    let keyword_max_depth: i64 = conn.query_row(
        "SELECT MAX(LENGTH(genealogy) - LENGTH(REPLACE(genealogy, '/', ''))) - 1 \
         FROM AgLibraryKeyword",
        [],
        |r| r.get(0),
    )?;
    let keyword_synonym_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM AgLibraryKeywordSynonym", [], |r| {
            r.get(0)
        })?;
    let keyword_image_link_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM AgLibraryKeywordImage", [], |r| {
            r.get(0)
        })?;
    let collection_kind_counts = group_counts(
        conn,
        "SELECT creationId, COUNT(*) FROM AgLibraryCollection GROUP BY creationId ORDER BY 2 DESC",
    )?;
    let collection_image_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM AgLibraryCollectionImage", [], |r| {
            r.get(0)
        })?;
    let iptc_row_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM AgLibraryIPTC", [], |r| r.get(0))?;
    let folder_stack_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM AgLibraryFolderStack", [], |r| {
            r.get(0)
        })?;

    Ok(Inventory {
        asset_count,
        file_format_counts,
        virtual_copy_count,
        root_folder_count,
        drive_letter_root_count,
        relative_path_root_count,
        folder_count,
        file_count,
        pick_counts,
        rating_counts,
        color_label_counts,
        keyword_count,
        keyword_max_depth,
        keyword_synonym_count,
        keyword_image_link_count,
        collection_kind_counts,
        collection_image_count,
        iptc_row_count,
        folder_stack_count,
    })
}

fn group_counts(conn: &Connection, sql: &str) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::open;
    use std::path::Path;
    use tempfile::TempDir;

    fn fixture_catalog(dir: &Path) -> rusqlite::Connection {
        let path = dir.join("fixture.lrcat");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE Adobe_images (id_local INTEGER PRIMARY KEY, fileFormat TEXT,
                masterImage INTEGER, pick REAL, rating REAL, colorLabels TEXT DEFAULT '');
            INSERT INTO Adobe_images (fileFormat, masterImage, pick, rating, colorLabels) VALUES
                ('RAW', NULL, 0, 5, ''),
                ('RAW', 1, 0, NULL, 'Red'),
                ('JPG', NULL, 1, 3, '');

            CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY,
                absolutePath TEXT, relativePathFromCatalog TEXT);
            INSERT INTO AgLibraryRootFolder (absolutePath, relativePathFromCatalog) VALUES
                ('C:/Photos/', NULL),
                ('E:/Archive/', '../../E:/Archive/');

            CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY);
            INSERT INTO AgLibraryFolder DEFAULT VALUES;

            CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY);
            INSERT INTO AgLibraryFile DEFAULT VALUES;
            INSERT INTO AgLibraryFile DEFAULT VALUES;

            CREATE TABLE AgLibraryKeyword (id_local INTEGER PRIMARY KEY, genealogy TEXT);
            INSERT INTO AgLibraryKeyword (genealogy) VALUES ('/1'), ('/1/2'), ('/1/2/3');

            CREATE TABLE AgLibraryKeywordSynonym (id_local INTEGER PRIMARY KEY);
            CREATE TABLE AgLibraryKeywordImage (id_local INTEGER PRIMARY KEY);
            INSERT INTO AgLibraryKeywordImage DEFAULT VALUES;

            CREATE TABLE AgLibraryCollection (id_local INTEGER PRIMARY KEY, creationId TEXT);
            INSERT INTO AgLibraryCollection (creationId) VALUES
                ('com.adobe.ag.library.collection'), ('com.adobe.ag.library.collection');

            CREATE TABLE AgLibraryCollectionImage (id_local INTEGER PRIMARY KEY);
            CREATE TABLE AgLibraryIPTC (id_local INTEGER PRIMARY KEY);
            CREATE TABLE AgLibraryFolderStack (id_local INTEGER PRIMARY KEY);
            "#,
        )
        .unwrap();
        drop(conn);
        open::open_backup(&path).unwrap()
    }

    #[test]
    fn inspects_a_synthetic_fixture_correctly() {
        let dir = TempDir::new().unwrap();
        let conn = fixture_catalog(dir.path());
        let inv = inspect(&conn).unwrap();

        assert_eq!(inv.asset_count, 3);
        assert_eq!(inv.virtual_copy_count, 1);
        assert_eq!(inv.root_folder_count, 2);
        assert_eq!(inv.drive_letter_root_count, 2);
        assert_eq!(inv.relative_path_root_count, 1);
        assert_eq!(inv.keyword_count, 3);
        assert_eq!(inv.keyword_max_depth, 2);
        assert_eq!(
            inv.collection_kind_counts,
            vec![("com.adobe.ag.library.collection".to_string(), 2)]
        );
    }
}
