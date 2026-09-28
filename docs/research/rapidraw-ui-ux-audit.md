# RapidRAW UI/UX audit — design reference for `nicti-pelt`

Findings feeding #31 (loupe), #32 (culling UX), #36 (AI culling assist), #46 (global
adjustments/Develop panel, closed), #47 (crop/straighten, closed), #49 (masks + local
adjustments), #52 (presets), and #57 (export). This is a UI/UX-only companion to
`docs/research/stalk-prior-art.md` (feeding ADR-0069) — that doc covered RapidRAW's Rust
backend architecture, license, and render-graph design ("not adopted, study-only"; its own
Correction section notes RapidRAW is Tauri+React, not egui/eframe, so it was never evidence for
the GUI-framework ADR either). This doc looks at the one thing the backend audit didn't: the
React frontend's actual interaction design. RapidRAW is React+Tauri; Nicti's `crates/nicti-pelt`
is Rust+egui, an immediate-mode framework — nothing here ports as code. The value is
design-reference only: layout decisions, interaction gestures, and visual-feedback conventions
worth re-implementing natively in egui, weighed against what's React-specific (retained-mode DOM
state, CSS transitions, drag-and-drop libraries) and won't translate at all.

## Method

Shallow-cloned RapidRAW (`CyberTimon/RapidRAW`) to a scratch directory, read its React frontend
(`src/`, ~13-15k lines across the panels/components listed below, plus one Rust backend file,
`src-tauri/src/culling.rs`, for the non-ML culling-heuristics reference below) via `ask-gemini`,
split into several smaller per-topic batches rather than one large combined call (a first attempt
at one giant batch across ~15 files hung and had to be killed after 18 minutes — a real gotcha
worth remembering for any future large multi-file `ask-gemini` audit: batch by topic, not by
"everything at once"). Lower-traffic panels (MetadataPanel, FolderTree, LibraryGrid/LibraryItems)
were skipped as lower-value for this pass; a future audit can pick them up if a specific ticket
needs them.

Line-number citations below were spot-checked afterward against the real upstream source (via the
GitHub API, read-only) rather than trusted as first reported — several of the batched summarizer's
citations turned out to point at the right file but the wrong range; corrected inline.

## Recommendation (lead, not buried)

**#31 (loupe/real NEF loading) stays the hard blocker** — nothing else is testable against real
photos until it lands, and nothing in this audit changes its scope.

**After #31, build #32 (culling UX) next — not #49/#52/#57.** This audit's clearest finding is
that RapidRAW's own culling flow has two real, avoidable gaps that Nicti's scale (600k-image
library, 100k-image single events, 2-10% keep rate) cannot tolerate:

- **No auto-advance after rating.** Rating an image leaves the active image unchanged
  (`useLibraryActions.ts:14-40`) — the user has to manually advance every time, which is a direct
  throughput cost multiplied across tens of thousands of images per event.
- **No undo stack for ratings/labels/deletions.** `Ctrl+Z` only undoes edit adjustments
  (`useKeyboardShortcuts.ts:277-282`), not a rating/flag/reject action — a single mis-key during a
  fast culling pass has no recovery path.

Both are cheap to get right from the start in egui (advance-on-rate is a one-line state
transition; an undo ring buffer is a pattern Nicti likely wants elsewhere too, e.g. for
`nicti-pawprint`'s edit history). Two things genuinely are worth adopting from RapidRAW's design:
**synchronized pan/zoom across burst-compare tiles** (`CullingView.tsx`'s `SyncViewport`
interface at `L34`, sync logic at `L309-319`/`L446-449`) for pixel-peeping a burst side by side,
and RapidRAW's **fast non-ML blur/exposure/duplicate heuristics** (Laplacian variance +
histogram-clip penalty + perceptual-hash burst clustering — `src-tauri/src/culling.rs:14-296`,
the one Rust backend file this audit read, not React) as a reasonable placeholder ahead of Nicti's
own DINOv2-based `spikes/litter`/`spikes/squint` work landing for #36.

**#49 (masks) has the second-most transferable design**, worth reading once real image data
exists: a 2-tier container/sub-mask list with a master-detail inspector directly below it, and a
compact inline cycling button for mask composition — Add/Subtract/Intersect as one 3-state toggle
rather than three separate controls (`MasksPanel.tsx:1842-1866`).

**#52/#57 can wait.** Their RapidRAW reference material (preset apply-with-intensity-lerp;
export's format-grid → options → destination → filename-tokens → morphing progress/cancel button)
is simple enough to reconstruct from memory when their time comes — nothing time-sensitive about
studying them now.

## Per-area findings

### Viewport / crop / masks (`ImageCanvas.tsx`, 3585 lines)

