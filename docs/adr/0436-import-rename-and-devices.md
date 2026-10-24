# ADR-0436: Import-time rename, date folders and DCIM device detection

**Status:** Proposed (no-go on lifting code; go on two slices; import-by-copy and in-place rename deferred pending a product call)
**Ticket:** #436
**Part of:** #424
**Related:** #342, #391, #392, #57, ADR-0026, ADR-0057

## Context

The LightCraft audit (#424) listed three files in `storytold/lightcraft` (MIT OR Apache-2.0) that might cover
ground Nicti lacks: `rename.rs` (1076 lines, batch rename with a token template, collision suffixing by file
identity, undoable), `import_move.rs` (354, a two-phase hard-link-or-verified-copy move that deletes the source
only after the catalog commit) and `devices.rs` (288, DCIM volume scan with a coalesced background rescan).

**What was read.** LightCraft at `265248c` (the audit's pin), plus the Nicti side: `nicti-preen/src/naming.rs`
(610) and `plan.rs`, `nicti-lair/src/carry.rs` (ADR-0026), `scruff.rs` (the ingest), and the `AssetFacts` /
`NewAsset` catalog model. None of LightCraft's code is copied, so there is no `docs/licensing.md` entry.

## What Nicti already has

- **Templates.** `naming.rs` (#57) has a token parser with errors (`Template::parse`), a separate subfolder
  template (`parse_subfolder`, split on `/` or `\` at render time), Windows-safe component sanitising, UTF-16 length
  truncation and a pure render function of `(AssetFacts, sequence)`. Its tokens are `Filename`, `Sequence:N`,
  `Date:<fmt>`, `Rating`, `Make`, `Model`, `Folder`. **Date folders are already covered**:
  `{Date:YYYY}/{Date:MM}/{Date:DD}` as a subfolder template is the LightCraft "date-folder template" feature.
- **Collisions.** `plan.rs` de-duplicates within a batch on a case-folded full path under every policy and
  `write.rs` re-checks at write time (`create_new` claim under `UniqueSuffix` and `Skip`; `Overwrite` renames over the
  existing file by design). Within a batch one photo never overwrites another under any policy.
- **Verified moves.** `Carry` (ADR-0026): BLAKE3 while streaming, a *re-read from the destination disk* and re-hash
  before the rename into place, a crash-recoverable `root_move` journal, and source deletion only after the catalog
  commit and only for files whose size and hash are unchanged. LightCraft's `import_move` verifies comparably
  (`copy_new` syncs, re-reads the destination and compares a 128-bit hash against the probe hash, or bytes as the
  fallback; never overwrites; rolls back on failure). Carry's real edge is the journal and crash recovery, which
  LightCraft lacks. LightCraft's Import->Copy mode (`copy_new`, never deletes the source) is the closest design
  reference for gap 3.

## What Nicti lacks (gap list)

| # | Gap | LightCraft has | Verdict |
|---|-----|----------------|---------|
| 1 | `{Lens}` and `{ISO}` tokens | `{lens}`, `{iso}`, `{camera}`, `{num}`, `{title}`, `{creator}` | **Go, but not free** (see below) |
| 2 | `{Num}` (trailing number of the original name, `IMG_0042` -> `0042`) | `{num}` | **Go, trivial**: derived from `stem` |
| 3 | Import by copy/move from a card into a library folder | `import_move.rs` | **Deferred**: needs a product call |
| 4 | Batch rename of files already in the library, catalog updated, undoable | `rename.rs` | **Deferred**, same call |
| 5 | Device (card) detection | `devices.rs` | **Go, as part of 3 only** |
| 6 | Collision identity by file identity, not name | `is_respelling` / `same_file` | **No-go for export; needed by 4** |

### 1. `{Lens}` / `{ISO}` need catalog columns first

`AssetFacts` carries make, model, capture time, rating and folder; `NewAsset` (the catalog row) has `make`,
`model`, `captured_at`, `width`, `height` and nothing for lens or ISO. A token that reads a field the catalog does
not hold would have to re-open the RAW at export time. The right slice is: extract `LensModel` and `ISOSpeedRatings`
in `scruff::read_exif_fields` (it already reads Make/Model/DateTimeOriginal through the `exif` crate; the IFD walker is
only for previews), add two nullable columns with a migration, then add the tokens. Lens and ISO may also live only
in the Nikon MakerNote for some bodies; unverified, check on real NEFs. **Backfill is not free:** `ingest_one` returns
early for a file whose size and mtime match its row (`scruff.rs`, before EXIF is read), so a plain re-scan never
fills the new columns. Slice 2 needs an explicit backfill pass (the `record_baseline_hashes` pattern #386 used) or a
"column is NULL, re-read EXIF" condition on that early return. Until then the tokens must not be advertised.
`{Camera}` (make + model) is sugar the existing `{Make} {Model}` already gives; not added.

### 2. `{Num}` is derivable

`AssetFacts.stem` is enough: take the trailing digit run, empty if none. No catalog change. Include it in the
same release as slice 2 so the token help list changes once.

### 3 and 4. Import-with-copy and in-place rename are one product decision

Nicti ingests **in place** (Scruff walks a registered root; nothing is copied). LightCraft's model is
"import = copy/move into a managed library", so its rename/place/devices code assumes a destination policy Nicti
does not have. Adopting any of it means choosing, as a product call for the operator:

- *Stay in-place.* Cards are copied with the OS (or a future "Copy from card" that reuses `Carry`'s verified
  copy for one folder), then the destination folder is added as a root. No import-time rename. Rename-in-place
  is then the only template consumer left, as feature 4.
- *Add import-by-copy.* A new job copies from a card into `root/<subfolder template>/<filename template>`,
  hashes while copying (the stream-hash-reread pattern of `Carry`, which is private to its `step` state machine
  and tied to the `root_move` journal, so a per-file copy-verify function has to be extracted first), registers the
  asset with the content hash already filled in, and only then offers to delete the card files. This reuses
  `naming.rs` unchanged apart from gap 1/2.

**Recommendation:** the second, as a *copy-only* import (card files are never deleted by Nicti; the user formats
the card), because it gives the verified-copy value without ever being the only copy-holder, avoids LightCraft's
rollback and source-delete machinery (Move mode), and fits "no unverified deletes" in ADR-0026. It is gated on a ticket
from the operator; this ADR does not start it. In-place rename (4) is a separate, later ticket: it needs an
undo story Nicti's catalog does not have for file moves (history is for edits, `edit_history`, #324).

### 5. Device detection

Only worth building with 3. The design to take, not the code: a cached list served immediately, one coalesced
background rescan after a `MAX_AGE`, an `on_change` repaint hook, and `DCIM` matched case-insensitively.
LightCraft scans `/Volumes`, `/media/$USER`, `/run/media/$USER`, `/media`, `/mnt` and, on Windows, drive letters.
Windows is Nicti's required CI platform (`CLAUDE.md`), so the Windows branch matters most and the rest are
nice-to-have. A volume that is mid-spin-up can block a `read_dir`, so the scan must stay off the UI thread
(LightCraft does this; the draw loop never touches disk).

### 6. Collision identity

LightCraft decides "is the existing target the photo itself" by file identity, so a case-only rename on a
case-insensitive volume works and `img_1.jpg` beside `IMG_1.JPG` on a case-sensitive one is a real collision.
Nicti's case-folded de-dupe is deliberately conservative: it can add an unneeded `-1` suffix on a
case-sensitive output folder, which is harmless, and never overwrites. For export there is nothing to gain. If
feature 4 is ever built, it **must** use identity (`same_file` / inode+volume or `GetFileInformationByHandle`),
since a rename-in-place has the file itself as the "collision".

## Alternatives rejected

- **Lift `rename.rs` / `import_move.rs`.** They are written against LightCraft's `Session`, `Op::SetFile` and
  `Photo` types, and its import model differs from Nicti's in-place model.
- **Adopt LightCraft's hard-link placement.** It only links when source and destination share a volume (never
  card -> internal disk, never FAT/exFAT) and only in Move mode, which Nicti does not offer; copy-only needs none.
- **Add the tokens now with empty values.** A token that silently renders blank produces colliding names.

## Provenance and safety checks

- Code is agent-written and the repo says its checkmarks are unverified; every behaviour above was re-derived from
  the Nicti side, not trusted from the LightCraft docs.
- Any import job reads external, possibly hostile card contents: names go through `sanitize_component`, never
  used as paths, symlinks on the card are not followed (LightCraft's `is_symlink` check is worth re-deriving),
  and the destination must be checked to stay under the chosen root (`inside`).

## Consequences

- **Slices (go):**
  1. `{Num}` token (falls back to empty when the stem has no trailing digits). **XS**
  2. Lens/ISO extraction in `scruff::read_exif_fields`, columns, migration, a backfill pass, `AssetFacts` fields,
     tokens, token help. **M**
- **Needs an operator decision before any ticket:** copy-only import from card (+ device list), then in-place
  rename. #342's folder picker is a prerequisite for the import destination UI.
- **Not taken:** LightCraft code, its hard-link placement, its source-deleting move, its undoable `SetFile` op.
- **Nothing in the archive-drive work (#391/#392) depends on this.**
