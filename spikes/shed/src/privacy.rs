//! Checks a set of files (meant to be every doc this research pass is about to commit) for any
//! string pulled from the real catalog: keyword names, collection names, and path segments. This
//! repo is public and the catalog's keywords include real people's names -- #61's own write-up
//! must describe the *shape* of the data (counts, table structure) without ever repeating a piece
//! of it, and this is the mechanical check that a hand review can miss.
//!
//! **This is a floor under human review, not a substitute for it.** It only catches a
//! byte-identical, case-sensitive substring of something already in the catalog -- it does
//! nothing for a short name/handle under the 6-character floor, a different casing, a
//! hyphenation/spacing variant, or a paraphrase (describing a person rather than quoting a
//! keyword verbatim). "privacy-check: clean" means "no exact match was found," not "this diff
//! contains nothing sensitive" -- every commit in this pass was also manually read before pushing
//! for exactly this reason (see the ADR/research doc's own privacy note).

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

/// Every distinct sensitive string found in the catalog: keyword names, collection names, and
/// `/`-delimited path segments of at least 6 characters, excluding all-digit segments. Real-world
/// tuning (this pass's own first draft, run against a real 380,300-asset catalog): a 4-character
/// floor with no digit exclusion flagged 54 matches across an 8-file doc diff, and every single
/// one was a coincidental generic-vocabulary collision, not an actual leak (year folders like
/// `2026`, product/feature nouns like `Lightroom`/`Denoise` that this very research prose also
/// uses for unrelated reasons, and a `Cons`/`hyper` substring match against ordinary words
/// ("Consequences") and an unrelated crate name (`hyper`) in `Cargo.lock`). Raising the floor to 6
/// and dropping pure-digit segments (a year is not identity-revealing the way a real keyword is)
/// cuts that noise substantially -- but this is a floor on a genuine false-positive rate, not a
/// promise of zero false positives: every flagged match still needs a human to confirm it isn't a
/// real leak before committing, exactly as `docs/adr/0022`'s privacy note describes.
pub fn sensitive_strings(conn: &Connection) -> Result<HashSet<String>> {
    let mut set = HashSet::new();

    // `name != ''` matters here, not just `IS NOT NULL`: an empty string is a substring of every
    // string, so a bare empty keyword name would make `check_files` flag every file it's given,
    // regardless of content -- the same reasoning the collection-name query below already applies.
    let mut stmt =
        conn.prepare("SELECT name FROM AgLibraryKeyword WHERE name IS NOT NULL AND name != ''")?;
    for name in stmt.query_map([], |r| r.get::<_, String>(0))? {
        set.insert(name?);
    }

    let mut stmt =
        conn.prepare("SELECT name FROM AgLibraryCollection WHERE name IS NOT NULL AND name != ''")?;
    for name in stmt.query_map([], |r| r.get::<_, String>(0))? {
        set.insert(name?);
    }

    let mut stmt = conn.prepare(
        "SELECT absolutePath FROM AgLibraryRootFolder \
         UNION SELECT pathFromRoot FROM AgLibraryFolder \
         UNION SELECT baseName FROM AgLibraryFile",
    )?;
    for path in stmt.query_map([], |r| r.get::<_, String>(0))? {
        let path = path?;
        for segment in path.split(['/', '\\']) {
            if segment.len() >= 6 && !segment.chars().all(|c| c.is_ascii_digit()) {
                set.insert(segment.to_string());
            }
        }
    }

    Ok(set)
}

/// Checks `files` for any occurrence of any string in `sensitive`, returning
/// `(file, matched_string)` for every hit found. An empty result means the diff is clean.
pub fn check_files(sensitive: &HashSet<String>, files: &[&Path]) -> Result<Vec<(String, String)>> {
    let mut hits = Vec::new();
    for file in files {
        let contents =
            std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
        for s in sensitive {
            if contents.contains(s.as_str()) {
                hits.push((file.display().to_string(), s.clone()));
            }
        }
    }
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::NamedTempFile;

    #[test]
    fn flags_a_planted_sensitive_string() {
        let mut sensitive = HashSet::new();
        sensitive.insert("RealPersonName".to_string());

        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "This doc mentions RealPersonName by accident.").unwrap();

        let hits = check_files(&sensitive, &[file.path()]).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "RealPersonName");
    }

    #[test]
    fn passes_clean_content() {
        let sensitive: HashSet<String> = ["RealPersonName".to_string()].into_iter().collect();
        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "Only aggregate counts here: 380300 assets.").unwrap();

        let hits = check_files(&sensitive, &[file.path()]).unwrap();
        assert!(hits.is_empty());
    }

    /// Regression test: an empty string is a substring of every string, so a bare empty keyword
    /// name being carried into the sensitive set would make `check_files` flag every file
    /// regardless of content -- `sensitive_strings` must exclude it, the same way it already
    /// excludes an empty collection name.
    #[test]
    fn excludes_an_empty_keyword_name() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE AgLibraryKeyword (name TEXT);
            INSERT INTO AgLibraryKeyword (name) VALUES (''), ('RealKeyword');
            CREATE TABLE AgLibraryCollection (name TEXT);
            CREATE TABLE AgLibraryRootFolder (absolutePath TEXT);
            CREATE TABLE AgLibraryFolder (pathFromRoot TEXT);
            CREATE TABLE AgLibraryFile (baseName TEXT);
            "#,
        )
        .unwrap();

        let sensitive = sensitive_strings(&conn).unwrap();
        assert!(!sensitive.contains(""));
        assert!(sensitive.contains("RealKeyword"));

        let file = NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "Nothing sensitive in this file at all.").unwrap();
        let hits = check_files(&sensitive, &[file.path()]).unwrap();
        assert!(hits.is_empty());
    }
}
