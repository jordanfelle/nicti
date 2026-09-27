# ADR-0023: Keywords, collections, and filter/search backend

- **Status:** Accepted
- **Date:** 2026-09-27
- **Ticket:** #23 Build: keywords, collections, search & filter bar (catalog backend)

## Context

Issue #23 is the catalog-layer half of hierarchical keywords, collections/smart collections, and
filter/search over the 600k+ (2M target) library — the UI widget half was split out to #242 once
it became clear no UI crate existed yet at the time (#241, since landed as `nicti-pelt`, #248).
ADR-0067/ADR-0103 already settled the database engine (SQLite) and a `(model, rating)`
facet-count cache, but left keyword/collection storage, the filter-query API, and filename search
entirely to "whichever ticket adds keyword tagging."

This landed as one PR against `crates/nicti-lair` rather than the originally-planned 4-PR stack
(schema fix, keywords, filter/query, collections) — the stack's per-PR CI/CodeRabbit/rebase
overhead outweighed the benefit for changes this tightly coupled (each layer depends on the one
below it, so nothing could actually merge independently anyway).

## Decision

### Culling-state schema fix + facet grain

- `asset.rating` becomes nullable: `NULL` = unrated, `-1` = reject, `0..=5` = star rating,
  matching XMP's `xmp:Rating`/ADR-0059's convention. The prior `NOT NULL DEFAULT 0` schema
  couldn't distinguish "never rated" from "rated 0 stars" — real impact, per `lrc-migration`'s own
  reference data: 277,137 of 380,300 real LRC assets are unrated.
- Adds `flag` (`NULL`/`1`, pick) and `label` (free text) columns.
- `facet_counts` moves from a `(model, rating)` grain to `(volume_id, model, rating)`. An offline
  volume's assets are excluded by joining to `volume.online = 1` at query time
  (`SqliteCatalog::facet_count`) rather than a trigger recomputing every row on every online/
  offline flip — ADR-0071 flagged this exclusion as unresolved and handed it to #22/#23.
  `rating`'s `NULL` case is represented in this table by a sentinel (`-128`,
  `FACET_UNRATED_SENTINEL`) since a `PRIMARY KEY` column can't itself be `NULL`.
- SQLite can't relax a `NOT NULL` constraint or add a table in place, so `asset` and
  `facet_counts` are rebuilt using SQLite's documented table-rebuild recipe (new table under a
  scratch name, copy data, drop the old table, rename into place) — not `ALTER TABLE ... RENAME`,
  which would leave `preview`/`edit_variant`'s `FOREIGN KEY REFERENCES asset(id)` pointing at the
  renamed (soon-to-be-dropped) table. `migrate()` toggles `PRAGMA foreign_keys` off for a
  migration's duration and runs `PRAGMA foreign_key_check` *before* `commit()`, since SQLite never
  validates FK targets at `DROP`/`CREATE TABLE` time itself and a check after commit couldn't roll
  a violation back. Landed as `MIGRATION_V3` — `MIGRATION_V2` was independently claimed by the
  concurrently-merged #24 (Patrol, `asset.missing_since`); `MIGRATION_V3` carries that column
  through the rebuild unchanged.
- No real catalog has shipped yet (pre-v1), so collapsing only the old default's `rating = 0` to
  `NULL` (preserving any other value already present) is a deliberate, narrow one-time correction.
- `CatalogStore` gains `set_rating`/`set_flag`/`set_label`, each taking `&[i64]`, chunked to stay
  under `SQLITE_MAX_VARIABLE_NUMBER` and committed atomically as one transaction across chunks —
  so a culling `rate_burst` across a large multi-selection is still one commit.
- `facet_count`'s signature changes to `rating: Option<i64>` (`None` reads the unrated bucket).
  Breaking change to an unreleased pre-v1 API — no external callers exist.

### Hierarchical keywords (`MIGRATION_V4`)

