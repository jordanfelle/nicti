# ADR-0024: Manual catalog sync, not a live filesystem watcher

- **Status:** Accepted
- **Date:** 2026-09-27
- **Ticket:** [#24](https://github.com/jordanfelle/nicti/issues/24) Build: manual catalog sync
  (LRC-style) + missing-folder detection

## Context

#24 originally scoped a background filesystem watcher (local NVMe + HDD) reconciling moves,
renames, and deletes into the catalog without a manual "synchronize folder" step — the opposite of
how Lightroom Classic works today. #37's spike (`spikes/retina`'s `watch` subcommand) already ran a
real `notify` 8.2 / `ReadDirectoryChangesW` test toward that design and found several open items
before it could be built on (#139): the 16KB `ReadDirectoryChangesW` rescan/buffer-overflow
threshold was never actually triggered at the burst sizes tested; `notify` 8.x has no matching
stable `notify-debouncer-full` release (0.7.0 only pairs with `notify` 7.x); and a 46-file burst
produced ~82 events per file, almost all `Modify`, with only 11 explicit `Create` events for 46 new
files — an unexplained gap that would have undermined using `Create` as an ingest trigger signal.

Separately, the project's own product direction is to match Lightroom Classic's existing workflow
(a user-triggered "Synchronize Folder" action) rather than always-on background watching — a UX
preference, not a technical limitation. #37's findings above are real evidence that the live-watch
path also carried non-trivial unresolved engineering cost, but the decision below is driven by the
product call, not by those findings alone.

`crates/nicti-lair` (#22) already has a working ingest pipeline (`scruff::ingest_root`) that scans
a folder, fingerprints and upserts each file, and relinks an in-root move by partial-BLAKE3 match —
but it only ever walks the disk, never the catalog, so a file that disappears from disk is never
detected.

## Decision

**#24 becomes a manual, user-triggered sync** (`patrol::sync_root`, "Patrol" per the feline naming
convention), not a live watcher. It wraps `scruff::ingest_root`'s existing disk-side pass with a
second, catalog-side pass:

- **Missing files, not deletion by default.** An asset whose file has disappeared from an otherwise
  reachable root is flagged (`asset.missing_since`, a new nullable column) and its row is kept —
  ratings, edits, and history all survive an accidental move or delete made outside Nicti. Removing
  a still-missing asset's row is opt-in per sync run (`SyncOptions::remove_missing`, off by
  default), matching LRC's own "Remove missing photos from catalog" checkbox rather than deleting
  automatically.
- **Missing folders are reported, not specially rendered.** A directory whose every cataloged asset
  is missing is included in the sync report (`SyncReport::missing_folders`) as a plain `rel_path`
  prefix — there is no separate folder table (ADR-0061 already treats folders as `rel_path`
  prefixes), and no UI treatment is specified by this ADR (that's #241's app-shell scope).
- **An unreachable root touches nothing.** If `root_path` itself doesn't resolve to a directory
  (an unplugged drive, a changed drive letter, a deleted folder), sync returns immediately with
  `root_unreachable: true` and flags or removes no rows at all. This is deliberate: an entire root
  going missing is ADR-0071's offline-volume/remap territory, not a case #24 should ever interpret
  as "every file in this root is individually gone."
- **Offline-volume handling is unchanged, and is out of scope here.** ADR-0071's "never delete,
  flip `volume.online` only, collapse to one 'not connected' node" design stands; this ADR does not
  add a greyed-out folder tree, and volume mount detection (`spikes/homing`, still Windows-only and
  unverified) is not promoted as part of this work.

### Alternatives considered and rejected

- **Continuous background watcher** (the original #24 scope) — rejected on product preference
  (match LRC's existing manual-sync workflow), reinforced by #37/#139's still-open engineering
  questions (rescan/overflow threshold untested at realistic burst sizes, no stable
  `notify`/`notify-debouncer-full` version pairing, and an unexplained Create-vs-Modify event-count
  mismatch that would need resolving before `Create` could be trusted as an ingest trigger).
- **Always delete a missing asset's row** — rejected: a temporarily unmounted subpath, a rename
  performed outside Nicti mid-shoot, or a slow network drive would permanently destroy edits and
  ratings for a file that was never actually gone. Opt-in removal, LRC's own model, avoids this.
- **LRC-style greyed-out missing-folder tree for offline volumes** — rejected; ADR-0071 already
  made this call for the volume-level offline case and this ADR doesn't reopen it. #24's own
  "missing folder" concept only applies to a folder on a volume that's still connected.

## Consequences

- **#139** (the `notify`/`notify-debouncer-full` groundwork toward a live watcher) is closed as
  superseded — its findings remain valid research evidence (cited above) but no longer block any
  open ticket.
- **`spikes/retina`'s `watch` subcommand and its `notify` dependency are left in place** — they're
  point-in-time research evidence for ADR-0037, not something this ADR asks to delete; removing
  them is unrelated cleanup, not part of this ticket.
- **The UI trigger for "Synchronize Folder"** (a button, and any "?" badge for a missing asset) is
  deferred to whichever ticket builds it against a real UI crate (#241 and its follow-ups) — this
  ADR and #24 only add the `nicti-lair` API the UI will call.
- **Running sync as a background job** (Pounce, ADR-0054) is deferred, the same follow-up ingest
  itself already has — `sync_root` runs synchronously, same as `ingest_root`.
- **Cross-root relink** (a file that moved to a different registered root, not just a different
  path within the same one) stays out of scope, same limitation `scruff::ingest_root` already has.
