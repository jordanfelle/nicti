# ADR-0437: Justified grid layout and cheap-frame assertions (GridStats)

**Status:** Proposed (assertion pattern adopted and landed; justified layout and date headers: no-go for now, with a design if wanted)
**Ticket:** #437
**Part of:** #424
**Related:** #233, #335, ADR-0068

## Context

The LightCraft audit (#424) pointed at `crates/ui-egui/src/panels/grid.rs` (pure `layout(aspects, groups, avail_w,
target, square)`, `rows_between` binary search, a `GridCache` keyed by the visible-list generation) and at its
`GridStats` counters, asserted in `tests_grid.rs` (an unchanged frame rebuilds no layout or index; fewer than 300 of
3000 cells are visited per frame). Read at `265248c`; nothing is copied, so there is no `docs/licensing.md` entry.

Nicti's grid (`nicti-pelt/src/grid/`) is a fixed square grid: `GridLayout { cols, cell }` is O(1) arithmetic
(`layout.rs`), and `view.rs` draws through `ScrollArea::show_rows`, so a frame touches only the visible rows.

## Findings

### 1. The assertion pattern: adopted, landed in this PR

Nicti already had a virtualisation test (`only_the_visible_rows_are_built...`, 200 photos, fewer than 40 cells in the
accessibility tree) but nothing that scales the library or counts work, so a regression to "build every cell" would
pass at 200 photos and only show as jank at 1M. This PR adds `GridStats { frames, cells_built }` to `ViewState`
(`view.rs`) and `a_frame_builds_only_the_cells_near_the_viewport_whatever_the_library_size`, which seeds 5000 photos
and asserts fewer than 60 cells built per frame both at the top and after End. **Proved by mutation:** replacing the
visible-row range with `0..rows` fails it with "built 5000 of 5000 cells per frame".

LightCraft's other assertions (an unchanged frame does not rebuild its layout, view or stack index) have no
counterpart: Nicti computes no cached layout or index per frame (the layout is arithmetic; the id snapshot is owned
by `GridSession` and rebuilt only on a query change). If a layout cache is ever introduced (below), add a
`layout_builds` counter with it and assert it stays flat across unchanged frames.

### 2. Justified rows: no-go for now

It is a product call; the engineering cost is the reason for deferring it, not a veto.

- **Layout is O(n) over aspects.** LightCraft's `layout` walks every aspect ratio to place cells, and rebuilds on any
  width change. At the 1M target (#335) that is a per-resize pass over a million floats plus a million `Rect`s
  (16 MB); it cannot run on the UI thread.
- **Nicti's index math assumes fixed columns.** `row_of`, `indices_for_rows`, `navigate` (arrow/page keys) and the
  thumbnail batching (`batches_for` over index ranges) all derive from `cols`. Variable-length rows need a
  row-offset table, and every one of those must move to it. `show_rows` itself needs a uniform row height, so
  justified rows would need a custom `show_viewport` loop.
- **Aspect data.** The catalog stores `width`/`height`. Whether those are orientation-corrected (a portrait NEF stored
  landscape with an EXIF rotation) was **not verified** here; justified rows with wrong aspects look worse than a
  square grid.
- **Date headers** need a group index over `captured_at` for the current sort, and only make sense when sorted by
  date; the same row-offset table would carry them.

### 3. If it is wanted: design

A `RowIndex { offsets: Vec<u32> /* first cell of each row */, tops: Vec<f32> }` built by a Pounce job from
`(generation, width bucket, target height)`, in chunks of groups so the first screenful is available immediately and
the rest fills in; the UI uses the previous index (scaled) until the new one lands. `rows_between` is a
`partition_point` on `tops`, as in LightCraft. Width is bucketed (for example to 16 px) so a live resize does not
rebuild per pixel. Keyboard navigation moves by row-offset. Ship behind a view toggle with the square grid as the
default, and measure against `docs/benchmarks.md` at ref-10k and a synthetic 1M list before enabling.

## Alternatives rejected

- **Port `grid.rs`.** It is bound to LightCraft's `Session`, `PhotoId` and stack model.
- **Count cells with a global static** instead of `ViewState`. Not thread-safe across parallel tests.
- **Assert on the accessibility tree only** (as before). It cannot see cells that were built and clipped.

## Consequences

- `GridStats` is part of the grid's test surface: new per-frame work in `view::show` should count into it.
- Justified layout: not started. If the operator wants it, file a build ticket with section 3 as the starting design
  and verify the aspect-ratio/orientation question first.
