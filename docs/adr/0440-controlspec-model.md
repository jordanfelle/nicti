# ADR-0440: A data-driven ControlSpec for develop sliders

**Status:** Proposed (no-go on a table that generates panels, presets or LRC mapping; conditional go on a small typed-accessor registry, built with ADR-0434's `develop.get/set`)
**Ticket:** #440
**Part of:** #424
**Related:** #425, #434, #52, ADR-0434, ADR-0435

## Context

LightCraft drives its Develop panels and its MCP schema from one macro table
(`crates/develop/src/controls.rs`, 503 lines at `265248c`, MIT OR Apache-2.0). Each row is
`id => field.path, label, section, min, max, default, step, decimals, track`, and the macro generates `get(&settings, id)`
and a range-clamping `set`, with indexed templates (`pointColor.<i>.hueShift`) for list-valued settings. The ticket
asks whether a spec table over Nicti's develop structs would simplify `develop_panel.rs`, presets and copy-paste
(knead), the LRC mapping, and any scripting / MCP work. Read at the pinned commit; only the idea ports (it is bound to
LightCraft's `DevelopSettings` field paths), so nothing is copied and there is no `docs/licensing.md` entry.

## What Nicti already has

- **A slider spec.** `fur::SliderSpec` (`nicti-pelt/src/fur/slider.rs`, from #425) already carries `id`, `label`, `min`,
  `max`, `default`, `step`, `decimals`, `track`, `signed`, `unit`, `scale` and `log`. `develop_panel.rs` declares 61 of them
  (58 `const`s plus three built inline in the HSL band loop; `mask_panel.rs` has 8 inline specs, two of them helpers that use the
  label as the id) and binds each at the call site: `slider(ui, &GRADE_HUE, &mut wheel.hue)`.
  So LightCraft's spec half exists. What it lacks is the **address** (which stage, which field) and a **section**.
- **Stage params are typed structs stored as JSON.** An `EditDocument` is a `BTreeMap<String, StageEntry>` with
  `params: serde_json::Value` (`nicti-pawprint`); the param structs live in `nicti-tapetum/src/coat.rs` and the panel
  reads and writes them through the generic `stage_params::<T>` / `set_stage_params::<T>` (`render.rs`), which also
  drops an entry equal to `T::default()` and scrubs NaN. Stage ids are in `nicti-tapetum/src/stages.rs`
  (`nicti.exposure`, `nicti.tone`, `nicti.color_grade`, `nicti.hsl`, `nicti.wb`, ...).
- **Generic preset and copy machinery.** knead's `Clipboard`, `StageSet`, `DevelopPreset` and `plan` work on whole
  `StageEntry` values, so presets and copy-paste need no slider table. (Relative paste, `apply_relative`, is deferred
  in knead and only adds top-level numeric fields; nested wheels and bands are overwritten.)
- **A catalog-driven LRC mapping.** `nicti-stray/src/develop/` (2018 lines, mapped on 380k real rows) translates LRC
  keys to stage params with process-version-dependent and nonlinear rules (ADR-0381, ADR-0435).

## Findings

1. **`develop_panel.rs` would not shrink much.** Of its 1451 lines most are layout and behaviour: the HSL band
   selector, grading wheels, the point-colour list, histogram, Auto buttons, heal and mask hosts, undo and redo, and the
   "write only on a real change" guard that stops opening a section from marking a photo edited. A table could
   generate the plain slider runs (Light, Detail, Effects) but not those, and it trades the compile-time binding
   `&mut field` for a string path that a typo breaks only at run time.
2. **Spec ids are not field addresses, and not unique.** `SliderSpec::id` is "unique within a panel: it salts the
   widget id". `grade.hue/sat/lum` are one const each, bound to the shadows, midtones, highlights or global wheel by a
   selector; `hsl.hue/saturation/luminance` are rebuilt per band (8 times); mask sliders use the *label* as the id
   ("Hue", "Temp", ...). Ids also differ from fields: `basic.temp` is `WbParams.temp_k`, `basic.exposure` is
   `ExposureParams.stops`, `lens.defringe_purple` lives in stage `nicti.defringe`. So the existing ids can never be
   registry keys; addresses must be new, with indexed templates for wheels and bands from day one.
3. **Presets and copy-paste: no gain** (above).
4. **LRC mapping: no-go.** Only the linear one-to-one keys would fit a table; the rest (process versions, curves,
   masks, profile resolution) cannot, so two mapping mechanisms would coexist. The existing `Translation.untranslated`
   report is the contract and it works.
5. **Agent / MCP work is the one real use, and a small one at first.** ADR-0434 slice 1 lists
   `develop.get/set` "for a few params" (and calls `develop.set` a `Mutate` command); its `Command.params` is a doc
   string, and the ADR says nothing about ranges, schemas or clamping. A `develop.set <control> <value>` that lists
   controls with range and default, clamps and writes by id is my inference of what it will want, not something 0434
   states. With "a few params" the registry starts with a handful of entries, which argues against building the whole
   table up front.
6. **A JSON-pointer write is the risky design.** (a) `set_stage_params` is typed (`T: Serialize + DeserializeOwned +
   Default`), so a registry that patches a raw `Value` has no `T` and would bypass the default-equals-remove rule and
   the NaN scrub. (b) `WbParams.temp_k` is `Option<f32>` and absent from default params, and the `basic.temp` slider is
   gated by a Manual checkbox, so "set temp" is not a bare field write. (c) Mask sliders sit in a nested list of
   corrections (a mask index plus a field), HSL in `bands[i]`, grading in named `shadows/midtones/highlights/global`
   fields (not a `wheels` array). (d) `coat::parse` is `from_value(...).unwrap_or_default()`, so a misspelled or
   mistyped write silently resets the whole stage on the next read. (e) Typed `f32` fields serialise as widened `f64`
   and `nicti-pawprint` documents that int `5500` and float `5500.0` hash differently, so a hand-written number can
   change the edit hash and Tapetum cache keys until a typed rewrite normalises it (this is read off the code, not run
   as a test).

## Decision

### 1. No-go: a macro table that generates panels, presets or LRC mapping

### 2. Conditional go: a small typed-accessor registry, built with ADR-0434's `develop.get/set`

Do nothing until that command is built. Then add, in `nicti-pelt`, a registry that keeps the compile-time binding:

```rust
pub struct Control {
    pub id: &'static str,            // new, globally unique: "light.exposure", "grade.shadows.hue"
    pub spec: &'static SliderSpec,   // range, default, step, decimals, label: reused, not duplicated
    pub section: &'static str,       // the panel's existing fur::section key, for the schema
    pub get: fn(&DevelopDoc) -> f32,
    pub set: fn(&mut DevelopDoc, f32), // typed: stage_params::<T> -> mutate the field -> set_stage_params
}
```

- Every `set` goes through the typed round-trip (`stage_params::<T>`, change one field, `set_stage_params`), so the
  default-equals-remove rule, the NaN scrub and canonical hashing behave exactly as for a panel edit. **Invariant, with a
  test:** a registry `set` of a value produces byte-identical stage params (and the same content hash) as the panel
  writing that value.
- Wheels, bands and masks are separate closures per entry (or a small generator over the four wheels and eight
  bands), not a JSON path; `Option` fields (`temp_k`) get an explicit entry that also sets the Manual state.
- The panel keeps its `&mut field` bindings and the registry cites the same `SliderSpec` consts, so a range lives in
  one place. A test checks every registered `get` returns the spec default on a default document, a `set` then `get`
  round-trips within the spec range, and (as a list, not a guarantee) which panel specs are deliberately unregistered.
- `fur::Track` is a plain `Clone, Copy, Debug, PartialEq` enum with no egui types (the file imports egui but the enum
  does not), so exporting it needs only `Serialize` or a numeric-only schema.

### 3. Cheaper wins that need no registry

- Add a `section` to `SliderSpec`, or a `&[&SliderSpec]` list per panel, to get a panel search box or a schema listing
  of ranges and labels without any address.
- Start `develop.set` with the handful of params 0434 names and grow the registry only when a ticket needs more.

## Alternatives rejected

- **Port the `controls!` macro.** It would duplicate `SliderSpec`, and Nicti's params are spread over typed stage
  structs in another crate.
- **A JSON-pointer registry.** See finding 6.
- **Generate `develop_panel.rs` from a table.** See finding 1.
- **A data-driven LRC mapping.** See finding 4.

## Consequences

- No work now. The registry is built with ADR-0434's `develop.get/set` command; starting with the few params that command
  names it is **S**, growing it is incremental.
- Nothing in the shipped code changes in this PR.
- If the registry is later extended to search or a command palette, `section` is already in place.
