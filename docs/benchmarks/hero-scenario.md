# Hero scenario — reproducible benchmark spec

Implements #43. Companion doc to `../benchmarks.md`, which owns the overall targets,
warm/cold rules, and `ref-10k` dataset — this file pins down the hero scenario specifically so
Nicti's own harness (#17) can reproduce the exact same interaction sequence later.

## Working set

50 files from the `z8` bucket of `../ref-10k-manifest.csv`, stratified across four ISO bands
(`<=800`, `801-3200`, `3201-6400`, `>6400`) so denoise load is representative of a real event mix,
selected deterministically (fixed seed) by `../../bench/select_hero_set.py`. Committed IDs:
`hero-set.txt` (one `id` per line, sorted). Re-running the selection script must reproduce the
same 50 IDs — that's the reproducibility guarantee for this half of the benchmark; if the script
or manifest ever changes, regenerate and diff before trusting a new run.

## Edit stack

Applied to the first image in the set (by `hero-set.txt` order), then synced to all 50:

- White balance: Temp 5500K, Tint +5 (fixed values, not "as shot" — keeps the stack identical
  regardless of per-image metering)
- Vibrance: +25
- Mask 1: Select Subject
- Mask 2: Select Subject, Invert
- AI Denoise: amount 50

If the installed LRC version implements AI Denoise as a destructive "Enhance" operation (writes a
new `-Enhanced-NR.dng` per image instead of a non-destructive setting), the edited set for the
navigation/crop/zoom passes below is the 50 resulting enhanced DNGs, not the original NEFs — record
which behavior was observed in the run's `meta.json`.

## LRC configurations measured

Both configurations use the same throwaway catalog structure (`bench/lrc/setup.ahk`) and the same
50-file working set; only the preview settings differ.

| Config | Previews | Editing source |
|---|---|---|
| `originals` | 1:1 previews built | LRC edits original files directly |
| `smart-previews` | Smart Previews built | "Use Smart Previews instead of originals for image editing" enabled |

## Timed interactions

All three run against the warm pass (every image in the set already visited once after sync — see
Warm vs. cold below) unless noted. Each interaction is measured as 1 discarded warm-up run + 5
measured runs, per `../benchmarks.md`.

### A. Switch (next/prev)

Right-arrow through all 50 images in Develop, one keypress per image.

- **Input marker:** a solid-color indicator square (top-left corner, outside the loupe region)
  toggles on every injected keypress — this is the unambiguous t0 for each switch.
- **Metric 1 — first-change latency:** indicator toggle → first frame where the loupe ROI differs
  from its pre-switch state. LRC typically paints a soft/blurred placeholder first; this number is
  recorded but not judged against the target.
- **Metric 2 — settled latency (the target):** indicator toggle → first frame of a run of 3+
  consecutive frames with no further pixel change in the loupe ROI. Judged against
  `../benchmarks.md`'s "switch between edited images < 100ms" (warm) target.
- 49 switches per run (image 1 → 2 → … → 50); report p50/p95/max across all switches pooled across
  the 5 measured runs (245 samples), not just per-run.

### B. Crop

`R` to enter crop mode on the current image, then a scripted 2-second corner drag (fixed start/end
screen coordinates, linear interpolation, injected at the capture's frame rate).

- **Metric:** frame-to-frame intervals between visually distinct loupe frames during the drag
  window (first injected mouse-move to drag release). Report the effective fps
  (1000 / p95 interval ms) and the p95 interval directly. Judged against the "crop/zoom 60fps"
  target — at 60fps that's an interval ≤ 16.7ms.
- Repeat on 5 different images per run (spread across the set, not just image 1) to avoid measuring
  a single cached case; pool all distinct-frame intervals across those 5 images for the run's
  numbers.

### C. Zoom

`Z` to toggle 1:1 zoom on the current image, then a scripted 2-second pan drag.

- **Metric 1 — zoom-settled latency:** `Z` keypress (indicator toggle) → settled frame, same
  settled definition as interaction A. Judged against "100% zoom < 100ms" in `../benchmarks.md`.
- **Metric 2 — pan frame-interval:** same method as interaction B's crop-drag metric, applied to
  the pan drag.
- Same 5-image spread as interaction B.

## Warm vs. cold

- **Warm (primary):** after the bulk edit + sync, step through the full 50-image set once
  (untimed) so every image's stage output is cached, then run interactions A/B/C as the timed
  passes. This is the hero scenario — matches "already-edited images" in #43's description.
- **Cold (secondary):** per `../benchmarks.md`'s cold-run rules (reboot or `RAMMap.exe -Et` to
  flush the Windows standby list, LRC's own on-disk caches cleared), run interaction A once more
  (switch only — cold crop/zoom on a never-visited image isn't part of the hero pain point).
  Recorded for context, not judged against a target.

## Secondary metric: bulk-apply wall-clock

Wall-clock time from "sync settings to 49 images" confirmed until LRC's activity indicator shows
all mask/denoise processing complete (progress spinner clears, or histogram stabilizes on the last
image — record which signal was used). Not a p95/target metric — plain single-run timing, both
configs. Captures part of the pain #43 describes even though it isn't itself in the interaction
loop above.

## Capture

- Windows `ffmpeg` with `ddagrab` (Desktop Duplication), NVENC-encoded, capture rate given at
  runtime (see deviation note below) — see `../../bench/run-hero.ps1`.
- **Capture-rate deviation (recorded 2026-09-23):** `../benchmarks.md` specifies 120fps capture.
  The machine used for the first baseline run was reached via Parsec remote desktop, whose virtual
  display reported 59Hz current / 60Hz max — below 120fps. The first baseline therefore captures
  at **60fps** (16.7ms frame-timing resolution instead of 8.3ms); this is recorded per-run in
  `meta.json`'s `capture_fps` field and repeated in the results table below. Re-run at 120fps once
  physically at the machine with a 120Hz+ display attached; `bench/run-hero.ps1` and
  `bench/whisker` both take capture rate as a parameter, so no code changes are needed for that
  re-run.
- Hardware identity (CPU, GPU, driver version, RAM, Windows build, drive models, display refresh
  rate actually achieved) is recorded per run in `meta.json`, per `../benchmarks.md`'s rule that
  results are never compared across different hardware.

## Analysis

`bench/whisker` (Rust) reads each run's raw frames and the indicator/ROI rectangles from
`run-hero.ps1`'s config, and emits per-interaction/per-config p50/p95/max plus the raw per-event
CSV. See `../../bench/whisker/README.md`.

## Results

_Filled in after the baseline runs — see `../benchmarks.md`'s Hero scenario row for the summary
that feeds back into the target table. Raw captures and per-run CSVs live in `bench-results/`
(gitignored, local only)._