Pointer/drag state lives in refs and per-render prop-derived coordinates, not retained DOM
state — this maps directly onto egui's per-frame `Response`/`PointerState` model, so there's
nothing to "unlearn" translating it. Straighten: `handleStraightenMouseDown/Move/Up`
(`L2902-2945`) draws a dashed guide line and computes the angle on release — **but this is a
toggled tool mode** (`isStraightenActive`, switched on via a CropPanel button or a keyboard
shortcut, cancelled with Esc — `useKeyboardShortcuts.ts` ~L437-443), **not** a
hold-a-modifier-and-drag gesture. This actually contradicts, rather than confirms, #47's own issue
body, which explicitly notes the hold-modifier-drag gesture was "confirmed missing from RapidRAW"
in the prior competitive check (#69) — that's still correct, and still Nicti's own addition on top
of the drag-a-line-then-level idea both share.

Mask overlays use color-coded outlines — selected `#0ea5e9`, subtract `#f43f5e`, intersect
`#a855f7`, additive white — at different opacity/stroke-width for selected vs. unselected
(`L840-865`). Worth matching exactly for #49; it's a legible, low-ambiguity convention.

Before/after does a full-image texture swap with a crossfade, not a split-view (base/fade
crossfade state around `L1428` and `L1520-1555`). **Skip the crossfade in egui** — it's pure
CSS-transition-style animation-easing plumbing with no functional value; a key-hold texture swap
is simpler and just as usable.

### Slider / curve / base-widget interaction (`Slider.tsx` 644 lines, `Curves.tsx` 1053 lines)

Linear drag-to-value with a 5x fine-adjust multiplier on Shift/Alt
(`FINE_ADJUSTMENT_MULTIPLIER = 0.2`), double-click-to-reset, Shift+scroll-wheel nudge,
click-to-type numeric entry, and a bipolar fill that paints the slider track from the *default*
value outward rather than from zero (`Slider.tsx:44-458`) — all directly implementable as a
custom `egui::Widget`, none of it React-specific.

Curves use a Fritsch-Carlson monotone cubic spline (avoids overshoot/ringing vs. a naive cubic),
click-empty-space to add a point, right-click to delete, a 5px edge-snap threshold
(`Curves.tsx:96-409`) — port the spline math directly; it's algorithm, not UI framework.

### Histogram / scopes (`Waveform.tsx`, 648 lines)

Five switchable scopes (Luma / RGB / RGB Parade / Vectorscope / Histogram) behind a
hover-revealed pill toolbar. GPU-friendly rendering — waveform/vectorscope blit raw RGBA byte
arrays rather than going through DOM/SVG. The multi-scope idea (not just a bare histogram) is
worth planning space for even if #46's histogram panel stays basic for now.

### Culling / rating flow (`CullingView.tsx` 1257, `CullingModal.tsx` 460, `Filmstrip.tsx` 735)

Covered above in the lead recommendation — this is the highest-value area of the whole audit
given #32/#36 are open and unstarted, and Nicti's whole premise is culling throughput at scale.

### Masks / local adjustments (`MasksPanel.tsx`, 2302 lines)

Covered above; also note AI subject/sky/background mask generation (SAM ViT-B + CLIP, per
`docs/research/stalk-prior-art.md`'s backend findings) is invoked from this same panel as one
more mask-source option alongside brush/gradient — worth keeping as a single unified mask-source
picker in #49 rather than a separate AI-masks-only surface.

### Presets (`PresetsPanel.tsx`, 1394 lines) / Export (`ExportPanel.tsx`, 1290 lines)

Presets: apply-with-0-200%-intensity-lerp, "Style" vs. "Tool" categorization. Export: format grid
→ quality/bit-depth options → destination picker → filename-token chips → a button that morphs
into a progress bar then a cancel control. Both simple, low-risk patterns to reference later —
not covered in depth here since #52/#57 aren't next in line.

### Overall panel / layout shell (`Editor.tsx`, `SidePanelArea.tsx`, `BottomBar.tsx`,
`EditorToolbar.tsx`, `App.tsx`)

Left = navigation/folders, center = canvas, right = tabbed tool inspectors
(Adjustments/Metadata/Crop/Masks/AI/Presets/Export), top toolbar swaps between library-chrome and
editor-chrome, bottom = collapsible filmstrip + curation cluster (ratings, copy/paste, filter) +
viewport cluster (zoom, panel-visibility toggles). This maps directly onto
`egui::SidePanel`/`TopBottomPanel`/`CentralPanel` — **confirms `nicti-pelt/app.rs`'s existing
view-routing approach is already the right shape**, nothing to change there based on this audit.

## Status

Complete for the panels listed above; MetadataPanel/FolderTree/LibraryGrid/LibraryItems
deliberately skipped as lower-priority for this pass.
