---
paths:
  - "crates/nicti-scent/**"
  - "crates/nicti-lair/src/scent_sync.rs"
  - "crates/nicti-pelt/src/xmp_sync.rs"
  - "docs/adr/0059*"
---

# XMP Interop — Quick Reference

Full reasoning/history: `docs/decisions/xmp-interop.md`.

- **XMP interop with LRC (#59)** — `docs/adr/0059`: **Proposed**, pending a hands-on LRC session
  (no real LRC-written file exists in this Linux/WSL sandbox — same constraint ADR-0071's `homing`
  spike already documents).
- **Field mapping**: `xmp:Rating` (`-1..=5`, `-1`=reject, absent=unrated — kept `None`, never
  defaulted to `0`, matching `shed-lrcat-schema.md`'s nullable `rating` finding), `xmp:Label`
  (text, never assumed to be a default color name), `dc:subject` (flat keywords),
  `lr:hierarchicalSubject` (`|`-joined hierarchical paths). LRC's positive Pick flag has **no**
  confirmed XMP mapping — deferred, carried only in Nicti's own `nicti:` namespace.
- **Library: `quick-xml`** (event-copy-and-patch — parse the whole packet, patch only owned
  fields, re-emit everything else, e.g. `crs:`, unchanged) over `roxmltree` (no write support),
  `little_exif`/Adobe XMP Toolkit/`exiv2`/`rexiv2` (all license-clear but not needed — no new C/C++
  dependency for a pure-Rust fit).
- **JPEG embedded XMP (APP1) implemented**; **DNG/TIFF tag-700 write deferred** — TIFF's absolute
  IFD offsets make a naive splice unsafe, unlike JPEG's flat segment list. v1: JPEG embedded
  read/write, DNG embedded read-only (writes go to a `.xmp` sidecar instead).
- **`crs:` write gate (layer c)**: write only if the sidecar's current hash still matches what
  Nicti last wrote there (`conflict::should_write_crs`) — never on an explicit-export-only basis.
- **Conflict rule**: ADR-0021's hash-then-mtime-then-ambiguity-window rule, reimplemented against
  real file mtimes/hashes (not depended on from `spikes/pawprint` — spikes don't depend on other
  spikes in this repo, only on real `crates/*`).

- **#60 (phase-1 coexistence, landed)**: sidecar sync for NEF/NRW, layer (a) only (+ `nicti:pick`).
  Sides compare by canonical `Markers`, not bytes. Read path = ADR-0021 newer-wins using
  `asset_sidecar.catalog_dirty_since_ms` as the catalog's "mtime" (`AMBIGUITY_WINDOW_MS` = 2000);
  write path is stricter: an un-ingested external sidecar edit is *held for review*, never
  clobbered. ADR-0059 stays Proposed until #187 (Reject/Pick/label mappings unverified).

## Package contents

- **`crates/nicti-scent`** (#60, promoted from `spikes/scent`) — the pieces below, minus the spike
  CLI. `nicti-lair`'s `scent_sync.rs` (`import_sidecar`/`write_sidecar`/`resolve_review`, schema v9
  `asset_sidecar`) is the catalog glue, hooked into `scruff.rs` (ingest + rescan) and
  `nicti-pelt`'s `xmp_sync.rs` (`XmpMeta` write-back thread, auto-write toggle, conflict panel).
- **(was) `spikes/scent`** (#59/ADR-0059's XMP-interop research) — the LRC-convention
  rating/label/keyword field mapping, a `quick-xml`-based event-copy-and-patch packet
  reader/writer that preserves every property it doesn't own, `.xmp` sidecar naming + atomic
  write, JPEG-embedded APP1 XMP read/write (DNG/TIFF tag-700 write deferred, see the ADR), and the
  ADR-0021 conflict rule + `crs:` write-gate wired to real file mtimes/hashes. Real, tested (36
  unit + 2 env-gated real-file integration tests, all passing — the real-file tests skip cleanly
  in this sandbox, same constraint `homing` already documents), not path-gated, pending the
  follow-up hands-on LRC session the ADR describes before it can move to Accepted. See
  `docs/research/scent-xmp-interop.md`.
