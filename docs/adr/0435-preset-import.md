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
- **Masks, curves, profile refs.** `develop/masks.rs`, `grade.rs`, `profile.rs` (ADR-0381) exist for the Lua form.
- **Zip.** `nicti-stalk` already depends on `zip = "9"` (deflate via `flate2` zlib-rs, ADR-0018-checked).

So LightCraft's reusable value reduces to ideas, not code.

## Decision

### 1. `.lrtemplate`: go, via `agprefs` + `translate`; lift nothing

Read the file with `agprefs`, take `value.settings`, run it through `translate` (re-serialising the sub-table
or adding a `translate_value(&Value)` entry point; whichever is smaller is a slice-1 detail). Title comes from
`title` / `internalName`. The result is converted `EditDocument` -> `DevelopPreset.stages`.
LightCraft's 350-line Lua parser is not needed and is not lifted.

Check in slice 1: that `agprefs` accepts the `.lrtemplate` outer form (`s = { id = ..., value = {...} }`) as it
does the catalog's `s = { ... }`; if it does not, strip the wrapper, still no new parser.

### 2. `.xmp` presets and photos-with-XMP: go, with one new adapter

XMP differs from the Lua form: scalars are strings (`"+0.50"`, `"True"`), curves are `rdf:Seq` of `"x, y"`,
masks are nested `crs:MaskGroupBasedCorrections` structures. A `crs:` XMP reader in `nicti-scent` (it
already has `quick-xml` and the attribute-vs-element handling, `lrc_fields.rs`) yields a key/value tree;
the adapter turns that into the same `agprefs::Value` shape `translate` takes, so mapping logic exists once.
Preset-specific rules, taken from LightCraft (the idea, not the code): when the source is a *photo's* XMP
rather than a preset, drop crop/geometry and absolute white balance (they belong to the photo, not a look);
ignore the packet-bookkeeping keys (`crs:Version`, `crs:UUID`, `crs:PresetType`, `crs:Supports*`, ...);
use `crs:Name` / `crs:Group`; id from `crs:UUID` lower-cased, else a slug of group+name.
Lightroom mobile "DNG presets" are an embedded-XMP read (`nicti-scent/embedded.rs`), cheap once this lands.

### 3. Zip bundles: go, use the `zip` crate, keep LightCraft's limits

Preset packs ship as zips. Use `zip` (already a dependency; no hand-rolled reader: LightCraft's is ~70 lines
of manual offset arithmetic, no zip64, and its only safety is that it was written carefully). Keep the
*limits* as policy: reject an entry that unpacks past 256 MiB and an archive past 1 GiB, enforced while
streaming (not trusting the stated size), skip directories, `__MACOSX` and dot-files. Entry names are only
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

Parsing of `.lrtemplate` and the zip walk belong with the importers (`nicti-stray`, which owns `translate`);
the `crs:` XMP reader belongs in `nicti-scent`. The pelt "Import presets..." action is a thin caller. Check
`scripts/check-repo-policy.py`'s layering table before adding any edge; no GUI dependency is needed.

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
  1. `.lrtemplate` -> `DevelopPreset` through `translate`, with fixtures and the unmapped report. **S**
  2. Zip bundles with limits (including a bomb test). **S**
  3. `crs:` XMP preset reader + adapter, photo-XMP stripping rules, DNG-embedded XMP. **M**
  4. Pelt "Import presets..." action and result summary. **S**
- **Unmapped is part of the contract.** Presets with masks, AI tools or profiles Nicti lacks import partially,
  and the user is told what was dropped (reuse `Translation.untranslated`).
- **PV2003/2010 presets** are not translated (as for catalogs, `legacy_process_version`); they import empty
  and are reported, not guessed.
- **Slice PRs** need the adversarial review and a manual security pass (external file input, archives).
- **Deferred:** Luminar; Capture One / DxO style packs; exporting Nicti presets back to `.xmp`.
