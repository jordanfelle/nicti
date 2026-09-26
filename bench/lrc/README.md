# bench/lrc — Lightroom Classic driver

Sets up a throwaway LRC catalog for the #43 hero scenario and drives the timed interactions.
See `docs/benchmarks/hero-scenario.md` for what's being measured and why; this doc is the
step-by-step for actually running it.

**Requires PowerShell 7+** (`winget install Microsoft.PowerShell`) — `run-hero.ps1`'s graceful
ffmpeg-stop logic uses a .NET-Core-only API that silently fails under Windows PowerShell 5.1.
Everything below invokes `pwsh`, not `powershell`.

**Never touches your real catalog.** Every script here targets an explicit bench catalog path
under `<BENCH_ROOT>\lrc-bench\` (`<BENCH_ROOT>` is your own scratch location, passed as
`setup.ahk`'s 3rd argument or the `NICTI_BENCH_ROOT` env var); LRC is only ever launched against
that path.

## One-time setup, per LRC config (`originals`, `smart-previews`)

1. **Stage the working set** (hardlinks, no duplicate storage):
   ```powershell
   .\stage-hero-set.ps1 -RefRoot <REF10K_ROOT> `
     -HeroSetFile ..\..\docs\benchmarks\hero-set.txt `
     -DestDir <BENCH_ROOT>\lrc-bench\hero-set
   ```
2. **Create the catalog + import** — launch any LRC catalog first, then:
   ```
   AutoHotkey64.exe setup.ahk originals <BENCH_ROOT>\lrc-bench\hero-set <BENCH_ROOT>
   ```
   (or `smart-previews` for the second config). This creates
   `<BENCH_ROOT>\lrc-bench\hero-<config>.lrcat`, walks through File > New Catalog, then pauses
   for you to complete the import in the dialog it opens (select all 50, **Add** not Copy/Move).
3. **Build previews:**
   - `originals` config: Library > Previews > Build 1:1 Previews, for all 50.
   - `smart-previews` config: Library > Previews > Build Smart Previews, for all 50, **and**
     enable Preferences > Performance > "Use Smart Previews instead of originals for image
     editing".
4. **Apply and sync the edit stack — manual, not scripted.** LRC's mask/AI-denoise UI doesn't
   have stable enough coordinates/timing to script blind, and a wrong sync would silently break
   the "identical edit stack on all 50 images" precondition every measurement depends on. On the
   first image in `hero-set.txt`'s order, in Develop:
   - WB: Temp 5500, Tint +5
   - Vibrance: +25
   - Masks: Select Subject → new mask; Select Subject again → Invert on the new mask
   - AI Denoise: amount 50 (note in `meta.json` whether your LRC version applies this
     non-destructively or writes a new `-Enhanced-NR.dng` — see hero-scenario.md's note on this)
   - Select all 50 in Library grid → right-click the edited image → **Sync Settings...** → check
     all the above, uncheck everything else → Synchronize
   - Wait for the sync + mask/denoise processing to finish (progress spinner clears) before
     recording the secondary bulk-apply wall-clock metric
   - **Verify by eye**: spot-check 3-4 other images in the set to confirm the masks and denoise
     actually applied, not just the WB/vibrance sliders (denoise/AI mask processing can silently
     lag behind a sync that reports "done").
5. **Warm the set:** step through all 50 in Develop once (arrow keys), untimed, so every image's
   render cache is populated before any timed run.

## Running a timed pass

1. Copy `hero-config.ini.example` → `hero-config.ini`, fill in the `[indicator]` rect and (for
   crop/zoom) the drag coordinates — see Calibration below.
2. With LRC focused on the first image of the warmed set (for `switch`) or the target image
   (for `crop`/`zoom`), start the capture (`../run-hero.ps1` does this for you), then:
   ```
   AutoHotkey64.exe hero.ahk hero-config.ini
   ```
3. Stop the capture once `hero.ahk` exits (`run-hero.ps1` handles this automatically).

`hero.ahk` never navigates between images itself (see its own header comment) — `run-hero.ps1`'s
`-PreNavigate` runs `navigate.ahk` to position the selection *before* the capture starts, so
navigation never lands inside a timed capture. You won't normally invoke either script directly;
see "Running a full series" below.

Crop's drag is flashed at both ends (drag-start, drag-end) in addition to the mode-entry (`r`)
flash, so a capture has 3 indicator edges total — `whisker drag --window-edges 1,2` derives the
timed window from the last two instead of needing hand-picked frame numbers. After the drag,
`hero.ahk` undoes the crop it just committed (`RevertCropAfter=1` in `hero-config.ini`, default
on) so every repeat run on the same image starts from the same uncropped state — **verify in the
calibration dry-run (below) that this undo actually restores the pre-crop image**, since a crop
that silently fails to revert invalidates every subsequent run on that image.

## Running a full series

`../run-hero-series.ps1` drives one (config, interaction) pair's whole warm-up + 5-measured-run
series (`docs/benchmarks.md`'s "1 warm-up discarded, 5 measured" rule), including crop/zoom's
5-image spread — this is what you actually invoke, not `run-hero.ps1`/`hero.ahk` one at a time:

```powershell
pwsh .\run-hero-series.ps1 -Config originals -Interaction switch `
  -IndicatorRect "20,20,60,60" -RoiRect "400,200,1200,800" `
  -ResultsRoot <BENCH_ROOT>\bench-results\hero
