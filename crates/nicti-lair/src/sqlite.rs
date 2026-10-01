//! `rusqlite`-backed `CatalogStore` (ADR-0067: SQLite, WAL). One `Connection` per store, guarded
//! by a `Mutex` — `rusqlite::Connection` needs `&mut self` for a transaction, and `Module` (via
//! `CatalogStore`) requires `Send + Sync` since it's shared as `Arc<dyn CatalogStore>` through the
//! Claw registry.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension, ToSql};
use unicode_normalization::UnicodeNormalization;

use crate::{
    Asset, AssetMeta, CatalogError, CatalogStore, Collection, CollectionKind, Cursor, DeleteItem,
    DeleteState, FacetCounts, Filter, Keyword, LrcChunkOutcome, LrcItem, LrcProvenance, MoveState,
    NewAsset, Page, Preview, PreviewTier, Root, RootMove, SidecarState, Sort, SortDirection,
    SortField,
};
use nicti_claw::Module;

/// An edit document's canonical JSON, mapped into this crate's error type.
fn canonical_doc(doc: &nicti_pawprint::EditDocument) -> Result<String, CatalogError> {
    nicti_pawprint::to_canonical_json(doc).map_err(|e| CatalogError::Document(e.to_string()))
}

fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Stable hash of a marker set, for `lrc_provenance.applied_meta_hash`.
fn meta_hash(meta: &AssetMeta) -> String {
    blake3_hex(format!("{:?}|{:?}|{:?}", meta.rating, meta.flag, meta.label).as_bytes())
}

/// `hunt`'s `rating` sort key stand-in for NULL/unrated -- distinct from `FACET_UNRATED_SENTINEL`
/// (a different table, different constraint), but the same idea: well outside the real `-1..=5`
/// range so it can never collide with a genuine rating, placing unrated assets at one end of a
/// rating-sorted page rather than interleaving with real ratings.
const RATING_SORT_SENTINEL: i64 = -1000;

/// Case/composition-folded form for `keyword.name_fold` -- same NFC-then-lowercase rule
/// `scruff.rs::fold` uses for `rel_path_fold`, so two visually-identical names typed via different
/// input methods (composed vs. decomposed Unicode) still collide as the same keyword.
fn fold_keyword_name(name: &str) -> String {
    let composed: String = name.nfc().collect();
    composed.to_lowercase()
}

/// Escapes SQLite `GLOB` metacharacters (`*`, `?`, `[`) as one-character classes (`[*]`, `[?]`,
/// `[[]`) so caller text used in `hunt`'s `rel_path_prefix`/`filename_contains` filters is matched
/// literally, not as a wildcard/character-class pattern -- without this, a search for `IMG_[1]`
/// would match `IMG_1` (character class), and a lone unmatched `[` would match nothing at all.
fn glob_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '*' | '?' | '[' => {
                out.push('[');
                out.push(ch);
                out.push(']');
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Runs a self-check pragma (`quick_check` or `integrity_check` -- always one of this module's own
/// hardcoded literals, never external input, so `format!`ing it into the SQL carries no injection
/// risk) against any connection and collapses its result rows to `None` (healthy) or `Some(msg)`
/// (every non-"ok" row joined together). Shared by `SqliteCatalog::quick_check` (the live catalog)
/// and `ninelives::verify` (a freshly written backup copy) so both read the same pragma the same
/// way.
pub(crate) fn first_check_problem(
    conn: &Connection,
    pragma: &str,
) -> Result<Option<String>, CatalogError> {
    let mut stmt = conn.prepare(&format!("PRAGMA {pragma}"))?;
    let mut rows = stmt.query([])?;
    let mut problems = Vec::new();
    while let Some(row) = rows.next()? {
        let msg: String = row.get(0)?;
        if !msg.eq_ignore_ascii_case("ok") {
            problems.push(msg);
        }
    }
    Ok(if problems.is_empty() {
        None
    } else {
        Some(problems.join("; "))
    })
}

fn row_to_keyword(row: &rusqlite::Row) -> rusqlite::Result<Keyword> {
    Ok(Keyword {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        name: row.get(2)?,
        path: row.get(3)?,
    })
}

/// `facet_counts.rating`'s stand-in for NULL/unrated (schema.rs's `MIGRATION_V3` — a PRIMARY KEY
/// column can't itself be NULL), chosen well outside the real `-1..=5` range so it can never
/// collide with a genuine rating.
const FACET_UNRATED_SENTINEL: i64 = -128;

/// A `Filter` compiled to a bound SQL fragment, shared by `hunt`/`hunt_count`/`facets` so all
/// three answer exactly the same predicate. Always includes `v.online = 1` (facet_count's own
/// online-volume scoping) -- the caller's query must join `asset a` to `root r` (`r.id =
/// a.root_id`) and `volume v` (`v.id = r.volume_id`) for this fragment's `v`/`a` aliases to
/// resolve.
struct FilterSql {
    where_clause: String,
    params: Vec<Box<dyn ToSql>>,
}

/// The ORDER BY / keyset-comparison expression for a `SortField`. These must match the
/// `idx_asset_sort_*` expression indexes (schema V7) term-for-term, or SQLite falls back to a
/// temp B-tree sort of the whole result set -- change one, change the other, and the
/// `hunt_sort_uses_an_index` test will say if they drift.
fn sort_column_sql(field: SortField) -> &'static str {
    match field {
        SortField::Captured => "COALESCE(a.captured_at, '')",
        SortField::Imported => "a.imported_at",
        SortField::Filename => "a.rel_path_fold",
        SortField::Rating => "COALESCE(a.rating, -1000)",
    }
}

fn build_filter_sql(conn: &Connection, filter: &Filter) -> Result<FilterSql, CatalogError> {
    let mut clauses: Vec<String> = vec!["v.online = 1".to_string()];
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    if let Some(keyword_id) = filter.keyword_id {
        if filter.include_subtree {
            // A saved smart-collection rule can outlive the keyword it names (`delete_keyword`
            // doesn't touch `collection.rule_json`) -- a missing keyword must make this filter
            // match nothing, not fail the whole query with a `QueryReturnedNoRows` error.
            let path: Option<String> = conn
                .query_row(
                    "SELECT path FROM keyword WHERE id = ?1",
                    [keyword_id],
                    |row| row.get(0),
                )
                .optional()?;
            match path {
                Some(path) => {
                    clauses.push(
                        "EXISTS (SELECT 1 FROM asset_keyword ak JOIN keyword k ON k.id = ak.keyword_id \
                         WHERE ak.asset_id = a.id AND k.path GLOB ?)"
                            .to_string(),
                    );
                    params.push(Box::new(format!("{path}*")));
                }
                None => clauses.push("0".to_string()),
            }
        } else {
            clauses.push(
                "EXISTS (SELECT 1 FROM asset_keyword ak \
                 WHERE ak.asset_id = a.id AND ak.keyword_id = ?)"
                    .to_string(),
            );
            params.push(Box::new(keyword_id));
        }
    }

    if filter.rating_min.is_some() || filter.rating_max.is_some() || filter.include_unrated {
        let min = filter.rating_min.unwrap_or(-1);
        let max = filter.rating_max.unwrap_or(5);
        if filter.include_unrated {
            clauses.push("(a.rating IS NULL OR (a.rating >= ? AND a.rating <= ?))".to_string());
        } else {
            clauses.push("(a.rating IS NOT NULL AND a.rating >= ? AND a.rating <= ?)".to_string());
        }
        params.push(Box::new(min));
        params.push(Box::new(max));
    }

    if filter.unrated {
        clauses.push("a.rating IS NULL".to_string());
    }
    if let Some(flag) = filter.flag {
        clauses.push("a.flag = ?".to_string());
        params.push(Box::new(flag));
    }
    if filter.unflagged {
        clauses.push("a.flag IS NULL".to_string());
    }
    if let Some(label) = &filter.label {
        clauses.push("a.label = ?".to_string());
        params.push(Box::new(label.clone()));
    }
    if filter.no_label {
        clauses.push("a.label IS NULL".to_string());
    }
    if let Some(make) = &filter.make {
        clauses.push("a.make = ?".to_string());
        params.push(Box::new(make.clone()));
    }
    if let Some(model) = &filter.model {
        clauses.push("a.model = ?".to_string());
        params.push(Box::new(model.clone()));
    }
    if filter.captured_after.is_some() || filter.captured_before.is_some() {
        // Ingest stores a blank DateTimeOriginal as the literal text 'unknown' (kamadak-exif's
        // display form), which would sort after every digit-leading bound and leak into every
        // "from" range while being excluded by every "to" range. Only real dates take part.
        clauses.push("a.captured_at GLOB '[0-9][0-9][0-9][0-9]-*'".to_string());
    }
    if let Some(after) = &filter.captured_after {
        clauses.push("a.captured_at >= ?".to_string());
        params.push(Box::new(after.clone()));
    }
    if let Some(before) = &filter.captured_before {
        clauses.push("a.captured_at <= ?".to_string());
        params.push(Box::new(before.clone()));
    }
    if let Some(root_id) = filter.root_id {
        clauses.push("a.root_id = ?".to_string());
        params.push(Box::new(root_id));
    }
    if let Some(prefix) = &filter.rel_path_prefix {
        clauses.push("a.rel_path_fold GLOB ?".to_string());
        params.push(Box::new(format!(
            "{}*",
            glob_escape(&fold_keyword_name(prefix))
        )));
    }
    if let Some(needle) = &filter.filename_contains {
        clauses.push("a.rel_path_fold GLOB ?".to_string());
        params.push(Box::new(format!(
            "*{}*",
            glob_escape(&fold_keyword_name(needle))
        )));
    }

    Ok(FilterSql {
        where_clause: clauses.join(" AND "),
        params,
    })
}

fn row_to_collection(row: &rusqlite::Row) -> rusqlite::Result<Collection> {
    let kind_str: String = row.get(2)?;
    let kind = kind_str.parse().unwrap_or(CollectionKind::Manual);
    Ok(Collection {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        kind,
        name: row.get(3)?,
    })
}

/// `collection.rule_json`'s on-disk shape for a `Smart` collection -- versioned so a future
/// change to `Filter`'s own shape can still read an old row (#155's LRC smart-collection mapping
/// is expected to be the first real consumer of a `v: 1` rule beyond this crate's own tests).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SmartRule {
    v: u32,
    filter: Filter,
}

/// SQLite's default `SQLITE_MAX_VARIABLE_NUMBER` is 32766 -- comfortably above any realistic
/// culling multi-selection, but chunked well under it anyway so a "select all" bulk action
/// against an unusually large batch can't hit the limit (found by CodeRabbit's review).
const MAX_IDS_PER_STATEMENT: usize = 500;

/// Shared by `set_rating`/`set_flag`/`set_label`: builds and runs `UPDATE asset SET <column> = ?
/// WHERE id IN (...)`, chunked to stay under SQLite's parameter limit, all chunks committed
/// together in one transaction so the whole call is atomic (a partial write across chunks would
/// otherwise be possible with autocommit-per-statement). `column` is always one of this module's
/// own hardcoded literals, never external input, so building its SQL with `format!` carries no
/// injection risk -- every actual *value* (`value`, and every element of `asset_ids`) is bound as
/// a `?` parameter, never interpolated.
fn set_column_for_assets(
    conn: &mut Connection,
    column: &str,
    value: &dyn ToSql,
    asset_ids: &[i64],
) -> Result<(), CatalogError> {
    if asset_ids.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for chunk in asset_ids.chunks(MAX_IDS_PER_STATEMENT) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = format!("UPDATE asset SET {column} = ?1 WHERE id IN ({placeholders})");
        let mut bound: Vec<&dyn ToSql> = Vec::with_capacity(chunk.len() + 1);
        bound.push(value);
        for id in chunk {
            bound.push(id);
        }
        tx.execute(&sql, bound.as_slice())?;
    }
    tx.commit()?;
    Ok(())
}

/// The empty `EditDocument` shape (`{ stages: BTreeMap<String, StageEntry> }`, ADR-0021),
/// serialized from the real `nicti_pawprint::EditDocument` (#45 promoted it from `spikes/pawprint`)
/// rather than a hand-written literal, so this can never drift from that type's actual shape.
fn empty_edit_document() -> String {
    serde_json::to_string(&nicti_pawprint::EditDocument::default())
        .expect("EditDocument always serializes")
}

pub struct SqliteCatalog {
    conn: Mutex<Connection>,
    /// This catalog's own file path, `None` for an in-memory catalog (tests only). Nine Lives
    /// (#25, `crate::ninelives`) needs this to open its own read-only snapshot connection rather
    /// than sharing (and blocking on) the locked writer connection above.
    path: Option<PathBuf>,
}

