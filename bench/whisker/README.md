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
  transitioning yet) look like a spurious "already settled" result.
- **`whisker drag`** — over an explicit frame window, reports the frame-to-frame intervals between
  visually distinct ROI repaints during a scripted drag, converted to ms and an effective fps
  (`1000 / p95 interval`). Used for interaction B (crop) and interaction C's pan half.

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
```

## Tuning the thresholds

Defaults (`--indicator-threshold 0.5`, `--quiet-threshold 0.02`, `--change-threshold 0.02`,
`--min-quiet-frames 3`) assume a bright (near-white) indicator flash against a dark background and
a reasonably noise-free capture (lossless/near-lossless NVENC, not a heavily compressed stream —
compression artifacts inflate the diff floor and can trip `change_threshold` on frames that didn't
actually change). If a run's `events_detected` doesn't match the expected keypress count, or the
settled/first-change latencies look implausible, re-check these against a manual frame-step
through the capture before trusting the numbers — see `hero-scenario.md`'s calibration step.
