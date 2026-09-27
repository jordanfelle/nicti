---
paths:
  - "spikes/shed/**"
  - "crates/nicti-catalog/**"
  - "docs/adr/0023*"
---

# LRC Migration — Quick Reference

Full reasoning/history: `docs/decisions/lrc-migration.md`.

- **Folder model**: LRC's `AgLibraryRootFolder`→`AgLibraryFolder`→`AgLibraryFile`+`Adobe_images`
  maps directly onto ADR-0020's `volume`/`root`/`asset`. All real roots use a drive-letter absolute
  path; some also carry a relative-path fallback, not universal — #62 handles both.
- **`pick`/`rating` are SQLite `REAL`, not `INTEGER`** — don't assume integer semantics from the
  column name; a naive numeric op on `hasRetouch` (a bitmask-string column) silently produces a
  wrong number instead of erroring.
- **Collections**: `AgLibraryCollection.creationId` — `com.adobe.ag.library.collection` is a real
  user collection; `*.unsaved` kinds are LRC's own per-module scratch state, never import those.
  Smart-collection rule-criteria mapping is unverified (none existed in the measured catalog).
- **Develop settings**: `agprefs` crate (MIT) parses LRC's `s = { Key = Value }` Lua literal with
  zero failures on a real 380,307-row catalog — adopted over hand-rolling. `develop.rs`'s
  `classify_key` is an exact-match table (not prefix heuristics — those missed `Enable*` toggles
  and per-channel HSL keys) sorting all real keys to owner tickets #42/#46/#47/#39/#51/#49/#40/#52.
  Keep raw LRC text verbatim in a provenance blob on import; translate only what's needed.
- **`FilterList`/`AllowFilters` gate 4 distinct AI filters (#157)**, not just Denoise: real usage
  is 20,303 Denoise / 47 People Removal / 6 Super Resolution / 1 Reflection Removal (of 380,307
  rows), read from `FilterList.Filters[].Title`. `classify_key` defaults the raw key to `#40`
  (dominant case) — #62's importer must inspect each `Filters[]` entry and route People/Reflection
  Removal to `#51`, Super Resolution to `#174`. `LensBlur` is present almost everywhere but always
  empty (0/380,307 rows with real content) — provenance-only, no owner ticket needed.
- **`lrcat-extractor` not a dependency** — its `rusqlite = "0.38"` pin conflicts with `den`'s
  `^0.40` via Cargo's workspace-wide `links = "sqlite3"` uniqueness (not per-binary, holds even
  behind an optional feature). Evaluate it standalone outside the workspace, not as a `spikes/*`
  dependency, until `den` is gone.
- **`shed`'s `open_backup` guard**: refuses a `.lock`/`-wal` sibling only when it has real content
  — a plain read-only open of an already-closed WAL-mode catalog leaves harmless zero-byte
  siblings behind, and presence-alone was a real false-positive this pass hit and fixed.
