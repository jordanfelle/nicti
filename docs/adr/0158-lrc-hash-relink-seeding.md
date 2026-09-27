# ADR-0158: LRC `md5`/`importHash` vs homing's relink fingerprints

- **Status:** Accepted
- **Date:** 2026-09-26
- **Ticket:** [#158](https://github.com/jordanfelle/nicti/issues/158) Cross-check
  `AgLibraryFile.md5`/`importHash` against `spikes/homing`'s relink fingerprints

## Context

ADR-0061 (#61, the `.lrcat` schema mapping) flagged `AgLibraryFile.md5`/`importHash` as candidate
inputs for ADR-0071's (#71) relink tiers (b)/(a) respectively, but never checked that claim against
`spikes/homing`'s actual fingerprint code — a follow-up this ADR resolves.

`spikes/homing/src/fingerprint.rs` defines four relink tiers, cheapest first:

- (a) `size_name_key` — size + file name, no content hash at all.
- (b) `partial_hash` — BLAKE3 over the file's length plus its first and last 64KB.
- (c) `full_hash` — full-file BLAKE3, deliberately excluded from routine import because LRC
  rewrites DNGs in place, which would silently invalidate it.
- (d) `natural_key` — an EXIF-derived identity, independent of file bytes.

None of these is MD5. **An MD5 value cannot seed tier (b) or (c) directly** — different hash
algorithm entirely, not a format conversion. If `md5` is a full-file hash (as its name suggests),
it is closer in kind to tier (c), the one already excluded from import for the same DNG-rewrite
reason. Whether it's worth carrying anyway — as a cheap duplicate-detection hint, or as pure
provenance — depends on facts only the real catalog and its real files can answer:

- How complete is `md5` (NULL rate)? Does it vary by extension (esp. DNG vs. NEF)?
- Is it genuinely a full-file hash, confirmed by recomputing MD5 against real sampled files?
- What is `importHash`, structurally (looks like a per-file id, or a per-import-session id)?

`spikes/shed` gained two new subcommands for this pass: `hash-stats` (aggregate shape only — NULL
rate, length, character-class, distinctness — never a raw value) and `verify-md5` (recomputes a
full-file MD5, and homing's own tier-(b) partial BLAKE3 as a bonus cost-table data point, against a
random sample of real files the catalog points at).

**Privacy note (this repo is public):** every number below is a count or a shape description
(e.g. "32 lowercase hex"), never a raw hash value, path, or filename. `shed privacy-check` was run
against every file this PR touches before each commit, same posture as ADR-0061.

## Decision

**`md5` cannot seed any relink tier — it is never populated at all.** `shed hash-stats` against the
real 380,298-row `AgLibraryFile` table found **`md5` NULL on every single row, with zero exceptions,
including split by every real extension present (DNG/NEF/JPG)**. This isn't a completeness gap to
work around; there is nothing there to use. `shed verify-md5 --sample 20`'s `WHERE md5 IS NOT NULL`
query correctly matched 0 rows, confirming `hash-stats`' own count rather than adding new
information. **#62 must not plan on `md5` as a relink input in any capacity** — not a seed, not a
hint, not even provenance (there's no value to preserve).

This makes the original ADR-0061 Q1 pairing doubly wrong: not only was `md5`↔tier-(b) wrong in kind
(a full-file-shaped hash can't seed a partial hash covering only 64KB from the head and 64KB from
the tail), the column this repo's own
LRC installation actually writes to disk is never non-NULL in the first place. Whatever real-world
condition fills LRC's `md5` column (a "Validate DNG"-only feature per secondary sources, never run
here) doesn't apply to a working library that's just imported and used normally — the case #62 has
to import.

**`importHash` is present (99.97%, 380,182/380,298 non-NULL) and every non-NULL value is
distinct** — `distinct_count` equals `non_empty_count` exactly, and every duplicate-group size in
the top-20 is 1. This is the shape of a per-file identifier, not a per-import-session token (which
would show many files sharing one value). Its exact derivation is undocumented and unconfirmed —
`shed`'s privacy-safe shape report can describe character class and length but can't reverse-engineer
an opaque LRC-internal format from that alone, and the length distribution splits across two
clear bands (27.8% at 30-52 characters, dominant at 31/34; 72.2% at 62-67 characters, dominant at
63/64), suggesting two different generations of the format across this catalog's import history
rather than one consistent scheme.
**Given the algorithm is unconfirmed, #62 should treat `importHash` as opaque provenance only** —
worth keeping in the raw-LRC-text provenance blob (ADR-0061 Q4's existing policy already covers
this), but not worth building any content-identity logic on top of without first confirming what it
actually hashes, which is out of this ADR's scope.

Net effect for #62: seed neither BLAKE3 tier from LRC data. Compute tier (b) (`partial_hash`) fresh
at import time, exactly as ADR-0071 already planned, at the real per-file cost `verify-md5`'s
`partial_hash_ms` field was built to measure (unmeasured on this catalog's real files since no
`md5`-bearing row existed to sample against — a real, unmeasured cost, not one this ADR reports a
number for; a future import-time timing pass against the real files directly, without needing `md5`
at all, can measure it).

**A related gap `md5`'s 100%-NULL rate left initially unverified, not just unmeasured**: `verify-
md5`'s `sample_rows` resolves each catalog row's real on-disk path by joining
`AgLibraryRootFolder.absolutePath` + `AgLibraryFolder.pathFromRoot` + `AgLibraryFile.baseName` +
`.extension`, exactly the join #62's importer will eventually need for every file, not only
`md5`-bearing ones. Because `WHERE md5 IS NOT NULL` matched zero real rows, this join ran
end-to-end only against synthetic test fixtures, and a pre-PR adversarial review caught that the
original naive string concatenation assumed `pathFromRoot` always ends in a separator — untrue for
a nested folder, it would glue the folder's last segment onto the base name instead of inserting
one (`"sub/dirIMG_0001.nef"` instead of `"sub/dir/IMG_0001.nef"`), silently (a garbage path just
falls into `missing_on_disk` rather than erroring). Fixed: `join_lrc_path` now normalizes each
segment's own separators before joining, correct regardless of which convention `pathFromRoot`
actually follows, with unit tests for both the with- and without-trailing-separator cases plus an
end-to-end test against a real nested directory. What's still genuinely unconfirmed is only which
convention the real catalog's `pathFromRoot` follows in practice (immaterial now that the join
handles either), and whether some other real-world case beyond nesting — a name containing a
literal path-separator character, for instance — could still defeat it; #62 should still verify
against the real catalog before relying on this exact function, but the join is no longer a known
gap, just an unconfirmed one.

## Consequences

- **#62** must not plan on `AgLibraryFile.md5` as a relink or dedupe input in any role — it's
  absent from this catalog entirely. `importHash` is worth preserving as opaque provenance
  (ADR-0061 Q4's provenance-blob policy already covers this) but not worth building relink or
  dedupe logic on top of until its derivation is independently confirmed.
- Corrects ADR-0061 Q1's inaccurate tier pairing (`md5`→(b), `importHash`→(a)) — see that ADR's
  Consequences section, updated to point here.
- `spikes/homing/src/fingerprint.rs`'s module doc comment is corrected: it described tier (a) as
  "size+mtime+name," but `SizeNameKey` has never carried an `mtime` field.
- **New follow-up** (`**Part of:** #11`): if `importHash`'s real derivation is ever confirmed (e.g.
  by finding Adobe's own documentation or reverse-engineering it against a small controlled test
  catalog), re-evaluate whether it's usable for de-duplication independent of relink.
- `join_lrc_path`'s root/folder/file path-join is now separator-convention-agnostic (fixed during
  this PR's own adversarial review, not a follow-up) — #62 can still choose to verify it against
  the real catalog's actual `pathFromRoot` values for extra confidence, but it isn't a known gap.

## Measured results

All numbers below are `shed hash-stats`/`shed verify-md5`'s real output against the same
380,300-asset catalog backup ADR-0061 used (extracted read-only from the user's own backup zip,
queried, then deleted immediately afterward — never the live working copy, never committed).
Aggregates only, per this repo's public-data policy; no raw hash value, path, or filename appears
anywhere in this ADR.

- `AgLibraryFile` row count: 380,298 — matches ADR-0061's own count exactly, a real cross-check
  that these two research passes measured the same table.
- `md5`: 380,298/380,298 NULL (100%), across every real extension present (37,418 DNG / 68,381 NEF
  / 274,499 JPG rows, all-NULL in every group). Zero non-NULL values to describe shape/length/
  distinctness for.
- `importHash`: 380,182/380,298 non-NULL (99.97%, 116 NULL), 380,182/380,182 distinct (0 duplicate
  groups of any size). Length distribution clusters in two bands: 105,696 rows (27.8%) at 30-52
  characters (dominant lengths 31 and 34) and 274,486 rows (72.2%) at 62-67 characters (dominant
  lengths 63 and 64) — real, measured, not a guessed split.
- `shed verify-md5 --sample 20`: 0 sampled (0 candidate rows exist with non-NULL `md5`), 0
  matched/mismatched/missing, confirming `hash-stats`' NULL-rate finding by an independent query
  path rather than adding new information.
- `cargo test -p shed`: 38/38 unit tests pass (35 lib + 3 bin), including 18 new tests for this
  module (`hashes::tests::*`) covering shape masking (including a regression test for a real bug an
  adversarial review caught, below), NULL/empty/duplicate detection, per-extension splitting, the
  WSL path mapping and root/folder/file join logic (including both trailing- and
  non-trailing-separator cases, another real bug the same review caught), a known-vector MD5
  check, a real on-disk match, a real mismatch, a real missing-file case, and a real hash-error
  case for `verify_sample` — all against synthetic fixtures, never the real catalog.
- **`spikes/shed` no longer depends on `spikes/homing`.** An early draft of `verify-md5` called
  `homing::fingerprint::partial_hash` directly via a `path = "../homing"` dependency; a pre-PR
  adversarial review caught that this violates CONTRIBUTING.md's spike policy ("don't build on top
  of a spike... expected to be deleted once its own ticket promotes the working logic elsewhere") —
  `verify-md5` would have silently broken the moment `homing` is deleted or promoted. Fixed by
  duplicating the ~15-line tier-(b) BLAKE3 function locally in `hashes.rs` instead.
- **The same review caught a real dead-code bug in `shape_of`**: it checked the hex-digit branch
  before the pure-digit branch, and every ASCII digit is also a valid hex digit, so a purely
  numeric value would have been silently misreported as hex rather than as digits. Didn't affect
  this ADR's own reported numbers (no `importHash` value in the real catalog was purely hex or
  purely numeric — every real shape fell into the "other" bucket), but fixed and regression-tested
  regardless.
- **The same review also caught a privacy-claim gap**: a hashing failure on a sampled file (e.g. a
  permission error, or the file vanishing between the existence check and the read) would have
  propagated an error containing that file's real path up through `main()`'s default error
  printer — directly contradicting this module's own "never a raw value" claim. Fixed: hashing
  errors are now counted (`hash_error`) rather than propagated, with a regression test proving
  `verify_sample` returns `Ok`, not `Err`, on such a failure.
- **The same review also caught the `join_lrc_path` bug described in the Decision section above**:
  naive concatenation assumed `pathFromRoot` always ends in a separator, which a nested folder
  without one would silently defeat. Fixed and regression-tested for both cases, plus an
  end-to-end test against a real nested directory.
- `cargo clippy -p shed --all-targets -- -D warnings` and the full workspace `cargo clippy
  --workspace --exclude den --exclude pelt-egui --exclude pelt-iced --exclude pelt-slint --exclude
  retina --all-targets --all-features -- -D warnings` / matching `cargo test --workspace`: both
  clean.
- `cargo fmt --all -- --check`: clean.
- `cargo deny --workspace --all-features check licenses`: `licenses ok` — `md5` (Apache-2.0 OR MIT)
  needed no new `deny.toml` entry, both arms already allowlisted.