- `keyword.path` is an **id-based** materialized path (`/1/5/12/`, one `/`-terminated segment per
  ancestor id). Ids never change on rename, so a rename touches only that one row; a move
  (`move_keyword`) rewrites the moved keyword's and every descendant's `path` by GLOB-matching the
  old path as a prefix and substituting the new one — computed and applied in Rust, not a trigger,
  since a multi-row rewrite is far more naturally a Rust walk than nested trigger logic.
- `UNIQUE(parent_id, name_fold)` alone can't stop two *top-level* keywords sharing a name: SQLite
  treats every `NULL` in a unique index as distinct from every other `NULL`. A second, partial
  unique index (`WHERE parent_id IS NULL`) closes that gap — the same pattern `collection` (below)
  reuses.
- `asset_keyword(keyword_id, asset_id)` is the tag link; `tag`/`untag` accept `&[i64]` batches.
- `keyword_by_path` takes a caller-split `&[&str]`, deliberately not committing to one separator —
  XMP's `lr:hierarchicalSubject` uses `|`, LRC's `AgLibraryKeyword.genealogy` uses `/`-joined
  ancestor ids, this repo's own deleted `spikes/den` prototype used `.`; each caller splits its own
  format before calling this method.

### Filter/query engine (`hunt.rs`)

- `Filter` (every field optional and AND-combined), `Sort` (one of four fields × direction), and
  keyset-paginated `hunt` (`Page::after` echoes the last row's own sort-key + id back in — never
  an `OFFSET`, which is O(n) at the 2M-asset scale ADR-0067 measured against). One `Cursor` variant
  per `Sort` field, since each field's key type (and NULL-sentinel) differs.
- `facets`: an unfiltered query reads the trigger-maintained `(volume_id, model, rating)` cache
  (ADR-0103's own optimized case); any narrowing filter (keyword, date, flag, etc.) falls back to
  a live, exact `GROUP BY` over the narrowed set, since the cache's fixed grain has no way to
  answer a keyword- or date-narrowed facet count on its own. `flag` has no cache at all (much
  lower cardinality than model/rating) — always a live `GROUP BY`.
- Filename search (`Filter::filename_contains`/`rel_path_prefix`) is a plain `GLOB '*text*'`
  scan — ADR-0067's flagged, never-closed gap. An FTS5 index (already compiled into the bundled
  `rusqlite`) is the documented follow-up once real-scale filename-search latency is measured
  against this; not attempted here to keep this PR's scope to correctness, not a perf rewrite.

### Collections (`clowder.rs`, `MIGRATION_V5`)

- `manual` and `smart` share one `collection` table (`kind` discriminates) and one nesting tree
  (organizational only — a collection can sit under another regardless of kind). Same NULL-parent
  uniqueness gap/fix as `keyword`.
- A `manual` collection's membership lives in `collection_asset`, with a `REAL position` so
  inserting between two existing rows never needs to renumber the rest.
- A `smart` collection's membership is never stored: `rule_json` holds a versioned, serialized
  `Filter` (`SmartRule { v: 1, filter }`) — resolving one means calling `hunt` with it.
  `set_smart_rule`/`collection_filter` round-trip this; #155's LRC-smart-collection-rule mapping
  (still unverified — no smart collections existed in the measured 380k-row real catalog) targets
  this same `Filter` shape as its output, once a real example exists to design against.

## Consequences

- `crates/nicti-lair`'s `CatalogStore` trait roughly doubles in surface area (see `lib.rs`); every
  new method is exercised by a real `SqliteCatalog` test (schema.rs/sqlite.rs), not just checked
  for compilation.
- Deferred, tracked separately, not blocking this PR: FTS5 filename search (see above), and #155's
  smart-collection LRC-rule mapping (blocked on a real example, not on this schema).
- #242 (filter bar UI) can now be built against a real `hunt`/`facets` API instead of a stub.
