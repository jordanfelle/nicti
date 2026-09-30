# ADR-0032: Culling UX (marking, survey/compare, filter-select-delete)

- **Status:** Accepted
- **Date:** 2026-09-29
- **Ticket:** [#32](https://github.com/jordanfelle/nicti/issues/32) Build: culling UX

## Context

The PRD (#4) names culling throughput as the critical path: a 100k-image event at a 2-10% keep
rate, keypress to next image under 50 ms (`docs/benchmarks.md`), running on the camera's embedded
previews rather than a RAW decode. `nicti-lair` already stored rating/flag/label and could filter on
them (`hunt.rs`, #23), but nothing in the UI set them, drew them, undid them, or removed a photo.

The owner's own flow shaped the scope. They rate with stars only (1 = discard, 4 = "edit this",
then re-rate the 4s to 5 or 1), and never use pick/reject or colour labels. Others will use
pick/reject or labels. The requirement that came out of that conversation: **every marker works,
none is privileged, and the discard step is simply "filter on whatever you marked with, select all,
Delete"** -- no profiles, no per-workflow configuration layer to maintain.

## Decision

1. **Marking keys** (`crates/nicti-pelt/src/cull/keys.rs`): the standard Lightroom Classic set, all
   live at once -- `0`-`5` stars, `P` pick, `X` reject, `U` unflag, `6`-`9` red/yellow/green/blue,
   Ctrl+Z / Ctrl+Shift+Z (or Ctrl+Y) undo/redo. Storage is the existing schema: reject is
   `rating = -1` (so rejecting replaces the stars), pick is `flag = 1`, the two are mutually
   exclusive, labels are the five Lightroom names. `P`/`X`/labels **toggle**, and a toggle decides
   once for a whole multi-selection (all already set -> clear, otherwise set on all) so a mixed
   selection ends up uniform.
2. **Key reading** (`cull/input.rs`): raw key events, not `key_pressed`. egui's `key_pressed`
   includes OS auto-repeat, so holding `X` would flip a reject on and off; and egui recomputes
   `repeat` from its own held-key state, so one press = one command regardless of what the platform
   claims. egui also reports the *logical* key, and a shifted digit is a symbol (Shift+3 is `#`), so
   the number row is bound by **physical position** (`binding_key`) -- otherwise Shift+digit, the
   invert-auto-advance chord, is dead on a real keyboard, as are unshifted digits on AZERTY-style
   layouts; letters stay logical. Off while a text field has focus and in the Develop view.
3. **Auto-advance** (toolbar toggle, default on; Shift inverts it for that one press): only when
   exactly one photo was marked. `cull::should_advance` is the single rule the app and its tests
   share.
4. **Writes never block a keypress** (`cull/worker.rs`, `cull/mod.rs`): the catalog is one
   `Mutex<Connection>` that an import holds in bursts. A writer thread owns every marker read and
   write; the UI updates a local cache first and moves on. One thread gives ordering for free
   (an undo runs after the writes it reverses), and it owns the undo ring (`cull/undo.rs`, 256
   entries and 200k photos' worth, always keeping the newest however large -- a deliberate
   exception, bounded to that one entry (~128 MB even for a select-all over 1M photos), because
   undoing a select-all is exactly the mis-key that matters; per-photo before/after
   so undoing a multi-select restores each photo's *own* old value). A reply (a write's or an
   undo's) only overwrites a photo's cached markers when no newer write for it is pending, so rapid
   marks never flicker back. A failed write drops the optimistic value and reports it; a failed
   read backs off instead of re-failing every frame; a writer thread that dies (a poisoned catalog
   mutex) is reported, not silent.
5. **Frozen list**: marking never reloads the grid query. A photo that stops matching the active
   filter stays put until the filter changes, so the cursor never jumps under the user.
6. **Filtering** is #242's Library filter bar (`filter_bar.rs`), extended rather than duplicated
   (an earlier draft of this change had its own three-dropdown row; #329 landed the real bar first,
   so the two were merged): rating gains *Unrated* and *Exactly N stars*, the Picks checkbox
   becomes a flag dropdown with *Unflagged*, and the label dropdown gains *No label*. They map to
   new `Filter` fields (`unrated`, `unflagged`, `no_label`, all `#[serde(default)]` so saved
   smart-collection rules still load), round-trip losslessly through a saved smart collection, and
   have facet counts -- each counted with its *own* constraint cleared, including the new fields,
   or choosing "Unrated" would zero every other rating's count.
7. **Multi-select** (`grid/selection.rs`): sorted index ranges (select-all at 1M photos is one
   range), remapped through photo ids when the snapshot changes. Click / Ctrl-click / Shift-click /
   Shift+arrows / Ctrl+A; an empty selection means "just the cursor".
8. **Delete** (`nicti_lair::shred`, `cull/delete.rs`): always asks how -- *Move to Recycle Bin*
   (RAW + `.xmp` sidecar, via the `trash` crate) or *Remove from catalog only* -- in a real modal
   (its backdrop blocks the grid and filters, so the photos named are the photos on screen). The
   engine is modelled on Carry (ADR-0026): steppable in chunks of 200, never returns `Err`, and a
   `delete_item` journal (schema v8) is written **before** any file moves. Per chunk: journal
   `pending` -> trash -> judge from the *filesystem* (a batch Recycle Bin call reports one error for
   the whole batch, so what is left on disk, not the return value, says what happened) -> mark
   `trashed` / drop the journal rows of files still there -> remove the catalog rows (children and
   journal row in one transaction). **Every disk question has three answers, not two**
   (`probe`): *present*, *absent from a reachable folder*, or *unknown* -- the stat errored
   (`Path::exists` would have said "missing" for a permission error or a flaky drive) or the
   folder itself is unreachable. Only "absent" ever removes a row; "unknown" keeps the row, and if
   it happens *after* the trash call (the drive was pulled mid-delete) the journal row is left
   `pending` for recovery to settle -- an unplugged drive must never look like a deleted photo.
   A locked file stays in the catalog and is reported. Sidecars go in a second
   bin call, decided *after* the RAWs are trashed from what is actually left on disk: only for RAWs
   confirmed gone, and only if no other same-stem file still exists (`IMG_1.NEF` + `IMG_1.NRW`
   share `IMG_1.xmp`). A sibling that is unselected, selected-but-locked, or can't be checked -- or
   a folder that can't be listed completely -- keeps the sidecar; it goes with the last sharer.
   Each folder is indexed once per run, not per photo or per chunk. Photos under a root with an
   unfinished `root_move` journal row are skipped, since `Carry`'s recovery reasons about exactly
   which copies exist. In Recycle Bin mode a catalog `rel_path` that is absolute or climbs out of its
   folder is refused (`Path::join` would let it replace the root). Startup recovery
   (`resume_open_deletes`) treats a `trashed` row and a `pending` one alike: it finishes (removes the
   row) only when the journaled file is gone from a reachable folder **and** the photo's *current*
   path (it may have moved since) holds no live file. A live file at either path rolls back -- for a
   `trashed` row that means the user restored it from the bin -- and an unknown answer at either
   path leaves the row alone, `trashed` included: that state proves the file reached the bin at that
   moment, not that it is still gone (it may have been restored and the drive then unplugged), so
   the invariant is never weakened for it. A delete itself is
   not undoable (files are restorable from the Recycle Bin); undo history for deleted photos is
   dropped.
9. **Survey (`N`) and Compare (`C`)** (`cull/survey.rs`, `cull/compare.rs`): up to 16 tiles /
   select-vs-candidate with synchronised zoom and pan, marks applying to the active tile. They draw
   the camera's embedded previews (T0 at once; T2 from the Larder for compare's two tiles, generated
   in the background by the same `T2Job` the loupe uses), never the shared `DevelopView` -- so
   culling never triggers the loupe's "Develop has unsaved edits" guard and never costs a RAW
   decode.

## Consequences

- The whole flow -- rate, filter, select all, delete -- works for stars, pick/reject, labels, or a
  mix, with no configuration. Custom key bindings and named "flows" are deliberately not built; if
  anyone needs different keys, that is a keymap ticket, not a reason for a profile system now.
- **Deferred:** writing rating/flag/label to XMP sidecars stays #60's job (this change adds no XMP
  write path and no dirty marker); burst/AI grouping stays #33-#36 -- survey/compare work from a
  manual selection; compare's synchronised zoom is a UV crop of the preview, not a true 1:1 pixel
  view of the RAW.
- **Known limits:** a folder is indexed for sidecar sharing once per run, so a same-stem file that
  appears in it mid-delete (a concurrent export) is not seen and its sidecar can go to the bin
  (recoverable). A `root_move` row that recovery can't settle (a destination that never returns)
  keeps every photo under that root undeletable, and unmovable, until Carry's recovery resolves it
  -- the leftover message says to restart with the drives connected; nothing in the UI clears such
  a row by hand. Windows decides per drive whether "Recycle Bin" really recycles -- a network
  share or some removable drives have none and may delete permanently (the prompt says so; the
  engine reports such a file as trashed because it is gone from where it was). Startup recovery
  runs synchronously in `PeltApp::new` and stats each journal row, so a dead network path could
  delay startup. A late T2 write can land in the Larder after its photo's purge (harmless: an
  orphan the Larder's own eviction reclaims). A marker filter over 1M photos costs about an
  unfiltered snapshot (~1.5-2 s, on a Pounce job), above the < 100 ms library budget; that gap
  predates this change (`crates/nicti-lair/tests/scale.rs`).
- **Not verified in this sandbox (no Windows desktop, no GPU):** the real Windows Recycle Bin path
  (`trash`'s `IFileOperation` backend compiles for `x86_64-pc-windows-gnu` but was never run), the
  drawn badges and tile layout (logic is unit-tested, pixels were not looked at), and the < 50 ms
  keypress budget on real hardware. The latency claim is tested structurally (a marking call
  returns while the store is blocked), not measured end to end.
- New dependency: `trash` (MIT), and its `urlencoding` (MIT) -- see `docs/licensing.md`.
