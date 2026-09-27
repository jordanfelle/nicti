# ADR-0023: Keywords, collections, and filter/search backend

- **Status:** Proposed
- **Date:** 2026-09-27
- **Ticket:** #23 Build: keywords, collections, search & filter bar (catalog backend)

## Context

Issue #23 is the catalog-layer half of hierarchical keywords, collections/smart collections, and
filter/search over the 600k+ (2M target) library — the UI widget half was split out to #242 once
it became clear no UI crate exists yet (#241). ADR-0067/ADR-0103 already settled the database
engine (SQLite) and a `(model, rating)` facet-count cache, but left keyword/collection storage,
the filter-query API, and filename search entirely to "whichever ticket adds keyword tagging."

This ADR is written incrementally as each of #23's four PRs lands, rather than all at once, since
each PR's design depends on the one below it.

## Decision

### PR 1 — culling state + facet grain fix

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
  renamed (soon-to-be-dropped) table. `migrate()` now toggles `PRAGMA foreign_keys` off for a
  migration's duration and runs `PRAGMA foreign_key_check` immediately after, since SQLite never
  validates FK targets at `DROP`/`CREATE TABLE` time itself.
- No real catalog has shipped yet (pre-v1), so collapsing every existing `rating = 0` to `NULL` on
  migration is a deliberate one-time correction, not a guess at real user data.

## Consequences

- `CatalogStore` gains `set_rating`/`set_flag`/`set_label`, each taking `&[i64]` so a culling
  `rate_burst` across a multi-selection is one `UPDATE ... WHERE id IN (...)` statement (one
  commit), not one round-trip per asset.
- `facet_count`'s signature changes to `rating: Option<i64>` (`None` reads the unrated bucket).
  This is a breaking change to an unreleased pre-v1 API — no external callers exist yet.
- Deferred to later PRs in this same ticket: keyword storage (PR 2), the filter/query API and
  filename search (PR 3), and collections/smart collections (PR 4).
