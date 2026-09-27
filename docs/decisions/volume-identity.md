## Volume identity and drive remapping

Covers #71's volume-identity key, the volume/root/asset schema, and file-fingerprint relinking.

- **Identity key (#71)**: `docs/adr/0071-volume-identity-and-remapping.md` — NTFS 64-bit volume
  serial + GPT partition GUID when both are present, falling back to MBR signature + partition
  offset + 32-bit volume serial on non-GPT disks. Neither the mount manager's per-machine
  `\\?\Volume{GUID}` path nor `sysinfo`'s disk listing (mount point, name, removable flag — no
  stable cross-mount field) qualifies as the key on its own; both are recorded for completeness.
  A `.nicti-volume` marker-file tie-breaker was considered and not adopted as the primary key — a
  read-only volume can't hold one, and a disk clone copies it verbatim, so it can't disambiguate
  the exact ambiguity case (two mounted volumes reporting the same identity) this ADR must catch
  rather than silently auto-merge.
  **Schema**: three levels (`volume`, `root`, `asset`), not a flat `asset(absolute_path)` table —
  a folder registered for tracking (`root`) can move from the active SSD to the archive drive
  (#72) as a single row update, without touching every `asset` row underneath it. Path
  normalization: UTF-8, forward slashes, NFC-composed, plus a case-folded lookup column for
  NTFS/exFAT's case-insensitive-but-preserving semantics, no 260-character `MAX_PATH` assumption.
  **Offline semantics** (this session's product decision, not inferred): a disconnected volume's
  `root`/`asset` rows are never deleted, only marked offline; the folder panel collapses it to a
  single "not connected" node and its assets drop out of the grid/search/facet counts until it
  reconnects — deliberately rejecting LRC's own persistent-offline-preview + grayed-out-tree
  pattern.
  **Relink fingerprint tiers** (merged in from an archived duplicate, #21): (a) size+name, cheap
  and weak; (b) partial BLAKE3 (first+last 64KB) — the spike's import-time default; (c) full-file
  BLAKE3 — reserved for on-demand relink only, since DNG's in-place rewrite by LRC is expected to
  invalidate a full-file hash without the underlying shot having changed; (d) an EXIF natural key
  (body serial + `ImageNumber` 0xA306 as the closest standard-EXIF proxy for Nikon's MakerNote-only
  shutter count + `DateTimeOriginal` + `SubSecTimeOriginal`), returned only when all four fields
  are present — a partial key is treated as worse than none, since it would falsely match every
  other file missing the same fields. Real per-body MakerNote shutter-count extraction is
  deferred, tracked as a follow-up once #71 merges.
  **#158/ADR-0158 cross-checked LRC's own `AgLibraryFile.md5`/`importHash` against these tiers**:
  neither can seed tier (b) or (c) — `md5` was flagged in ADR-0061 as a candidate but turned out to
  be NULL on every one of 380,298 real rows measured (never populated in this catalog at all, not a
  partial gap), and `importHash`, while 99.97% present and 100% distinct among non-NULL values, has
  an unconfirmed derivation, so it's kept as opaque provenance rather than used for relink or
  dedupe. #62 computes tier (b) fresh at import time as originally planned.
  **Mount-change detection**: `sysinfo`-based polling is implemented
  (`spikes/homing/src/mount_events.rs`); `CM_Register_Notification` push-based detection is
  stubbed as an explicit, documented follow-up rather than silently missing — the issue's own ask
  was to evaluate the Windows-native API first, not to ship a polling-only solution unexamined.
  **Sandbox constraint**: this research pass ran in Linux/WSL with no Windows toolchain and no
  mountable NTFS volume — `spikes/homing`'s Windows-only code (`volume::windows_impl`,
  `mount_events::windows_impl`) is written against `windows-sys`' documented API shapes but has
  never been compiled or run; the schema/fingerprint/path logic (cross-platform, no `cfg(windows)`
  gate) is real, measured: 29 unit tests pass, full workspace `clippy`/`test`/`fmt`/`cargo deny`
  are clean with `homing` added (not path-gated, like `sniff` — no heavy native build). **ADR-0071
  stays Proposed** until the reference-machine (RTX 5080/Windows) pass fills in the
  volume-identity survival table, mount-detection latency/CPU comparison, and fingerprint-cost
  benchmarks — same "spec + tooling merged, baseline measurement deferred" shape as
  ADR-0068/ADR-0050/#90.

**#24 (2026-09-27): manual catalog sync, not a live watcher.** #24 was re-scoped from a continuous
background filesystem watcher to a manual, user-triggered "Synchronize Folder" (LRC-style) sync —
see ADR-0024. `crates/nicti-lair::patrol::sync_root` layers a catalog-side pass on top of Scruff's
existing disk-side ingest: a file gone from an otherwise-reachable root is flagged
(`asset.missing_since`) and kept, not deleted, unless the caller opts into `remove_missing`; an
entire root that fails to resolve on disk (unplugged drive, changed letter) touches nothing and is
left to this ADR's own offline-volume path instead of being misread as every file underneath it
individually vanishing. This ADR's own offline-volume semantics (never delete, `volume.online`
only, no greyed-out tree) are unchanged.
