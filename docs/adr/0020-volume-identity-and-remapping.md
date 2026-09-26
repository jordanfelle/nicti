# ADR-0020: Volume identity and drive remapping

- **Status:** Proposed — static findings (identity-field design, schema shape, fingerprint-tier
  design) are final; the survival-table and throughput/latency measurements are pending a
  reference-machine run (see Measured results)
- **Date:** 2026-09-26
- **Ticket:** [#71](https://github.com/jordanfelle/nicti/issues/71) Research: dynamic drive
  remapping (Volume UUID + relative path)

## Context

Nicti's catalog must keep every asset's link intact when a drive letter changes, a volume is
detached and reattached, or a volume is reformatted — this covers both the internal archive HDD
and the ~21.8TB removable external archive drive (confirmed in scope for both per #4/E0's PRD
sign-off). #71 explicitly rejects Lightroom Classic's own pattern (persistent offline preview
thumbnails plus a permanently grayed-out folder tree when a volume is disconnected) in favor of
pure remap-on-reconnect: the catalog re-resolves paths against whatever volume identity shows up,
with no offline-preview purgatory.

#71 was merged in from an archived duplicate (#21, file identity & relinking), which additionally
asked for a content-hash-based identity option for the case where a file moves to an entirely
unrecognized volume (e.g. a fresh external drive) — not just the archive-drive-reconnect case.

Constraints already fixed by earlier ADRs/docs:

- **ADR-0002** already names this ticket as the owner of `asset` (file identity): "asset (file
  identity, #71) 1—N edit_variant," and defers the physical schema to #22 once #67 picked the DB
  engine.
- **ADR-0008** picked SQLite (`rusqlite`, WAL) as the catalog engine, and its Consequences section
  already flags "#71 (drive remapping) and any future full-text/filename-search work should treat
  leading substring search as a known, unindexable-in-any-of-these-three-engines gap" — this ADR's
  schema design works within that constraint (prefix/exact lookups via an index, no substring
  search promised).
- **ADR-0011**'s facet-count cache is a real consequence of this ADR's offline semantics (below):
  an offline volume's assets must not appear in facet counts, so the cache needs either
  per-volume partitioning or a subtraction step — flagged here for #22/#23, not solved by this ADR.
- **#136** retired the frozen `ref-10k` reference set's per-machine full-copy model — this ADR's
  fingerprint-cost measurements use a stratified sample walked directly from the live library
  instead (the same approach #37's `retina scan` already validated), not a `ref-10k` copy.
- v1 targets Windows only (#4/E0's PRD sign-off); non-Windows volume identity is deferred to #73.
- **Sandbox note**, matching ADR-0006/0007's own precedent: this research pass ran in a Linux/WSL
  sandbox with no Windows toolchain, no mountable NTFS volumes, and no way to attach/detach a real
  or virtual drive. The `spikes/homing` crate's Windows-only code (`volume::windows_impl`,
  `mount_events::windows_impl`) is therefore **written but unverified** — it type-checks against
  `windows-sys`' documented API shapes but has never been compiled or run. Every schema/fingerprint
  finding below (the parts of the spike that don't need Windows — `schema.rs`, `fingerprint.rs`,
  `relink.rs`, `path.rs`) is real, measured against this sandbox's Rust toolchain: 23 unit tests
  pass, `cargo clippy -p homing --all-targets --all-features -- -D warnings` is clean, and the
  crate participates in the workspace's normal (non-path-gated) `clippy`/`test` jobs like `sniff`
  does. The volume-identity survival table and the Windows throughput/latency numbers are
  placeholders pending the reference-machine pass — see Measured results below.

## Decision rule (stated before measuring)

- **Identity key**: must survive a drive-letter change and a detach/reattach cycle. Whether it
  survives a reformat is recorded but not required — a reformat is a deliberate user action, not
  the a spontaneous case this ADR must transparently paper over.
- **Ambiguity**: two mounted volumes reporting the same identity (a disk clone, or a VHDX file
  copied and both copies attached) must never silently resolve to one asset set winning — it must
  be flagged for manual review.
- **Mount-change detection**: prefer a push notification over polling if one exists with
  acceptable latency and near-zero idle CPU; polling is the fallback, not the default.
- **Relink fingerprint cost**: must be affordable to run at import time for the cheap tiers, with
  the expensive tier (full-file hash) reserved for on-demand relink, not routine import.

## Decision

**Identity key: NTFS 64-bit volume serial + GPT partition GUID, when both are present; MBR
signature + partition offset + 32-bit volume serial as the non-GPT fallback.** Neither the mount
manager's `\\?\Volume{GUID}` path (assigned per-machine, not guaranteed stable across a reformat)
nor `sysinfo`'s own disk listing (mount point, name, removable flag — no stable cross-mount
identity field at all) qualifies on its own; both are recorded for completeness in
`VolumeInfo` (`spikes/homing/src/volume.rs`) but neither is the identity key.

