//! #22's real catalog schema, promoted from the spike prototypes ADR-0071 (`spikes/homing`) and
//! ADR-0067/0103 (`spikes/den`) settled the shape of:
//!
//! - `volume`/`root`/`asset`: three levels (ADR-0071) so a registered folder can move between
//!   drives (#72) as a single `root` row update, without touching every `asset` row underneath.
//! - `preview`: the T0 grid-preview cache (ADR-0029), keyed by `(asset_id, tier)`.
//! - `edit_variant`/`edit_history`: the physical shape ADR-0021 left for this ticket — one master
//!   variant per asset (enforced by a partial unique index), each variant's edit document plus an
//!   append-only history log.
//! - `facet_counts`: the trigger-maintained `(model, rating)` aggregate ADR-0103 adopted, kept
//!   current on every asset insert/update/delete rather than scanned fresh per query.
//!
//! Migrations are gated on `PRAGMA user_version` (real, persisted in the SQLite file header), not
//! `CREATE TABLE IF NOT EXISTS` — each migration in [`MIGRATIONS`] runs at most once per database
//! file, in order, and a database already at the current version runs none of them on reopen.

use rusqlite::Connection;

use crate::CatalogError;

const MIGRATION_V1: &str = r#"
CREATE TABLE volume (
    id              INTEGER PRIMARY KEY,
    identity_key    TEXT NOT NULL UNIQUE,
    label           TEXT,
    -- Secondary identity signal (ADR-0071's `.nicti-volume` marker file, read-only-volume/clone
    -- caveats and all) -- not the primary key, but a real disagreement with an already-registered
    -- volume under the same `identity_key` is refused rather than silently merged. This closes
    -- part, not all, of ADR-0071's flagged "two volumes, same identity -> never auto-merge" gap;
    -- a genuine identity_key *collision* between two physically distinct volumes that also agree
    -- on this marker is still unresolved, same as ADR-0071 itself leaves it (Proposed, not
    -- Accepted).
    marker_uuid     TEXT,
    online          INTEGER NOT NULL DEFAULT 1,
    last_seen_at    INTEGER NOT NULL
);

-- A registered folder tracked for assets, scoped to one volume. #72's archive-transition toggles
-- `archived` here, not per-asset.
CREATE TABLE root (
    id          INTEGER PRIMARY KEY,
    volume_id   INTEGER NOT NULL REFERENCES volume(id),
    rel_path    TEXT NOT NULL,
    archived    INTEGER NOT NULL DEFAULT 0,
    UNIQUE(volume_id, rel_path)
);

CREATE TABLE asset (
    id              INTEGER PRIMARY KEY,
    root_id         INTEGER NOT NULL REFERENCES root(id),
    rel_path        TEXT NOT NULL,
    rel_path_fold   TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    mtime_unix      INTEGER NOT NULL,
    fingerprint     TEXT,
    natural_key     TEXT,
    make            TEXT,
    model           TEXT,
    captured_at     TEXT,
    rating          INTEGER NOT NULL DEFAULT 0,
    width           INTEGER,
    height          INTEGER,
    imported_at     INTEGER NOT NULL,
    UNIQUE(root_id, rel_path)
);

CREATE INDEX idx_asset_root_fold ON asset(root_id, rel_path_fold);
CREATE INDEX idx_asset_fingerprint ON asset(fingerprint) WHERE fingerprint IS NOT NULL;
CREATE INDEX idx_asset_natural_key ON asset(natural_key) WHERE natural_key IS NOT NULL;
-- ADR-0067's composite facet index: (model, rating) range/equality scans.
CREATE INDEX idx_asset_model_rating ON asset(model, rating);

-- T0 grid preview only, for now (ADR-0029): the Nikon PreviewIFD JPEG, copied verbatim at import
-- time. T1/T2/T3 (RAM-only / pack-file / on-demand full decode) are a render-pipeline concern, not
-- catalog storage, and land with whichever ticket wires up the render pipeline.
CREATE TABLE preview (
    asset_id    INTEGER NOT NULL REFERENCES asset(id),
    tier        TEXT NOT NULL,
    width       INTEGER,
    height      INTEGER,
    bytes       BLOB NOT NULL,
    PRIMARY KEY (asset_id, tier)
);

