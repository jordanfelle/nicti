# #59: XMP interop with Lightroom Classic (spikes/scent)

Full write-up backing `docs/adr/0059-xmp-interop.md` — this doc is the method and reproduction
detail; the ADR is the decision record.

## Method

ADR-0002 handed this ticket three open questions: the field-level LRC-metadata mapping, the real
XMP library/packet structure, and how to gate the `crs:` write-only projection. `spikes/scent`
answers all three with real, tested code rather than a paper design:

1. **`lrc_fields.rs`** parses `xmp:Rating`/`xmp:Label`/`dc:subject`/`lr:hierarchicalSubject` from a
   full XMP packet, accepting both the attribute form (compact, what real sidecars mostly use for
   scalars) and the element form (rarer, but a real pitfall `spikes/calico`'s `xmp_profile.rs`
   already ran into for `crs:` properties).
2. **`packet.rs`** is the library-choice deliverable: an event-copy-and-patch reader/writer built
   on `quick-xml`'s `Reader`/`Writer` event API. The whole packet is parsed into a flat event list;
   only the first `rdf:Description`'s owned attributes are rewritten, and only its
   `dc:subject`/`lr:hierarchicalSubject` child blocks are replaced when a write touches keywords —
   everything else (an unrelated `crs:` property, another tool's namespace) is re-emitted
   unchanged. This also hosts the lossless `nicti:editDocument` attribute (ADR-0002 layer b),
   replacing `spikes/pawprint`'s placeholder hand-built-string packet with a real one.
3. **`sidecar.rs`** and **`embedded.rs`** are the two file-shapes LRC actually uses: a `.xmp`
   sidecar next to a RAW file, or an XMP packet embedded inside a JPEG's APP1 segment. Both are
   real read/write paths, not just read.
4. **`conflict.rs`** re-derives ADR-0002's newer-wins rule against real file mtimes (via
   `fs::metadata`) and content hashes (via `blake3`, already a workspace dependency since
   ADR-0020), and adds the `crs:` write-gate policy this ADR decides.

## Candidates measured

The decision rule (round-trip fidelity, sidecar + JPEG write support, license, dependency weight)
ruled between `roxmltree` (already a dependency, read-only — disqualified outright), `little_exif`
and the Adobe XMP Toolkit SDK / `exiv2`/`rexiv2` (all already license-clear per `docs/licensing.md`,
but none built around "preserve every property you don't recognize," which is the actual shape
this spike needs), and `quick-xml` (chosen — event-based Reader/Writer, pure Rust, MIT, no new
`deny.toml` entry). See the ADR's Options considered section for the full reasoning per candidate.

## Real-file cross-check

**None was possible this pass.** This spike was written in the same Linux/WSL sandbox ADR-0020's
`homing` spike already flagged as having no mountable path to the user's real photo library — the
37 real Z8 NEF+LRC-XMP sidecar pairs `spikes/litter`'s own test uses, and any real LRC-touched
JPEG/DNG files, live on the user's Windows machine, not here.

Two integration tests are written and wired up regardless, gated on env vars, so a run on a real
machine (or a future CI runner with access) exercises them automatically once the vars are set:

- `tests/real_lrc_sidecars.rs` — gated on `NICTI_TEST_REAL_NEF_DIR` (the same folder
  `spikes/litter`'s `real_nef_cross_check.rs` already uses — same 37 real pairs). Parses every real
  sidecar, asserts a no-op patch never changes what a fresh read sees, and reports field-presence
  counts (rating/label/keywords/hierarchical).
- `tests/real_embedded.rs` — gated on a new `NICTI_TEST_REAL_EMBEDDED_DIR` (real LRC-touched JPEGs;
  no such folder is documented elsewhere in this repo yet). Same no-op-round-trip proof, for the
  embedded-XMP case.

Both currently print a "skipping: set `<VAR>`..." message and pass trivially, exactly like
`spikes/litter`'s own pattern — this is expected in this sandbox, not a gap in the test itself.

What the real `.lrcat` research (`docs/research/shed-lrcat-schema.md`) *does* establish without
needing a live file: `rating` is nullable (277k/380k real rows NULL, settling the None-vs-0
question), `pick` is a separate column from `rating` (settling that Pick and star-rating are
different concepts, though not what XMP field, if any, carries Pick), and the real format mix
(274,499 JPEG / 68,352 RAW / 37,449 DNG of 380,307 total) — the number that drove this ADR's
"research embedded formats too" scope decision.

## What was and wasn't reachable this pass

**Reachable, and done:**
- The field mapping's shape (which XMP property, in which form) for rating/label/keywords.
- A real, tested packet patcher proving unrelated *attribute values* survive a patch exactly
  (not full byte-identical tag serialization — see the ADR's Measured results for that
  distinction).
- JPEG-embedded XMP read/write, proven not to touch unrelated bytes.
- The `crs:` write-gate policy (not its field-level content — that's masking's own ADR-0024).
- The conflict rule wired to real file I/O.

**Not reachable without a live Lightroom Classic install** (tracked as a follow-up issue, part of
#11, gating this ADR's Proposed → Accepted transition):
- Whether Reject (`xmp:Rating="-1"`) and Pick actually reach XMP at all, under LRC's real default
  settings.
- What "Read Metadata from File" does when it meets a field it doesn't recognize.
- Whether a renamed color-label set changes the literal `xmp:Label` text.
- The real behavior difference (if any) between "Automatically write changes into XMP" for a
  sidecar-backed RAW vs. an embedded-XMP JPEG/DNG.

**Deferred as a scope decision, not a gap:**
- DNG/TIFF tag-700 embedded-XMP *write* (read could reuse `spikes/sniff`'s TIFF/IFD walker
  relatively cheaply; write needs a dedicated TIFF writer this spike didn't build, and TIFF's
  absolute IFD offsets make a naive byte splice unsafe in a way JPEG's flat segment list isn't).

## Reproducing

```bash
# Unit tests (35) + the two env-gated real-file tests (skip cleanly without the env vars):
cargo test -p scent

# Lint/format:
cargo clippy -p scent --all-targets -- -D warnings
cargo fmt -p scent -- --check

# CLI, against a synthetic sidecar (real-file use needs NICTI_TEST_REAL_NEF_DIR-style access):
cargo run -p scent -- dump path/to/DSC_0001.NEF        # reads DSC_0001.xmp
cargo run -p scent -- write path/to/DSC_0001.NEF --rating 5
cargo run -p scent -- roundtrip path/to/DSC_0001.NEF     # no-op patch, checks metadata survives
cargo run -p scent -- survey path/to/a/folder            # field/namespace frequency across a folder

# On a real machine with the library available:
NICTI_TEST_REAL_NEF_DIR=/path/to/nef+xmp/pairs cargo test -p scent --test real_lrc_sidecars
NICTI_TEST_REAL_EMBEDDED_DIR=/path/to/lrc/jpegs cargo test -p scent --test real_embedded
```
