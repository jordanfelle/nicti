# ADR-0441: LightCraft small utilities: two build slices, five declined

**Status:** Proposed (library-open failure UX and crop-to-rotated-image constraint: build; container walkers, smart-album rule editor, availability cache, XMP merge, maker-note locator: no build, reasons below)
**Ticket:** #441
**Part of:** #424
**Related:** #371, #383, #155, #332, #327, #375, #272, #379, #239, #368, #557, #558, ADR-0071

## Context

The LightCraft audit (#424) bundled seven small pieces of `storytold/lightcraft` (MIT OR Apache-2.0) that might fill
gaps in Nicti. Read at `23f5480` (the audit was at `265248c`; line counts below are from `23f5480` and differ a little
from the ticket's). Nothing is copied by this ADR, so there is no `docs/licensing.md` entry. If a later ticket does port
code, it adds the row then. Each piece was read against the Nicti code that would take its place.

## Decision

### 1. Library-open failure UX: build (#558)

LightCraft's `panels/library_problem.rs` is a blocking window (path, error, Try Again / Choose Another / Continue
Without Saving / Quit) that turns into a banner while a session is temporary.

Nicti on a failed open (`app.rs`, `CatalogOpenState::Error`): `show_library` draws the heading and the controls and
returns, and the settings column shows one red "Failed to open catalog ..." label. There is no retry and no way to pick
another catalog; the user fixes the file and relaunches. `catalog::resolve_path`'s own comment says a catalog picker is
"left to whichever ticket adds catalog-picker UI", so this is the gap.

Take the shape (blocking window, three exits, a pure transition table that tests without egui), not the code.
**Drop "Continue Without Saving".** It exists because LightCraft has an in-memory library; Nicti does not, and a silent
unsaved session is the failure the window is meant to prevent.

### 2. Crop constrained to the rotated image: build (#557)

LightCraft's `geom/src/crop.rs` has `max_inscribed_scale` (largest centred crop of a given aspect that fits a `w x h`
image rotated by `a`), `crop_fit_angle`, and `constrained` (pulls an off-centre crop back inside). Nicti's
`nicti_tapetum::geometry` clamps the angle (`clamp_rotation_degrees`) and builds the affine, but nothing keeps the crop
inside the rotated frame, and sampling clamps to the edge (`out_of_bounds_sample_clamps_to_the_edge_not_black`), so a
rotated crop that pokes out shows smeared edge pixels. The corner test in `max_inscribed_scale` is the standard one
(`|x|cos a + |y|sin a <= w/2`, `|x|sin a + |y|cos a <= h/2`) and is not worth porting line for line: re-derive it with a
property test (result fits, 1% larger does not), which is what LightCraft's own test does. Only `max_inscribed_scale`
covers centred crops; the off-centre clamp is a separate piece of work in the same ticket. #379 (LRC `CropAngle` on
import) may need the same helper, and the ticket says to check that first. The ticket also has to decide what happens to saved or imported
`CropParams` that already poke out of the rotated frame (clamp on load, or only on the next edit); the develop panel
only guards against stale persisted crops during an edge drag (`develop_panel.rs`, the `start_rect` comment; it clamps each edge to the unrotated source rect). That is not a rotated-frame constraint, so it does not answer the question.

### 3. Container walkers (`container.rs`, `iptc.rs`): no build

`container.rs` walks JPEG segments and PNG/WebP chunks for ICC/EXIF/XMP; `iptc.rs` parses IPTC-IIM record 2.

- **JPEG XMP** is already read and written by `nicti-scent` (`embedded.rs`, JPEG APP1), which is what #371 extends. A
  second JPEG walker would duplicate it.
- **PNG and WebP** are not inputs to a RAW editor and DAM.
- **IPTC (#383)**: the LRC importer reads IPTC from the *catalog* (`AgLibraryIPTC` in `nicti-stray/src/read.rs`:
  caption, copyright), not from files. File-level IPTC only matters once JPEG ingestion exists, which #371 is still
  waiting on.

If file IPTC is ever wanted, `iptc.rs` (124 lines) is the reference: a `0x1C record dataset size` stream, UTF-8 when
record 1:90 is `ESC % G`, otherwise UTF-8-if-valid else Latin-1. Re-derive from the IPTC-IIM spec. No ticket until
JPEG ingestion lands.

### 4. Smart-album rule editor (`rules_editor.rs`, `rules.rs`): no build, shape noted for #155

