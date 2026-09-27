---
paths:
  - "spikes/scent/**"
  - "docs/adr/0059*"
---

# XMP Interop — Quick Reference

Full reasoning/history: `docs/decisions/xmp-interop.md`.

- **XMP interop with LRC (#59)** — `docs/adr/0059`: **Proposed**, pending a hands-on LRC session
  (no real LRC-written file exists in this Linux/WSL sandbox — same constraint ADR-0020's `homing`
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
- **Conflict rule**: ADR-0002's hash-then-mtime-then-ambiguity-window rule, reimplemented against
  real file mtimes/hashes (not depended on from `spikes/pawprint` — spikes don't depend on other
  spikes in this repo, only on real `crates/*`).
