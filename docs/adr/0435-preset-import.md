# ADR-0435: Preset import (.xmp, .lrtemplate, zip bundles; Luminar declined)

**Status:** Proposed (go for three formats, phased; Luminar no-go; nothing built yet)
**Ticket:** #435
**Part of:** #424
**Related:** #216, #381, #383, #52, ADR-0061, ADR-0381

## Context

Nicti imports `.lrcat` catalogs (`nicti-stray`) and reads/writes XMP sidecars (`nicti-scent`), but a user
who moves over from another editor cannot bring their *presets*. Nicti's own presets (#52,
`nicti-pelt/src/knead/presets.rs`) are `DevelopPreset { name, stages: BTreeMap<String, StageEntry> }` in a
`<catalog>.develop-presets.json` sibling file.

The LightCraft audit (#424) pointed at four import paths in `storytold/lightcraft` (MIT OR Apache-2.0):
`preset_import.rs` (hand-written zip reader, Lua-table parser, XMP-in-file extraction, group naming),
`preset_luminar.rs` (plist parser and slider mapping for `.lmp` / `.mplumpack`), `crs.rs` / `crs_masks.rs`
(`crs:` field mapping), and `docs/xmp-interop.md`.

**What was read.** The sources at LightCraft `4ae30894` (2026-10-23). The audit's pinned `265248c` is not
reachable in a shallow clone, and the files have grown since (`preset_import.rs` 845 lines, `crs.rs` 911,
`crs_masks.rs` 782; the issue quotes 744/542/379). Findings below are about the code as read; none of it is
copied, so there is no `docs/licensing.md` entry.

## What Nicti already has (the deciding fact)

LightCraft needs its own Lua parser and its own `crs:` -> settings mapping because it had neither. Nicti has both:

- **Lua.** `nicti_stray::develop::translate(text, &Context) -> Translation` parses LRC's develop-settings Lua
  with the `agprefs` crate (data-only, never executes Lua; 0 failures on 380,307 real catalog rows) and maps it to
  an `EditDocument`. An `.lrtemplate`'s `value.settings` table is the same key set (`Exposure2012`,
  `ToneCurvePV2012`, `HueAdjustmentRed`, `MaskGroupBasedCorrections`, ...) in the same Lua syntax.
- **Report.** `Translation.untranslated` + `Stats` already tell the user what could not be mapped. This is
  the `Imported.unmapped` UX pattern the issue singles out; Nicti has it, built on 380k real rows.
- **Masks, curves, profile names.** `develop/masks.rs`, `grade.rs`, `profile.rs` (ADR-0381) exist for the Lua form. The camera profile / Look name comes back as `Translation.camera_profile`, *outside* the `EditDocument`, and is resolved by the import job, so a preset has nowhere to keep it today (see section 6).
- **Zip.** `nicti-stalk` already depends on `zip = "9"` (deflate via `flate2` zlib-rs, ADR-0018-checked).

So LightCraft's reusable value reduces to ideas, not code.

## Decision

### 1. `.lrtemplate`: go, via `agprefs` + `translate`; lift nothing

