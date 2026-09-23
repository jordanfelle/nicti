# Performance targets and benchmark methodology

Source of truth for Nicti's performance targets, how they're measured, and the reference dataset
every research ticket benchmarks against. Finalized 2026-09-23 (#14), following the
v1 PRD sign-off (#13).

## Targets

All targets are **p95**, measured on the reference dataset (below) on the reference hardware, and
reported alongside the hardware/dataset identity — never compared across different hardware.

| Area | Target |
|---|---|
| Ingest | 10k NEFs grid-browsable < 60s from NVMe; 100k < 10 min; HDD allowance 3x |
| Loupe | next/prev < 50ms (prefetch); 100% zoom < 100ms |
| Culling | keypress -> next image displayed < 50ms, including auto-advance |
| Develop | slider -> preview <= 16.7ms (60fps, screen res); image switch < 100ms warm / < 200ms cold |
| Hero scenario | switch between edited images < 100ms; crop/zoom 60fps regardless of edit stack — spec: `benchmarks/hero-scenario.md` |
| Library | filter/search/sort < 100ms at 600k real + 2M synthetic; cold start < 2s at 2M |
| Export | >= 2x LRC throughput on the same batch and settings |
| Maintenance | no "optimize catalog"; continuous crash-safe backup; bounded cache; checksum RAW backup; auto-relink |
| Denoise | matches or exceeds LRC AI Denoise on quality and speed (see ticket #40) |

**Warm vs. cold (Develop image switch):**
- *Warm* = the image was already visited or edited this session, so its stage cache (denoise, AI
  masks, lens correction — see the ticket #44 render-graph design) is populated. This is the hero
  scenario.
- *Cold* = first visit this session, nothing cached yet.

## Methodology

- **Reference hardware:** every result records CPU, GPU, NVIDIA driver version, RAM, Windows build,
  and drive models (NVMe/HDD). Results from different hardware are never compared to each other.
- **Runs:** 1 warm-up run discarded, then 5 measured runs. Report p50, p95, and max. Targets are
  judged on p95.
- **Cold vs. warm runs:**
  - Cold = after a reboot, or after flushing the Windows standby list with Sysinternals
    `RAMMap.exe -Et`, with Nicti's own on-disk caches cleared.
  - Warm = the second pass immediately after a cold run.
  - Benchmarks run natively on Windows, not under WSL — WSL cannot reliably drop the Windows file
    cache, which would invalidate the cold-run numbers.
- **Measuring Nicti:** built-in `tracing` spans from the originating input event through to frame
  present, exported as JSON per run.
- **Measuring the Lightroom Classic baseline** (LRC can't be instrumented internally):
  - A scripted AutoHotkey input sequence drives the same interaction (slider drag, next/prev, zoom)
    against a 120fps screen capture.
  - Latency is counted in frames, from an on-screen keypress indicator to the corresponding pixel
    change.
  - Export and ingest are measured with plain wall-clock timing instead.
- **Library scale (2M synthetic assets):** a generator builds a synthetic catalog at 2M rows,
  sampling EXIF/keyword distributions from the real reference dataset so no additional real image
  content is needed to test at that scale.
- **Result format:** each run's raw data lands as CSV/JSON under `bench-results/` (gitignored, local
  only). Only summarized numbers go into ADRs or ticket updates.

## Reference dataset: `ref-10k`

9,142 real files (not padded to a round 10k — see composition below), frozen as a fixed, versioned
copy so results are repeatable and unaffected by files moving around in the live archive.

**Composition:**

| Bucket | Count | Camera | Compression | Role |
|---|---|---|---|---|
| Anthrocon 2025 | 4,763 | Nikon Z8 | High Efficiency | Primary event — general cull/develop mix, performance-critical |
| Midwest FurFest 2024 | 2,663 | Nikon Z8 | High Efficiency | Second event — volume + most of the high-ISO/denoise coverage |
| Anthrocon 2024 | 1,287 | Nikon Z8 | Lossless + High Efficiency* | Compression-format diversity |
| Rory (Fursonacon 2025) | 129 | Nikon D7500 | Lossless | Decoder-compatibility only, not performance-gated |
| Images 2017-2018 (sampled) | 300 of 1,338 | Nikon D3400 | n/a (DNG-converted) | Decoder-compatibility only — **known gap: no native NEF survives for D3400, these are Lightroom-converted DNGs**, so they can't exercise the actual NEF decoder path |

The Z8 buckets (8,713 files, ~232 GB) are what every performance target above actually measures
against — the Z8 is the v1 MVP body and by far the largest, slowest files. The D7500/D3400 buckets
exist only so decoder-/catalog-import work (#37, #61/#62) has *some* non-Z8 coverage; they are
explicitly **not** used to judge any target in the table above.

**Known gaps** (tracked for backfill, not blocking #14's exit):
- No Z6III coverage. Referenced as "sometimes Rory's Z6III," but no such files were found accessible
  locally as of 2026-09-23 — Rory's other event folders (Furpocalypse 2025, Socials 2026) contain
  only placeholder files, nothing synced yet.
- D3400 coverage is DNG-only (Lightroom-converted), not native NEF — see table above.

**Freeze locations** (both are exact byte-identical copies, verified by SHA-256 at copy time):
- `H:\NictiBench\ref-10k\` — NVMe, for the "from NVMe" ingest/loupe targets.
- `E:\NictiBench\ref-10k\` — HDD, for the "HDD allowance" targets.

Files are renamed sequentially (`ref-00001.NEF`, `ref-00002.dng`, ...) on copy, so no event or
person names leak into the committed manifest.

**Manifests:**
- `docs/ref-10k-manifest.csv` (committed, public): `id, sha256, bucket, model, iso, compression,
  width, height, size_bytes`. No source paths, no event/person names beyond the generic bucket
  label above.
- `H:\NictiBench\ref-10k\PRIVATE-source-map.csv` (local only, never committed): `id, source_path,
  event` — the actual original path and event/photographer this file came from. Any tooling that
  needs to re-derive provenance reads this file locally; it never leaves the machine.

Any benchmark harness (see ticket #17) should verify the local `ref-10k` copy's files against the
committed manifest's SHA-256 column before trusting a run.
