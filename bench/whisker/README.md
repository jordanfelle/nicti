# whisker

Frame-diff analyzer for the #43 hero-scenario benchmark (`docs/benchmarks/hero-scenario.md`).
Not a production crate — lives under `bench/` alongside the AutoHotkey driver and PowerShell
runner that produce its inputs.

## What it does

Reads two raw gray8 crops of the same capture — the keypress-indicator square, and the loupe ROI —
and turns pixel-level frame differences into the latency/fps numbers `hero-scenario.md` defines:

- **`whisker switch`** — for each indicator flash (a keypress), reports first-change latency (LRC's
  placeholder paint) and settled latency (no further change for N frames), in ms, plus p50/p95/max
  across all detected events. Used for interaction A (switch) and interaction C's zoom-settled
  half. The settled search starts from the detected first-change frame, not the raw indicator
  edge — searching from the edge would let a brief pre-change plateau (the ROI hasn't started
  transitioning yet) look like a spurious "already settled" result. `--edges` restricts analysis
  to specific 0-based indicator-edge indices (e.g. `--edges 0` for a zoom capture, whose 3 flashes
  are `[Z keypress, pan-drag-start, pan-drag-end]` and only the first is switch-shaped) — the
  report's `events_detected` still counts every raw edge regardless, so the calibration "confirm
  events_detected == N" check still works on a filtered call.
- **`whisker drag`** — over a transition-index window, reports the frame-to-frame intervals
  between visually distinct ROI repaints during a scripted drag, converted to ms and an effective
  fps (`1000 / p95 interval`). Used for interaction B (crop) and interaction C's pan half. The
  window can be given explicitly (`--start-frame`/`--end-frame`) or derived from a pair of
  indicator edges (`--indicator-raw`/`--window-edges start,end`, e.g. `--window-edges 1,2` for
  crop/zoom, whose edge 0 is the mode-entry keypress `r`/`z` and edges 1/2 bracket the drag —
  `hero.ahk`'s `ScriptedDrag` flashes the indicator at both) — the latter means no per-capture
  hand-picked frame numbers are needed for the ~120 crop/zoom captures the full spec produces.
- **`whisker analyze --results-root <dir>`** — walks a `run-hero-series.ps1` results tree (every
  directory containing a `meta.json`), analyzes each non-warm-up capture with the same pipeline as
  the two subcommands above, and pools raw samples per (config, interaction) before computing
  p50/p95/max — matching hero-scenario.md's "pool ... across those 5 images"/"across all the
  measured runs" wording, rather than averaging each capture's own p95. Writes `summary.json` and
  a `summary.md` table into `--out-dir` (default: `--results-root` itself). Any capture that fails
  to parse or analyze (bad `meta.json`, a missing indicator edge, mismatched frame counts) is
  listed in `skipped` rather than silently dropped from the pooled counts — always check that list
  before trusting a summary.

## Producing the raw inputs

`whisker` never calls ffmpeg itself — `run-hero.ps1` does the capture and crop, then hands whisker
two raw files per run:

```powershell
# From the full-frame mkv capture, crop out just the indicator square and just the loupe ROI,
# each as headerless gray8 rawvideo. Coordinates come from run-hero.ps1's calibration step.
ffmpeg -i capture.mkv -vf "crop=$iw:$ih:$ix:$iy,format=gray" -f rawvideo indicator.raw
ffmpeg -i capture.mkv -vf "crop=$rw:$rh:$rx:$ry,format=gray" -f rawvideo roi.raw
```

Both crops must come from the *same* capture (same frame count, same timing) — `whisker switch`
errors out if the indicator and ROI frame counts don't match, since that means they came from
different captures or one crop dropped frames the other didn't.

## Example

```bash
cargo run -p whisker --bin whisker -- switch \
  --indicator-raw indicator.raw --indicator-width 40 --indicator-height 40 \
  --roi-raw roi.raw --roi-width 1200 --roi-height 800 \
  --fps 60 \
  --out switch-report.json

cargo run -p whisker --bin whisker -- drag \
  --roi-raw roi.raw --roi-width 1200 --roi-height 800 \
  --fps 60 --start-frame 30 --end-frame 150 \
  --out drag-report.json

# Same drag, but deriving the window from indicator edges 1 and 2 instead of hand-picked frames:
cargo run -p whisker --bin whisker -- drag \
  --roi-raw roi.raw --roi-width 1200 --roi-height 800 \
  --indicator-raw indicator.raw --indicator-width 40 --indicator-height 40 \
  --window-edges 1,2 --fps 60 \
  --out drag-report.json

cargo run -p whisker --bin whisker -- analyze \
  --results-root ../../bench-results/hero
```

## Tuning the thresholds

Defaults (`--indicator-threshold 0.5`, `--quiet-threshold 0.02`, `--change-threshold 0.02`,
`--min-quiet-frames 3`) assume a bright (near-white) indicator flash against a dark background and
a reasonably noise-free capture (lossless/near-lossless NVENC, not a heavily compressed stream —
compression artifacts inflate the diff floor and can trip `change_threshold` on frames that didn't
actually change). If a run's `events_detected` doesn't match the expected keypress count, or the
settled/first-change latencies look implausible, re-check these against a manual frame-step
through the capture before trusting the numbers — see `hero-scenario.md`'s calibration step.