Read the file with `agprefs`, take `value.settings`, run it through `translate` (re-serialising the sub-table
or adding a `translate_value(&Value)` entry point; whichever is smaller is a slice-1 detail). Title comes from
`title` / `internalName`, with Adobe's `$$$/Key=Default text` localisation form reduced to its default text
(LightCraft's `delocalize`; the idea is worth re-deriving). The result is converted `EditDocument` -> `DevelopPreset.stages`.
LightCraft's 350-line Lua parser is not needed and is not lifted.

`agprefs` accepts the `s = { ... }` outer form and `Agpref.values` is a struct, so `.value.settings` is reachable;
`Tx.root` is a `&Value`, so a `translate_value` entry point is small. **The process version must be passed:**
`translate` decides PV2003/2010 only from `Context.process_version`, so the importer fills it from the
template's `ProcessVersion`. With a default `Context` a legacy preset would map `Saturation` etc. through the PV2012
keys and import *wrong*, not empty.

### 2. `.xmp` presets and photos-with-XMP: go, with one new adapter

XMP differs from the Lua form: scalars are strings (`"+0.50"`, `"True"`), curves are `rdf:Seq` of `"x, y"`,
masks are nested `crs:MaskGroupBasedCorrections` structures. A `crs:` XMP reader in `nicti-scent` (it
already has `quick-xml` and the attribute-vs-element handling, `lrc_fields.rs`) yields a key/value tree;
the adapter turns that into the same `agprefs::Value` shape `translate` takes, so mapping logic exists once.
Preset-specific rules, taken from LightCraft (the idea, not the code): when the source is a *photo's* XMP
rather than a preset, drop crop/geometry and absolute white balance (they belong to the photo, not a look);
ignore the packet-bookkeeping keys (`crs:Version`, `crs:UUID`, `crs:PresetType`, `crs:Supports*`, ...);
use `crs:Name` / `crs:Group`; id from `crs:UUID` lower-cased, else a slug of group+name.
Lightroom mobile "DNG presets" are **not** cheap: `nicti-scent/embedded.rs` is JPEG APP1 only (DNG/TIFF tag 700 is
explicitly out of scope there; a read would reuse `spikes/sniff`'s TIFF walker). They are split out as a separate,
later slice. The `crs:` property reading lives in `nicti-scent/packet.rs`, not `lrc_fields.rs` (rating/label/keywords only).
LightCraft strips crop/geometry and custom/as-shot white balance only for XMP embedded in a *photo*
(dng/jpg/tif); a sidecar `.xmp` is imported whole. Nicti follows that split: there is no way to tell a preset `.xmp` from a
sidecar by content, so the rule is by source (embedded vs file), not guessed.

### 3. Zip bundles: go, use the `zip` crate, keep LightCraft's limits

Preset packs ship as zips. Use `zip` (already a dependency; no hand-rolled reader: LightCraft's is ~70 lines
of manual offset arithmetic, no zip64; it does use checked arithmetic, bounded inflate and has a bomb test, so the
objection is maintenance and zip64, not safety). Keep the
*limits* as policy: reject an entry that unpacks past 256 MiB and an archive past 1 GiB *unpacked* in total, enforced while
streaming (not trusting the stated size), plus an entry-count cap (LightCraft has none; a 1 GiB archive can hold ~65k
tiny files); skip directories, `__MACOSX` and dot-prefixed names (not a leading `./`). Entry names are only
used to pick an extension and a group name and are **never** used as a filesystem path (no extraction to
disk; read into memory), so there is no zip-slip surface. Nested zips are not followed. Group name = innermost
folder that is not a generic container (`Lightroom`, `XMP`, `Presets`, ...): LightCraft's filler-word list is a
good idea to re-derive.

### 4. Luminar `.lmp` / `.mplumpack`: no-go for now

- The mapping table is black-box guesswork by the other project's own account ("freely distributed looks";
  Exposure "read as ±100 <-> ±4 EV" is an assumption). Nothing here can be validated without Luminar and its
  looks, which we should not redistribute as fixtures.
- Most of a Luminar look is AI tools, Orton/glow and LUT resources that are reported as unmapped, so what
  survives is a handful of basic sliders.
- Cost is a plist parser plus a mapping table to own, for a format Skylum can change.

Revisit only if a user asks (file a ticket then). The plist-parser idea needs no decision now.

### 5. Where it lives

`LAYERS` (`scripts/check-repo-policy.py`) puts `nicti-scent` at 0, `nicti-stalk` at 1, `nicti-stray` at 5 and
`nicti-pelt` at 6. `DevelopPreset` lives in pelt, above stray, so:
- `nicti-stray` owns `.lrtemplate` -> `EditDocument` (via `translate_value`) and the zip walk; it must add `zip`
  itself with `nicti-stalk`'s feature set (`default-features = false`, deflate only), since stalk exports no zip API.
- The `crs:` XMP reader needs a crate home: `nicti-scent` has `quick-xml` and `packet.rs`, but no `agprefs`, and stray
  has no scent dependency. Slice 3 picks one edge (stray -> scent is allowed by `LAYERS`) or a plain key/value
  tree type that scent returns and stray converts; no new GUI dependency either way.
- `EditDocument` -> `DevelopPreset.stages` and the "Import presets..." action live in pelt.

### 6. Preset semantics the importer must decide (not yet solved)

- **Presets replace whole stages.** `Tx::put` emits a stage only when it differs from default, and
  `Clipboard::apply` (`knead/mod.rs`) inserts a stage wholesale. A preset containing only `Highlights2012 = -50`
  becomes a full `TONE` stage and applying it resets the target's contrast/shadows/whites/blacks; a preset with masks
  replaces all of the target's masks. Slice 1 must either merge at slider level or restrict stage emission to the
  keys the template actually names, and document the choice.
- **Strength.** `crs:Amount` / `SupportsAmount` (preset strength) has no Nicti counterpart; it is ignored and
  reported, not silently applied at 100%.
- **Looks / camera profile.** `Translation.camera_profile` has no slot in `DevelopPreset`. Adobe "Look" presets are
  mostly a profile reference, so slice 1 adds an optional profile name to the preset and resolves it at apply time via
  `ProfileResolver`, or reports it as unmapped; it must not drop it silently.
- **Crop / heal / WB in `.lrtemplate`.** With no frame size in `Context`, crops and heal spots are counted as skipped
  (good); absolute white balance would import as a stage and is stripped for presets, as LightCraft does for photos.

## Provenance and safety checks (done)

- LightCraft's "clean-room, black-box" claim is self-attested and the code is agent-written. Nicti takes no
  code, so the claim does not matter for licensing; it does matter for *behavioural* trust, which is why each
  mapping below is re-validated rather than copied.
- No Adobe-derived data bundled: the preset code carries no embedded tables of third-party presets, and
  LightCraft's `assets/ATTRIBUTION.md` states no Adobe assets. The only literals are key names and the group
  filler words.
- The format keys (`Exposure2012`, ...) are the same names `nicti-stray` already maps; no new naming risk.
- Fixtures for tests must be written by us (a handwritten `.lrtemplate`, XMP preset, zip of both). No
  third-party preset pack is committed.

## Alternatives rejected

- **Lift `preset_import.rs` wholesale.** Duplicates a Lua parser and a `crs:` mapping Nicti has, with a second
  develop model on the other end of it.
- **Hand-written zip reader.** `zip` is already vetted and handles zip64.
- **Support Luminar now.** See section 4.
- **Execute the Lua** to evaluate templates. Never; data-only parsing is the point of `agprefs`.

## Consequences

- **Slices:**
  1. `.lrtemplate` -> `DevelopPreset` through `translate_value`, process version passed, section 6 decisions,
     fixtures and the unmapped report. **M**
  2. Zip bundles with limits (including a bomb test). **S**
  3. `crs:` XMP preset reader + adapter, photo-XMP stripping rules. **M**
  3b. DNG-embedded XMP (TIFF tag 700 reader). **M**, optional
  4. Pelt "Import presets..." action and result summary. **S**
- **Unmapped is part of the contract.** Presets with masks, AI tools or profiles Nicti lacks import partially,
  and the user is told what was dropped (reuse `Translation.untranslated`).
- **PV2003/2010 presets** are not translated (as for catalogs, `legacy_process_version`); they import empty
  and are reported, not guessed.
- **Slice PRs** need the adversarial review and a manual security pass (external file input, archives).
- **Deferred:** Luminar; Capture One / DxO style packs; exporting Nicti presets back to `.xmp`.