impl SqliteCatalog {
    /// Runs a long read-only scan on its own snapshot connection when the catalog is file-backed
    /// (WAL: consistent, never blocks a writer), else on the shared one. Holding the shared mutex
    /// for a full-table scan would freeze every UI-thread catalog call (`list_roots`, `get_asset`)
    /// -- see `hunt_ids`.
    fn with_scan_conn<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, CatalogError>,
    ) -> Result<T, CatalogError> {
        if self.path.is_some() {
            f(&self.open_snapshot_reader()?)
        } else {
            f(&self.conn.lock().unwrap())
        }
    }

    fn facets_on(conn: &Connection, filter: &Filter) -> Result<FacetCounts, CatalogError> {
        // An unfiltered query reads the trigger-maintained cache (ADR-0103's own optimized case);
        // any narrowing filter falls back to a live, exact GROUP BY, since the cache's
        // (volume_id, model, rating) grain has no way to answer a keyword/date/etc-narrowed facet
        // count on its own.
        let is_unfiltered = *filter == Filter::default();

        let (by_model, by_rating) = if is_unfiltered {
            let mut stmt = conn.prepare(
                "SELECT fc.model, SUM(fc.cnt) FROM facet_counts fc \
                     JOIN volume v ON v.id = fc.volume_id \
                     WHERE v.online = 1 GROUP BY fc.model",
            )?;
            let by_model = stmt
                .query_map([], |row| {
                    let model: String = row.get(0)?;
                    let cnt: i64 = row.get(1)?;
                    Ok((
                        if model.is_empty() { None } else { Some(model) },
                        cnt as u64,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut stmt = conn.prepare(
                "SELECT fc.rating, SUM(fc.cnt) FROM facet_counts fc \
                     JOIN volume v ON v.id = fc.volume_id \
                     WHERE v.online = 1 GROUP BY fc.rating",
            )?;
            let by_rating = stmt
                .query_map([], |row| {
                    let rating: i64 = row.get(0)?;
                    let cnt: i64 = row.get(1)?;
                    Ok((
                        if rating == FACET_UNRATED_SENTINEL {
                            None
                        } else {
                            Some(rating)
                        },
                        cnt as u64,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            (by_model, by_rating)
        } else {
            let filter_sql = build_filter_sql(conn, filter)?;
            let base = format!(
                "FROM asset a \
                     JOIN root r ON r.id = a.root_id \
                     JOIN volume v ON v.id = r.volume_id \
                     WHERE {}",
                filter_sql.where_clause
            );

            let sql = format!("SELECT a.model, COUNT(*) {base} GROUP BY a.model");
            let param_refs: Vec<&dyn ToSql> =
                filter_sql.params.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let by_model = stmt
                .query_map(param_refs.as_slice(), |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)? as u64,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let sql = format!("SELECT a.rating, COUNT(*) {base} GROUP BY a.rating");
            let param_refs: Vec<&dyn ToSql> =
                filter_sql.params.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let by_rating = stmt
                .query_map(param_refs.as_slice(), |row| {
                    Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)? as u64))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            (by_model, by_rating)
        };

        // `flag` has no trigger-maintained cache at all (it's a much lower-cardinality, cheaper
        // aggregate than model/rating), so it's always a live GROUP BY, filtered or not.
        let by_flag: Vec<(Option<i64>, u64)> = {
            let filter_sql = build_filter_sql(conn, filter)?;
            let sql = format!(
                "SELECT a.flag, COUNT(*) FROM asset a \
                 JOIN root r ON r.id = a.root_id \
                 JOIN volume v ON v.id = r.volume_id \
                 WHERE {} GROUP BY a.flag",
                filter_sql.where_clause
            );
            let param_refs: Vec<&dyn ToSql> =
                filter_sql.params.iter().map(|p| p.as_ref()).collect();
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(param_refs.as_slice(), |row| {
                    Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, i64>(1)? as u64))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };

        let total: u64 = by_model.iter().map(|(_, cnt)| cnt).sum();

        Ok(FacetCounts {
            by_model,
            by_rating,
            by_flag,
            total,
        })
    }

    /// Distinct non-empty values of one `asset` text column, online volumes only. `column` is
    /// always a compile-time literal from this file, never caller input.
    fn distinct_asset_column(&self, column: &str) -> Result<Vec<String>, CatalogError> {
        self.with_scan_conn(|conn| Self::distinct_asset_column_on(conn, column))
    }

    fn distinct_asset_column_on(
        conn: &Connection,
        column: &str,
    ) -> Result<Vec<String>, CatalogError> {
        let sql = format!(
            "SELECT DISTINCT a.{column} FROM asset a \
             JOIN root r ON r.id = a.root_id \
             JOIN volume v ON v.id = r.volume_id \
             WHERE v.online = 1 AND a.{column} IS NOT NULL AND a.{column} <> '' \
             ORDER BY a.{column} COLLATE NOCASE ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn open(path: &Path) -> Result<Self, CatalogError> {
        let conn = Connection::open(path)?;
        Self::init(conn, Some(path.to_path_buf()))
    }

    pub fn open_in_memory() -> Result<Self, CatalogError> {
        let conn = Connection::open_in_memory()?;
        Self::init(conn, None)
    }

    fn init(mut conn: Connection, path: Option<PathBuf>) -> Result<Self, CatalogError> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        crate::schema::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// Inserts `n` synthetic assets under `root_id` in one transaction -- for scale tests and
    /// benchmarks (#30's 1M-asset grid snapshot) only, hence `doc(hidden)`; real assets always go
    /// through `insert_asset`/Scruff. Rows get distinct filenames, spread capture times (with
    /// ties and NULLs), import times and ratings, so every `SortField` has real work to do. If
    /// `previews` is `Some((preview, k))`, the first `k` assets also get a copy of `preview` as
    /// their T0 (giving every asset one would make a 1M-row catalog hundreds of GB).
    #[doc(hidden)]
    pub fn bulk_seed(
        &self,
        root_id: i64,
        n: usize,
        previews: Option<(&Preview, usize)>,
    ) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO asset (root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                 captured_at, rating, imported_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            )?;
            let mut insert_preview = tx.prepare(
                "INSERT INTO preview (asset_id, tier, width, height, bytes) VALUES (?1,?2,?3,?4,?5)",
            )?;
            for i in 0..n {
                let name = format!("{:07}.NEF", (i * 7919) % n.max(1));
                // Every fifth asset has no capture time; the rest collide in groups of three.
                let captured = (i % 5 != 0).then(|| {
                    format!(
                        "2026:01:01 {:02}:{:02}:{:02}",
                        (i / 3600) % 24,
                        (i / 60) % 60,
                        (i / 3) % 60
                    )
                });
                let rating = (i % 4 != 0).then_some((i % 6) as i64 - 1);
                insert.execute(params![
                    root_id,
                    name,
                    name.to_lowercase(),
                    1000i64,
                    0i64,
                    captured,
                    rating,
                    (i % 100_000) as i64
                ])?;
                if let Some((preview, k)) = previews {
                    if i < k {
                        insert_preview.execute(params![
                            tx.last_insert_rowid(),
                            PreviewTier::T0.as_str(),
                            preview.width,
                            preview.height,
                            preview.bytes
                        ])?;
                    }
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// This catalog's own file path, `None` for an in-memory catalog (tests only).
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Rows changed on this connection since it was opened (`sqlite3_total_changes`) -- a
    /// per-connection counter that resets to 0 every process start, not a persisted one. Nine
    /// Lives (#25) uses this alongside its own startup bookkeeping to tell "nothing changed since
    /// the last backup this session" apart from "a previous session crashed before backing up its
    /// own changes" (see `ninelives::NineLives::due`'s own doc comment).
    pub fn change_counter(&self) -> u64 {
        self.conn.lock().unwrap().total_changes()
    }

    /// The full-file BLAKE3 (hex) a verified folder move (#26) recorded for this asset; `None`
    /// for an asset that has never been moved.
    pub fn content_hash(&self, asset_id: i64) -> Result<Option<String>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT content_hash FROM asset WHERE id = ?1",
                params![asset_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten())
    }

    /// `PRAGMA user_version` of the live catalog -- Nine Lives compares this against a freshly
    /// written backup's own `user_version` as one of its verification checks (a `VACUUM INTO` copy
    /// always carries the source's schema version, so a mismatch would mean something read a
    /// different file than it meant to, not a real migration race).
    pub fn user_version(&self) -> Result<i64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    /// `PRAGMA quick_check` on the live catalog -- the cheap structural check Nine Lives runs
    /// before every backup attempt (`docs/adr/0025`'s "check the live DB too" step), so a corrupt
    /// live catalog is reported rather than silently backed up over yesterday's still-good copies.
    /// `Ok(None)` means healthy; `Ok(Some(msg))` carries the first problem SQLite reported.
    pub fn quick_check(&self) -> Result<Option<String>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        match first_check_problem(&conn, "quick_check") {
            // Sufficiently damaged pages make SQLite fail the pragma itself rather than list a
            // problem row -- that's still "the live catalog is corrupt", not an I/O hiccup, and
            // callers (Nine Lives) must see it as such instead of a generic error.
            Err(CatalogError::Sqlite(rusqlite::Error::SqliteFailure(e, msg)))
                if matches!(
                    e.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                ) =>
            {
                Ok(Some(msg.unwrap_or_else(|| e.to_string())))
            }
            other => other,
        }
    }

    /// Opens a fresh, independent read-only connection onto this catalog's own file for Nine
    /// Lives to run `VACUUM INTO` against, so a ~2s snapshot at 2M rows (ADR-0067's own measured
    /// figure) never holds the `conn` mutex every other catalog query goes through. Returns
    /// `Err(CatalogError::Io(_))` for an in-memory catalog (`path` is `None`) -- there is no file
    /// to reopen read-only; a caller backing up an in-memory catalog (tests only) uses the locked
    /// main connection directly instead, see `ninelives::snapshot_into`'s own doc comment.
    pub fn open_snapshot_reader(&self) -> Result<Connection, CatalogError> {
        let path = self
            .path
            .as_deref()
            .ok_or_else(|| CatalogError::Io("catalog has no backing file to snapshot".into()))?;
        Ok(Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?)
    }

    /// Runs `VACUUM INTO ?1` (bound, never interpolated) on the *locked* main connection --
    /// `ninelives::snapshot_into`'s only path for an in-memory catalog, since there's no file to
    /// open a second, independent read-only connection onto. Never used for a file-backed catalog
    /// (that path uses `open_snapshot_reader` instead, precisely to avoid holding this lock for
    /// the duration of a multi-second vacuum).
    pub(crate) fn vacuum_into_locked(&self, target: &Path) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        let target_str = target
            .to_str()
            .ok_or_else(|| CatalogError::Io("backup target path is not valid UTF-8".into()))?;
        conn.execute("VACUUM INTO ?1", params![target_str])?;
        Ok(())
    }

    /// Total asset count across every root/volume -- a placeholder library-view figure
    /// (`nicti-pelt`'s Library panel, #241) until #30's real paged/faceted grid query exists.
    /// Inherent, not on `CatalogStore`: a plain count has no per-backend variation worth an
    /// extension-point method yet, unlike the trait's other queries.
    pub fn asset_count(&self) -> Result<u64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM asset", [], |row| row.get(0))?;
        Ok(count as u64)
    }

    fn row_to_asset(row: &rusqlite::Row) -> rusqlite::Result<Asset> {
        Ok(Asset {
            id: row.get(0)?,
            root_id: row.get(1)?,
            rel_path: row.get(2)?,
            rel_path_fold: row.get(3)?,
            size_bytes: row.get::<_, i64>(4)? as u64,
            mtime_unix: row.get(5)?,
            fingerprint: row.get(6)?,
            natural_key: row.get(7)?,
            make: row.get(8)?,
            model: row.get(9)?,
            captured_at: row.get(10)?,
            rating: row.get(11)?,
            flag: row.get(12)?,
            label: row.get(13)?,
            width: row.get::<_, Option<i64>>(14)?.map(|w| w as u32),
            height: row.get::<_, Option<i64>>(15)?.map(|h| h as u32),
            imported_at: row.get(16)?,
            missing_since: row.get(17)?,
        })
    }
}

const ASSET_COLUMNS: &str = "id, root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
    fingerprint, natural_key, make, model, captured_at, rating, flag, label, width, height, \
    imported_at, missing_since";

impl Module for SqliteCatalog {
    fn id(&self) -> &str {
        "nicti.catalog.sqlite"
    }

    fn schema_version(&self) -> u32 {
        1
    }

    fn migrate_params(
        &self,
        _from_version: u32,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        Some(params)
    }
}

impl SqliteCatalog {
    /// `<stmt> WHERE asset_id IN (...)`, chunked, one transaction: the shared shape of the
    /// `delete_item` journal's two bulk updates. `stmt` is always a hardcoded literal from this
    /// module, never external input.
    fn update_delete_items(&self, asset_ids: &[i64], stmt: &str) -> Result<(), CatalogError> {
        if asset_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for chunk in asset_ids.chunks(MAX_IDS_PER_STATEMENT) {
            let ph = vec!["?"; chunk.len()].join(",");
            tx.execute(
                &format!("{stmt} WHERE asset_id IN ({ph})"),
                rusqlite::params_from_iter(chunk.iter()),
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

impl SqliteCatalog {
    /// Shared body of `get_master_edit`/`get_master_edits`, on an already-locked connection.
    fn read_master_edit(
        conn: &Connection,
        asset_id: i64,
    ) -> Result<Option<nicti_pawprint::EditDocument>, CatalogError> {
        let json: Option<String> = conn
            .query_row(
                "SELECT document FROM edit_variant WHERE asset_id = ?1 AND is_master = 1",
                params![asset_id],
                |row| row.get(0),
            )
            .optional()?;
        match json {
            Some(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|e| CatalogError::Document(e.to_string())),
            None => {
                // No master row: an existing asset still reads as "no edits yet"; a missing
                // asset reads as `None`.
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM asset WHERE id = ?1)",
                    params![asset_id],
                    |row| row.get(0),
                )?;
                Ok(exists.then(nicti_pawprint::EditDocument::default))
            }
        }
    }
}

impl CatalogStore for SqliteCatalog {
    fn upsert_volume(
        &self,
        identity_key: &str,
        label: Option<&str>,
        marker_uuid: Option<&str>,
        now_unix: i64,
    ) -> Result<i64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT id, marker_uuid FROM volume WHERE identity_key = ?1",
                [identity_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;

        if let Some((id, existing_marker)) = existing {
            if let (Some(existing_marker), Some(incoming_marker)) = (&existing_marker, marker_uuid)
            {
                if existing_marker != incoming_marker {
                    return Err(CatalogError::VolumeIdentityConflict {
                        identity_key: identity_key.to_string(),
                        existing: Some(existing_marker.clone()),
                        incoming: Some(incoming_marker.to_string()),
                    });
                }
            }
            let marker_to_store = marker_uuid.map(str::to_string).or(existing_marker);
            conn.execute(
                "UPDATE volume SET label = ?1, marker_uuid = ?2, online = 1, last_seen_at = ?3 \
                 WHERE id = ?4",
                params![label, marker_to_store, now_unix, id],
            )?;
            Ok(id)
        } else {
            conn.execute(
                "INSERT INTO volume (identity_key, label, marker_uuid, online, last_seen_at) \
                 VALUES (?1, ?2, ?3, 1, ?4)",
                params![identity_key, label, marker_uuid, now_unix],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }

    fn ensure_root(&self, volume_id: i64, rel_path: &str) -> Result<i64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO root (volume_id, rel_path) VALUES (?1, ?2) \
             ON CONFLICT(volume_id, rel_path) DO NOTHING",
            params![volume_id, rel_path],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM root WHERE volume_id = ?1 AND rel_path = ?2",
            params![volume_id, rel_path],
            |row| row.get(0),
        )?)
    }

    fn list_roots(&self) -> Result<Vec<Root>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT id, volume_id, rel_path, archived FROM root ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok(Root {
                id: row.get(0)?,
                volume_id: row.get(1)?,
                path: row.get(2)?,
                archived: row.get::<_, i64>(3)? != 0,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn begin_root_move(
        &self,
        root_id: i64,
        dest_path: &str,
        now_unix: i64,
    ) -> Result<i64, CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (volume_id, src_path): (i64, String) = tx
            .query_row(
                "SELECT volume_id, rel_path FROM root WHERE id = ?1",
                params![root_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| CatalogError::Io(format!("root {root_id} does not exist")))?;
        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM root_move WHERE root_id = ?1",
            params![root_id],
            |row| row.get(0),
        )?;
        if open > 0 {
            return Err(CatalogError::Io(format!(
                "root {root_id} already has a move in progress"
            )));
        }
        let taken: i64 = tx.query_row(
            "SELECT COUNT(*) FROM root WHERE volume_id = ?1 AND rel_path = ?2",
            params![volume_id, dest_path],
            |row| row.get(0),
        )?;
        if taken > 0 {
            return Err(CatalogError::Io(format!(
                "{dest_path} is already a registered folder"
            )));
        }
        tx.execute(
            "INSERT INTO root_move (root_id, src_path, dest_path, state, started_at) \
             VALUES (?1, ?2, ?3, 'copying', ?4)",
            params![root_id, src_path, dest_path, now_unix],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(id)
    }

    fn commit_root_move(&self, move_id: i64, hashes: &[(i64, String)]) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let (root_id, dest_path, state): (i64, String, String) = tx
            .query_row(
                "SELECT root_id, dest_path, state FROM root_move WHERE id = ?1",
                params![move_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| CatalogError::Io(format!("move {move_id} does not exist")))?;
        if state == "committed" {
            return Ok(());
        }
        tx.execute(
            "UPDATE root SET rel_path = ?1 WHERE id = ?2",
            params![dest_path, root_id],
        )?;
        {
            let mut stmt =
                tx.prepare("UPDATE asset SET content_hash = ?1 WHERE id = ?2 AND root_id = ?3")?;
            for (asset_id, hash) in hashes {
                stmt.execute(params![hash, asset_id, root_id])?;
            }
        }
        tx.execute(
            "UPDATE root_move SET state = 'committed' WHERE id = ?1",
            params![move_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn set_root_move_state(&self, move_id: i64, state: MoveState) -> Result<(), CatalogError> {
        let name = match state {
            MoveState::Copying => "copying",
            MoveState::Renaming => "renaming",
            MoveState::Committed => {
                return Err(CatalogError::Io(
                    "a move is only committed via commit_root_move".into(),
                ))
            }
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE root_move SET state = ?1 WHERE id = ?2 AND state != 'committed'",
            params![name, move_id],
        )?;
        Ok(())
    }

    fn finish_root_move(&self, move_id: i64) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM root_move WHERE id = ?1", params![move_id])?;
        Ok(())
    }

    fn open_root_moves(&self) -> Result<Vec<RootMove>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id, root_id, src_path, dest_path, state FROM root_move ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            let state: String = row.get(4)?;
            Ok(RootMove {
                id: row.get(0)?,
                root_id: row.get(1)?,
                src_path: row.get(2)?,
                dest_path: row.get(3)?,
                state: match state.as_str() {
                    "committed" => MoveState::Committed,
                    "renaming" => MoveState::Renaming,
                    _ => MoveState::Copying,
                },
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn find_asset_by_path(
        &self,
        root_id: i64,
        rel_path: &str,
    ) -> Result<Option<Asset>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {ASSET_COLUMNS} FROM asset WHERE root_id = ?1 AND rel_path = ?2"),
                params![root_id, rel_path],
                Self::row_to_asset,
            )
            .optional()?)
    }

    fn get_asset(&self, id: i64) -> Result<Option<Asset>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {ASSET_COLUMNS} FROM asset WHERE id = ?1"),
                params![id],
                Self::row_to_asset,
            )
            .optional()?)
    }

    fn get_master_edit(
        &self,
        asset_id: i64,
    ) -> Result<Option<nicti_pawprint::EditDocument>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Self::read_master_edit(&conn, asset_id)
    }

    fn get_master_edits(
        &self,
        asset_ids: &[i64],
    ) -> Result<Vec<(i64, Option<nicti_pawprint::EditDocument>)>, CatalogError> {
        // One lock for the whole batch: the mutex already keeps every writer out meanwhile.
        let conn = self.conn.lock().unwrap();
        asset_ids
            .iter()
            .map(|&id| Ok((id, Self::read_master_edit(&conn, id)?)))
            .collect()
    }

    fn put_master_edit(
        &self,
        asset_id: i64,
        doc: &nicti_pawprint::EditDocument,
    ) -> Result<(), CatalogError> {
        let json = nicti_pawprint::to_canonical_json(doc)
            .map_err(|e| CatalogError::Document(e.to_string()))?;
        let conn = self.conn.lock().unwrap();
        // Upsert on the (asset_id, name) key ingest also uses, so an asset whose master row is
        // somehow missing still gets one. A nonexistent asset trips the FK and errors.
        conn.execute(
            "INSERT INTO edit_variant (asset_id, name, is_master, document) \
             VALUES (?1, 'master', 1, ?2) \
             ON CONFLICT(asset_id, name) DO UPDATE SET document = excluded.document",
            params![asset_id, json],
        )?;
        Ok(())
    }

    fn put_master_edits(
        &self,
        edits: &[(i64, nicti_pawprint::EditDocument)],
    ) -> Result<(), CatalogError> {
        // Serialise every document before touching the database, so a bad one is refused without
        // opening a transaction at all.
        let rows = edits
            .iter()
            .map(|(id, doc)| {
                nicti_pawprint::to_canonical_json(doc)
                    .map(|json| (*id, json))
                    .map_err(|e| CatalogError::Document(e.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO edit_variant (asset_id, name, is_master, document) \
                 VALUES (?1, 'master', 1, ?2) \
                 ON CONFLICT(asset_id, name) DO UPDATE SET document = excluded.document",
            )?;
            for (id, json) in &rows {
                stmt.execute(params![id, json])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn apply_lrc_chunk(&self, items: &[LrcItem]) -> Result<LrcChunkOutcome, CatalogError> {
        let mut out = LrcChunkOutcome::default();
        if items.is_empty() {
            return Ok(out);
        }
        // Serialise every document up front: a non-finite float is refused before a transaction
        // opens, the same as `put_master_edits`.
        let docs = items
            .iter()
            .map(|item| {
                item.document
                    .as_ref()
                    .map(canonical_doc)
                    .transpose()
                    .map(|json| json.map(|j| (blake3_hex(j.as_bytes()), j)))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let empty_hash =
            blake3_hex(canonical_doc(&nicti_pawprint::EditDocument::default())?.as_bytes());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for (item, doc) in items.iter().zip(&docs) {
            let existing: Option<(i64, Option<String>, Option<String>, i64)> = tx
                .query_row(
                    "SELECT variant_id, applied_doc_hash, applied_meta_hash, asset_id \
                     FROM lrc_provenance WHERE lrc_image_global = ?1",
                    params![item.provenance.image_global],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .optional()?;
            // The same LRC image now resolves to a different asset (a changed remap): its
            // document and markers would land on different assets, so leave it alone.
            if existing.as_ref().is_some_and(|e| e.3 != item.asset_id) {
                out.skipped_conflicts += 1;
                continue;
            }
            let existing = existing.map(|(v, d, m, _)| (v, d, m));

            let variant_id = match &existing {
                Some((id, _, _)) => *id,
                None => match &item.variant_name {
                    None => {
                        tx.execute(
                            "INSERT INTO edit_variant (asset_id, name, is_master, document) \
                             VALUES (?1, 'master', 1, ?2) ON CONFLICT(asset_id, name) DO NOTHING",
                            params![item.asset_id, empty_edit_document()],
                        )?;
                        tx.query_row(
                            "SELECT id FROM edit_variant WHERE asset_id = ?1 AND is_master = 1",
                            params![item.asset_id],
                            |row| row.get(0),
                        )?
                    }
                    Some(base) => {
                        let mut created = None;
                        for n in 0..1000 {
                            let name = if n == 0 {
                                base.clone()
                            } else {
                                format!("{base} ({n})")
                            };
                            let inserted = tx.execute(
                                "INSERT INTO edit_variant (asset_id, name, is_master, document) \
                                 VALUES (?1, ?2, 0, ?3) ON CONFLICT(asset_id, name) DO NOTHING",
                                params![item.asset_id, name, empty_edit_document()],
                            )?;
                            if inserted == 1 {
                                created = Some(tx.last_insert_rowid());
                                break;
                            }
                        }
                        out.variants_created += 1;
                        created.ok_or_else(|| {
                            CatalogError::Io(format!("no free variant name for {base:?}"))
                        })?
                    }
                },
            };

            // Two different LRC masters resolving to one asset (two roots remapped to the same
            // folder): the variant already belongs to another LRC image's provenance row.
            if existing.is_none() {
                let owner: Option<String> = tx
                    .query_row(
                        "SELECT lrc_image_global FROM lrc_provenance WHERE variant_id = ?1",
                        params![variant_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if owner.is_some_and(|g| g != item.provenance.image_global) {
                    out.skipped_conflicts += 1;
                    continue;
                }
            }

            // Document: written only while the variant still holds what the import last wrote
            // (or, first time, is still empty) -- never over the user's own edit.
            let mut applied_doc_hash = existing.as_ref().and_then(|e| e.1.clone());
            if let Some((new_hash, new_json)) = doc {
                let current: String = tx.query_row(
                    "SELECT document FROM edit_variant WHERE id = ?1",
                    params![variant_id],
                    |row| row.get(0),
                )?;
                let current_canon = serde_json::from_str::<nicti_pawprint::EditDocument>(&current)
                    .ok()
                    .and_then(|d| canonical_doc(&d).ok());
                let baseline = applied_doc_hash
                    .clone()
                    .unwrap_or_else(|| empty_hash.clone());
                match current_canon {
                    Some(canon) if blake3_hex(canon.as_bytes()) == baseline => {
                        if &canon != new_json {
                            tx.execute(
                                "UPDATE edit_variant SET document = ?1 WHERE id = ?2",
                                params![new_json, variant_id],
                            )?;
                            out.docs_written += 1;
                        }
                        applied_doc_hash = Some(new_hash.clone());
                    }
                    _ => out.kept_local_docs += 1,
                }
            }

            // Markers: master only. First import overrides what XMP ingest filled in; a re-run
            // only while the catalog still equals what the import last wrote.
            let mut applied_meta_hash = existing.as_ref().and_then(|e| e.2.clone());
            if let Some(meta) = &item.meta {
                let current: AssetMeta = tx.query_row(
                    "SELECT rating, flag, label FROM asset WHERE id = ?1",
                    params![item.asset_id],
                    |row| {
                        Ok(AssetMeta {
                            rating: row.get(0)?,
                            flag: row.get(1)?,
                            label: row.get(2)?,
                        })
                    },
                )?;
                let allowed = match &applied_meta_hash {
                    Some(h) => *h == meta_hash(&current),
                    None => true,
                };
                if allowed {
                    // LRC's *absence* of a rating/flag/label never erases one nicti already has
                    // (rated in nicti, or newer XMP) -- only a value LRC actually carries
                    // overrides, on the first run and on every re-run.
                    let target = AssetMeta {
                        rating: meta.rating.or(current.rating),
                        flag: meta.flag.or(current.flag),
                        label: meta.label.clone().or_else(|| current.label.clone()),
                    };
                    if current != target {
                        tx.execute(
                            "UPDATE asset SET rating = ?1, flag = ?2, label = ?3 WHERE id = ?4",
                            params![target.rating, target.flag, target.label, item.asset_id],
                        )?;
                        out.meta_applied += 1;
                        out.meta_changed_assets.push(item.asset_id);
                    }
                    applied_meta_hash = Some(meta_hash(&target));
                } else {
                    out.kept_local_meta += 1;
                }
            }

            let p = &item.provenance;
            let untranslated = serde_json::to_string(&p.untranslated)
                .map_err(|e| CatalogError::Document(e.to_string()))?;
            tx.execute(
                "INSERT INTO lrc_provenance (variant_id, asset_id, lrc_image_global, \
                 lrc_image_local, import_hash, process_version, develop_text, has_masks, \
                 has_ai_masks, has_big_data, iptc_caption, iptc_copyright, lrc_rating, lrc_pick, \
                 untranslated, applied_doc_hash, applied_meta_hash, imported_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, \
                 ?17, ?18) \
                 ON CONFLICT(variant_id) DO UPDATE SET \
                 lrc_image_local = excluded.lrc_image_local, import_hash = excluded.import_hash, \
                 process_version = excluded.process_version, develop_text = excluded.develop_text, \
                 has_masks = excluded.has_masks, has_ai_masks = excluded.has_ai_masks, \
                 has_big_data = excluded.has_big_data, iptc_caption = excluded.iptc_caption, \
                 iptc_copyright = excluded.iptc_copyright, lrc_rating = excluded.lrc_rating, \
                 lrc_pick = excluded.lrc_pick, untranslated = excluded.untranslated, \
                 applied_doc_hash = excluded.applied_doc_hash, \
                 applied_meta_hash = excluded.applied_meta_hash, imported_at = excluded.imported_at",
                params![
                    variant_id,
                    item.asset_id,
                    p.image_global,
                    p.image_local,
                    p.import_hash,
                    p.process_version,
                    p.develop_text,
                    p.has_masks,
                    p.has_ai_masks,
                    p.has_big_data,
                    p.iptc_caption,
                    p.iptc_copyright,
                    p.lrc_rating,
                    p.lrc_pick,
                    untranslated,
                    applied_doc_hash,
                    applied_meta_hash,
                    now,
                ],
            )?;
        }
        tx.commit()?;
        Ok(out)
    }

    fn lrc_provenance(&self, image_global: &str) -> Result<Option<LrcProvenance>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT lrc_image_local, import_hash, process_version, develop_text, has_masks, \
                 has_ai_masks, has_big_data, iptc_caption, iptc_copyright, lrc_rating, lrc_pick, \
                 untranslated FROM lrc_provenance WHERE lrc_image_global = ?1",
                params![image_global],
                |row| {
                    Ok((
                        LrcProvenance {
                            image_global: image_global.to_string(),
                            image_local: row.get(0)?,
                            import_hash: row.get(1)?,
                            process_version: row.get(2)?,
                            develop_text: row.get(3)?,
                            has_masks: row.get(4)?,
                            has_ai_masks: row.get(5)?,
                            has_big_data: row.get(6)?,
                            iptc_caption: row.get(7)?,
                            iptc_copyright: row.get(8)?,
                            lrc_rating: row.get(9)?,
                            lrc_pick: row.get(10)?,
                            untranslated: Vec::new(),
                        },
                        row.get::<_, String>(11)?,
                    ))
                },
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((mut p, untranslated)) => {
                p.untranslated = serde_json::from_str(&untranslated)
                    .map_err(|e| CatalogError::Document(e.to_string()))?;
                Ok(Some(p))
            }
        }
    }

    fn variant_names(&self, asset_id: i64) -> Result<Vec<String>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT name FROM edit_variant WHERE asset_id = ?1 ORDER BY is_master DESC, id",
        )?;
        let names = stmt
            .query_map(params![asset_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(names)
    }

    fn get_variant_edit(
        &self,
        asset_id: i64,
        name: &str,
    ) -> Result<Option<nicti_pawprint::EditDocument>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let json: Option<String> = conn
            .query_row(
                "SELECT document FROM edit_variant WHERE asset_id = ?1 AND name = ?2",
                params![asset_id, name],
                |row| row.get(0),
            )
            .optional()?;
        json.map(|text| {
            serde_json::from_str(&text).map_err(|e| CatalogError::Document(e.to_string()))
        })
        .transpose()
    }

    fn get_root_path(&self, root_id: i64) -> Result<Option<String>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT rel_path FROM root WHERE id = ?1",
                params![root_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    fn find_by_fingerprint(&self, fingerprint: &str) -> Result<Vec<Asset>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ASSET_COLUMNS} FROM asset WHERE fingerprint = ?1 ORDER BY id ASC"
        ))?;
        let rows = stmt
            .query_map([fingerprint], Self::row_to_asset)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn insert_asset(
        &self,
        root_id: i64,
        asset: &NewAsset,
        t0_preview: Option<&Preview>,
    ) -> Result<i64, CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // `rating`/`flag`/`label` are never written here -- omitted from the column list means
        // they default to NULL on a fresh insert, and the ON CONFLICT branch below likewise never
        // touches them, so a rescan of an already-imported asset can't clobber ratings/flags/
        // labels the user has since set.
        tx.execute(
            "INSERT INTO asset (root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                fingerprint, natural_key, make, model, captured_at, width, height, \
                imported_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) \
             ON CONFLICT(root_id, rel_path) DO UPDATE SET \
                rel_path_fold = excluded.rel_path_fold, \
                size_bytes = excluded.size_bytes, \
                mtime_unix = excluded.mtime_unix, \
                fingerprint = excluded.fingerprint, \
                natural_key = excluded.natural_key, \
                make = excluded.make, \
                model = excluded.model, \
                captured_at = excluded.captured_at, \
                width = excluded.width, \
                height = excluded.height",
            params![
                root_id,
                asset.rel_path,
                asset.rel_path_fold,
                asset.size_bytes as i64,
                asset.mtime_unix,
                asset.fingerprint,
                asset.natural_key,
                asset.make,
                asset.model,
                asset.captured_at,
                asset.width.map(|w| w as i64),
                asset.height.map(|h| h as i64),
                asset.imported_at,
            ],
        )?;
        // `last_insert_rowid()` isn't reliable across an ON CONFLICT DO UPDATE branch (only a real
        // INSERT sets it) -- look the row up by its actual unique key instead, same pattern
        // `spikes/homing`'s upsert helpers use.
        let asset_id: i64 = tx.query_row(
            "SELECT id FROM asset WHERE root_id = ?1 AND rel_path = ?2",
            params![root_id, asset.rel_path],
            |row| row.get(0),
        )?;
        // Only ever created once per asset -- `ON CONFLICT DO NOTHING` makes a rescan of an
        // already-imported asset a no-op here, leaving any real edits the master variant has
        // since accumulated untouched.
        tx.execute(
            "INSERT INTO edit_variant (asset_id, name, is_master, document) \
             VALUES (?1, 'master', 1, ?2) \
             ON CONFLICT(asset_id, name) DO NOTHING",
            params![asset_id, empty_edit_document()],
        )?;
        match t0_preview {
            Some(preview) => tx.execute(
                "INSERT INTO preview (asset_id, tier, width, height, bytes) \
                 VALUES (?1,?2,?3,?4,?5) \
                 ON CONFLICT(asset_id, tier) DO UPDATE SET \
                    width = excluded.width, height = excluded.height, bytes = excluded.bytes",
                params![
                    asset_id,
                    PreviewTier::T0.as_str(),
                    preview.width.map(|w| w as i64),
                    preview.height.map(|h| h as i64),
                    preview.bytes,
                ],
            )?,
            None => tx.execute(
                "DELETE FROM preview WHERE asset_id = ?1 AND tier = ?2",
                params![asset_id, PreviewTier::T0.as_str()],
            )?,
        };
        tx.commit()?;
        Ok(asset_id)
    }

    fn relink_asset(
        &self,
        asset_id: i64,
        new_root_id: i64,
        new_rel_path: &str,
        new_rel_path_fold: &str,
        size_bytes: u64,
        mtime_unix: i64,
    ) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE asset SET root_id = ?1, rel_path = ?2, rel_path_fold = ?3, \
                size_bytes = ?4, mtime_unix = ?5, missing_since = NULL WHERE id = ?6",
            params![
                new_root_id,
                new_rel_path,
                new_rel_path_fold,
                size_bytes as i64,
                mtime_unix,
                asset_id
            ],
        )?;
        Ok(())
    }

    fn put_preview(
        &self,
        asset_id: i64,
        tier: PreviewTier,
        preview: &Preview,
    ) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO preview (asset_id, tier, width, height, bytes) VALUES (?1,?2,?3,?4,?5) \
             ON CONFLICT(asset_id, tier) DO UPDATE SET \
                width = excluded.width, height = excluded.height, bytes = excluded.bytes",
            params![
                asset_id,
                tier.as_str(),
                preview.width.map(|w| w as i64),
                preview.height.map(|h| h as i64),
                preview.bytes,
            ],
        )?;
        Ok(())
    }

    fn get_preview(
        &self,
        asset_id: i64,
        tier: PreviewTier,
    ) -> Result<Option<Preview>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT width, height, bytes FROM preview WHERE asset_id = ?1 AND tier = ?2",
                params![asset_id, tier.as_str()],
                |row| {
                    Ok(Preview {
                        width: row.get::<_, Option<i64>>(0)?.map(|w| w as u32),
                        height: row.get::<_, Option<i64>>(1)?.map(|h| h as u32),
                        bytes: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    fn get_previews(
        &self,
        asset_ids: &[i64],
        tier: PreviewTier,
    ) -> Result<Vec<(i64, Preview)>, CatalogError> {
        // SQLite's default bound-parameter cap is 32766 (999 on very old builds); chunk well
        // under either so an arbitrarily long caller slice can't trip it.
        const CHUNK: usize = 500;
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::with_capacity(asset_ids.len());
        for chunk in asset_ids.chunks(CHUNK) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT asset_id, width, height, bytes FROM preview \
                 WHERE tier = ? AND asset_id IN ({placeholders})"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let mut params: Vec<&dyn ToSql> = Vec::with_capacity(chunk.len() + 1);
            let tier_str = tier.as_str();
            params.push(&tier_str);
            params.extend(chunk.iter().map(|id| id as &dyn ToSql));
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    Preview {
                        width: row.get::<_, Option<i64>>(1)?.map(|w| w as u32),
                        height: row.get::<_, Option<i64>>(2)?.map(|h| h as u32),
                        bytes: row.get(3)?,
                    },
                ))
            })?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    fn clear_preview(&self, asset_id: i64, tier: PreviewTier) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM preview WHERE asset_id = ?1 AND tier = ?2",
            params![asset_id, tier.as_str()],
        )?;
        Ok(())
    }

    fn facet_count(&self, model: Option<&str>, rating: Option<i64>) -> Result<u64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let cnt: i64 = conn.query_row(
            "SELECT COALESCE(SUM(fc.cnt), 0) FROM facet_counts fc \
             JOIN volume v ON v.id = fc.volume_id \
             WHERE v.online = 1 AND fc.model = ?1 AND fc.rating = ?2",
            params![
                model.unwrap_or(""),
                rating.unwrap_or(FACET_UNRATED_SENTINEL)
            ],
            |row| row.get(0),
        )?;
        Ok(cnt as u64)
    }

    fn set_rating(&self, asset_ids: &[i64], rating: Option<i64>) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        set_column_for_assets(&mut conn, "rating", &rating, asset_ids)
    }

    fn set_flag(&self, asset_ids: &[i64], flag: Option<i64>) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        set_column_for_assets(&mut conn, "flag", &flag, asset_ids)
    }

    fn set_label(&self, asset_ids: &[i64], label: Option<&str>) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        set_column_for_assets(&mut conn, "label", &label, asset_ids)
    }

    fn list_assets_by_root(&self, root_id: i64) -> Result<Vec<Asset>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ASSET_COLUMNS} FROM asset WHERE root_id = ?1 ORDER BY id ASC"
        ))?;
        let rows = stmt
            .query_map([root_id], Self::row_to_asset)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn set_asset_missing(
        &self,
        asset_id: i64,
        missing_since: Option<i64>,
    ) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE asset SET missing_since = ?1 WHERE id = ?2",
            params![missing_since, asset_id],
        )?;
        Ok(())
    }

    fn remove_asset(&self, asset_id: i64) -> Result<(), CatalogError> {
        self.remove_assets(&[asset_id])
    }

    fn remove_assets(&self, asset_ids: &[i64]) -> Result<(), CatalogError> {
        if asset_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // No `ON DELETE CASCADE` on most of these foreign keys (schema.rs's `asset`/`edit_variant`
        // references are plain `REFERENCES`, and `PRAGMA foreign_keys = ON` only *enforces*
        // referential integrity -- it never cascades a delete on its own) -- every child row is
        // deleted explicitly, in dependency order, or the asset delete itself would fail its own
        // foreign-key check with orphaned children left behind. (`asset_keyword` and
        // `collection_asset` do cascade.) The `delete_item` journal row goes in the same
        // transaction, so a finished delete leaves no journal behind.
        for chunk in asset_ids.chunks(MAX_IDS_PER_STATEMENT) {
            let ph = vec!["?"; chunk.len()].join(",");
            let bound: Vec<&dyn ToSql> = chunk.iter().map(|id| id as &dyn ToSql).collect();
            tx.execute(
                &format!(
                    "DELETE FROM edit_history WHERE variant_id IN \
                        (SELECT id FROM edit_variant WHERE asset_id IN ({ph}))"
                ),
                bound.as_slice(),
            )?;
            for table in ["edit_variant", "preview", "delete_item", "asset_sidecar"] {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE asset_id IN ({ph})"),
                    bound.as_slice(),
                )?;
            }
            tx.execute(
                &format!("DELETE FROM asset WHERE id IN ({ph})"),
                bound.as_slice(),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn get_meta(&self, asset_ids: &[i64]) -> Result<HashMap<i64, AssetMeta>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut out = HashMap::with_capacity(asset_ids.len());
        for chunk in asset_ids.chunks(MAX_IDS_PER_STATEMENT) {
            let ph = vec!["?"; chunk.len()].join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, rating, flag, label FROM asset WHERE id IN ({ph})"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    AssetMeta {
                        rating: row.get(1)?,
                        flag: row.get(2)?,
                        label: row.get(3)?,
                    },
                ))
            })?;
            for row in rows {
                let (id, meta) = row?;
                out.insert(id, meta);
            }
        }
        Ok(out)
    }

    fn set_meta(&self, items: &[(i64, AssetMeta)]) -> Result<(), CatalogError> {
        if items.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE asset SET rating = ?1, flag = ?2, label = ?3 WHERE id = ?4",
            )?;
            for (id, meta) in items {
                stmt.execute(params![meta.rating, meta.flag, meta.label, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn begin_delete_items(&self, items: &[(i64, String)]) -> Result<(), CatalogError> {
        if items.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                // A still-`pending` row from an earlier run is refreshed to the current path (a
                // photo moved since would otherwise be judged by its old location); a `trashed`
                // row is never touched -- its files are already in the bin.
                "INSERT INTO delete_item (asset_id, abs_path, state) VALUES (?1, ?2, 'pending') \
                 ON CONFLICT(asset_id) DO UPDATE SET abs_path = excluded.abs_path \
                 WHERE delete_item.state = 'pending'",
            )?;
            for (id, path) in items {
                stmt.execute(params![id, path])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn mark_delete_items_trashed(&self, asset_ids: &[i64]) -> Result<(), CatalogError> {
        self.update_delete_items(asset_ids, "UPDATE delete_item SET state = 'trashed'")
    }

    fn abandon_delete_items(&self, asset_ids: &[i64]) -> Result<(), CatalogError> {
        self.update_delete_items(asset_ids, "DELETE FROM delete_item")
    }

    fn open_delete_items(&self) -> Result<Vec<DeleteItem>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT asset_id, abs_path, state FROM delete_item ORDER BY asset_id")?;
        let rows = stmt.query_map([], |row| {
            let state: String = row.get(2)?;
            Ok(DeleteItem {
                asset_id: row.get(0)?,
                abs_path: row.get(1)?,
                state: if state == "trashed" {
                    DeleteState::Trashed
                } else {
                    DeleteState::Pending
                },
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn sidecar_state(&self, asset_id: i64) -> Result<Option<SidecarState>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT path, last_seen_hash, last_seen_mtime_ms, last_written_hash, catalog_dirty_since_ms, needs_review \
                 FROM asset_sidecar WHERE asset_id = ?1",
                [asset_id],
                |row| {
                    Ok(SidecarState {
                        path: row.get(0)?,
                        last_seen_hash: row.get(1)?,
                        last_seen_mtime_ms: row.get(2)?,
                        last_written_hash: row.get(3)?,
                        catalog_dirty_since_ms: row.get(4)?,
                        needs_review: row.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()?)
    }

    fn record_sidecar(&self, asset_id: i64, state: &SidecarState) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO asset_sidecar \
                 (asset_id, path, last_seen_hash, last_seen_mtime_ms, last_written_hash, \
                  catalog_dirty_since_ms, needs_review) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(asset_id) DO UPDATE SET \
                 path = excluded.path, \
                 last_seen_hash = excluded.last_seen_hash, \
                 last_seen_mtime_ms = excluded.last_seen_mtime_ms, \
                 last_written_hash = excluded.last_written_hash, \
                 catalog_dirty_since_ms = excluded.catalog_dirty_since_ms, \
                 needs_review = excluded.needs_review",
            params![
                asset_id,
                state.path,
                state.last_seen_hash,
                state.last_seen_mtime_ms,
                state.last_written_hash,
                state.catalog_dirty_since_ms,
                state.needs_review as i64
            ],
        )?;
        Ok(())
    }

    fn sidecar_review_assets(&self) -> Result<Vec<i64>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT asset_id FROM asset_sidecar WHERE needs_review = 1 ORDER BY asset_id",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn create_keyword(&self, parent_id: Option<i64>, name: &str) -> Result<i64, CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let name_fold = fold_keyword_name(name);
        let parent_path: Option<String> = match parent_id {
            Some(pid) => Some(tx.query_row(
                "SELECT path FROM keyword WHERE id = ?1",
                [pid],
                |row| row.get(0),
            )?),
            None => None,
        };
        tx.execute(
            "INSERT INTO keyword (parent_id, name, name_fold, path) VALUES (?1, ?2, ?3, '')",
            params![parent_id, name, name_fold],
        )?;
        let keyword_id = tx.last_insert_rowid();
        let path = format!(
            "{}{}/",
            parent_path.unwrap_or_else(|| "/".to_string()),
            keyword_id
        );
        tx.execute(
            "UPDATE keyword SET path = ?1 WHERE id = ?2",
            params![path, keyword_id],
        )?;
        tx.commit()?;
        Ok(keyword_id)
    }

    fn rename_keyword(&self, keyword_id: i64, new_name: &str) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        let name_fold = fold_keyword_name(new_name);
        conn.execute(
            "UPDATE keyword SET name = ?1, name_fold = ?2 WHERE id = ?3",
            params![new_name, name_fold, keyword_id],
        )?;
        Ok(())
    }

    fn move_keyword(
        &self,
        keyword_id: i64,
        new_parent_id: Option<i64>,
    ) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let old_path: String = tx.query_row(
            "SELECT path FROM keyword WHERE id = ?1",
            [keyword_id],
            |row| row.get(0),
        )?;
        let new_parent_path: Option<String> = match new_parent_id {
            Some(pid) => Some(tx.query_row(
                "SELECT path FROM keyword WHERE id = ?1",
                [pid],
                |row| row.get(0),
            )?),
            None => None,
        };

        // Reject moving under itself or one of its own descendants -- either would corrupt the
        // materialized path (the node's own id would appear twice, or a descendant would end up
        // "above" its own ancestor). `old_path` is a literal id-based prefix of every descendant's
        // path (including the node's own, trivially) and of no other keyword's, so this one
        // string check covers both cases without an extra query.
        if let Some(new_parent_path) = &new_parent_path {
            if new_parent_path.starts_with(&old_path) {
                return Err(CatalogError::WouldCreateCycle);
            }
        }

        let new_path = format!(
            "{}{}/",
            new_parent_path.unwrap_or_else(|| "/".to_string()),
            keyword_id
        );

        tx.execute(
            "UPDATE keyword SET parent_id = ?1 WHERE id = ?2",
            params![new_parent_id, keyword_id],
        )?;

        // Rewrite this keyword's own path, plus every descendant's -- a descendant's path always
        // starts with the parent's old path as a literal prefix (id-based segments), so a plain
        // suffix-preserving replace on every matching row is correct without a recursive walk.
        let glob = format!("{old_path}*");
        let rows: Vec<(i64, String)> = {
            let mut stmt = tx.prepare("SELECT id, path FROM keyword WHERE path GLOB ?1")?;
            let rows = stmt
                .query_map([glob], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (id, path) in rows {
            let rewritten = format!("{new_path}{}", &path[old_path.len()..]);
            tx.execute(
                "UPDATE keyword SET path = ?1 WHERE id = ?2",
                params![rewritten, id],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    fn delete_keyword(&self, keyword_id: i64) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let path: String = tx.query_row(
            "SELECT path FROM keyword WHERE id = ?1",
            [keyword_id],
            |row| row.get(0),
        )?;
        let glob = format!("{path}*");
        tx.execute(
            "DELETE FROM asset_keyword WHERE keyword_id IN \
                (SELECT id FROM keyword WHERE path GLOB ?1)",
            [glob.clone()],
        )?;
        tx.execute("DELETE FROM keyword WHERE path GLOB ?1", [glob])?;
        tx.commit()?;
        Ok(())
    }

    fn tag(&self, asset_ids: &[i64], keyword_id: i64) -> Result<(), CatalogError> {
        if asset_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for asset_id in asset_ids {
            tx.execute(
                "INSERT INTO asset_keyword (keyword_id, asset_id) VALUES (?1, ?2) \
                 ON CONFLICT(keyword_id, asset_id) DO NOTHING",
                params![keyword_id, asset_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn untag(&self, asset_ids: &[i64], keyword_id: i64) -> Result<(), CatalogError> {
        if asset_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for asset_id in asset_ids {
            tx.execute(
                "DELETE FROM asset_keyword WHERE keyword_id = ?1 AND asset_id = ?2",
                params![keyword_id, asset_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn keywords_for(&self, asset_id: i64) -> Result<Vec<Keyword>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT k.id, k.parent_id, k.name, k.path FROM keyword k \
             JOIN asset_keyword ak ON ak.keyword_id = k.id \
             WHERE ak.asset_id = ?1 ORDER BY k.path ASC",
        )?;
        let rows = stmt
            .query_map([asset_id], row_to_keyword)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn list_keywords(&self) -> Result<Vec<Keyword>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, path FROM keyword ORDER BY name_fold ASC, id ASC",
        )?;
        let rows = stmt
            .query_map([], row_to_keyword)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn list_collections(&self) -> Result<Vec<Collection>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, kind, name FROM collection ORDER BY name_fold ASC, id ASC",
        )?;
        let rows = stmt
            .query_map([], row_to_collection)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn distinct_makes(&self) -> Result<Vec<String>, CatalogError> {
        self.distinct_asset_column("make")
    }

    fn distinct_labels(&self) -> Result<Vec<String>, CatalogError> {
        self.distinct_asset_column("label")
    }

    fn keyword_by_path(&self, segments: &[&str]) -> Result<Option<Keyword>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut parent_id: Option<i64> = None;
        let mut found: Option<Keyword> = None;
        for segment in segments {
            let name_fold = fold_keyword_name(segment);
            let row: Option<Keyword> = conn
                .query_row(
                    "SELECT id, parent_id, name, path FROM keyword \
                     WHERE name_fold = ?1 AND parent_id IS ?2",
                    params![name_fold, parent_id],
                    row_to_keyword,
                )
                .optional()?;
            match row {
                Some(k) => {
                    parent_id = Some(k.id);
                    found = Some(k);
                }
                None => return Ok(None),
            }
        }
        Ok(found)
    }

    fn hunt(&self, filter: &Filter, sort: Sort, page: &Page) -> Result<Vec<i64>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let filter_sql = build_filter_sql(&conn, filter)?;
        let mut where_parts = vec![filter_sql.where_clause];
        let mut all_params: Vec<Box<dyn ToSql>> = filter_sql.params;

        let sort_column = sort_column_sql(sort.field);
        let (cmp, order) = match sort.direction {
            SortDirection::Asc => (">", "ASC"),
            SortDirection::Desc => ("<", "DESC"),
        };

        if let Some(cursor) = &page.after {
            let (cursor_key, cursor_id): (Box<dyn ToSql>, i64) = match (cursor, sort.field) {
                (Cursor::Captured { captured_at, id }, SortField::Captured) => {
                    (Box::new(captured_at.clone().unwrap_or_default()), *id)
                }
                (Cursor::Imported { imported_at, id }, SortField::Imported) => {
                    (Box::new(*imported_at), *id)
                }
                (Cursor::Filename { rel_path_fold, id }, SortField::Filename) => {
                    (Box::new(rel_path_fold.clone()), *id)
                }
                (Cursor::Rating { rating, id }, SortField::Rating) => {
                    (Box::new(rating.unwrap_or(RATING_SORT_SENTINEL)), *id)
                }
                _ => return Err(CatalogError::CursorSortMismatch),
            };
            where_parts.push(format!("({sort_column}, a.id) {cmp} (?, ?)"));
            all_params.push(cursor_key);
            all_params.push(Box::new(cursor_id));
        }

        let where_clause = where_parts.join(" AND ");
        let limit = page.limit.max(1);
        let sql = format!(
            "SELECT a.id FROM asset a \
             JOIN root r ON r.id = a.root_id \
             JOIN volume v ON v.id = r.volume_id \
             WHERE {where_clause} \
             ORDER BY {sort_column} {order}, a.id {order} \
             LIMIT ?"
        );
        all_params.push(Box::new(limit));

        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn ToSql> = all_params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn hunt_ids(&self, filter: &Filter, sort: Sort) -> Result<Vec<i64>, CatalogError> {
        // A file-backed catalog scans on its own read-only connection (WAL: a consistent snapshot,
        // never blocking a writer): this runs for ~0.2-1 s at 1M rows, and holding the shared
        // mutex that long would freeze every UI-thread catalog call (`list_roots`, `get_asset`)
        // for the duration. An in-memory catalog (tests) has no second connection to open.
        let reader = if self.path.is_some() {
            Some(self.open_snapshot_reader()?)
        } else {
            None
        };
        let guard;
        let conn: &Connection = match &reader {
            Some(reader) => reader,
            None => {
                guard = self.conn.lock().unwrap();
                &guard
            }
        };
        let filter_sql = build_filter_sql(conn, filter)?;
        let sort_column = sort_column_sql(sort.field);
        let order = match sort.direction {
            SortDirection::Asc => "ASC",
            SortDirection::Desc => "DESC",
        };
        let sql = format!(
            "SELECT a.id FROM asset a \
             JOIN root r ON r.id = a.root_id \
             JOIN volume v ON v.id = r.volume_id \
             WHERE {} \
             ORDER BY {sort_column} {order}, a.id {order}",
            filter_sql.where_clause
        );
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn ToSql> = filter_sql.params.iter().map(|p| p.as_ref()).collect();
        let ids = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    fn hunt_count(&self, filter: &Filter) -> Result<u64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let filter_sql = build_filter_sql(&conn, filter)?;
        let sql = format!(
            "SELECT COUNT(*) FROM asset a \
             JOIN root r ON r.id = a.root_id \
             JOIN volume v ON v.id = r.volume_id \
             WHERE {}",
            filter_sql.where_clause
        );
        let param_refs: Vec<&dyn ToSql> = filter_sql.params.iter().map(|p| p.as_ref()).collect();
        let count: i64 = conn.query_row(&sql, param_refs.as_slice(), |row| row.get(0))?;
        Ok(count as u64)
    }

    fn facets(&self, filter: &Filter) -> Result<FacetCounts, CatalogError> {
        self.with_scan_conn(|conn| Self::facets_on(conn, filter))
    }

    fn collection(&self, collection_id: i64) -> Result<Option<Collection>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT id, parent_id, kind, name FROM collection WHERE id = ?1",
                [collection_id],
                row_to_collection,
            )
            .optional()?)
    }

    fn create_collection(
        &self,
        parent_id: Option<i64>,
        name: &str,
        kind: CollectionKind,
    ) -> Result<i64, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let name_fold = fold_keyword_name(name);
        conn.execute(
            "INSERT INTO collection (parent_id, kind, name, name_fold) VALUES (?1, ?2, ?3, ?4)",
            params![parent_id, kind.as_str(), name, name_fold],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn rename_collection(&self, collection_id: i64, new_name: &str) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        let name_fold = fold_keyword_name(new_name);
        conn.execute(
            "UPDATE collection SET name = ?1, name_fold = ?2 WHERE id = ?3",
            params![new_name, name_fold, collection_id],
        )?;
        Ok(())
    }

    fn move_collection(
        &self,
        collection_id: i64,
        new_parent_id: Option<i64>,
    ) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();

        // Unlike `keyword`, a collection has no materialized path to check with a single string
        // comparison -- walk the `parent_id` chain from the proposed new parent upward instead,
        // rejecting the move if it ever reaches `collection_id` itself (moving under itself or
        // one of its own descendants, either of which would create a cycle: `delete_collection`'s
        // subtree walk assumes a tree, not a graph, and would never terminate against one).
        if let Some(new_parent_id) = new_parent_id {
            let mut current = Some(new_parent_id);
            let mut seen = std::collections::HashSet::new();
            while let Some(id) = current {
                if id == collection_id {
                    return Err(CatalogError::WouldCreateCycle);
                }
                if !seen.insert(id) {
                    // Already-corrupted data unrelated to this move (shouldn't happen once this
                    // check is in place, but don't loop forever walking a pre-existing cycle).
                    break;
                }
                current = conn
                    .query_row(
                        "SELECT parent_id FROM collection WHERE id = ?1",
                        [id],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
            }
        }

        conn.execute(
            "UPDATE collection SET parent_id = ?1 WHERE id = ?2",
            params![new_parent_id, collection_id],
        )?;
        Ok(())
    }

    fn delete_collection(&self, collection_id: i64) -> Result<(), CatalogError> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // Collections form a small tree too (organizational nesting), same subtree-delete shape
        // as `delete_keyword` -- walked in Rust rather than a recursive CTE, since the tree is
        // expected to be shallow and this keeps the same style as the rest of this file.
        // `seen` guards against a cycle in already-on-disk data (should be impossible going
        // forward now that `move_collection` rejects one, but this walk must never hang against
        // data that predates that check, or that got there some other way).
        let mut to_delete = vec![collection_id];
        let mut seen: std::collections::HashSet<i64> = std::iter::once(collection_id).collect();
        let mut i = 0;
        while i < to_delete.len() {
            let parent = to_delete[i];
            let mut stmt = tx.prepare("SELECT id FROM collection WHERE parent_id = ?1")?;
            let children: Vec<i64> = stmt
                .query_map([parent], |row| row.get(0))?
                .collect::<Result<Vec<_>, _>>()?;
            for child in children {
                if seen.insert(child) {
                    to_delete.push(child);
                }
            }
            i += 1;
        }
        for id in &to_delete {
            tx.execute(
                "DELETE FROM collection_asset WHERE collection_id = ?1",
                [id],
            )?;
        }
        // Deepest descendants first: `collection.parent_id REFERENCES collection(id)`, so
        // deleting an ancestor while a child row still points at it violates that FK. BFS order
        // (`to_delete`'s own construction) always lists a parent before its children, so the
        // reverse is exactly children-before-parents for a tree.
        for id in to_delete.iter().rev() {
            tx.execute("DELETE FROM collection WHERE id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(())
    }

    fn add_to_collection(&self, collection_id: i64, asset_ids: &[i64]) -> Result<(), CatalogError> {
        if asset_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut next_position: f64 = tx.query_row(
            "SELECT COALESCE(MAX(position), 0) FROM collection_asset WHERE collection_id = ?1",
            [collection_id],
            |row| row.get(0),
        )?;
        for asset_id in asset_ids {
            next_position += 1.0;
            tx.execute(
                "INSERT INTO collection_asset (collection_id, asset_id, position) \
                 VALUES (?1, ?2, ?3) ON CONFLICT(collection_id, asset_id) DO NOTHING",
                params![collection_id, asset_id, next_position],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn remove_from_collection(
        &self,
        collection_id: i64,
        asset_ids: &[i64],
    ) -> Result<(), CatalogError> {
        if asset_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for asset_id in asset_ids {
            tx.execute(
                "DELETE FROM collection_asset WHERE collection_id = ?1 AND asset_id = ?2",
                params![collection_id, asset_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn collection_assets(&self, collection_id: i64) -> Result<Vec<i64>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT asset_id FROM collection_asset WHERE collection_id = ?1 ORDER BY position ASC",
        )?;
        let rows = stmt
            .query_map([collection_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    fn set_smart_rule(&self, collection_id: i64, filter: &Filter) -> Result<(), CatalogError> {
        let conn = self.conn.lock().unwrap();
        let rule = SmartRule {
            v: 1,
            filter: filter.clone(),
        };
        let rule_json = serde_json::to_string(&rule)
            .map_err(|e| CatalogError::Io(format!("serializing smart rule: {e}")))?;
        conn.execute(
            "UPDATE collection SET rule_json = ?1 WHERE id = ?2",
            params![rule_json, collection_id],
        )?;
        Ok(())
    }

    fn collection_filter(&self, collection_id: i64) -> Result<Option<Filter>, CatalogError> {
        let conn = self.conn.lock().unwrap();
        let rule_json: Option<String> = conn
            .query_row(
                "SELECT rule_json FROM collection WHERE id = ?1",
                [collection_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        match rule_json {
            Some(json) => {
                let rule: SmartRule = serde_json::from_str(&json)
                    .map_err(|e| CatalogError::Io(format!("deserializing smart rule: {e}")))?;
                Ok(Some(rule.filter))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_edit_document_matches_the_on_disk_shape_adr_0021_documents() {
        assert_eq!(empty_edit_document(), r#"{"stages":{}}"#);
    }

    #[test]
    fn asset_count_is_zero_on_a_fresh_catalog() {
        let catalog = SqliteCatalog::open_in_memory().unwrap();
        assert_eq!(catalog.asset_count().unwrap(), 0);
    }

    #[test]
    fn asset_count_reflects_inserted_assets() {
        let catalog = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = catalog.upsert_volume("test-volume", None, None, 0).unwrap();
        let root_id = catalog.ensure_root(volume_id, "root").unwrap();
        for i in 0..3 {
            catalog
                .insert_asset(
                    root_id,
                    &NewAsset {
                        rel_path: format!("photo{i}.nef"),
                        rel_path_fold: format!("photo{i}.nef"),
                        size_bytes: 100,
                        mtime_unix: 0,
                        fingerprint: Some(format!("fp{i}")),
                        natural_key: None,
                        make: None,
                        model: None,
                        captured_at: None,
                        width: None,
                        height: None,
                        imported_at: 0,
                    },
                    None,
                )
                .unwrap();
        }
        assert_eq!(catalog.asset_count().unwrap(), 3);
    }

    fn new_asset(rel_path: &str, model: Option<&str>) -> NewAsset {
        NewAsset {
            rel_path: rel_path.to_string(),
            rel_path_fold: rel_path.to_lowercase(),
            size_bytes: 100,
            mtime_unix: 0,
            fingerprint: None,
            natural_key: None,
            make: None,
            model: model.map(str::to_string),
            captured_at: None,
            width: None,
            height: None,
            imported_at: 0,
        }
    }

    #[test]
    fn get_asset_returns_the_row_by_id() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let id = store
            .insert_asset(root_id, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();

        let asset = store.get_asset(id).unwrap().unwrap();
        assert_eq!(asset.id, id);
        assert_eq!(asset.rel_path, "a.NEF");
        assert_eq!(asset.model, Some("Z8".to_string()));
    }

    fn edit_doc(exposure: f64) -> nicti_pawprint::EditDocument {
        let mut doc = nicti_pawprint::EditDocument::default();
        doc.stages.insert(
            "nicti.exposure".to_string(),
            nicti_pawprint::StageEntry {
                schema_version: 1,
                params: serde_json::json!({ "ev": exposure }),
            },
        );
        doc
    }

    #[test]
    fn master_edit_round_trips_and_a_fresh_asset_reads_empty() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let id = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();

        assert_eq!(
            store.get_master_edit(id).unwrap(),
            Some(nicti_pawprint::EditDocument::default())
        );
        store.put_master_edit(id, &edit_doc(0.75)).unwrap();
        assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(0.75)));
        // A second put replaces, never duplicates, the master row.
        store.put_master_edit(id, &edit_doc(-1.0)).unwrap();
        assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(-1.0)));
    }

    #[test]
    fn master_edit_is_none_for_an_unknown_asset_and_put_errors() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        assert_eq!(store.get_master_edit(4242).unwrap(), None);
        assert!(store.put_master_edit(4242, &edit_doc(0.0)).is_err());
    }

    #[test]
    fn put_master_edit_refuses_a_non_finite_float_without_touching_the_row() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let id = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();
        store.put_master_edit(id, &edit_doc(0.5)).unwrap();

        let mut bad = edit_doc(0.0);
        bad.stages.get_mut("nicti.exposure").unwrap().params = serde_json::Value::Null; // what a NaN becomes after serde_json::to_value
        assert!(matches!(
            store.put_master_edit(id, &bad),
            Err(CatalogError::Document(_))
        ));
        assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(0.5)));
    }

    fn seeded_assets(store: &SqliteCatalog, n: usize) -> Vec<i64> {
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        (0..n)
            .map(|i| {
                store
                    .insert_asset(root_id, &new_asset(&format!("{i}.NEF"), None), None)
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn put_master_edits_writes_a_large_batch_and_get_reads_it_back() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let ids = seeded_assets(&store, 1200);
        let edits: Vec<_> = ids
            .iter()
            .map(|&id| (id, edit_doc(id as f64 / 10.0)))
            .collect();
        store.put_master_edits(&edits).unwrap();

        let back = store.get_master_edits(&ids).unwrap();
        assert_eq!(back.len(), ids.len());
        for ((id, doc), (want_id, want)) in back.iter().zip(&edits) {
            assert_eq!(id, want_id);
            assert_eq!(doc.as_ref(), Some(want));
        }
    }

    #[test]
    fn get_master_edits_keeps_request_order_and_reports_a_missing_asset_as_none() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let ids = seeded_assets(&store, 2);
        store.put_master_edit(ids[1], &edit_doc(1.5)).unwrap();

        let back = store.get_master_edits(&[ids[1], 999_999, ids[0]]).unwrap();
        assert_eq!(back[0], (ids[1], Some(edit_doc(1.5))));
        assert_eq!(back[1], (999_999, None));
        // A freshly ingested asset reads as an empty document, not `None`.
        assert_eq!(
            back[2],
            (ids[0], Some(nicti_pawprint::EditDocument::default()))
        );
    }

    #[test]
    fn put_master_edits_is_atomic_when_one_document_is_non_finite() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let ids = seeded_assets(&store, 3);
        for &id in &ids {
            store.put_master_edit(id, &edit_doc(0.5)).unwrap();
        }
        let mut bad = edit_doc(0.0);
        bad.stages.get_mut("nicti.exposure").unwrap().params = serde_json::Value::Null;

        let edits = vec![
            (ids[0], edit_doc(9.0)),
            (ids[1], bad),
            (ids[2], edit_doc(9.0)),
        ];
        assert!(matches!(
            store.put_master_edits(&edits),
            Err(CatalogError::Document(_))
        ));
        for &id in &ids {
            assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(0.5)));
        }
    }

    #[test]
    fn put_master_edits_rolls_back_when_a_later_asset_does_not_exist() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let ids = seeded_assets(&store, 1);
        store.put_master_edit(ids[0], &edit_doc(0.5)).unwrap();

        let edits = vec![(ids[0], edit_doc(9.0)), (999_999, edit_doc(9.0))];
        assert!(store.put_master_edits(&edits).is_err());
        assert_eq!(store.get_master_edit(ids[0]).unwrap(), Some(edit_doc(0.5)));
    }

    #[test]
    fn master_edit_survives_a_reingest_of_the_same_asset() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let id = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();
        store.put_master_edit(id, &edit_doc(2.0)).unwrap();

        let again = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();
        assert_eq!(again, id);
        assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(2.0)));
    }

    #[test]
    fn list_keywords_and_collections_are_name_sorted_and_flat() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let zoo = store.create_keyword(None, "Zoo").unwrap();
        let apple = store.create_keyword(None, "apple").unwrap();
        let cat = store.create_keyword(Some(zoo), "Cats").unwrap();
        let names: Vec<_> = store
            .list_keywords()
            .unwrap()
            .into_iter()
            .map(|k| (k.id, k.parent_id))
            .collect();
        assert_eq!(names, vec![(apple, None), (cat, Some(zoo)), (zoo, None)]);

        let smart = store
            .create_collection(None, "Picks", CollectionKind::Smart)
            .unwrap();
        let manual = store
            .create_collection(Some(smart), "Album", CollectionKind::Manual)
            .unwrap();
        let ids: Vec<_> = store
            .list_collections()
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(ids, vec![manual, smart]);
    }

    #[test]
    fn distinct_makes_and_labels_skip_empty_and_offline_volumes() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let v1 = store.upsert_volume("v1", None, None, 0).unwrap();
        let v2 = store.upsert_volume("v2", None, None, 0).unwrap();
        let r1 = store.ensure_root(v1, "").unwrap();
        let r2 = store.ensure_root(v2, "").unwrap();
        let mut a = new_asset("a.NEF", Some("Z8"));
        a.make = Some("NIKON".to_string());
        let mut b = new_asset("b.NEF", Some("Z6"));
        b.make = Some("NIKON".to_string());
        let mut c = new_asset("c.NEF", None);
        c.make = Some("Canon".to_string());
        let mut d = new_asset("d.NEF", None);
        d.make = Some("Sony".to_string()); // on the offline volume below
        let a_id = store.insert_asset(r1, &a, None).unwrap();
        let b_id = store.insert_asset(r1, &b, None).unwrap();
        store.insert_asset(r1, &c, None).unwrap();
        let d_id = store.insert_asset(r2, &d, None).unwrap();
        store.set_label(&[a_id, d_id], Some("Red")).unwrap();
        store.set_label(&[b_id], Some("")).unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE volume SET online = 0 WHERE id = ?1", params![v2])
            .unwrap();

        assert_eq!(store.distinct_makes().unwrap(), vec!["Canon", "NIKON"]);
        assert_eq!(store.distinct_labels().unwrap(), vec!["Red"]);
    }

    #[test]
    fn get_asset_returns_none_for_an_id_that_does_not_exist() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        assert_eq!(store.get_asset(999).unwrap(), None);
    }

    #[test]
    fn get_root_path_returns_the_root_s_rel_path() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "/photos/2026-event").unwrap();

        assert_eq!(
            store.get_root_path(root_id).unwrap(),
            Some("/photos/2026-event".to_string())
        );
    }

    #[test]
    fn get_root_path_returns_none_for_a_root_id_that_does_not_exist() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        assert_eq!(store.get_root_path(999).unwrap(), None);
    }

    #[test]
    fn root_move_journal_begin_commit_finish() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "/ssd/e").unwrap();
        let other = store.ensure_root(volume_id, "/archive/taken").unwrap();
        let asset = store
            .insert_asset(
                root_id,
                &NewAsset {
                    rel_path: "a.NEF".into(),
                    rel_path_fold: "a.nef".into(),
                    size_bytes: 1,
                    mtime_unix: 0,
                    fingerprint: None,
                    natural_key: None,
                    make: None,
                    model: None,
                    captured_at: None,
                    width: None,
                    height: None,
                    imported_at: 0,
                },
                None,
            )
            .unwrap();

        // A destination that is already a registered root is refused before any copying.
        assert!(store.begin_root_move(root_id, "/archive/taken", 1).is_err());
        assert!(store.begin_root_move(999, "/x", 1).is_err());

        let id = store.begin_root_move(root_id, "/archive/e", 1).unwrap();
        // One open move per root.
        assert!(store.begin_root_move(root_id, "/archive/e2", 1).is_err());
        assert_eq!(
            store.open_root_moves().unwrap()[0].state,
            MoveState::Copying
        );
        // Nothing is re-pointed until the commit.
        assert_eq!(store.get_root_path(root_id).unwrap().unwrap(), "/ssd/e");

        store
            .commit_root_move(id, &[(asset, "abc".into()), (999, "ignored".into())])
            .unwrap();
        assert_eq!(store.get_root_path(root_id).unwrap().unwrap(), "/archive/e");
        assert_eq!(store.content_hash(asset).unwrap().as_deref(), Some("abc"));
        assert_eq!(
            store.open_root_moves().unwrap()[0].state,
            MoveState::Committed
        );
        // Committing again is a no-op, not an error.
        store.commit_root_move(id, &[]).unwrap();

        store.finish_root_move(id).unwrap();
        assert!(store.open_root_moves().unwrap().is_empty());
        assert_eq!(store.list_roots().unwrap().len(), 2);
        let _ = other;
    }

    #[test]
    fn set_rating_flag_label_round_trip_across_multiple_assets() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let a = store
            .insert_asset(root_id, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        let b = store
            .insert_asset(root_id, &new_asset("b.NEF", Some("Z8")), None)
            .unwrap();

        store.set_rating(&[a, b], Some(4)).unwrap();
        store.set_flag(&[a], Some(1)).unwrap();
        store.set_label(&[b], Some("Selects")).unwrap();

        let asset_a = store.find_asset_by_path(root_id, "a.NEF").unwrap().unwrap();
        let asset_b = store.find_asset_by_path(root_id, "b.NEF").unwrap().unwrap();
        assert_eq!(asset_a.rating, Some(4));
        assert_eq!(asset_a.flag, Some(1));
        assert_eq!(asset_a.label, None);
        assert_eq!(asset_b.rating, Some(4));
        assert_eq!(asset_b.flag, None);
        assert_eq!(asset_b.label, Some("Selects".to_string()));
    }

    #[test]
    fn set_rating_on_an_empty_slice_is_a_no_op() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        // Must not panic/error building a `WHERE id IN ()` with zero placeholders.
        store.set_rating(&[], Some(3)).unwrap();
    }

    #[test]
    fn set_rating_across_more_ids_than_one_chunk_holds_updates_all_of_them() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();

        let count = MAX_IDS_PER_STATEMENT + 5; // forces at least two chunks
        let ids: Vec<i64> = (0..count)
            .map(|i| {
                store
                    .insert_asset(root_id, &new_asset(&format!("{i}.NEF"), Some("Z8")), None)
                    .unwrap()
            })
            .collect();

        store.set_rating(&ids, Some(3)).unwrap();

        assert_eq!(
            store.facet_count(Some("Z8"), Some(3)).unwrap(),
            count as u64,
            "every id across every chunk must be updated, not just the first chunk"
        );
    }

    #[test]
    fn facet_count_distinguishes_unrated_from_reject_and_star_ratings() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let a = store
            .insert_asset(root_id, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        let b = store
            .insert_asset(root_id, &new_asset("b.NEF", Some("Z8")), None)
            .unwrap();
        store
            .insert_asset(root_id, &new_asset("c.NEF", Some("Z8")), None)
            .unwrap();

        store.set_rating(&[a], Some(-1)).unwrap();
        store.set_rating(&[b], Some(5)).unwrap();
        // c is left unrated (NULL).

        assert_eq!(store.facet_count(Some("Z8"), Some(-1)).unwrap(), 1);
        assert_eq!(store.facet_count(Some("Z8"), Some(5)).unwrap(), 1);
        assert_eq!(store.facet_count(Some("Z8"), None).unwrap(), 1);
    }

    #[test]
    fn facet_count_excludes_assets_on_an_offline_volume() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let online_volume = store.upsert_volume("online", None, None, 0).unwrap();
        let offline_volume = store.upsert_volume("offline", None, None, 0).unwrap();
        let online_root = store.ensure_root(online_volume, "").unwrap();
        let offline_root = store.ensure_root(offline_volume, "").unwrap();

        store
            .insert_asset(online_root, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        store
            .insert_asset(offline_root, &new_asset("b.NEF", Some("Z8")), None)
            .unwrap();

        assert_eq!(
            store.facet_count(Some("Z8"), None).unwrap(),
            2,
            "both volumes are online so far"
        );

        // No `CatalogStore` method sets a volume offline yet (that's #24's live-sync scope) --
        // flip it directly for this test.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "UPDATE volume SET online = 0 WHERE id = ?1",
                params![offline_volume],
            )
            .unwrap();

        assert_eq!(
            store.facet_count(Some("Z8"), None).unwrap(),
            1,
            "the offline volume's asset must drop out of the facet count"
        );
    }

    /// Regression test for the `trg_facet_asset_update` trigger's `OR OLD.root_id IS NOT
    /// NEW.root_id` clause: a relink across volumes (ADR-0071's relink tiers) must move the
    /// asset's facet-count bucket even though `model`/`rating` didn't change.
    #[test]
    fn relink_across_volumes_moves_the_asset_facet_bucket() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let v1 = store.upsert_volume("v1", None, None, 0).unwrap();
        let v2 = store.upsert_volume("v2", None, None, 0).unwrap();
        let root1 = store.ensure_root(v1, "").unwrap();
        let root2 = store.ensure_root(v2, "").unwrap();

        let asset_id = store
            .insert_asset(root1, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        assert_eq!(store.facet_count(Some("Z8"), None).unwrap(), 1);

        // Mark v2 offline directly -- no `CatalogStore` method sets this yet (#24's scope).
        store
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE volume SET online = 0 WHERE id = ?1", params![v2])
            .unwrap();

        store
            .relink_asset(asset_id, root2, "a.NEF", "a.nef", 100, 0)
            .unwrap();

        assert_eq!(
            store.facet_count(Some("Z8"), None).unwrap(),
            0,
            "the asset's facet bucket must move to v2 (offline), dropping out of the online sum"
        );
    }

    #[test]
    fn create_keyword_builds_an_id_based_materialized_path() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let events = store.create_keyword(None, "Events").unwrap();
        let named = store.create_keyword(Some(events), "Named").unwrap();
        let birthday = store.create_keyword(Some(named), "birthday-2026").unwrap();

        assert_eq!(
            store.keyword_by_path(&["Events"]).unwrap().unwrap().path,
            format!("/{events}/")
        );
        assert_eq!(
            store
                .keyword_by_path(&["Events", "Named"])
                .unwrap()
                .unwrap()
                .path,
            format!("/{events}/{named}/")
        );
        assert_eq!(
            store
                .keyword_by_path(&["Events", "Named", "birthday-2026"])
                .unwrap()
                .unwrap()
                .path,
            format!("/{events}/{named}/{birthday}/")
        );
    }

    #[test]
    fn sibling_keywords_with_the_same_name_are_rejected_at_root_and_under_a_parent() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        store.create_keyword(None, "Events").unwrap();
        assert!(
            store.create_keyword(None, "Events").is_err(),
            "two root keywords must not share a name (case-insensitively)"
        );
        assert!(
            store.create_keyword(None, "events").is_err(),
            "the unique-root-name check is case-insensitive"
        );

        let a = store.create_keyword(None, "A").unwrap();
        store.create_keyword(Some(a), "Child").unwrap();
        assert!(
            store.create_keyword(Some(a), "Child").is_err(),
            "two siblings under the same parent must not share a name"
        );
    }

    #[test]
    fn rename_keyword_does_not_change_its_path_or_its_childrens() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let parent = store.create_keyword(None, "Old Name").unwrap();
        let child = store.create_keyword(Some(parent), "Child").unwrap();
        let path_before = store.keyword_by_path(&["Old Name"]).unwrap().unwrap().path;

        store.rename_keyword(parent, "New Name").unwrap();

        let renamed = store.keyword_by_path(&["New Name"]).unwrap().unwrap();
        assert_eq!(renamed.id, parent);
        assert_eq!(
            renamed.path, path_before,
            "an id-based path never changes on rename"
        );
        assert!(
            store
                .keyword_by_path(&["New Name", "Child"])
                .unwrap()
                .is_some(),
            "the child keyword must still resolve under the renamed parent"
        );
        let _ = child;
    }

    #[test]
    fn move_keyword_rewrites_its_own_and_every_descendants_path() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let a = store.create_keyword(None, "A").unwrap();
        let b = store.create_keyword(None, "B").unwrap();
        let child = store.create_keyword(Some(a), "Child").unwrap();
        let grandchild = store.create_keyword(Some(child), "Grandchild").unwrap();

        store.move_keyword(child, Some(b)).unwrap();

        let child_path = store
            .keyword_by_path(&["B", "Child"])
            .unwrap()
            .unwrap()
            .path;
        assert_eq!(child_path, format!("/{b}/{child}/"));
        let grandchild_row = store
            .keyword_by_path(&["B", "Child", "Grandchild"])
            .unwrap()
            .unwrap();
        assert_eq!(grandchild_row.id, grandchild);
        assert_eq!(grandchild_row.path, format!("/{b}/{child}/{grandchild}/"));
        assert!(
            store.keyword_by_path(&["A", "Child"]).unwrap().is_none(),
            "the keyword must no longer resolve under its old parent"
        );
    }

    #[test]
    fn move_keyword_into_itself_or_a_descendant_is_rejected() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let parent = store.create_keyword(None, "Parent").unwrap();
        let child = store.create_keyword(Some(parent), "Child").unwrap();
        let grandchild = store.create_keyword(Some(child), "Grandchild").unwrap();

        assert!(
            matches!(
                store.move_keyword(parent, Some(parent)),
                Err(CatalogError::WouldCreateCycle)
            ),
            "moving a keyword under itself must be rejected"
        );
        assert!(
            matches!(
                store.move_keyword(parent, Some(child)),
                Err(CatalogError::WouldCreateCycle)
            ),
            "moving a keyword under its own child must be rejected"
        );
        assert!(
            matches!(
                store.move_keyword(parent, Some(grandchild)),
                Err(CatalogError::WouldCreateCycle)
            ),
            "moving a keyword under its own grandchild must be rejected"
        );

        // The tree must be untouched by any of the rejected attempts.
        assert_eq!(
            store
                .keyword_by_path(&["Parent", "Child"])
                .unwrap()
                .unwrap()
                .id,
            child
        );
        assert_eq!(
            store
                .keyword_by_path(&["Parent", "Child", "Grandchild"])
                .unwrap()
                .unwrap()
                .id,
            grandchild
        );
    }

    #[test]
    fn delete_keyword_removes_its_subtree_and_every_tag_link() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let asset_id = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();

        let parent = store.create_keyword(None, "Parent").unwrap();
        let child = store.create_keyword(Some(parent), "Child").unwrap();
        store.tag(&[asset_id], child).unwrap();

        store.delete_keyword(parent).unwrap();

        assert!(store.keyword_by_path(&["Parent"]).unwrap().is_none());
        assert!(store
            .keyword_by_path(&["Parent", "Child"])
            .unwrap()
            .is_none());
        assert!(
            store.keywords_for(asset_id).unwrap().is_empty(),
            "the asset_keyword link into the deleted subtree must be gone too"
        );
    }

    #[test]
    fn tag_and_untag_round_trip_across_multiple_assets() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let a = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();
        let b = store
            .insert_asset(root_id, &new_asset("b.NEF", None), None)
            .unwrap();
        let keyword = store.create_keyword(None, "Tag").unwrap();

        store.tag(&[a, b], keyword).unwrap();
        assert_eq!(store.keywords_for(a).unwrap().len(), 1);
        assert_eq!(store.keywords_for(b).unwrap().len(), 1);

        // Re-tagging an already-tagged asset is a no-op, not an error.
        store.tag(&[a], keyword).unwrap();
        assert_eq!(store.keywords_for(a).unwrap().len(), 1);

        store.untag(&[a], keyword).unwrap();
        assert!(store.keywords_for(a).unwrap().is_empty());
        assert_eq!(store.keywords_for(b).unwrap().len(), 1);
    }

    #[test]
    fn keyword_by_path_returns_none_for_an_unresolvable_path() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        store.create_keyword(None, "Events").unwrap();
        assert!(store.keyword_by_path(&["Nope"]).unwrap().is_none());
        assert!(store
            .keyword_by_path(&["Events", "Nope"])
            .unwrap()
            .is_none());
    }

    fn default_sort() -> Sort {
        Sort {
            field: SortField::Imported,
            direction: SortDirection::Asc,
        }
    }

    fn full_page() -> Page {
        Page {
            after: None,
            limit: 1000,
        }
    }

    #[test]
    fn hunt_with_no_filter_returns_every_asset_in_the_requested_sort() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let mut a = new_asset("a.NEF", Some("Z8"));
        a.imported_at = 1;
        let mut b = new_asset("b.NEF", Some("Z8"));
        b.imported_at = 2;
        let a_id = store.insert_asset(root_id, &a, None).unwrap();
        let b_id = store.insert_asset(root_id, &b, None).unwrap();

        let ids = store
            .hunt(&Filter::default(), default_sort(), &full_page())
            .unwrap();
        assert_eq!(ids, vec![a_id, b_id]);
    }

    #[test]
    fn hunt_filters_by_rating_range_and_unrated_inclusion() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let a = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();
        let b = store
            .insert_asset(root_id, &new_asset("b.NEF", None), None)
            .unwrap();
        let c = store
            .insert_asset(root_id, &new_asset("c.NEF", None), None)
            .unwrap();
        store.set_rating(&[a], Some(-1)).unwrap();
        store.set_rating(&[b], Some(4)).unwrap();
        // c is left unrated.

        let picks_and_up = Filter {
            rating_min: Some(0),
            ..Default::default()
        };
        assert_eq!(
            store
                .hunt(&picks_and_up, default_sort(), &full_page())
                .unwrap(),
            vec![b]
        );

        let including_unrated = Filter {
            rating_min: Some(0),
            include_unrated: true,
            ..Default::default()
        };
        let mut ids = store
            .hunt(&including_unrated, default_sort(), &full_page())
            .unwrap();
        ids.sort();
        let mut expected = vec![b, c];
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn hunt_keyword_filter_respects_subtree_flag() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let parent = store.create_keyword(None, "Events").unwrap();
        let child = store.create_keyword(Some(parent), "Named").unwrap();

        let tagged_parent = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();
        let tagged_child = store
            .insert_asset(root_id, &new_asset("b.NEF", None), None)
            .unwrap();
        store.tag(&[tagged_parent], parent).unwrap();
        store.tag(&[tagged_child], child).unwrap();

        let exact = Filter {
            keyword_id: Some(parent),
            ..Default::default()
        };
        assert_eq!(
            store.hunt(&exact, default_sort(), &full_page()).unwrap(),
            vec![tagged_parent]
        );

        let subtree = Filter {
            keyword_id: Some(parent),
            include_subtree: true,
            ..Default::default()
        };
        let mut ids = store.hunt(&subtree, default_sort(), &full_page()).unwrap();
        ids.sort();
        let mut expected = vec![tagged_parent, tagged_child];
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn hunt_keyset_pagination_has_no_gaps_or_duplicates() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let mut expected = Vec::new();
        let mut imported_at_by_id = std::collections::HashMap::new();
        for i in 0..25 {
            let mut asset = new_asset(&format!("{i:03}.NEF"), None);
            asset.imported_at = i;
            let id = store.insert_asset(root_id, &asset, None).unwrap();
            expected.push(id);
            imported_at_by_id.insert(id, i);
        }

        let sort = default_sort();
        let mut collected = Vec::new();
        let mut after = None;
        loop {
            let page = Page { after, limit: 7 };
            let ids = store.hunt(&Filter::default(), sort, &page).unwrap();
            if ids.is_empty() {
                break;
            }
            let last_id = *ids.last().unwrap();
            let last_imported = imported_at_by_id[&last_id];
            collected.extend(ids);
            after = Some(Cursor::Imported {
                imported_at: last_imported,
                id: last_id,
            });
        }

        assert_eq!(collected, expected);
    }

    const ALL_SORT_FIELDS: [SortField; 4] = [
        SortField::Captured,
        SortField::Imported,
        SortField::Filename,
        SortField::Rating,
    ];
    const BOTH_DIRECTIONS: [SortDirection; 2] = [SortDirection::Asc, SortDirection::Desc];

    /// Seeds a spread of values (with ties and NULLs on the nullable sort fields) so every
    /// `SortField` has something non-trivial to order.
    fn seed_for_sorting(store: &SqliteCatalog, n: i64) -> i64 {
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        for i in 0..n {
            let mut asset = new_asset(&format!("f{:03}.NEF", (i * 7) % n), None);
            asset.imported_at = i % 5;
            // Every third asset has no capture time; the rest collide in pairs.
            asset.captured_at = (i % 3 != 0).then(|| format!("2026:01:01 00:00:{:02}", i / 2));
            let id = store.insert_asset(root_id, &asset, None).unwrap();
            if i % 4 != 0 {
                store.set_rating(&[id], Some(i % 6 - 1)).unwrap();
            }
        }
        root_id
    }

    #[test]
    fn hunt_ids_matches_paging_through_hunt_for_every_sort() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        seed_for_sorting(&store, 40);

        for field in ALL_SORT_FIELDS {
            for direction in BOTH_DIRECTIONS {
                let sort = Sort { field, direction };
                let all = store.hunt_ids(&Filter::default(), sort).unwrap();
                assert_eq!(all.len(), 40, "{field:?} {direction:?}");

                // Reference: page through `hunt` with the sort's own cursor type.
                let mut paged = Vec::new();
                let mut after = None;
                loop {
                    let ids = store
                        .hunt(&Filter::default(), sort, &Page { after, limit: 9 })
                        .unwrap();
                    let Some(&last_id) = ids.last() else { break };
                    let last = store.get_asset(last_id).unwrap().unwrap();
                    after = Some(match field {
                        SortField::Captured => Cursor::Captured {
                            captured_at: last.captured_at.clone(),
                            id: last_id,
                        },
                        SortField::Imported => Cursor::Imported {
                            imported_at: last.imported_at,
                            id: last_id,
                        },
                        SortField::Filename => Cursor::Filename {
                            rel_path_fold: last.rel_path_fold.clone(),
                            id: last_id,
                        },
                        SortField::Rating => Cursor::Rating {
                            rating: last.rating,
                            id: last_id,
                        },
                    });
                    paged.extend(ids);
                }
                assert_eq!(all, paged, "{field:?} {direction:?}");
            }
        }
    }

    #[test]
    fn hunt_ids_respects_the_filter_and_offline_volumes() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let root_id = seed_for_sorting(&store, 12);
        let filter = Filter {
            root_id: Some(root_id),
            ..Default::default()
        };
        let sort = default_sort();
        assert_eq!(store.hunt_ids(&filter, sort).unwrap().len(), 12);
        assert_eq!(
            store.hunt_ids(&filter, sort).unwrap().len() as u64,
            store.hunt_count(&filter).unwrap()
        );

        let other = store.upsert_volume("other", None, None, 0).unwrap();
        let other_root = store.ensure_root(other, "").unwrap();
        store
            .insert_asset(other_root, &new_asset("elsewhere.NEF", None), None)
            .unwrap();
        assert_eq!(store.hunt_ids(&filter, sort).unwrap().len(), 12);
        assert_eq!(store.hunt_ids(&Filter::default(), sort).unwrap().len(), 13);
    }

    /// Every sort `hunt`/`hunt_ids` can issue must be served by an index in order -- a
    /// `TEMP B-TREE` in the plan means each page re-sorts the whole result set, which is what made
    /// the grid unusable at 1M rows before schema V7.
    #[test]
    fn hunt_sort_uses_an_index() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let root_id = seed_for_sorting(&store, 200);
        // Deliberately no `ANALYZE`: the catalog never runs it (Nine Lives' "no optimize catalog"
        // rule, `ninelives.rs`), so production plans are the stats-free ones. With stats on a
        // catalog whose `root`/`volume` hold a single row each, SQLite prefers to scan those as
        // outer loops and re-sorts `asset` -- not something this crate can control, but a reason
        // never to add an `ANALYZE`/`PRAGMA optimize` here without re-running this test with it.

        let filters = [
            Filter::default(),
            Filter {
                root_id: Some(root_id),
                ..Default::default()
            },
        ];
        for filter in &filters {
            for field in ALL_SORT_FIELDS {
                for direction in BOTH_DIRECTIONS {
                    let conn = store.conn.lock().unwrap();
                    let filter_sql = build_filter_sql(&conn, filter).unwrap();
                    let order = match direction {
                        SortDirection::Asc => "ASC",
                        SortDirection::Desc => "DESC",
                    };
                    let col = sort_column_sql(field);
                    let sql = format!(
                        "EXPLAIN QUERY PLAN SELECT a.id FROM asset a \
                         JOIN root r ON r.id = a.root_id \
                         JOIN volume v ON v.id = r.volume_id \
                         WHERE {} ORDER BY {col} {order}, a.id {order}",
                        filter_sql.where_clause
                    );
                    let param_refs: Vec<&dyn ToSql> =
                        filter_sql.params.iter().map(|p| p.as_ref()).collect();
                    let mut stmt = conn.prepare(&sql).unwrap();
                    let plan: Vec<String> = stmt
                        .query_map(param_refs.as_slice(), |row| row.get::<_, String>(3))
                        .unwrap()
                        .collect::<Result<_, _>>()
                        .unwrap();
                    assert!(
                        !plan.iter().any(|line| line.contains("TEMP B-TREE")),
                        "{field:?} {direction:?} filter={filter:?} sorts through a temp b-tree:\n{}",
                        plan.join("\n")
                    );
                }
            }
        }
    }

    /// V7 kept `idx_asset_captured` on purpose: the sort indexes are expressions and can't serve
    /// a raw `captured_at >= ?` range, so dropping it would turn every date filter into a scan.
    #[test]
    fn a_capture_date_range_filter_still_uses_an_index() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        seed_for_sorting(&store, 200);
        let filter = Filter {
            captured_after: Some("2026:01:01 00:00:10".to_string()),
            captured_before: Some("2026:01:01 00:00:50".to_string()),
            ..Default::default()
        };
        let conn = store.conn.lock().unwrap();
        let filter_sql = build_filter_sql(&conn, &filter).unwrap();
        let sql = format!(
            "EXPLAIN QUERY PLAN SELECT a.id FROM asset a \
             JOIN root r ON r.id = a.root_id JOIN volume v ON v.id = r.volume_id WHERE {}",
            filter_sql.where_clause
        );
        let param_refs: Vec<&dyn ToSql> = filter_sql.params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&sql).unwrap();
        let plan: Vec<String> = stmt
            .query_map(param_refs.as_slice(), |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|l| l.contains("USING INDEX idx_asset_captured")
                    || l.contains("USING COVERING INDEX idx_asset_captured")),
            "a capture-date range should be served by idx_asset_captured:\n{}",
            plan.join("\n")
        );
    }

    /// The snapshot scan runs for up to a second at 1M rows; if it held the shared connection
    /// mutex, every UI-thread catalog call would freeze meanwhile.
    #[test]
    fn hunt_ids_on_a_file_backed_catalog_does_not_take_the_shared_connection() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(SqliteCatalog::open(&dir.path().join("c.db")).unwrap());
        seed_for_sorting(&store, 20);

        // Hold the shared connection for the whole call: a `hunt_ids` that needs it can't finish.
        let held = store.conn.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let store = store.clone();
            std::thread::spawn(move || {
                let ids = store.hunt_ids(&Filter::default(), default_sort()).unwrap();
                tx.send(ids.len()).unwrap();
            })
        };
        let got = rx.recv_timeout(std::time::Duration::from_secs(30));
        drop(held);
        worker.join().unwrap();
        assert_eq!(got, Ok(20), "hunt_ids blocked on the shared connection");
    }

    /// Same guarantee as `hunt_ids`: facets and the distinct-value scans must not hold the shared
    /// connection (the UI thread's `list_roots` needs it every frame).
    #[test]
    fn facets_and_distinct_scans_on_a_file_backed_catalog_do_not_take_the_shared_connection() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(SqliteCatalog::open(&dir.path().join("c.db")).unwrap());
        let v = store.upsert_volume("v", None, None, 0).unwrap();
        let r = store.ensure_root(v, "").unwrap();
        let mut a = new_asset("a.NEF", Some("Z8"));
        a.make = Some("NIKON".to_string());
        store.insert_asset(r, &a, None).unwrap();

        let held = store.conn.lock().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let store = store.clone();
            std::thread::spawn(move || {
                let narrowed = Filter {
                    model: Some("Z8".into()),
                    ..Default::default()
                };
                let total = store.facets(&narrowed).unwrap().total;
                let makes = store.distinct_makes().unwrap();
                tx.send((total, makes)).unwrap();
            })
        };
        let got = rx.recv_timeout(std::time::Duration::from_secs(30));
        drop(held);
        worker.join().unwrap();
        assert_eq!(
            got,
            Ok((1, vec!["NIKON".to_string()])),
            "blocked on shared conn"
        );
    }

    #[test]
    fn date_bounds_skip_unknown_and_malformed_captured_at() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let v = store.upsert_volume("v", None, None, 0).unwrap();
        let r = store.ensure_root(v, "").unwrap();
        let mut ids = Vec::new();
        for (name, at) in [
            ("real", Some("2026-03-01 10:00:00")),
            ("unknown", Some("unknown")),
            ("none", None),
        ] {
            let mut a = new_asset(&format!("{name}.NEF"), None);
            a.captured_at = at.map(str::to_string);
            ids.push(store.insert_asset(r, &a, None).unwrap());
        }
        let after = Filter {
            captured_after: Some("2026-01-01 00:00:00".into()),
            ..Default::default()
        };
        assert_eq!(
            store.hunt_ids(&after, default_sort()).unwrap(),
            vec![ids[0]]
        );
        let before = Filter {
            captured_before: Some("2026-12-31 23:59:59".into()),
            ..Default::default()
        };
        assert_eq!(
            store.hunt_ids(&before, default_sort()).unwrap(),
            vec![ids[0]]
        );
        assert_eq!(
            store
                .hunt_ids(&Filter::default(), default_sort())
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn get_previews_matches_get_preview_per_id_and_omits_missing() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        // More than one internal chunk (500), with every 3rd asset lacking a preview.
        let mut ids = Vec::new();
        for i in 0..1100u32 {
            let preview = (i % 3 != 0).then(|| Preview {
                width: Some(640),
                height: Some(424),
                bytes: i.to_le_bytes().to_vec(),
            });
            let id = store
                .insert_asset(
                    root_id,
                    &new_asset(&format!("{i}.NEF"), None),
                    preview.as_ref(),
                )
                .unwrap();
            ids.push(id);
        }

        let mut batch = store.get_previews(&ids, PreviewTier::T0).unwrap();
        batch.sort_by_key(|(id, _)| *id);
        let mut expected = Vec::new();
        for &id in &ids {
            if let Some(p) = store.get_preview(id, PreviewTier::T0).unwrap() {
                expected.push((id, p));
            }
        }
        expected.sort_by_key(|(id, _)| *id);

        assert_eq!(batch.len(), expected.len());
        for ((bid, bp), (eid, ep)) in batch.iter().zip(&expected) {
            assert_eq!(bid, eid);
            assert_eq!(
                (bp.width, bp.height, &bp.bytes),
                (ep.width, ep.height, &ep.bytes)
            );
        }
        assert!(store.get_previews(&[], PreviewTier::T0).unwrap().is_empty());
    }

    #[test]
    fn hunt_count_matches_hunt_with_an_unbounded_page() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        for i in 0..5 {
            store
                .insert_asset(root_id, &new_asset(&format!("{i}.NEF"), Some("Z8")), None)
                .unwrap();
        }
        assert_eq!(store.hunt_count(&Filter::default()).unwrap(), 5);
        assert_eq!(
            store
                .hunt(&Filter::default(), default_sort(), &full_page())
                .unwrap()
                .len(),
            5
        );
    }

    #[test]
    fn facets_unfiltered_reads_the_trigger_cache_and_matches_a_live_filtered_query() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let a = store
            .insert_asset(root_id, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        let b = store
            .insert_asset(root_id, &new_asset("b.NEF", Some("Z8")), None)
            .unwrap();
        store.set_rating(&[a], Some(5)).unwrap();

        let unfiltered = store.facets(&Filter::default()).unwrap();
        assert_eq!(unfiltered.total, 2);
        assert!(unfiltered.by_model.contains(&(Some("Z8".to_string()), 2)));
        assert!(unfiltered.by_rating.contains(&(Some(5), 1)));
        assert!(unfiltered.by_rating.contains(&(None, 1)));

        let filtered = store
            .facets(&Filter {
                model: Some("Z8".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(filtered.total, 2);
        let _ = b;
    }

    #[test]
    fn manual_collection_add_remove_and_ordering() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let a = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();
        let b = store
            .insert_asset(root_id, &new_asset("b.NEF", None), None)
            .unwrap();
        let c = store
            .insert_asset(root_id, &new_asset("c.NEF", None), None)
            .unwrap();

        let collection_id = store
            .create_collection(None, "Selects", CollectionKind::Manual)
            .unwrap();
        store.add_to_collection(collection_id, &[a, b, c]).unwrap();
        assert_eq!(
            store.collection_assets(collection_id).unwrap(),
            vec![a, b, c]
        );

        store.remove_from_collection(collection_id, &[b]).unwrap();
        assert_eq!(store.collection_assets(collection_id).unwrap(), vec![a, c]);

        // Adding an already-present asset again is a no-op, not a duplicate/reorder.
        store.add_to_collection(collection_id, &[a]).unwrap();
        assert_eq!(store.collection_assets(collection_id).unwrap(), vec![a, c]);

        let collection = store.collection(collection_id).unwrap().unwrap();
        assert_eq!(collection.name, "Selects");
        assert_eq!(collection.kind, CollectionKind::Manual);
    }

    #[test]
    fn smart_collection_rule_round_trips_and_resolves_via_hunt() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let a = store
            .insert_asset(root_id, &new_asset("a.NEF", Some("Z8")), None)
            .unwrap();
        let b = store
            .insert_asset(root_id, &new_asset("b.NEF", Some("D850")), None)
            .unwrap();

        let collection_id = store
            .create_collection(None, "Z8 shots", CollectionKind::Smart)
            .unwrap();
        assert!(store.collection_filter(collection_id).unwrap().is_none());

        let rule = Filter {
            model: Some("Z8".to_string()),
            ..Default::default()
        };
        store.set_smart_rule(collection_id, &rule).unwrap();

        let resolved = store.collection_filter(collection_id).unwrap().unwrap();
        assert_eq!(resolved, rule);
        assert_eq!(
            store.hunt(&resolved, default_sort(), &full_page()).unwrap(),
            vec![a]
        );
        let _ = b;
    }

    #[test]
    fn collection_delete_cascades_to_subtree_and_membership() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let asset_id = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();

        let parent = store
            .create_collection(None, "Parent", CollectionKind::Manual)
            .unwrap();
        let child = store
            .create_collection(Some(parent), "Child", CollectionKind::Manual)
            .unwrap();
        store.add_to_collection(child, &[asset_id]).unwrap();

        store.delete_collection(parent).unwrap();

        assert!(store.collection(parent).unwrap().is_none());
        assert!(store.collection(child).unwrap().is_none());
        assert!(store.collection_assets(child).unwrap().is_empty());
    }

    #[test]
    fn move_collection_into_itself_or_a_descendant_is_rejected() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let parent = store
            .create_collection(None, "Parent", CollectionKind::Manual)
            .unwrap();
        let child = store
            .create_collection(Some(parent), "Child", CollectionKind::Manual)
            .unwrap();
        let grandchild = store
            .create_collection(Some(child), "Grandchild", CollectionKind::Manual)
            .unwrap();

        assert!(
            matches!(
                store.move_collection(parent, Some(parent)),
                Err(CatalogError::WouldCreateCycle)
            ),
            "moving a collection under itself must be rejected"
        );
        assert!(
            matches!(
                store.move_collection(parent, Some(child)),
                Err(CatalogError::WouldCreateCycle)
            ),
            "moving a collection under its own child must be rejected"
        );
        assert!(
            matches!(
                store.move_collection(parent, Some(grandchild)),
                Err(CatalogError::WouldCreateCycle)
            ),
            "moving a collection under its own grandchild must be rejected"
        );

        // The tree, and the ability to delete it cleanly, must be untouched by the rejections --
        // a would-be cycle that slipped through would make this hang instead of returning.
        store.delete_collection(parent).unwrap();
        assert!(store.collection(child).unwrap().is_none());
        assert!(store.collection(grandchild).unwrap().is_none());
    }

    #[test]
    fn rename_keyword_into_an_existing_sibling_name_is_rejected() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let parent = store.create_keyword(None, "Parent").unwrap();
        store.create_keyword(Some(parent), "Existing").unwrap();
        let renaming = store.create_keyword(Some(parent), "Original").unwrap();

        assert!(
            store.rename_keyword(renaming, "Existing").is_err(),
            "renaming onto an already-used sibling name must be rejected"
        );
        assert!(
            store.rename_keyword(renaming, "existing").is_err(),
            "the sibling-name check is case-insensitive"
        );
    }

    #[test]
    fn hunt_keyset_pagination_breaks_ties_correctly_on_a_nullable_sort_field() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();

        // Every asset shares the same rating (a tie on the sort key), so correctness here hinges
        // entirely on the `id` tiebreaker, not the rating value itself.
        let mut ids = Vec::new();
        for i in 0..6 {
            let id = store
                .insert_asset(root_id, &new_asset(&format!("{i}.NEF"), None), None)
                .unwrap();
            store.set_rating(&[id], Some(3)).unwrap();
            ids.push(id);
        }

        let sort = Sort {
            field: SortField::Rating,
            direction: SortDirection::Asc,
        };
        let mut collected = Vec::new();
        let mut after = None;
        loop {
            let page = Page { after, limit: 2 };
            let page_ids = store.hunt(&Filter::default(), sort, &page).unwrap();
            if page_ids.is_empty() {
                break;
            }
            let last_id = *page_ids.last().unwrap();
            collected.extend(page_ids);
            after = Some(Cursor::Rating {
                rating: Some(3),
                id: last_id,
            });
        }

        assert_eq!(
            collected, ids,
            "ties on rating must break by id, ascending, with no gaps or duplicates across pages"
        );
    }

    /// Regression test for a CodeRabbit finding: `remove_asset` only ever deleted
    /// `edit_history`/`edit_variant`/`preview`/`asset` rows, never `asset_keyword`/
    /// `collection_asset` -- with `PRAGMA foreign_keys = ON` (always the case at runtime), the
    /// `DELETE FROM asset` itself would fail with a foreign-key violation for any asset that had
    /// ever been tagged or added to a manual collection. Fixed via `ON DELETE CASCADE` on both
    /// tables' `asset_id` foreign key.
    #[test]
    fn remove_asset_succeeds_for_a_tagged_asset_in_a_manual_collection() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let asset_id = store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();
        let keyword = store.create_keyword(None, "Tag").unwrap();
        store.tag(&[asset_id], keyword).unwrap();
        let collection_id = store
            .create_collection(None, "Selects", CollectionKind::Manual)
            .unwrap();
        store.add_to_collection(collection_id, &[asset_id]).unwrap();

        store.remove_asset(asset_id).unwrap();

        assert!(store
            .find_asset_by_path(root_id, "a.NEF")
            .unwrap()
            .is_none());
        assert!(store.keywords_for(asset_id).unwrap().is_empty());
        assert!(store.collection_assets(collection_id).unwrap().is_empty());
    }

    /// Regression test for a CodeRabbit finding: a smart collection's saved `Filter` can
    /// reference a keyword by id that's since been deleted (`delete_keyword` never touches
    /// `collection.rule_json`) -- resolving it must match nothing, not error out.
    #[test]
    fn hunt_with_a_deleted_keyword_in_the_filter_matches_nothing_instead_of_erroring() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap();
        let keyword = store.create_keyword(None, "Gone").unwrap();
        store.delete_keyword(keyword).unwrap();

        let subtree_filter = Filter {
            keyword_id: Some(keyword),
            include_subtree: true,
            ..Default::default()
        };
        assert_eq!(
            store
                .hunt(&subtree_filter, default_sort(), &full_page())
                .unwrap(),
            Vec::<i64>::new()
        );
        assert_eq!(store.hunt_count(&subtree_filter).unwrap(), 0);
        assert_eq!(store.facets(&subtree_filter).unwrap().total, 0);
    }

    /// Regression test for a CodeRabbit finding: `rel_path_prefix`/`filename_contains` embedded
    /// caller text directly into a `GLOB` pattern with no escaping and only `to_lowercase()`
    /// folding (not the NFC-then-lowercase fold `rel_path_fold` itself uses).
    #[test]
    fn filename_search_escapes_glob_metacharacters_and_folds_unicode_like_rel_path_fold() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        // A literal `[1]` in the filename -- without escaping, a GLOB search for the same text
        // would instead be interpreted as a character class and match "IMG_1.NEF" too.
        let literal_id = store
            .insert_asset(root_id, &new_asset("IMG_[1].NEF", None), None)
            .unwrap();
        store
            .insert_asset(root_id, &new_asset("IMG_1.NEF", None), None)
            .unwrap();

        let filter = Filter {
            filename_contains: Some("[1]".to_string()),
            ..Default::default()
        };
        assert_eq!(
            store.hunt(&filter, default_sort(), &full_page()).unwrap(),
            vec![literal_id],
            "a literal `[1]` in filename_contains must match only the literal filename, \
             not be interpreted as a GLOB character class"
        );
    }

    // ---- #32: culling markers, batch removal, delete journal ------------------------------

    fn store_with_assets(n: usize) -> (SqliteCatalog, i64, Vec<i64>) {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        let ids = (0..n)
            .map(|i| {
                store
                    .insert_asset(root_id, &new_asset(&format!("{i}.NEF"), Some("Z8")), None)
                    .unwrap()
            })
            .collect();
        (store, root_id, ids)
    }

    #[test]
    fn get_meta_reads_all_three_markers_and_skips_unknown_ids() {
        let (store, _, ids) = store_with_assets(3);
        store.set_rating(&[ids[0]], Some(4)).unwrap();
        store.set_rating(&[ids[1]], Some(-1)).unwrap();
        store.set_flag(&[ids[0]], Some(1)).unwrap();
        store.set_label(&[ids[0]], Some("Red")).unwrap();

        let meta = store.get_meta(&[ids[0], ids[1], ids[2], 9999]).unwrap();
        assert_eq!(meta.len(), 3, "an unknown id is absent, not an error");
        assert_eq!(
            meta[&ids[0]],
            AssetMeta {
                rating: Some(4),
                flag: Some(1),
                label: Some("Red".into())
            }
        );
        assert_eq!(meta[&ids[1]].rating, Some(-1));
        assert_eq!(meta[&ids[2]], AssetMeta::default());
        assert!(store.get_meta(&[]).unwrap().is_empty());
    }

    #[test]
    fn get_meta_spans_more_ids_than_one_chunk() {
        let (store, _, ids) = store_with_assets(MAX_IDS_PER_STATEMENT + 5);
        store.set_rating(&ids, Some(2)).unwrap();
        let meta = store.get_meta(&ids).unwrap();
        assert_eq!(meta.len(), ids.len());
        assert!(meta.values().all(|m| m.rating == Some(2)));
    }

    #[test]
    fn set_meta_restores_mixed_per_asset_values_in_one_call() {
        let (store, _, ids) = store_with_assets(3);
        store.set_rating(&ids, Some(5)).unwrap();
        store.set_label(&[ids[2]], Some("Blue")).unwrap();

        store
            .set_meta(&[
                (
                    ids[0],
                    AssetMeta {
                        rating: None,
                        flag: Some(1),
                        label: None,
                    },
                ),
                (
                    ids[1],
                    AssetMeta {
                        rating: Some(-1),
                        flag: None,
                        label: Some("Red".into()),
                    },
                ),
                // ids[2] is reset to fully unmarked, clearing its label.
                (ids[2], AssetMeta::default()),
                (9999, AssetMeta::default()), // unknown id is ignored
            ])
            .unwrap();

        let meta = store.get_meta(&ids).unwrap();
        assert_eq!(meta[&ids[0]].rating, None);
        assert_eq!(meta[&ids[0]].flag, Some(1));
        assert_eq!(meta[&ids[1]].rating, Some(-1));
        assert_eq!(meta[&ids[1]].label.as_deref(), Some("Red"));
        assert_eq!(meta[&ids[2]], AssetMeta::default());
        store.set_meta(&[]).unwrap();
    }

    #[test]
    fn set_meta_is_atomic_when_one_value_violates_a_check_constraint() {
        let (store, _, ids) = store_with_assets(2);
        store.set_rating(&ids, Some(3)).unwrap();
        let bad = AssetMeta {
            rating: Some(99), // outside -1..=5
            flag: None,
            label: None,
        };
        let good = AssetMeta {
            rating: Some(1),
            flag: None,
            label: None,
        };
        assert!(store.set_meta(&[(ids[0], good), (ids[1], bad)]).is_err());
        let meta = store.get_meta(&ids).unwrap();
        assert_eq!(
            meta[&ids[0]].rating,
            Some(3),
            "the failed batch must leave the earlier row untouched"
        );
    }

    #[test]
    fn remove_assets_deletes_every_child_row_across_chunks_and_ignores_unknown_ids() {
        let (store, root_id, ids) = store_with_assets(MAX_IDS_PER_STATEMENT + 5);
        let keyword = store.create_keyword(None, "Tag").unwrap();
        store.tag(&ids[..3], keyword).unwrap();
        let collection = store
            .create_collection(None, "Selects", CollectionKind::Manual)
            .unwrap();
        store.add_to_collection(collection, &ids[..3]).unwrap();
        store
            .begin_delete_items(&[(ids[0], "/x/0.NEF".into())])
            .unwrap();

        let mut doomed: Vec<i64> = ids[..ids.len() - 2].to_vec();
        doomed.push(9999);
        store.remove_assets(&doomed).unwrap();

        assert_eq!(store.asset_count().unwrap(), 2, "the two untouched survive");
        assert!(store.open_delete_items().unwrap().is_empty());
        assert!(store.collection_assets(collection).unwrap().is_empty());
        assert!(store.keywords_for(ids[0]).unwrap().is_empty());
        assert!(store
            .find_asset_by_path(root_id, &format!("{}.NEF", ids.len() - 1))
            .unwrap()
            .is_some());
        store.remove_assets(&[]).unwrap();
    }

    #[test]
    fn delete_journal_begin_mark_abandon_and_open() {
        let (store, _, ids) = store_with_assets(3);
        store
            .begin_delete_items(&[(ids[0], "/r/0.NEF".into()), (ids[1], "/r/1.NEF".into())])
            .unwrap();
        let open = store.open_delete_items().unwrap();
        assert_eq!(open.len(), 2);
        assert!(open.iter().all(|i| i.state == DeleteState::Pending));

        store.mark_delete_items_trashed(&[ids[0]]).unwrap();
        // Re-journaling an already-trashed asset must not reset it to pending.
        store
            .begin_delete_items(&[(ids[0], "/elsewhere.NEF".into())])
            .unwrap();
        let open = store.open_delete_items().unwrap();
        assert_eq!(open[0].asset_id, ids[0]);
        assert_eq!(open[0].state, DeleteState::Trashed);
        assert_eq!(open[0].abs_path, "/r/0.NEF");
        assert_eq!(open[1].state, DeleteState::Pending);

        store.abandon_delete_items(&[ids[1]]).unwrap();
        assert_eq!(store.open_delete_items().unwrap().len(), 1);
        assert_eq!(
            store.asset_count().unwrap(),
            3,
            "abandoning a journal row never touches the asset itself"
        );
        store.begin_delete_items(&[]).unwrap();
        store.mark_delete_items_trashed(&[]).unwrap();
        store.abandon_delete_items(&[]).unwrap();
    }

    #[test]
    fn hunt_unflagged_and_no_label_filters() {
        let (store, _, ids) = store_with_assets(4);
        store.set_flag(&[ids[0]], Some(1)).unwrap();
        store.set_label(&[ids[1]], Some("Red")).unwrap();
        let sort = Sort {
            field: SortField::Filename,
            direction: SortDirection::Asc,
        };

        let unflagged = Filter {
            unflagged: true,
            ..Filter::default()
        };
        assert_eq!(
            store.hunt_ids(&unflagged, sort).unwrap(),
            vec![ids[1], ids[2], ids[3]]
        );
        let no_label = Filter {
            no_label: true,
            ..Filter::default()
        };
        assert_eq!(
            store.hunt_ids(&no_label, sort).unwrap(),
            vec![ids[0], ids[2], ids[3]]
        );
        let both = Filter {
            unflagged: true,
            no_label: true,
            ..Filter::default()
        };
        assert_eq!(store.hunt_ids(&both, sort).unwrap(), vec![ids[2], ids[3]]);
    }

    #[test]
    fn hunt_unrated_filter_and_its_combination_with_a_rating_range() {
        let (store, _, ids) = store_with_assets(3);
        store.set_rating(&[ids[0]], Some(4)).unwrap();
        store.set_rating(&[ids[1]], Some(-1)).unwrap();
        let sort = Sort {
            field: SortField::Filename,
            direction: SortDirection::Asc,
        };
        let unrated = Filter {
            unrated: true,
            ..Filter::default()
        };
        assert_eq!(store.hunt_ids(&unrated, sort).unwrap(), vec![ids[2]]);
        let contradictory = Filter {
            unrated: true,
            rating_min: Some(0),
            ..Filter::default()
        };
        assert!(store.hunt_ids(&contradictory, sort).unwrap().is_empty());
    }

    #[test]
    fn a_saved_filter_rule_from_before_these_fields_still_deserializes() {
        // `clowder.rs`'s `v: 1` rule contract: a rule serialized before `unflagged`/`no_label`
        // existed has neither key.
        let old = r#"{"keyword_id":null,"include_subtree":false,"rating_min":2,"rating_max":null,
            "include_unrated":false,"flag":null,"label":null,"make":null,"model":null,
            "captured_after":null,"captured_before":null,"root_id":null,"rel_path_prefix":null,
            "filename_contains":null}"#;
        let f: Filter = serde_json::from_str(old).unwrap();
        assert_eq!(f.rating_min, Some(2));
        assert!(!f.unflagged && !f.no_label && !f.unrated);
    }

    fn lrc_item(
        asset_id: i64,
        global: &str,
        meta: Option<AssetMeta>,
        doc: Option<nicti_pawprint::EditDocument>,
        variant: Option<&str>,
    ) -> LrcItem {
        LrcItem {
            asset_id,
            meta,
            variant_name: variant.map(str::to_string),
            document: doc,
            provenance: LrcProvenance {
                image_global: global.to_string(),
                image_local: 1,
                develop_text: Some("s = { Exposure2012 = 0.5 }".to_string()),
                untranslated: vec!["Clarity2012".to_string()],
                ..LrcProvenance::default()
            },
        }
    }

    fn lrc_meta(rating: Option<i64>) -> AssetMeta {
        AssetMeta {
            rating,
            flag: Some(1),
            label: Some("Red".to_string()),
        }
    }

    fn one_asset(store: &SqliteCatalog) -> i64 {
        let volume_id = store.upsert_volume("v", None, None, 0).unwrap();
        let root_id = store.ensure_root(volume_id, "").unwrap();
        store
            .insert_asset(root_id, &new_asset("a.NEF", None), None)
            .unwrap()
    }

    #[test]
    fn apply_lrc_chunk_writes_markers_document_and_provenance_then_is_idempotent() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let id = one_asset(&store);
        let item = lrc_item(id, "G1", Some(lrc_meta(Some(4))), Some(edit_doc(0.5)), None);

        let first = store.apply_lrc_chunk(std::slice::from_ref(&item)).unwrap();
        assert_eq!((first.docs_written, first.meta_applied), (1, 1));
        assert_eq!(first.meta_changed_assets, vec![id]);
        assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(0.5)));
        assert_eq!(store.get_meta(&[id]).unwrap()[&id], lrc_meta(Some(4)));
        let prov = store.lrc_provenance("G1").unwrap().unwrap();
        assert_eq!(
            prov.develop_text.as_deref(),
            Some("s = { Exposure2012 = 0.5 }")
        );
        assert_eq!(prov.untranslated, vec!["Clarity2012".to_string()]);

        let again = store.apply_lrc_chunk(std::slice::from_ref(&item)).unwrap();
        assert_eq!(again, LrcChunkOutcome::default());
        assert_eq!(store.variant_names(id).unwrap(), vec!["master".to_string()]);
    }

    #[test]
    fn apply_lrc_chunk_keeps_a_users_later_edit_but_not_the_first_import_over_xmp_values() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let id = one_asset(&store);
        // XMP ingest already filled in a rating: the first import overrides it.
        store.set_rating(&[id], Some(1)).unwrap();
        let item = lrc_item(id, "G1", Some(lrc_meta(Some(4))), Some(edit_doc(0.5)), None);
        store.apply_lrc_chunk(std::slice::from_ref(&item)).unwrap();
        assert_eq!(store.get_meta(&[id]).unwrap()[&id].rating, Some(4));

        // The user edits in nicti afterwards; a re-run with different LRC data leaves both alone.
        store.set_rating(&[id], Some(2)).unwrap();
        store.put_master_edit(id, &edit_doc(-1.0)).unwrap();
        let changed = lrc_item(id, "G1", Some(lrc_meta(Some(5))), Some(edit_doc(0.9)), None);
        let out = store.apply_lrc_chunk(&[changed]).unwrap();
        assert_eq!((out.kept_local_docs, out.kept_local_meta), (1, 1));
        assert_eq!((out.docs_written, out.meta_applied), (0, 0));
        assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(-1.0)));
        assert_eq!(store.get_meta(&[id]).unwrap()[&id].rating, Some(2));
    }

    #[test]
    fn apply_lrc_chunk_does_not_overwrite_a_master_that_already_has_edits_on_first_import() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let id = one_asset(&store);
        store.put_master_edit(id, &edit_doc(2.0)).unwrap();
        let out = store
            .apply_lrc_chunk(&[lrc_item(id, "G1", None, Some(edit_doc(0.5)), None)])
            .unwrap();
        assert_eq!((out.docs_written, out.kept_local_docs), (0, 1));
        assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(2.0)));
    }

    #[test]
    fn apply_lrc_chunk_makes_a_virtual_copy_variant_without_touching_markers() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let id = one_asset(&store);
        let master = lrc_item(id, "G1", Some(lrc_meta(Some(3))), Some(edit_doc(0.1)), None);
        let copy = lrc_item(id, "G2", None, Some(edit_doc(0.7)), Some("Copy 1"));
        let out = store.apply_lrc_chunk(&[master, copy.clone()]).unwrap();
        assert_eq!(out.variants_created, 1);
        assert_eq!(
            store.variant_names(id).unwrap(),
            vec!["master".to_string(), "Copy 1".to_string()]
        );
        assert_eq!(
            store.get_variant_edit(id, "Copy 1").unwrap(),
            Some(edit_doc(0.7))
        );
        assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(0.1)));
        assert_eq!(store.get_meta(&[id]).unwrap()[&id], lrc_meta(Some(3)));
        // Re-run finds the copy by provenance instead of minting "Copy 1 (1)".
        let again = store.apply_lrc_chunk(&[copy]).unwrap();
        assert_eq!(again.variants_created, 0);
        assert_eq!(store.variant_names(id).unwrap().len(), 2);
    }

    #[test]
    fn apply_lrc_chunk_disambiguates_a_duplicate_copy_name() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let id = one_asset(&store);
        store
            .apply_lrc_chunk(&[
                lrc_item(id, "G1", None, None, Some("Copy")),
                lrc_item(id, "G2", None, None, Some("Copy")),
            ])
            .unwrap();
        assert_eq!(
            store.variant_names(id).unwrap(),
            vec![
                "master".to_string(),
                "Copy".to_string(),
                "Copy (1)".to_string()
            ]
        );
    }

    #[test]
    fn apply_lrc_chunk_rolls_back_the_whole_chunk_on_a_missing_asset() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let id = one_asset(&store);
        let ok = lrc_item(id, "G1", Some(lrc_meta(Some(2))), None, None);
        let bad = lrc_item(9999, "G2", None, None, None);
        assert!(store.apply_lrc_chunk(&[ok, bad]).is_err());
        assert!(store.lrc_provenance("G1").unwrap().is_none());
        assert_eq!(store.get_meta(&[id]).unwrap()[&id].rating, None);
    }

    #[test]
    fn removing_an_asset_drops_its_lrc_provenance() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let id = one_asset(&store);
        store
            .apply_lrc_chunk(&[lrc_item(id, "G1", None, Some(edit_doc(0.5)), None)])
            .unwrap();
        store.remove_assets(&[id]).unwrap();
        assert!(store.lrc_provenance("G1").unwrap().is_none());
    }

    #[test]
    fn first_import_never_erases_a_marker_nicti_already_has_when_lrc_has_none() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let id = one_asset(&store);
        store.set_rating(&[id], Some(3)).unwrap();
        store.set_label(&[id], Some("Blue")).unwrap();
        // LRC: unrated, unflagged, unlabelled -- but flagged pick.
        let none = AssetMeta {
            rating: None,
            flag: Some(1),
            label: None,
        };
        let out = store
            .apply_lrc_chunk(&[lrc_item(id, "G1", Some(none), None, None)])
            .unwrap();
        let got = store.get_meta(&[id]).unwrap()[&id].clone();
        assert_eq!(
            got,
            AssetMeta {
                rating: Some(3),
                flag: Some(1),
                label: Some("Blue".into())
            }
        );
        assert_eq!(out.meta_changed_assets, vec![id]);
        // The re-run is stable (the merged result is what the import recorded).
        let again = store
            .apply_lrc_chunk(&[lrc_item(
                id,
                "G1",
                Some(AssetMeta {
                    rating: None,
                    flag: Some(1),
                    label: None,
                }),
                None,
                None,
            )])
            .unwrap();
        assert_eq!((again.meta_applied, again.kept_local_meta), (0, 0));
    }

    #[test]
    fn two_lrc_masters_on_one_asset_do_not_corrupt_each_others_provenance() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let id = one_asset(&store);
        let first = lrc_item(id, "G1", None, Some(edit_doc(0.5)), None);
        let second = lrc_item(id, "G3", None, Some(edit_doc(0.9)), None);
        let out = store.apply_lrc_chunk(&[first.clone(), second]).unwrap();
        assert_eq!(out.skipped_conflicts, 1);
        assert!(store.lrc_provenance("G3").unwrap().is_none());
        assert_eq!(store.get_master_edit(id).unwrap(), Some(edit_doc(0.5)));
        // G1 is still recognised on a re-run.
        let again = store.apply_lrc_chunk(&[first]).unwrap();
        assert_eq!((again.skipped_conflicts, again.kept_local_docs), (0, 0));
    }

    #[test]
    fn an_lrc_image_that_now_resolves_to_another_asset_is_left_alone() {
        let store = SqliteCatalog::open_in_memory().unwrap();
        let a = one_asset(&store);
        let root = store.list_roots().unwrap()[0].id;
        let b = store
            .insert_asset(root, &new_asset("b.NEF", None), None)
            .unwrap();
        store
            .apply_lrc_chunk(&[lrc_item(
                a,
                "G1",
                Some(lrc_meta(Some(4))),
                Some(edit_doc(0.5)),
                None,
            )])
            .unwrap();
        let moved = store
            .apply_lrc_chunk(&[lrc_item(
                b,
                "G1",
                Some(lrc_meta(Some(1))),
                Some(edit_doc(0.1)),
                None,
            )])
            .unwrap();
        assert_eq!(moved.skipped_conflicts, 1);
        assert_eq!(store.get_meta(&[b]).unwrap()[&b].rating, None);
        assert_eq!(
            store.get_master_edit(b).unwrap(),
            Some(nicti_pawprint::EditDocument::default())
        );
    }
}