LightCraft rules are `{field, op, value: serde_json::Value}` in nested all/any/none groups. Nicti smart collections are
different by design: membership is never stored; a smart collection is a saved `hunt::Filter` resolved through
`CatalogStore::hunt` (`nicti-lair/src/clowder.rs`), edited through the filter bar. That model is typed already, so the
ticket's advice (typed enums rather than `Value`) is satisfied.

What the filter bar does not have is OR/NOT nesting. Whether it needs it depends on #155's real LRC smart-collection
example, which does not exist yet (#155: "needs a real smart-collection example to measure against"). LRC's rules combine
all/any/none, so nesting may turn out to matter. Until #155 has data, building a rule editor would be designing against
a guess. When it does, the transferable parts are the match mode (`all`/`any`/`none`), the field-kind to operator table,
and the "in the last N" and "between" date editors; the code is not.

### 5. Availability cache (`availability.rs`): no build

LightCraft's cache answers "is this file there" from a worker-filled map, returns "assume present" for unknown paths and
rechecks after 15 s. It was written for per-frame grid and Info-panel queries.

The tickets it was meant to inform are closed, each solved its own way: #332 (drive probe off the UI thread) with a
stepped background probe in `folder_panel.rs` and a test (`a_hung_drive_probe_does_not_hold_back_the_catalog`); #327
(cache-cap shrink / purge) by running the work as a Pounce `CacheOpJob` instead of on the UI thread
(`cache_settings.rs`). Neither was an existence problem (a blocked probe, and a long
lock hold), so neither needed an existence cache. The grid
module (`nicti-pelt/src/grid/`) makes no filesystem calls at all (grepped for `exists`, `metadata`, `is_file`, `is_dir`,
`try_exists`; non-test hits elsewhere in `nicti-pelt/src` are listed here); the non-test hits are
`check_is_dir` (user-triggered, one `metadata` call, deliberately reports real errors), `xmp_sync::raw_path_for` (on the
writer thread), and `t2.rs` / `camera_profiles.rs` / `eyeshine.rs` (off the UI thread). Other hits were not traced:
`folder_panel.rs` (`mounted_drives`, behind the stepped probe), `lrc_import.rs`, `folder_dialog.rs`,
`knead/presets.rs`. The Info panel and other views were not traced frame by frame. If a future view (a Missing Photos list) does need per-row existence on the UI thread, this file is the
design to start from, and ADR-0071's offline semantics already say what "offline" means.

### 6. XMP merge (`xmp_merge.rs`, `xmp.rs`): no build

`nicti-scent/src/packet.rs` solves the same problem the other way: `apply(xmp, &Patch)` edits the packet in place and
leaves everything it does not own alone, with round-trip tests against the real packet. LightCraft's owned-property
merge plus re-parse check is a different design for the same guarantee. #375 (sync hardening) is closed, so there is no
open gap to cross-check against. One idea worth a look if packet corruption ever shows up: LightCraft re-parses its
output and checks that every written property reads back; `packet.rs` has round-trip tests but I found no equivalent
runtime check on the write path. That is a hardening idea, not a ticket.

### 7. Maker-note locator (`makernote.rs`): no build

Per-vendor maker-note IFD header and offset-base handling for Nikon, Sony, Fuji, Olympus and Panasonic, written from
ExifTool's tag documentation. Nicti already walks the Nikon MakerNote (`walk_nikon_maker_note` in
`nicti-cornea/src/embedded/ifd.rs`) and reads Nikon AF areas in the culling spike (`spikes/squint/src/af.rs`, Z8
`AFInfo2` only, unverified against more bodies per #239); `nicti-iris/src/nikon.rs` is the separate lens-correction blob
decoder (#410), not a maker-note parser. Other vendors come from LibRaw. The target is #239 (AF points). Narrow is the right size
for now: the reference library and benchmark sets are Nikon Z8 (`docs/benchmarks.md`), the other vendors' layouts could
not be checked against any real file Nicti has, and a locator built from ExifTool's tag docs would add a
`licensing.md` row for no verifiable result. If Nicti gains a second body, widening the Nikon path comes first and
this decision is revisited. It would also need a `licensing.md` row. Not worth it.

## Alternatives rejected

- **Lift `library_problem.rs` as is.** Its fourth exit presumes an in-memory library Nicti does not have.
- **Port `container.rs` for #371.** `nicti-scent` already walks JPEG APP1; two walkers would drift.
- **Build a rule editor now for #155.** There is no real smart-collection data, so the field set and nesting needs would be guessed.

## Consequences

- Two small build tickets: #558 (catalog-open failure window) and #557 (crop constrained to the rotated image).
- #371, #383, #155, #239 get no new dependency from this ADR; each keeps its own blocker.
- #441 closes with this ADR.
