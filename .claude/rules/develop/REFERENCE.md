---
paths:
  - "spikes/pupil/**"
  - "bench/lrc/auto-tone.ahk"
  - "docs/adr/0099-classic-auto-tone.md"
---

# Classic Auto-Tone — Quick Reference

Full reasoning/history: `docs/decisions/develop.md`.

- **Classic auto-tone (#99)** — `docs/adr/0099-classic-auto-tone.md`: **Proposed**, pending #202
  (reference-machine run — no LRC install and `spikes/retina` can't build in this sandbox).
  Candidate A (percentile heuristic) vs. candidate B (ridge-regression fit); decision rule
  (Exposure MAE ≤ 0.15 EV, other sliders MAE ≤ 8 / p95 ≤ 20) fixed before real data exists.
- **Primary metric: per-slider value error, not a golden image** — nothing can render PV2012
  sliders to pixels yet (#46's job), so a rendered-image comparison would just add a second
  approximation-error source. #46 owns that comparison once it exists.
- **`spikes/pupil::render`** reuses `spikes/rods`' fixed "camera RGB → XYZ(D50) → linear sRGB →
  sRGB OETF" color treatment (not ADR-0038's real DCP pipeline) plus as-shot white balance, to
  reconstruct the default-render proxy LRC's own Auto Settings analyzes.
- **`input`/`truth` are small independent copies**, not path dependencies on `calico`/`shed` —
  this repo's spikes stay self-contained (same pattern `spikes/rods` already follows).
- **The real LRC capture AND the sample-set selection are both deferred to #202** — `ref-10k`
  isn't reachable from the sandbox this ADR was authored in, so `bench/lrc/auto-tone.ahk` takes
  its source directory as a CLI arg rather than reading a frozen file list.

## Package contents

- **`spikes/pupil`** (#99/ADR-0099's classic auto-tone research) — `input` (retina dump-linear
  reader), `render` (default-render color treatment), `histogram` (percentile/mean/clip-fraction
  lookup), `sliders` (the six PV2012 targets + ranges), `heuristic` (candidate A), `fit`
  (candidate B: hand-rolled ridge regression), `truth` (LRC ground truth from a `.lrcat`), `eval`
  (per-slider MAE/p95/bias). See `docs/research/pupil-auto-tone.md` for the module breakdown and
  the RapidRAW auto-adjust study.
