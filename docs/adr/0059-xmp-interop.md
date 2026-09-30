# ADR-0059: XMP interop with Lightroom Classic

- **Status:** Proposed — pending a hands-on LRC pass (see "What wasn't reachable this pass")
- **Date:** 2026-09-26
- **Ticket:** [#59](https://github.com/jordanfelle/nicti/issues/59) Research: XMP interop

**Numbering note:** this ADR uses ticket-number keying (`0059-*`, matching issue #59) per the
convention `docs/adr/README.md` adopted 2026-09-27 (landed by PR
[#167](https://github.com/jordanfelle/nicti/pull/167)) — a new ADR's number is its GitHub issue
number, not the next sequential count. This ADR was originally written against the in-flight,
not-yet-official version of that same scheme; no change was needed once #167 merged and made it
official.

## Context

Phase-1 coexistence ([#60](https://github.com/jordanfelle/nicti/issues/60)) means Nicti and
Lightroom Classic both read and write the same files during the transition before `.lrcat` import
([#61](https://github.com/jordanfelle/nicti/issues/61)/[#62](https://github.com/jordanfelle/nicti/issues/62)).
ADR-0021 already set the frame this ticket was handed:

- The catalog database is authoritative; XMP is an interop/recovery layer, not the source of truth.
- Three XMP responsibilities, not one: **(a)** LRC-convention metadata (rating/flag/label/keywords)
  — this ADR's main job; **(b)** a lossless `nicti:` namespace carrying the full current edit
  document, for catalog recovery; **(c)** a best-effort, write-only `crs:` projection, mainly for AI
  masks, so LRC/ACR can display Nicti's edits during the transition.
- A newer-wins conflict rule (content hash, then mtime, with an ambiguity window that flags for
  manual review) — proven in the abstract in `spikes/pawprint/src/xmp.rs`, but "actually reading a
  file's mtime and applying the resolution... is real I/O plumbing left to #59" (ADR-0021 line 212).

ADR-0021 explicitly left three things open for this ticket: the field-level mapping for layer (a),
how layer-(c) writes are gated, and the real XMP library/packet structure (the pawprint spike's
packet format is a stated placeholder, not a proposal).

**Scope decisions carried into this pass:**

- **Both sidecar and embedded-XMP writes are researched.** `docs/research/shed-lrcat-schema.md`'s
  real 380,307-row `.lrcat` count (274,499 JPEG / 68,352 RAW / 37,449 DNG) means roughly 82% of the
  real library is a format where LRC embeds XMP *inside* the original file rather than writing a
  `.xmp` sidecar — a mapping that only covers sidecars would leave most of the library unaddressed.
- **This pass ran without a hands-on LRC session.** The strongest evidence for several open
  questions below (does a Reject reach XMP at all? does a custom label set change what text gets
  written? what does "Read Metadata from File" do with a Nicti-written field?) is a short LRC
  session on throwaway file copies — genuinely useful, but not run this pass. Evidence here instead
  comes from: this repo's own `.lrcat` research (`shed-lrcat-schema.md`), the Adobe XMP
  specification and public `crs:`/`lr:` namespace documentation, and this spike's own code —
  **not** from a real Lightroom install. This ADR merges as **Proposed**, gated to **Accepted** by
  a follow-up issue running that session (see Consequences).

## Decision rule (stated before measuring)

The library/format choice is decided against, in order: (1) round-trip fidelity on a real XMP
packet — every property this spike doesn't own must survive unchanged; (2) write support for both
a `.xmp` sidecar and a JPEG's embedded APP1 segment; (3) license (must already clear
`docs/licensing.md`/`deny.toml`, or be trivially addable); (4) dependency weight — a pure-Rust
crate is preferred over one that pulls in a new C/C++ build, all else equal.

## Decision

### Spike: `spikes/scent`

Real, tested code (35 tests: 33 unit + 2 env-gated real-file integration tests that skip cleanly
without the env vars set, plus CLI smoke-tested end-to-end on a synthetic sidecar — see the
research doc). Four modules, matching the scope handed off by ADR-0021:

- **`lrc_fields`** — layer (a): `LrcMeta { rating, label, keywords, hierarchical_keywords }`, read
  from (and patched into) a real packet.
- **`packet`** — an event-copy-and-patch XMP reader/writer: parses the whole packet into
  `quick_xml` events, rewrites only the fields this spike owns, and re-emits everything else —
  `crs:`, `exif:`, `aux:`, any other tool's namespace — completely unchanged. This is also where
  the lossless `nicti:editDocument` attribute (layer b) rides, replacing `spikes/pawprint`'s
  placeholder base64-in-a-hand-built-string packet with a real one.
- **`sidecar`** — RAW → `.xmp` sidecar naming (extension replacement, matching
  `spikes/litter/tests/real_nef_cross_check.rs`'s confirmed real convention) and an atomic
  temp-file-then-rename write.
- **`embedded`** — JPEG APP1 XMP segment read/write, with a from-scratch JPEG segment walker (no
  new image-parsing dependency). **DNG/TIFF tag-700 write is out of scope** — see below.
- **`conflict`** — ADR-0021's `resolve_conflict` re-hosted against real file mtimes/hashes (BLAKE3,
  already a workspace dependency since ADR-0071), plus the layer-(c) write gate this ADR decides
  below.

### Library choice: `quick-xml`, not `roxmltree`/`little_exif`/the Adobe XMP Toolkit/`exiv2`

`roxmltree` (already a workspace dependency, used read-only by `spikes/calico`) has no write
support at all — disqualified by the decision rule's write requirement. `little_exif` and the
Adobe XMP Toolkit SDK are both already cleared in `docs/licensing.md`, and `exiv2`/`rexiv2` have
been usable since ADR-0066's AGPL switch — but none of them are built around "parse the whole
document as a stream of events, patch just the ones you care about, emit the rest unchanged",
which is exactly this spike's requirement (preserving `crs:` and every other tool's data
byte-for-byte-in-spirit). `quick-xml`'s `Reader`/`Writer` event API fits that shape directly.
It's pure Rust (`memchr` is its only real dependency), MIT-licensed, and needs no new `deny.toml`
entry (see `docs/licensing.md`'s 2026-09-26 update).

### Field mapping (layer a)

| Field | XMP property | Notes |
|---|---|---|
| Rating | `xmp:Rating`, `-1..=5` | `-1` = rejected (Bridge/exiftool convention, not core XMP spec but near-universal); absent = unrated, kept as `None`, never defaulted to `0` — `shed-lrcat-schema.md` found `Adobe_images.rating` is nullable and 277k/380k real rows are NULL, so collapsing "unrated" into "0 stars" would be a real data-loss bug on import/export. |
| Color label | `xmp:Label` | The label **text** (e.g. `"Red"`), read/written verbatim — LRC's default label set uses color names, but a renamed label set changes the literal text, so this never assumes one of the 5 defaults. |
| Flat keywords | `dc:subject` (`rdf:Bag`/`rdf:li`) | |
| Hierarchical keywords | `lr:hierarchicalSubject` (`rdf:Bag` of `\|`-joined paths) | This, not `dc:subject`, is where nested keyword structure survives. |
| LRC's positive "Pick" flag | **no mapping — deferred** | The `.lrcat`'s own `pick` column is separate from `rating` (`shed-lrcat-schema.md`), and no standard XMP field for a positive pick flag was found in this pass. Whether LRC writes it to XMP at all (and under what field) is one of the open questions a real LRC session would settle — see "What wasn't reachable." Carried only in Nicti's own `nicti:` namespace until confirmed, not invented here. |

Both the attribute form (`xmp:Rating="4"` on `rdf:Description`, the compact form real sidecars
use for scalars) and the element form (`<xmp:Rating>4</xmp:Rating>`, rarer, but used by some tools
and hand-edited files) are read — the same attribute-vs-element pitfall `spikes/calico`'s
`xmp_profile.rs` already documents for `crs:` properties. List properties (`dc:subject`,
`lr:hierarchicalSubject`) are necessarily element-form only — RDF containers can't be attributes.

**Simplifying assumption**: exactly one `rdf:Description` carries these properties, and namespace
prefixes are matched by local name only, not resolved via `xmlns` URI — both match `spikes/calico`'s
existing precedent in this repo, and real files overwhelmingly use the conventional `dc`/`xmp`/`lr`
prefixes.

### Embedded formats: JPEG implemented, DNG/TIFF deferred

JPEG's APP1 XMP segment (`0xFFE1`, signature `"http://ns.adobe.com/xap/1.0/\0"`) is read and
written by a from-scratch segment walker in `embedded.rs`. Writing either replaces the existing
segment in place (content shifts, since the new payload length generally differs — JPEG's format
doesn't depend on absolute byte offsets, only segment structure, so this is always valid) or
inserts a new segment right after `SOI` if none existed. The `write_preserves_bytes_outside_the_segment`
test independently re-derives the untouched prefix/suffix and asserts byte equality, proving the
splice touches nothing else.

**DNG/TIFF tag-700 (embedded XMP) write is explicitly deferred, not implemented.** Unlike JPEG's
flat segment list, TIFF's IFD structure uses absolute file offsets throughout — a naive byte splice
that changes tag 700's length would corrupt every IFD entry pointing past it, which is a real
TIFF-rewrite problem, not a segment splice. `spikes/sniff`'s existing from-scratch TIFF/IFD walker
could be reused for a **read**-only path (locating tag 700's bytes) reasonably cheaply, but a safe
**write** needs a dedicated TIFF writer this spike didn't build. This also interacts with
ADR-0071: any DNG content change (embedded XMP included) already breaks a full-file BLAKE3 hash
regardless of how it's produced, falling back to ADR-0071's partial-hash/EXIF-natural-key relink
tiers — so writing embedded DNG XMP wouldn't introduce a *new* identity problem, but it doesn't
remove the existing one either.

**Recommendation**: v1 writes embedded XMP for JPEG, but treats DNG as **read-only** for embedded
XMP (Nicti can see what LRC wrote into a DNG, but writes its own ratings/labels/keywords for a DNG
asset to a `.xmp` sidecar instead, alongside the DNG, rather than rewriting the DNG itself). This
avoids a real TIFF-rewrite implementation risk for a scope (68,352 RAW + a portion of 37,449 DNG
real assets, per `shed-lrcat-schema.md`) that's smaller than JPEG's 274,499. A DNG-embedded XMP
writer is a reasonable follow-up once #61/#62's importer needs symmetric DNG write support, not
blocking for phase-1 coexistence.

### Conflict resolution wired to real files

`conflict::resolve_conflict` reimplements ADR-0021's rule (identical content hash → no conflict;
otherwise newer mtime wins; mtimes within an ambiguity window → flag for manual review) against
real `blake3::hash`/`fs::metadata` reads, via `conflict::hash_and_mtime`. Reimplemented rather than
imported from `spikes/pawprint` — a spike-depending-on-another-spike isn't this repo's convention
(only real `crates/*` are shared across spikes, e.g. `nicti-prowl`).

### The `crs:` write gate (layer c)

ADR-0021 asked this ticket to pick between "write `crs:` only when Nicti can confirm LRC hasn't
independently edited the photo since Nicti's last write" and "restrict `crs:` writes to an explicit
export action." **Decision: the former** — `conflict::should_write_crs(last_written_hash,
current_sidecar_hash)` returns `true` only if the sidecar's current hash still matches what Nicti
itself last wrote there (or if Nicti has never written to it before), and `false` otherwise. This
keeps the projection live during normal editing (an "export for LRC preview" action would leave
LRC's preview stale between exports, undermining the whole point of a best-effort live projection)
while never silently clobbering an edit LRC just made to the same sidecar. The actual field-level
`crs:` mapping (which mask-correction properties to emit) is out of scope here — that's
masking's own territory (#48/ADR-0048) — this ADR only resolves the write-gating *policy*
ADR-0021 asked for.

## Measured results

- **38 tests pass** in `spikes/scent` (36 unit, 2 env-gated real-file integration tests that skip
  cleanly here — no real LRC-written files exist in this Linux/WSL sandbox, same constraint
  ADR-0071's `homing` spike already documents). `cargo clippy -p scent --all-targets -D warnings`
  and `cargo fmt -p scent -- --check` both pass clean.
- **Content-preservation is proven for both formats, at different precision levels.**
  `embedded::tests::write_preserves_bytes_outside_the_segment` proves true byte-identity outside
  the touched JPEG APP1 segment (the splice never touches a single byte elsewhere). For the XMP
  packet itself, `packet.rs` rebuilds the whole `rdf:Description` tag attribute-by-attribute
  (rather than a byte-level splice), so what's proven there is that every *value* this patch
  doesn't own survives exactly — `packet::tests::patching_one_attribute_preserves_every_other_attributes_exact_value`
  covers a value containing `&`/`"` specifically, since those are the characters a careless rebuild
  could re-escape differently — **not** that the tag's serialized bytes are identical (attribute
  quoting/order can be normalized by `quick-xml`'s writer). This distinction was underspecified in
  an earlier draft of this ADR and caught by adversarial review before merge — see the "Adversarial
  review" note below.
- **NULL-vs-zero is proven**: `lrc_fields::tests::absent_rating_is_none_not_zero` and
  `explicit_zero_rating_is_some_zero` both pass, matching `shed-lrcat-schema.md`'s real finding
  that `rating` is nullable.
- **The CLI was smoke-tested end-to-end** against a synthetic sidecar (`dump`/`roundtrip`/`survey`
  all produce correct output) — see the research doc's Reproducing section.
- **No real LRC-written file was available to measure against this pass** — the two env-gated
  integration tests (`real_lrc_sidecars.rs`, `real_embedded.rs`) are written and wired to
  `NICTI_TEST_REAL_NEF_DIR`/a new `NICTI_TEST_REAL_EMBEDDED_DIR`, but this sandbox has neither the
  files nor a mountable path to them (same constraint as ADR-0071). Running them for real, plus the
  hands-on LRC session below, is what promotes this ADR to Accepted.

## Adversarial review

A hostile review pass before merge found 6 issues; all 4 CONFIRMED findings were fixed (with a new
regression test each), the 2 SPECULATIVE findings were also fixed since they were cheap and real:

- **CONFIRMED**: a self-closing `<dc:subject/>`/`<lr:hierarchicalSubject/>` (no `rdf:Bag` at all)
  has no matching `Event::End`, so `lrc_fields.rs`'s `in_list` state was left stuck permanently on,
  silently dropping every later scalar in the document. Fixed by only entering list-tracking state
  on `Event::Start`, never `Event::Empty` — an empty container has no keywords to collect either
  way. See `self_closing_empty_list_container_does_not_leak_list_state`.
- **CONFIRMED**: `packet.rs`'s `matching_end` panicked on a truncated/unbalanced document (a real
  possibility — an LRC crash or disk-full mid-save, not just an adversarial input; `quick_xml`
  doesn't require balanced nesting and just runs to EOF). Fixed: returns `PatchError::Unbalanced`
  instead. See `malformed_truncated_xmp_returns_an_error_not_a_panic`.
- **CONFIRMED**: the ADR's "byte-preservation is proven" claim overstated what the code/tests
  actually established for the XMP-packet path (see the Measured results section above for the
  corrected, precise claim).
- **CONFIRMED**: the `crs:` write gate's calling contract (when `last_written_hash` updates, why it
  must come from the just-written bytes rather than a re-read) was undocumented. Fixed: documented
  on `should_write_crs` and demonstrated in `crs_write_gate_sequential_usage_pattern`.
- **SPECULATIVE, fixed anyway**: `hash_and_mtime` silently mapped a pre-1970/invalid mtime to `0`
  rather than erroring, which could bias the conflict rule. Now a real `io::Error`.
- **SPECULATIVE, fixed anyway**: patching keywords to `Some(vec![])` (explicit clear) inserted a
  needless empty `<dc:subject><rdf:Bag/></dc:subject>` instead of removing the container entirely.
  See `clearing_keywords_to_empty_removes_the_container_entirely`.

**A second review pass (CodeRabbit, on the pushed PR) found 6 more issues, all real, all fixed:**

- Element-form `Rating`/`Label` children weren't removed when a patch touched those fields, so a
  packet carrying both an attribute *and* a child element for the same property would have the old
  child value silently win back on the next read (`lrc_fields::read` processes the Description's
  attributes first, then any child element). Fixed the same way the `subject`/`hierarchicalSubject`
  blocks already were. See `patching_rating_removes_a_stale_element_form_child_that_would_otherwise_win_on_read`.
- `main.rs`'s `Command::Write` collapsed every `load_xmp` error (a sidecar that exists but fails to
  read — non-UTF-8, permissions — or a JPEG's read error) into the same "nothing here yet" case as
  a genuinely missing file, silently replacing real existing `crs:`/`exif:`/other content with a
  blank packet. Fixed: `load_xmp` now returns a typed `LoadedXmp::Existing`/`Missing`, and only
  `Missing` (confirmed `io::ErrorKind::NotFound`) falls back to a blank packet; every other error
  propagates.
- `hash_and_mtime` read a file's content (`fs::read`) and its metadata (`fs::metadata`) via two
  separate path-based calls — a real TOCTOU window if LRC replaces the sidecar in between, pairing
  the old content's hash with the new file's mtime. Fixed: both now come from one opened `File`
  handle.
- `should_write_crs`'s `None` (no prior Nicti write) case assumed "never written before" meant
  "safe to write" — but the sidecar could already carry real `crs:` content LRC itself wrote,
  unrelated to Nicti. Fixed: added `packet::has_crs_content` and a new
  `sidecar_already_has_crs_content` parameter — `None` is only safe when the sidecar is genuinely
  virgin (no `crs:` properties of any kind yet), not merely "Nicti hasn't written here."
- The `crs:` write gate's decision-summary doc (`docs/decisions/xmp-interop.md`) stated the gate
  "keeps... without risking clobbering an LRC edit," without qualifying that the hash comparison
  alone can't close the race between reading the hash and writing — `conflict.rs`'s own docs
  already specified the lock/single-writer discipline this requires; the summary now says so too.
- The research doc's own claims had drifted from the ADR's corrected wording (still said
  "byte-preserving packet patcher" and a stale "28" test count). Both brought back in sync.

**A third pass (CodeRabbit's re-review after the fixes above) found one more, also real:**

- `embedded.rs`'s "no existing XMP" insertion path always inserted the new APP1 segment
  immediately after SOI — but JFIF requires its own APP0 segment to be the very first marker after
  SOI. A JPEG with a JFIF APP0 segment and no existing XMP would have its APP1 inserted *ahead* of
  APP0, breaking that requirement for any reader that checks JFIF's exact prescribed marker order.
  Fixed: `write_xmp` now checks whether the second segment is a JFIF-signed APP0 and inserts after
  it instead, falling back to right-after-SOI for every other case (EXIF APP1, no APP0 at all,
  etc.), which has no such positional requirement. See
  `insert_when_no_existing_xmp_goes_after_a_jfif_app0_segment`.

Not flagged as bugs (already-acknowledged design assumptions): the single-`rdf:Description`
assumption, local-name-only namespace matching, and the DNG/TIFF write scope decision above.

## Options considered

- **`roxmltree`-only** (already a dependency) — rejected: no write support at all.
- **`little_exif`** — a real, already-cleared candidate; not adopted this pass mainly because its
  API is oriented around known EXIF/XMP tags rather than "preserve every unrecognized property,"
  which is this spike's central requirement. Worth a second look if a future need (e.g. writing
  EXIF alongside XMP in one pass) makes it a better fit than composing two separate crates.
- **Adobe XMP Toolkit SDK / `exiv2`/`rexiv2`** — both real, license-clear options; not adopted to
  avoid a new C/C++ build dependency when a pure-Rust crate satisfies the decision rule.
- **Byte-offset string splicing instead of an event-copy** — considered for `packet.rs`, rejected:
  `quick-xml`'s event API gives the same "leave everything else alone" guarantee with less
  fragile code than manual byte-range bookkeeping.

## What wasn't reachable this pass

Explicitly unverified, pending the follow-up hands-on LRC session (tracked as a new issue, part of
#11):

- Whether LRC's Reject flag actually reaches XMP as `xmp:Rating="-1"`, or isn't written to XMP at
  all — the `.lrcat` itself has no reject value in `shed-lrcat-schema.md`'s real data, which
  settles the catalog side but not the XMP side.
- Whether LRC's positive Pick flag reaches XMP under any field.
- What "Read Metadata from File" does when it encounters a Nicti-written field it doesn't
  recognize (the `nicti:` namespace, or a `crs:` property Nicti wrote) — does it warn, ignore, or
  show a conflict indicator?
- Whether a custom (renamed) color-label set changes the literal `xmp:Label` text LRC writes, or
  whether LRC keeps writing a fixed internal token regardless of display name.
- The "Automatically write changes into XMP" setting's actual behavior for JPEG/DNG (does it
  really behave the same as for a `.xmp` sidecar?).

## Consequences

- **Unblocks #60** (phase-1 coexistence build): the field mapping, library choice, and conflict/
  write-gate decisions above are what #60 implements against.
- **Feeds #62** (`.lrcat` import): the same `LrcMeta` shape this ADR defines is the natural landing
  spot for `shed`'s already-parsed `rating`/`pick`/`colorLabels`/keyword data.
- **Follow-up issue filed** (part of #11): a scripted hands-on LRC session (set rating/reject/pick/
  a custom label set/hierarchical keywords on throwaway file copies, save metadata, then inspect
  what actually landed in XMP) — this is the gate from Proposed to Accepted.
- **Explicitly deferred**: DNG/TIFF embedded-XMP write (see above); the `crs:` field-level mask
  mapping itself (masking's own territory, #48/ADR-0048); LRC's Pick-flag XMP mapping (unverified,
  above).

## Implemented in #60 (phase-1 coexistence)

The `scent` spike was promoted to `crates/nicti-scent` (spike deleted); `nicti-lair`'s
`scent_sync.rs` is the one place the catalog and XMP meet. **This ADR is still Proposed** -- the
Reject=`-1`, Pick and label-text mappings below are the best guess until #187's hands-on LRC pass
confirms or amends them; #60 builds against them as written.

- **Scope**: `.xmp` sidecars for NEF/NRW only (Scruff doesn't ingest JPEG/DNG yet, so the embedded
  JPEG writer is ported but has no caller). Layer (a) only -- rating/reject, label, keywords --
  plus Pick as `nicti:pick="1"` on the Description (no LRC-side mapping, per above). Layers (b)
  (`nicti:editDocument`) and (c) (`crs:`) are not wired.
- **Sides compare by meaning, not bytes**: both are reduced to a canonical marker set
  (`scent_sync::Markers`), so LRC reformatting a packet is never a conflict and an unchanged
  state never touches the file. A flat `dc:subject` entry that is the leaf of an
  `lr:hierarchicalSubject` path is the same keyword, not a second top-level one.
- **State**: schema v9 `asset_sidecar` (what nicti last saw and wrote, and
  `catalog_dirty_since_ms` -- when the catalog last diverged from the sidecar unwritten, which is
  the catalog side's "mtime" for the ADR-0021 rule, since the catalog has no metadata-modified
  time of its own).
- **Read path (ingest, and Synchronize Folder via Scruff's rescan)**: sidecar changed since last
  seen -> if the catalog isn't dirty the file wins; if it is, ADR-0021's newer-wins applies with
  `AMBIGUITY_WINDOW_MS = 2000` (inside it: flagged for review, neither side overwritten). No prior
  record: a pristine catalog yields to the file, a non-pristine one is flagged, never guessed.
- **Write path (after every marker change, auto-write on by default)**: stricter than the read
  path -- if the sidecar changed under nicti and that change was never ingested, the write is
  **held for review** rather than clobbering it. Writes patch the existing packet in place
  (foreign namespaces, including `crs:`, preserved), go through `atomic_write`, and record the
  hash of the bytes written. A sidecar that can't be read or parsed is an error, never a blank
  packet. One process-wide lock covers the read-hash -> gate -> write -> record sequence.
- **Known limits** (deliberate, tracked as follow-ups rather than fixed in #60):
  - Keywords are all written as `lr:hierarchicalSubject` paths, single-segment ones included, so a
    top-level and a nested keyword sharing a leaf stay distinct; whether LRC writes top-level
    keywords there too is unverified (#187). A `|` in a keyword name (the separator can't be
    escaped) is replaced by `/` on both sides, so it compares stable but is renamed on import.
  - `Markers` compares keyword names case-sensitively while the catalog folds case, so a
    case-only difference converges on the next write rather than flagging forever.
  - `apply_to_catalog` is not one transaction (rating/flag/label commit, then keywords); a
    failure partway leaves a partial apply until the next sync. Bad ratings are rejected at parse
    time so the known trigger is closed.
  - A rescan can write a sidecar with auto-write **off** when the catalog is dirty and newer
    (ADR-0021 newer-wins); turning auto-write back on doesn't flush already-dirty assets until
    their next marker change. A sync that lands between a marker write and the writer thread
    picking it up sees the catalog as clean.
  - The stat-based mtime precheck can miss a rewrite that preserves the recorded mtime
    (`rsync -t`, FAT's 2 s granularity).
  - `keyword_name_paths` reads the whole keyword table per tagged asset: O(tagged assets x
    keywords) per rescan. Only the first `rdf:Description` is patched while the reader merges all
    of them. The write-back only covers culling markers; there is no keyword-tagging UI yet.
  - A sidecar that uses `lr:`/`dc:` elements without declaring the namespace would get an unbound
    prefix if nicti adds keywords to it (LRC always declares them).
