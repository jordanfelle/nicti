//! #22's real catalog schema, promoted from the spike prototypes ADR-0020 (`spikes/homing`) and
//! ADR-0008/0011 (`spikes/den`) settled the shape of:
//!
//! - `volume`/`root`/`asset`: three levels (ADR-0020) so a registered folder can move between
//!   drives (#72) as a single `root` row update, without touching every `asset` row underneath.
//! - `preview`: the T0 grid-preview cache (ADR-0017), keyed by `(asset_id, tier)`.
//! - `edit_variant`/`edit_history`: the physical shape ADR-0002 left for this ticket — one master
//!   variant per asset (enforced by a partial unique index), each variant's edit document plus an
//!   append-only history log.
//! - `facet_counts`: the trigger-maintained `(model, rating)` aggregate ADR-0011 adopted, kept
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
    -- Secondary identity signal (ADR-0020's `.nicti-volume` marker file, read-only-volume/clone
    -- caveats and all) -- not the primary key, but a real disagreement with an already-registered
    -- volume under the same `identity_key` is refused rather than silently merged. This closes
    -- part, not all, of ADR-0020's flagged "two volumes, same identity -> never auto-merge" gap;
    -- a genuine identity_key *collision* between two physically distinct volumes that also agree
    -- on this marker is still unresolved, same as ADR-0020 itself leaves it (Proposed, not
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
-- ADR-0008's composite facet index: (model, rating) range/equality scans.
CREATE INDEX idx_asset_model_rating ON asset(model, rating);

-- T0 grid preview only, for now (ADR-0017): the Nikon PreviewIFD JPEG, copied verbatim at import
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

-- One row per edit variant (the master, plus any future virtual copy — ADR-0002's "virtual copies
-- = multiple edit rows"). `document` is the serialized `EditDocument` (ADR-0002's fixed-order
-- stage-parameter map) as JSON text.
CREATE TABLE edit_variant (
    id          INTEGER PRIMARY KEY,
    asset_id    INTEGER NOT NULL REFERENCES asset(id),
    name        TEXT NOT NULL,
    is_master   INTEGER NOT NULL DEFAULT 0,
    document    TEXT NOT NULL,
    UNIQUE(asset_id, name)
);
-- Enforces "one flagged master" per asset (ADR-0002's `asset 1—N edit_variant` shape) without a
-- separate lookup table: a partial unique index over rows where is_master=1.
CREATE UNIQUE INDEX idx_edit_variant_one_master ON edit_variant(asset_id) WHERE is_master = 1;

-- Append-only delta log (ADR-0002): `delta` is one compacted history entry's JSON. Burst
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

-- ADR-0011's trigger-maintained facet-count aggregate, scoped to (model, rating) — the grain
-- ADR-0008's own composite index already covers. A keyword facet dimension isn't part of this
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

/// Ordered migrations, one `user_version` step each. Add new migrations by appending to this
/// slice — never edit an already-shipped entry in place, the same rule every other versioned
/// schema in this codebase (den's candidate schemas, homing's) follows implicitly by never having
/// shipped a v1 to begin with.
const MIGRATIONS: &[&str] = &[MIGRATION_V1];

/// Runs every migration past the database's current `PRAGMA user_version`, in order. Safe to call
/// on every open: a database already at the latest version runs nothing.
///
/// Each migration's DDL and its `user_version` bump run inside one transaction, committed
/// together — found by CodeRabbit's review: without this, a migration that failed partway through
/// (e.g. process killed mid-`execute_batch`) could leave its tables created but `user_version`
/// still at the old value, so the next open would retry the same (non-idempotent, plain
/// `CREATE TABLE`) DDL against a database that already has some of those tables, failing outright
/// with no way to recover short of deleting the catalog file.
pub fn migrate(conn: &mut Connection) -> Result<(), CatalogError> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = (i + 1) as i64;
        if version > current {
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            // `PRAGMA user_version = N` doesn't accept a bound parameter, only a literal. The
            // pragma write participates in the same transaction as any other write here, so a
            // rollback undoes it along with the DDL.
            tx.execute_batch(&format!("PRAGMA user_version = {version}"))?;
            tx.commit()?;
        }
    }
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
}
