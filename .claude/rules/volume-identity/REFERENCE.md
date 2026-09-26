---
paths:
  - "spikes/homing/**"
  - "crates/nicti-catalog/**"
---

# Volume Identity — Quick Reference

Full reasoning/history: `docs/decisions/volume-identity.md`.

- **Identity key: NTFS 64-bit serial + GPT partition GUID** (fallback: MBR signature+offset+32-bit
  serial) — `docs/adr/0020`. Not `\\?\Volume{GUID}` (per-machine, mount-manager-assigned) or plain
  `sysinfo` fields (no stable cross-mount ID). `.nicti-volume` marker file: secondary signal only,
  not the primary key (read-only volumes can't hold one; clones copy it).
- **Two volumes, same identity → never auto-merge.** `schema::upsert_volume`'s current
  `ON CONFLICT DO UPDATE` collapses them by construction — a real ambiguity guard is a flagged
  follow-up, not solved yet.
- **Schema: `volume` / `root` / `asset`**, three levels — `root` (a registered folder) is what #72
  moves between SSD/archive, one row, not a per-asset rewrite. `crates/nicti-catalog` inherits
  this shape for #22.
- **Offline volume → never delete rows**, only `volume.online = 0`. Folder panel collapses to one
  "not connected" node; assets drop out of grid/search/facet counts until reconnect. Rejects LRC's
  persistent-offline-preview + grayed-tree pattern.
- **Relink tiers, cheapest first**: (a) size+name (weak) → (b) partial BLAKE3, 64KB head+tail
  (import-time default) → (c) full-file BLAKE3 (on-demand only — DNG in-place rewrites by LRC are
  expected to break this) → (d) EXIF natural key (body serial + `ImageNumber` 0xA306 + DateTimeOriginal
  + SubSecTimeOriginal, all-or-nothing).
- **Mount detection**: `sysinfo` polling implemented; `CM_Register_Notification` push backend
  stubbed, not implemented — flagged follow-up, not a silent gap.
- **ADR-0020 is Proposed, not Accepted** — this sandbox has no Windows/mountable NTFS volume, so
  `volume::windows_impl`/`mount_events::windows_impl` are unverified. Everything cross-platform
  (schema/fingerprint/path) is real: 20 tests pass, workspace clippy/test/fmt/cargo-deny clean.
  Reference-machine run required before Accepted — see ADR-0020's Measured results (all TBD).