```

Repeat per config × interaction (`originals`/`smart-previews` × `switch`/`crop`/`zoom`), plus a
cold switch pass with `-Cold` after reverting to a cold cache state (see
`docs/benchmarks.md`'s cold-run rules and `hero-scenario.md`'s Warm vs. cold section) — pass
`-ResultsRoot <BENCH_ROOT>\bench-results\hero`, not the local-disk default, to keep multi-GB
captures off the UNC session path. It stops on the first failed run rather than continuing past a
broken series.

## Calibration

Coordinates are specific to this machine's resolution/window layout and must be set once before
the first real run (and rechecked if the window moves/resizes):

1. Position the LRC window and note the loupe region's screen rect — this becomes whisker's ROI
   crop (passed to `run-hero.ps1`, not stored in `hero-config.ini`).
2. Pick a spot for the indicator square that's fully outside the ROI crop and outside LRC's own
   UI chrome (a screen corner works) — set `[indicator] X`/`Y`/`Size`.
3. For crop/zoom: with an image open, enter crop mode (`R`) or zoom (`Z`), and note screen
   coordinates for a corner handle's start/end drag points (crop) or a pan start/end (zoom).
4. **Dry-run before trusting numbers**: run one `switch` pass with capture on, then step through
   the recording frame-by-frame (or use `ffprobe`/`ffplay`) to confirm the indicator flash and the
   actual loupe change are both clearly visible and land where you expect. Confirm `ffmpeg`'s
   dropped-frame count is 0 for the capture rate in use (see `../run-hero.ps1`). Also run one
   `crop` pass and confirm three things: `whisker switch`'s `events_detected` reads 3 (mode-entry +
   drag-start + drag-end), the drag-start/drag-end flashes visibly bracket the scripted drag in the
   recording, and — after the run — the image in LRC is back to its pre-crop state (the
   `RevertCropAfter` undo). Do the same sanity pass once for `zoom` (also 3 edges: `Z` keypress +
   pan-start + pan-end).

## Never automate on a machine you're actively using for something else

`hero.ahk`, `setup.ahk`, and `navigate.ahk` take over the mouse and keyboard and can run for tens
of seconds unattended. Don't kick off a run (or a `run-hero-series.ps1` series, which chains many
of these back to back) while doing other work on this machine or in the same remote session —
confirm the session is free first.