-- One row per edit variant (the master, plus any future virtual copy — ADR-0021's "virtual copies
-- = multiple edit rows"). `document` is the serialized `EditDocument` (ADR-0021's fixed-order
-- stage-parameter map) as JSON text.
CREATE TABLE edit_variant (
    id          INTEGER PRIMARY KEY,
    asset_id    INTEGER NOT NULL REFERENCES asset(id),
    name        TEXT NOT NULL,
    is_master   INTEGER NOT NULL DEFAULT 0,
    document    TEXT NOT NULL,
    UNIQUE(asset_id, name)
);
-- Enforces "one flagged master" per asset (ADR-0021's `asset 1—N edit_variant` shape) without a
-- separate lookup table: a partial unique index over rows where is_master=1.
CREATE UNIQUE INDEX idx_edit_variant_one_master ON edit_variant(asset_id) WHERE is_master = 1;

-- Append-only delta log (ADR-0021): `delta` is one compacted history entry's JSON. Burst
-- compaction and named, never-pruned snapshots are a write-path concern for whichever ticket
-- implements live editing (the shape here just needs to hold them, per-variant, in order).
CREATE TABLE edit_history (
    id              INTEGER PRIMARY KEY,
    variant_id      INTEGER NOT NULL REFERENCES edit_variant(id),
    seq             INTEGER NOT NULL,
    delta           TEXT NOT NULL,
    snapshot_name   TEXT,
    UNIQUE(variant_id, seq)
);

-- ADR-0103's trigger-maintained facet-count aggregate, scoped to (model, rating) — the grain
-- ADR-0067's own composite index already covers. A keyword facet dimension isn't part of this
-- ticket's schema (no keyword table exists yet); extending this table/its triggers to a
-- (model, rating, keyword) grain, the way `spikes/den/src/facet_cache_trigger.rs` prototyped, is
-- left to whichever ticket adds keyword tagging.
CREATE TABLE facet_counts (
    model   TEXT NOT NULL,
    rating  INTEGER NOT NULL,
    cnt     INTEGER NOT NULL,
    PRIMARY KEY (model, rating)
);

CREATE TRIGGER trg_facet_asset_insert AFTER INSERT ON asset
BEGIN
    INSERT INTO facet_counts (model, rating, cnt)
    VALUES (COALESCE(NEW.model, ''), NEW.rating, 1)
    ON CONFLICT(model, rating) DO UPDATE SET cnt = cnt + 1;
END;

CREATE TRIGGER trg_facet_asset_delete AFTER DELETE ON asset
BEGIN
    UPDATE facet_counts SET cnt = cnt - 1
    WHERE model = COALESCE(OLD.model, '') AND rating = OLD.rating;
    DELETE FROM facet_counts
    WHERE cnt <= 0 AND model = COALESCE(OLD.model, '') AND rating = OLD.rating;
END;

-- Fires on the upsert-on-move/re-import path (`model`/`rating` can both change on an in-place
-- edit or a rescan that picks up new EXIF), moving this asset's count from its old bucket to its
-- new one rather than double-counting or leaking a stale row.
CREATE TRIGGER trg_facet_asset_update AFTER UPDATE OF model, rating ON asset
WHEN OLD.model IS NOT NEW.model OR OLD.rating IS NOT NEW.rating
BEGIN
    UPDATE facet_counts SET cnt = cnt - 1
    WHERE model = COALESCE(OLD.model, '') AND rating = OLD.rating;
    DELETE FROM facet_counts
    WHERE cnt <= 0 AND model = COALESCE(OLD.model, '') AND rating = OLD.rating;

    INSERT INTO facet_counts (model, rating, cnt)
    VALUES (COALESCE(NEW.model, ''), NEW.rating, 1)
    ON CONFLICT(model, rating) DO UPDATE SET cnt = cnt + 1;
END;
"#;

/// #24: adds the `missing_since` column `patrol::sync_root` uses to flag an asset whose file
/// disappeared from disk since the last sync, without ever deleting the row (ADR-0071's "never
/// delete, only flag offline" stance for a volume applies the same way here at the asset grain
/// for a *file* gone from a still-connected volume). `NULL` means present; this is also the
/// default for every pre-existing row, so a catalog upgrading from V1 treats everything already
/// cataloged as present until the next sync says otherwise.
const MIGRATION_V2: &str = r#"
ALTER TABLE asset ADD COLUMN missing_since INTEGER;
"#;

/// #23 (v3): `rating` becomes nullable so "unrated" is distinct from "0 stars" (ADR-0059/0061's
/// XMP/LRC convention: `xmp:Rating` absent means unrated, `-1` means reject), adds a pick `flag`
/// and a free-text `label` column, adds a `captured_at` index for keyset date paging, and moves
/// `facet_counts` to a `(volume_id, model, rating)` grain so an offline volume's assets can be
/// excluded by a query-time join against `volume.online` (see `SqliteCatalog::facet_count`)
/// instead of a trigger that would otherwise have to recompute every row on every online/offline
/// flip (ADR-0071's facet hand-off).
///
/// SQLite can't relax a column's `NOT NULL` constraint or add a table in place, so `asset` and
/// `facet_counts` are rebuilt from scratch using SQLite's documented "12-step" table-rebuild
/// recipe (build the new table under a scratch name, copy data across, drop the old table, rename
/// the new one into place) rather than `ALTER TABLE ... RENAME`: `preview` and `edit_variant` hold
/// `FOREIGN KEY REFERENCES asset(id)`, and renaming `asset` itself would leave those clauses
/// pointing at the renamed (soon-to-be-dropped) table rather than the freshly rebuilt one.
/// `migrate` toggles `PRAGMA foreign_keys` off for the duration and re-verifies with
/// `PRAGMA foreign_key_check` immediately after, since SQLite doesn't itself validate FK targets
/// at `DROP TABLE`/`CREATE TABLE` time.
///
/// Also carries #24's `missing_since` column (added by V2) straight through unchanged -- this
/// migration has no reason to touch it, only to not lose it during the rebuild.
///
/// No real catalog has shipped yet (pre-v1), so collapsing every existing `rating = 0` row to NULL
/// (unrated) on migration is a documented, deliberate one-time correction, not a guess at real
/// user intent that would matter for an already-in-use catalog.
const MIGRATION_V3: &str = r#"
DROP TRIGGER trg_facet_asset_insert;
DROP TRIGGER trg_facet_asset_delete;
DROP TRIGGER trg_facet_asset_update;

CREATE TABLE asset_v3 (
    id              INTEGER PRIMARY KEY,
    root_id         INTEGER NOT NULL REFERENCES root(id),
    rel_path        TEXT NOT NULL,
    rel_path_fold   TEXT NOT NULL,
    size_bytes      INTEGER NOT NULL,
    mtime_unix      INTEGER NOT NULL,
    fingerprint     TEXT,
    natural_key     TEXT,
    make            TEXT,
    model           TEXT,
    captured_at     TEXT,
    -- NULL = unrated, -1 = reject, 0-5 = star rating (ADR-0059/0061).
    rating          INTEGER CHECK (rating IS NULL OR rating BETWEEN -1 AND 5),
    -- NULL = unflagged, 1 = pick. Reject lives on `rating`, not here (matches LRC/XMP, which has
    -- no separate reject flag distinct from a rating of -1).
    flag            INTEGER CHECK (flag IS NULL OR flag = 1),
    label           TEXT,
    width           INTEGER,
    height          INTEGER,
    imported_at     INTEGER NOT NULL,
    -- #24's column, added by V2 -- carried through unchanged, not this migration's concern.
    missing_since   INTEGER,
    UNIQUE(root_id, rel_path)
);

INSERT INTO asset_v3 (id, root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, fingerprint,
    natural_key, make, model, captured_at, rating, flag, label, width, height, imported_at,
    missing_since)
SELECT id, root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, fingerprint, natural_key,
    make, model, captured_at,
    -- Only the old NOT-NULL-default's own "0" collapses to unrated -- any other value a dev
    -- catalog happened to have wasn't possible to write through the old (pre-#23) API, since
    -- `insert_asset` hardcoded 0 and nothing else ever set `rating`, but this preserves it rather
    -- than assuming that's still true by the time this migration actually runs against a file.
    CASE WHEN rating = 0 THEN NULL ELSE rating END,
    NULL, NULL, width, height, imported_at, missing_since
FROM asset;

DROP TABLE asset;
ALTER TABLE asset_v3 RENAME TO asset;

CREATE INDEX idx_asset_root_fold ON asset(root_id, rel_path_fold);
CREATE INDEX idx_asset_fingerprint ON asset(fingerprint) WHERE fingerprint IS NOT NULL;
CREATE INDEX idx_asset_natural_key ON asset(natural_key) WHERE natural_key IS NOT NULL;
CREATE INDEX idx_asset_model_rating ON asset(model, rating);
-- Keyset pagination sorted by capture date (#23's `hunt`).
CREATE INDEX idx_asset_captured ON asset(captured_at, id);

DROP TABLE facet_counts;

-- `rating` here stores the real -1..5 value, or -128 (FACET_UNRATED_SENTINEL in sqlite.rs, chosen
-- well outside the real range) standing in for NULL/unrated -- a PRIMARY KEY column can't itself
-- be NULL. `facet_count` sums across every `volume_id` whose `volume.online = 1` rather than this
-- table tracking online state itself.
CREATE TABLE facet_counts (
    volume_id   INTEGER NOT NULL REFERENCES volume(id),
    model       TEXT NOT NULL,
    rating      INTEGER NOT NULL,
    cnt         INTEGER NOT NULL,
    PRIMARY KEY (volume_id, model, rating)
);
CREATE INDEX idx_facet_counts_model ON facet_counts(model, rating);

INSERT INTO facet_counts (volume_id, model, rating, cnt)
SELECT r.volume_id, COALESCE(a.model, ''), COALESCE(a.rating, -128), COUNT(*)
FROM asset a JOIN root r ON r.id = a.root_id
GROUP BY r.volume_id, COALESCE(a.model, ''), COALESCE(a.rating, -128);

CREATE TRIGGER trg_facet_asset_insert AFTER INSERT ON asset
BEGIN
    INSERT INTO facet_counts (volume_id, model, rating, cnt)
    VALUES (
        (SELECT volume_id FROM root WHERE id = NEW.root_id),
        COALESCE(NEW.model, ''),
        COALESCE(NEW.rating, -128),
        1
    )
    ON CONFLICT(volume_id, model, rating) DO UPDATE SET cnt = cnt + 1;
END;

CREATE TRIGGER trg_facet_asset_delete AFTER DELETE ON asset
BEGIN
    UPDATE facet_counts SET cnt = cnt - 1
    WHERE volume_id = (SELECT volume_id FROM root WHERE id = OLD.root_id)
      AND model = COALESCE(OLD.model, '') AND rating = COALESCE(OLD.rating, -128);
    DELETE FROM facet_counts
    WHERE cnt <= 0
      AND volume_id = (SELECT volume_id FROM root WHERE id = OLD.root_id)
      AND model = COALESCE(OLD.model, '') AND rating = COALESCE(OLD.rating, -128);
END;

-- Also fires on `root_id` changing (a relink across volumes, ADR-0071's relink tiers), not just
-- model/rating, so a relinked asset's count moves to its new volume's bucket even when its
-- model/rating didn't change.
CREATE TRIGGER trg_facet_asset_update AFTER UPDATE OF model, rating, root_id ON asset
WHEN OLD.model IS NOT NEW.model OR OLD.rating IS NOT NEW.rating OR OLD.root_id IS NOT NEW.root_id
BEGIN
    UPDATE facet_counts SET cnt = cnt - 1
    WHERE volume_id = (SELECT volume_id FROM root WHERE id = OLD.root_id)
      AND model = COALESCE(OLD.model, '') AND rating = COALESCE(OLD.rating, -128);
    DELETE FROM facet_counts
    WHERE cnt <= 0
      AND volume_id = (SELECT volume_id FROM root WHERE id = OLD.root_id)
      AND model = COALESCE(OLD.model, '') AND rating = COALESCE(OLD.rating, -128);

    INSERT INTO facet_counts (volume_id, model, rating, cnt)
    VALUES (
        (SELECT volume_id FROM root WHERE id = NEW.root_id),
        COALESCE(NEW.model, ''),
        COALESCE(NEW.rating, -128),
        1
    )
    ON CONFLICT(volume_id, model, rating) DO UPDATE SET cnt = cnt + 1;
END;
"#;

/// #23 (v4): hierarchical keywords. `keyword.path` is an id-based materialized path (see
/// `model::Keyword`'s doc comment) -- computed and maintained in Rust
/// (`SqliteCatalog::create_keyword`/`move_keyword`), not by a trigger, since a rename/move needs
/// to rewrite every descendant's `path` in one pass, which is far more naturally expressed as a
/// recursive Rust walk than nested trigger logic.
///
/// `UNIQUE(parent_id, name_fold)` alone can't enforce "no two sibling keywords share a name" for
/// *top-level* keywords: SQLite treats every `NULL` in a `UNIQUE` index as distinct from every
/// other `NULL`, so a plain table-level constraint would silently let two root keywords both be
/// named, say, "Events". A regular unique index handles every non-root case; a second, partial
/// unique index (`WHERE parent_id IS NULL`) closes the root-level gap.
const MIGRATION_V4: &str = r#"
CREATE TABLE keyword (
    id          INTEGER PRIMARY KEY,
    parent_id   INTEGER REFERENCES keyword(id),
    name        TEXT NOT NULL,
    name_fold   TEXT NOT NULL,
    path        TEXT NOT NULL
);
CREATE UNIQUE INDEX idx_keyword_unique_child ON keyword(parent_id, name_fold);
CREATE UNIQUE INDEX idx_keyword_unique_root_child ON keyword(name_fold) WHERE parent_id IS NULL;
CREATE INDEX idx_keyword_path ON keyword(path);

CREATE TABLE asset_keyword (
    keyword_id  INTEGER NOT NULL REFERENCES keyword(id),
    asset_id    INTEGER NOT NULL REFERENCES asset(id) ON DELETE CASCADE,
    PRIMARY KEY (keyword_id, asset_id)
);
CREATE INDEX idx_asset_keyword_asset ON asset_keyword(asset_id);
"#;

/// #23 (v5): collections. `manual` and `smart` share one table (`kind` discriminates); a manual
/// collection's membership lives in `collection_asset` (with a `REAL position` for user-ordering
/// -- fractional so inserting between two existing rows never needs to renumber the rest), a
/// smart collection's membership is never stored -- `rule_json` holds a serialized, versioned
/// `hunt::Filter` (`SmartRule` in `sqlite.rs`), and resolving one means calling `hunt` with it.
/// Same NULL-parent uniqueness gap as `keyword` (see `MIGRATION_V4`'s doc comment), same fix.
const MIGRATION_V5: &str = r#"
CREATE TABLE collection (
    id          INTEGER PRIMARY KEY,
    parent_id   INTEGER REFERENCES collection(id),
    kind        TEXT NOT NULL CHECK (kind IN ('manual', 'smart')),
    name        TEXT NOT NULL,
    name_fold   TEXT NOT NULL,
    rule_json   TEXT
);
CREATE UNIQUE INDEX idx_collection_unique_child ON collection(parent_id, name_fold);
CREATE UNIQUE INDEX idx_collection_unique_root_child ON collection(name_fold) WHERE parent_id IS NULL;

CREATE TABLE collection_asset (
    collection_id   INTEGER NOT NULL REFERENCES collection(id),
    asset_id        INTEGER NOT NULL REFERENCES asset(id) ON DELETE CASCADE,
    position        REAL NOT NULL,
    PRIMARY KEY (collection_id, asset_id)
);
CREATE INDEX idx_collection_asset_position ON collection_asset(collection_id, position);
"#;

/// #26 (v6): verified folder move. `asset.content_hash` is the full-file BLAKE3 (hex) a move
/// records for each cataloged asset it carried (`fingerprint` is only a partial hash, see
/// `scruff::partial_hash`); NULL for assets never moved. `root_move` is the crash-recovery
/// journal (ADR-0026): one row per in-flight move, `copying` until the catalog commit,
/// `committed` until the source deletion finishes. At most one open move per root.
const MIGRATION_V6: &str = r#"
ALTER TABLE asset ADD COLUMN content_hash TEXT;

CREATE TABLE root_move (
    id          INTEGER PRIMARY KEY,
    root_id     INTEGER NOT NULL UNIQUE REFERENCES root(id),
    src_path    TEXT NOT NULL,
    dest_path   TEXT NOT NULL,
    state       TEXT NOT NULL CHECK (state IN ('copying', 'committed')),
    started_at  INTEGER NOT NULL
);
"#;

/// Ordered migrations, one `user_version` step each. Add new migrations by appending to this
/// slice — never edit an already-shipped entry in place, the same rule every other versioned
/// schema in this codebase (den's candidate schemas, homing's) follows implicitly by never having
/// shipped a v1 to begin with.
const MIGRATIONS: &[&str] = &[
    MIGRATION_V1,
    MIGRATION_V2,
    MIGRATION_V3,
    MIGRATION_V4,
    MIGRATION_V5,
    MIGRATION_V6,
];

/// Runs every migration past the database's current `PRAGMA user_version`, in order. Safe to call
/// on every open: a database already at the latest version runs nothing.
///
/// Each migration's DDL and its `user_version` bump run inside one transaction, committed
/// together — found by CodeRabbit's review: without this, a migration that failed partway through
/// (e.g. process killed mid-`execute_batch`) could leave its tables created but `user_version`
/// still at the old value, so the next open would retry the same (non-idempotent, plain
/// `CREATE TABLE`) DDL against a database that already has some of those tables, failing outright
/// with no way to recover short of deleting the catalog file.
///
/// `PRAGMA foreign_keys` is toggled off for the duration of each migration (it can't be changed
/// inside an active transaction, so this happens outside the migration's own `tx.transaction()`)
/// — required for a table-rebuild migration like V3 (see its doc comment), harmless for a plain
/// additive one. `PRAGMA foreign_key_check` runs *inside* that transaction, before `commit()` --
/// matching SQLite's own documented table-rebuild recipe's ordering (check, then commit, then
/// re-enable enforcement) rather than the reverse -- so a migration that left a dangling
/// reference rolls back (via the transaction's `Drop`, since it's never committed) instead of
/// permanently recording a broken schema under a `user_version` that `migrate` would then treat
/// as already-done on every future open. Enforcement is restored (`ON`) on every exit path from
/// [`run_one_migration`] -- including a failed `execute_batch` or a caught violation, not just
/// the success path -- via `result` being captured before the restore rather than an early
/// `return` inside the branch, per CodeRabbit's review: a caller that catches this error and
/// keeps using the same `Connection` (this crate's own tests do exactly that) must never observe
/// enforcement left permanently disabled.
pub fn migrate(conn: &mut Connection) -> Result<(), CatalogError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version > current {
            conn.pragma_update(None, "foreign_keys", "OFF")?;
            let result = run_one_migration(conn, sql, version);
            conn.pragma_update(None, "foreign_keys", "ON")?;
            result?;
        }
    }
    Ok(())
}

fn run_one_migration(conn: &mut Connection, sql: &str, version: i64) -> Result<(), CatalogError> {
    let tx = conn.transaction()?;
    tx.execute_batch(sql)?;
    // `PRAGMA user_version = N` doesn't accept a bound parameter, only a literal. The pragma
    // write participates in the same transaction as any other write here, so a rollback undoes
    // it along with the DDL.
    tx.execute_batch(&format!("PRAGMA user_version = {version}"))?;

    let violations = {
        let mut check = tx.prepare("PRAGMA foreign_key_check")?;
        let count = check.query_map([], |_row| Ok(()))?.count();
        count
    };
    if violations > 0 {
        // `tx` is dropped here without `commit()`, rolling back the whole migration.
        return Err(CatalogError::Io(format!(
            "migration {version} left {violations} foreign key violation(s); rolled back"
        )));
    }

    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_is_idempotent_against_an_already_migrated_database() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        migrate(&mut conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    /// A catalog created under V1 alone (before #24), then migrated forward, must end up with
    /// every pre-existing asset row reading `missing_since = NULL` (present) rather than the
    /// column simply not existing or defaulting to something that would misreport an
    /// already-cataloged file as missing the moment #24's sync runs against it. Also proves V3's
    /// rebuild (#23) doesn't drop the column V2 (#24) added.
    #[test]
    fn upgrading_from_v1_gives_existing_assets_a_null_missing_since() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();

        conn.execute(
            "INSERT INTO volume (identity_key, online, last_seen_at) VALUES ('v', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO root (volume_id, rel_path) VALUES (1, 'x')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO asset (root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                imported_at) VALUES (1, 'a.nef', 'a.nef', 1, 1, 1)",
            [],
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        let missing_since: Option<i64> = conn
            .query_row("SELECT missing_since FROM asset WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(missing_since, None);
    }

    /// Simulates a pre-#23 database already at V2 (#24's `missing_since` column present, rebuilt
    /// by hand here since the public `migrate` always runs to the latest version), then migrates
    /// it forward and checks V3's rebuild preserved rows (including `missing_since`), converted
    /// only the old NOT-NULL-default's `0` to unrated (not any other value a dev catalog happened
    /// to already have, per CodeRabbit's review), and rebuilt `facet_counts` at its new grain.
    #[test]
    fn migration_v3_converts_only_zero_ratings_to_unrated_and_preserves_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATION_V1).unwrap();
        conn.execute_batch(MIGRATION_V2).unwrap();
        conn.execute_batch("PRAGMA user_version = 2").unwrap();

        conn.execute(
            "INSERT INTO volume (id, identity_key, online, last_seen_at) VALUES (1, 'v', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO root (id, volume_id, rel_path) VALUES (1, 1, '')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset (id, root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                model, rating, imported_at, missing_since) \
             VALUES (1, 1, 'a.NEF', 'a.nef', 100, 0, 'Z8', 0, 0, 12345)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO asset (id, root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                model, rating, imported_at) \
             VALUES (2, 1, 'b.NEF', 'b.nef', 100, 0, 'Z8', 3, 0)",
            [],
        )
        .unwrap();

        migrate(&mut conn).unwrap();

        let rating: Option<i64> = conn
            .query_row("SELECT rating FROM asset WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            rating, None,
            "an existing rating=0 row becomes unrated on migration"
        );

        let rel_path: String = conn
            .query_row("SELECT rel_path FROM asset WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rel_path, "a.NEF", "the row itself must survive the rebuild");

        let missing_since: Option<i64> = conn
            .query_row("SELECT missing_since FROM asset WHERE id = 1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            missing_since,
            Some(12345),
            "V2's missing_since column must survive V3's rebuild unchanged"
        );

        let rating_2: Option<i64> = conn
            .query_row("SELECT rating FROM asset WHERE id = 2", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            rating_2,
            Some(3),
            "a genuinely non-zero rating must survive the migration, not be collapsed to NULL too"
        );

        let facet_cnt_unrated: i64 = conn
            .query_row(
                "SELECT cnt FROM facet_counts \
                 WHERE volume_id = 1 AND model = 'Z8' AND rating = -128",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            facet_cnt_unrated, 1,
            "facet_counts is rebuilt at its new grain"
        );

        let facet_cnt_three: i64 = conn
            .query_row(
                "SELECT cnt FROM facet_counts WHERE volume_id = 1 AND model = 'Z8' AND rating = 3",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(facet_cnt_three, 1);
    }

    #[test]
    fn rating_check_constraint_rejects_out_of_range_values() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO volume (identity_key, online, last_seen_at) VALUES ('v', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO root (volume_id, rel_path) VALUES (1, '')", [])
            .unwrap();

        let result = conn.execute(
            "INSERT INTO asset (root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                rating, imported_at) VALUES (1, 'a', 'a', 1, 0, 6, 0)",
            [],
        );
        assert!(result.is_err(), "rating outside -1..=5 must be rejected");
    }

    #[test]
    fn flag_check_constraint_rejects_values_other_than_null_or_one() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO volume (identity_key, online, last_seen_at) VALUES ('v', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO root (volume_id, rel_path) VALUES (1, '')", [])
            .unwrap();

        let result = conn.execute(
            "INSERT INTO asset (root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                flag, imported_at) VALUES (1, 'a', 'a', 1, 0, 2, 0)",
            [],
        );
        assert!(
            result.is_err(),
            "flag values other than NULL/1 must be rejected"
        );
    }

    #[test]
    fn foreign_keys_into_asset_still_work_after_the_v3_rebuild() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        conn.execute(
            "INSERT INTO volume (identity_key, online, last_seen_at) VALUES ('v', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO root (volume_id, rel_path) VALUES (1, '')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO asset (root_id, rel_path, rel_path_fold, size_bytes, mtime_unix, \
                imported_at) VALUES (1, 'a', 'a', 1, 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO preview (asset_id, tier, bytes) VALUES (1, 't0', X'00')",
            [],
        )
        .unwrap();

        let result = conn.execute("DELETE FROM asset WHERE id = 1", []);
        assert!(
            result.is_err(),
            "preview's FK into the rebuilt asset table must still be live, not dangling"
        );
    }
}