A `.nicti-volume` marker file (a UUID written to the volume root) was considered as a portable
tie-breaker but **not adopted as the primary key** — a read-only volume can't hold one, and a
disk clone copies it verbatim, so it can't disambiguate the exact ambiguity case this ADR must
catch. `volume::windows_impl::ensure_marker` exists in the spike as an opportunistic secondary
signal only (still pending real Windows validation).

**Two mounted volumes with the same identity key are never auto-merged.** `schema::upsert_volume`
is keyed on `identity_key` via `ON CONFLICT DO UPDATE`, so two volumes reporting the same key
today collapse to one row by construction — this ADR flags that as the sharp edge a real
implementation must add an explicit ambiguity check in front of, before the catalog decides one
mounted copy silently wins. Tracked as a follow-up (see Consequences).

**Mount-change detection**: the spike implements a `sysinfo`-based polling backend
(`mount_events::windows_impl::poll_for`) at a configurable interval, and stubs
`CM_Register_Notification`-based push detection (`watch_push`) as an explicitly unimplemented,
documented follow-up rather than a silent gap — see Measured results for why this wasn't finished
in this pass.

**Schema**: three levels, not a flat `asset(absolute_path)` table — `volume` (identity, mount
state, online flag), `root` (a registered folder scoped to one volume — this is what #72's
SSD→archive transition updates, a single row, not a per-asset rewrite), and `asset` (rel_path,
case-folded lookup column, size/mtime, fingerprint, natural key). See
`spikes/homing/src/schema.rs` for the full DDL and `resolve()`'s path-reconstruction logic.

**Path normalization**: UTF-8, forward slashes (never backslash), NFC-composed, plus a
case-folded `rel_path_fold` column for NTFS/exFAT's case-insensitive-but-preserving semantics — no
260-character `MAX_PATH` assumption. See `spikes/homing/src/path.rs`.

**Relink fingerprint tiers**, cheapest first: (a) size+name — implemented, not used alone, too
weak on its own; (b) partial BLAKE3 (first+last 64KB, size folded into the hash input) — the
spike's default at import time; (c) full-file BLAKE3 — reserved for on-demand relink, not routine
import, given DNG's in-place-rewrite instability (LRC rewrites DNGs, invalidating a full-file
hash even though the shot itself hasn't changed); (d) an EXIF natural key (body serial + Exif
`ImageNumber` 0xA306 as Nikon shutter-count's closest standard-EXIF proxy — the real MakerNote
shutter-count field is body-specific and deferred, see Consequences — + `DateTimeOriginal` +
`SubSecTimeOriginal`), only returned when all four fields are present; a partial key is treated as
worse than no key, since it would falsely match every other file missing the same fields. See
`spikes/homing/src/fingerprint.rs`.

**Offline semantics**: a disconnected volume's `root`/`asset` rows are **never deleted** —
`schema::mark_offline_except` only flips `volume.online`, and `schema::resolve` returns `None`
for any asset owned by an offline volume. Per this session's product decision, the folder panel
collapses a disconnected volume to a single "`<label>` — not connected" node; its assets drop out
of the grid, search, and facet counts until the drive reconnects. Ratings, keywords, and edit
history are preserved regardless — nothing about being offline touches catalog data, only
resolvability.

### Alternatives considered and rejected

- **Drive-letter-only identity** (LRC's own apparent approach, inferred from its offline-preview
  behavior) — doesn't survive a letter reassignment at all, the exact failure mode #71 exists to
  fix.
- **`\\?\Volume{GUID}` as the identity key** — per-machine, assigned by the mount manager; not
  confirmed stable across a reformat either (untested in this pass, flagged as a real open
  question for the reference-machine run, not asserted either way).
- **DuckDB/other catalog engine for this slice specifically** — out of scope; ADR-0008 already
  settled the engine for the whole catalog, this ADR only adds tables within it.

## Consequences

- **#22** (catalog schema + import/ingest pipeline) inherits the `volume`/`root`/`asset` shape
  above as its identity/volume-resolution slice — not the whole catalog schema, which #22 still
  owns.
- **#72** (tiered thumbnail storage) hooks its SSD/archive transition into `root.archived`, keyed
  by the same volume resolution this ADR defines.
