# bench/lrc — Lightroom Classic driver

Sets up a throwaway LRC catalog for the #43 hero scenario and drives the timed interactions.
See `docs/benchmarks/hero-scenario.md` for what's being measured and why; this doc is the
step-by-step for actually running it.

**Never touches your real catalog.** Every script here targets an explicit bench catalog path
under `H:\NictiBench\lrc-bench\`; LRC is only ever launched against that path.

## One-time setup, per LRC config (`originals`, `smart-previews`)

1. **Stage the working set** (hardlinks, no duplicate storage):
   ```powershell
   .\stage-hero-set.ps1 -RefRoot H:\NictiBench\ref-10k `
     -HeroSetFile ..\..\docs\benchmarks\hero-set.txt `
     -DestDir H:\NictiBench\lrc-bench\hero-set
   ```
2. **Create the catalog + import** — launch any LRC catalog first, then:
   ```
   AutoHotkey64.exe setup.ahk originals H:\NictiBench\lrc-bench\hero-set
   ```
   (or `smart-previews` for the second config). This creates
   `H:\NictiBench\lrc-bench\hero-<config>.lrcat`, walks through File > New Catalog, then pauses
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
   dropped-frame count is 0 for the capture rate in use (see `../run-hero.ps1`).

## Never automate on a machine you're actively using for something else

`hero.ahk` and `setup.ahk` take over the mouse and keyboard and can run for tens of seconds
unattended. Don't kick off a run while doing other work on this machine or in the same remote
session — confirm the session is free first.
