## XMP interop with Lightroom Classic

Covers #59's XMP-interop research — full reasoning and every measured number in
`docs/adr/0059-xmp-interop.md`. This file is the per-topic summary; that ADR is the full research
trail.

- **Handoff from ADR-0002**: the non-destructive edit model ADR set three XMP responsibilities
  (LRC-convention metadata, a lossless `nicti:` recovery namespace, a best-effort write-only `crs:`
  projection) and left this ticket the field-level mapping, the write library, and the `crs:`
  write-gating policy.
- **Field mapping**: `xmp:Rating` (`-1..=5`), `xmp:Label` (text), `dc:subject`/
  `lr:hierarchicalSubject` (flat/hierarchical keywords). Unrated stays `None`, distinct from a `0`
  rating — `docs/research/shed-lrcat-schema.md` found the real `.lrcat`'s `rating` column is
  nullable (277k/380k rows NULL), so collapsing the two would silently invent data on import.
  LRC's positive Pick flag has no confirmed XMP field and isn't mapped here.
- **Library: `quick-xml`**, chosen for its event-based Reader/Writer API — the spike's `packet.rs`
  parses a full packet into events and only rewrites the properties it owns, re-emitting `crs:`/
  `exif:`/`aux:`/any other namespace completely unchanged. Proven by test, not just asserted.
- **Embedded formats**: `shed-lrcat-schema.md`'s real 380,307-row catalog is ~82% JPEG/DNG, where
  LRC embeds XMP in the file rather than a `.xmp` sidecar. JPEG's APP1 XMP segment is implemented
  (read + write, with a from-scratch segment walker and a byte-diff-guard test proving nothing
  outside the segment changes). DNG/TIFF tag-700 write is deferred — TIFF's absolute IFD offsets
  make a naive splice unsafe in a way JPEG's flat segment list isn't; v1 treats DNG as read-only
  for embedded XMP and writes a `.xmp` sidecar for DNG assets instead.
- **`crs:` write gate**: write only if the sidecar's hash still matches what Nicti itself last
  wrote there, otherwise skip and flag — keeps the projection live during normal editing without
  risking clobbering an LRC edit made to the same file in between. **The hash comparison alone
  cannot close the race**: an LRC save landing between Nicti's hash read and its write is still
  possible; `conflict.rs`'s own docs specify the lock/single-writer discipline a real caller needs
  to close that window, which this gate function doesn't itself enforce.
- **No hands-on LRC session was run this pass** — real evidence came from `shed-lrcat-schema.md`,
  the public XMP/`crs:`/`lr:` specs, and this spike's own real-code test suite (38 tests, all
  passing), not from a live Lightroom install. The ADR merges as Proposed; a follow-up issue (part
  of #11) runs a scripted LRC session (rating/reject/pick/custom label set/hierarchical keywords on
  throwaway copies) to settle several open questions (does Reject/Pick reach XMP at all? does a
  renamed label set change the written text? what does "Read Metadata from File" do with a
  Nicti-written field?) before promoting this to Accepted.