- **ADR-0011's facet-count cache** must exclude offline volumes' assets from its counts — either
  per-volume-partitioned counts or a subtraction step at query time. Not solved here; flagged for
  #22/#23 to pick up when the real facet-count implementation lands.
- **#24**'s filesystem watcher reconciles moves/renames/deletes *within* a volume already known to
  the catalog. A move *across* volumes goes through this ADR's relink path instead, not #24's.
- **Real MakerNote-based Nikon shutter-count extraction** (rather than the `ImageNumber` EXIF-tag
  proxy this ADR's natural key uses) is deferred — a real per-body offset table is a separate,
  larger piece of work than this spike's scope, tracked as a follow-up issue once #71 merges, not
  solved inline here.
- **The two-mounted-volumes-same-identity ambiguity check** (disk clone / copied VHDX) needs an
  explicit guard in front of `schema::upsert_volume`'s current `ON CONFLICT DO UPDATE` — flagged
  above as a known gap in the spike, tracked as a follow-up issue.
- **`CM_Register_Notification` push-based mount detection** is unimplemented in this pass (see
  Measured results) — a follow-up issue once the reference-machine run is scheduled.
- Non-Windows volume identity stays deferred to **#73** (v2: macOS + Linux release builds).

## Measured results

**Everything below needs the reference-machine (RTX 5080/Windows box) pass this ADR is Proposed
pending** — same shape as ADR-0006/0007/0090's own "spec + tooling merged, baseline measurement
deferred" pattern. No placeholder number is asserted as real; every row below is explicitly TBD.

### Volume-identity field survival (per candidate field, per scenario)

| Field | Letter change | Detach/reattach | Folder mount point | Reformat | Disk clone (2 copies mounted) |
|---|---|---|---|---|---|
| `\\?\Volume{GUID}` path | TBD | TBD | TBD | TBD | TBD |
| 32-bit volume serial (`GetVolumeInformationW`) | TBD | TBD | TBD | TBD | TBD |
| NTFS 64-bit serial (`FSCTL_GET_NTFS_VOLUME_DATA`) | TBD | TBD | TBD | TBD | TBD |
| GPT partition GUID | TBD | TBD | TBD | TBD | TBD |
| MBR signature + offset | TBD | TBD | TBD | TBD | TBD |
| `.nicti-volume` marker file | TBD | TBD | TBD | TBD | TBD |
| `sysinfo::Disks` fields | TBD | TBD | TBD | TBD | TBD |

Filled in by `spikes/homing/scripts/remap-test.ps1` (VHDX-only scenarios) — see that script for
the exact step sequence.

### Mount-change detection

| Backend | Attach→detected latency | Detach→detected latency | Idle CPU |
|---|---|---|---|
| `sysinfo` poll, 1s interval | TBD | TBD | TBD |
| `sysinfo` poll, 5s interval | TBD | TBD | TBD |
| `CM_Register_Notification` (push) | not implemented this pass — see Consequences | — | — |

### Fingerprint cost (per file, stratified sample walked from the live library per #136)

| Tier | NVMe p50 | NVMe p95 | HDD p50 | HDD p95 | Collision/false-match rate |
|---|---|---|---|---|---|
| (a) size+name | TBD | TBD | TBD | TBD | TBD |
| (b) partial BLAKE3 (64KB head+tail) | TBD | TBD | TBD | TBD | TBD |
| (c) full-file BLAKE3 | TBD | TBD | TBD | TBD | TBD |
| (d) EXIF natural key | TBD | TBD | TBD | TBD | TBD (coverage: what fraction of real files have all four fields) |

DNG rewrite-stability check (LRC rewrites DNGs in place — does tier (c) actually go unstable on a
real LRC-edited DNG, as hypothesized): TBD.

### Sandbox-measured (real, not TBD)

- `cargo test -p homing --all-targets --all-features`: 23/23 unit tests pass (schema, fingerprint,
  path normalization, identity-key selection logic — everything not requiring a live Windows
  volume).
- `cargo clippy -p homing --all-targets --all-features -- -D warnings`: clean.
- `cargo fmt --all -- --check` and the full workspace `cargo clippy --workspace --exclude den
  --exclude pelt-egui --exclude pelt-iced --exclude pelt-slint --exclude retina --all-targets
  --all-features -- -D warnings` / `cargo test` (same excludes): clean, `homing` included (not
  path-gated — like `sniff`, it needs no heavy native build).
- `cargo deny --workspace --all-features check licenses`: `licenses ok`, no new `deny.toml` entry
  needed (see `docs/licensing.md`'s 2026-09-26 update).
