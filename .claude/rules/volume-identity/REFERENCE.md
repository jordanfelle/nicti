---
paths:
  - "spikes/homing/**"
  - "crates/nicti-catalog/**"
---

# Volume Identity — Quick Reference

Full reasoning/history: `docs/decisions/volume-identity.md`.

- **Identity key: NTFS 64-bit serial + GPT partition GUID** (fallback: MBR signature+offset+32-bit
  serial) — `docs/adr/0071`. Not `\\?\Volume{GUID}` (per-machine, mount-manager-assigned) or plain
  `sysinfo` fields (no stable cross-mount ID). `.nicti-volume` marker file: secondary signal only,
  not the primary key (read-only volumes can't hold one; clones copy it).
- **Two volumes, same identity → never auto-merge.** `spikes/homing/schema::upsert_volume`'s
  `ON CONFLICT DO UPDATE` collapses them by construction — **partially closed in
  `crates/nicti-catalog::sqlite::SqliteCatalog::upsert_volume`** (#22, landed): a `marker_uuid`
  disagreement under the same `identity_key` now returns `CatalogError::VolumeIdentityConflict`
  instead of merging. Still not a full solution — a genuine `identity_key` *collision* between two
  physically distinct volumes that also agree on `marker_uuid` (or where neither side has one yet)
  remains unresolved, same as ADR-0071 itself leaves it (Proposed, not Accepted).
- **Schema: `volume` / `root` / `asset`**, three levels — `root` (a registered folder) is what #72
  moves between SSD/archive, one row, not a per-asset rewrite. `crates/nicti-catalog::schema`
  implements this shape for real (#22, landed), promoted from `spikes/homing/src/schema.rs`.
- **Offline volume → never delete rows**, only `volume.online = 0`. Folder panel collapses to one
  "not connected" node; assets drop out of grid/search/facet counts until reconnect. Rejects LRC's
  persistent-offline-preview + grayed-tree pattern.
- **Relink tiers, cheapest first**: (a) size+name (weak) → (b) partial BLAKE3, 64KB head+tail
  (import-time default) → (c) full-file BLAKE3 (on-demand only — DNG in-place rewrites by LRC are
  expected to break this) → (d) EXIF natural key (body serial + `ImageNumber` 0xA306 + DateTimeOriginal
  + SubSecTimeOriginal, all-or-nothing).
- **Mount detection**: `sysinfo` polling implemented; `CM_Register_Notification` push backend
  stubbed, not implemented — flagged follow-up, not a silent gap.
- **ADR-0071 is Proposed, not Accepted** — this sandbox has no Windows/mountable NTFS volume, so
  `volume::windows_impl`/`mount_events::windows_impl` are unverified. Everything cross-platform
  (schema/fingerprint/path) is real: 29 tests pass, workspace clippy/test/fmt/cargo-deny clean.
  Reference-machine run required before Accepted — see ADR-0071's Measured results (all TBD).

## Package contents

- **`spikes/homing`** (#71/ADR-0071's volume-identity research) — candidate identity keys measured
  against a drive-letter change/detach-reattach/reformat survival table, a `volume`/`root`/`asset`
  SQLite schema, size+name/partial-BLAKE3/full-BLAKE3/EXIF-natural-key relink tiers, and a
  `sysinfo`-poll-vs-`CM_Register_Notification` mount-detection comparison — lib+bin split so its
  currently-CLI-unwired helpers don't trip `dead_code` the way a bin-only spike would; `lib.rs`
  re-exports `fingerprint`/`mount_events`/`path`/`relink`/`schema`/`volume`. Windows-only research
  written in a Linux/WSL sandbox with no mountable NTFS volume: its `windows_impl` modules are
  unverified against real hardware, while its cross-platform schema/fingerprint/path logic is real,
  tested (29 unit tests), and — unlike `den`/`pelt-*`/`retina` — not path-gated out of CI's normal
  `clippy`/`test` jobs, since it needs no heavy native build (same as `sniff`).
