# ADR-0440: A data-driven ControlSpec for develop sliders

**Status:** Proposed (no-go on a table that generates panels or LRC mapping; conditional go on a small control registry, built with ADR-0434's `develop.set` command)
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
  as constants (8 more in `mask_panel.rs`) and binds each at the call site: `slider(ui, &GRADE_HUE, &mut wheel.hue)`.
  So LightCraft's spec half exists. What it lacks is the **address** (which stage, which field) and a **section**.
- **Stage params are serde JSON.** An `EditDocument` is a `BTreeMap<String, StageEntry>` with `params: serde_json::Value`
  (`nicti-pawprint`), and the panel reads and writes typed structs through `stage_params::<T>` / `set_stage_params`.
  LightCraft needs a macro because its settings are plain typed fields with no path-addressable form; Nicti's stages
  are already path-addressable as JSON.
- **Generic preset and copy machinery.** knead's `Clipboard`, `StageSet`, `DevelopPreset` and `plan` work on whole
  stage entries, and the relative-paste helper in `nicti-pawprint` adds numeric JSON fields generically. Presets
  need no slider table.
- **A catalog-driven LRC mapping.** `nicti-stray/src/develop/` (2018 lines, mapped on 380k real rows) translates LRC
  keys to stage params with process-version-dependent and nonlinear rules (ADR-0381, ADR-0435).

## Findings

1. **`develop_panel.rs` would not shrink much.** Of its 1451 lines most are layout and behaviour: the HSL band
   selector, grading wheels, the point-colour list, histogram, Auto buttons, heal and mask hosts, undo and redo, and the
   "write only on a real change" guard that stops opening a section from marking a photo edited. A table could
   generate the plain slider runs (Light, Detail, Effects) but not those, and it trades the compile-time binding
   `&mut field` for a string path that a typo breaks only at run time.
2. **Spec ids are not field addresses.** `SliderSpec::id` is "unique within a panel: it salts the widget id", and
   `grade.hue` is shared by all the grading wheels. A table keyed by address needs LightCraft's indexed templates
   (`grade.<wheel>.hue`) from day one.
3. **Presets and copy-paste: no gain** (above).
4. **LRC mapping: no-go.** Only the linear one-to-one keys would fit a table; the rest (process versions, curves,
   masks, profile resolution) cannot, so two mapping mechanisms would coexist. The existing `Translation.untranslated`
   report is the contract and it works.
5. **Agent / MCP work is the one real use.** ADR-0434 slice 1 gives a pelt-local command table whose `params` are
   described by a string. A `develop.set <control> <value>` command wants exactly the spec table's job: list
   controls with range and default, clamp, and write by id. That is the moment to add addresses, not before.

## Decision

### 1. No-go: a macro table that generates panels, presets or LRC mapping

### 2. Conditional go: a small control registry, built with ADR-0434's `develop.set`

Do nothing until a command needs it. Then add, in `nicti-pelt`, a registry next to the existing specs:

```rust
pub struct Control {
    pub spec: &'static SliderSpec,   // range, default, step, decimals, label: reused, not duplicated
    pub section: Section,            // panel grouping, for the schema and a future search box
    pub stage: &'static str,         // "nicti.light", "nicti.grade", ...
    pub path: &'static str,          // JSON pointer into that stage's params: "/exposure", "/wheels/{i}/hue"
}
```

- `get(doc, id)` and `set(doc, id, v)` resolve `stage` + `path` over the stage's JSON `Value`, clamp to `spec`, and
  write back through the same `set_stage_params` route the panel uses, so the cache keys and "no write unless changed"
  behaviour are unchanged. `{i}` is the indexed form.
- The panel keeps its typed `&mut field` bindings. The registry **cites the same `SliderSpec` consts**, so a range
  lives in one place.
- **Anti-drift test:** for every `Control`, the path must resolve in the stage's default params, a `set` followed by
  a typed read must round-trip, and every `SliderSpec` const used by a panel must be registered or listed as
  deliberately absent. This replaces the compile-time guarantee the macro gives LightCraft.

`Track` variants in pelt are egui-bound (`fur::Track`); the schema exports only the numeric part (range, default,
step, decimals, label, section).

## Alternatives rejected

- **Port the `controls!` macro.** It needs typed field paths Nicti's JSON-backed stages do not have, and it would
  duplicate `SliderSpec`.
- **Generate `develop_panel.rs` from a table.** See finding 1.
- **A data-driven LRC mapping.** See finding 4.

## Consequences

- No work now. The registry is a **S/M** slice inside ADR-0434's `develop.set` command, with the anti-drift test.
- Nothing in the shipped code changes in this PR.
- If the registry is later extended to search or a command palette, `section` is already in place.
