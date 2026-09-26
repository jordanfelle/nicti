# Stalk: prior-art survey — RapidRAW, vkdt, Ansel

Findings for [#69](https://github.com/jordanfelle/nicti/issues/69), feeding #37 (RAW decoder), #38
(color pipeline), #39 (lens corrections), #44/#45 (Tapetum render graph), #47 (crop/auto-level),
#48/#51 (masking/healing), #56 (export), #68/ADR-0006 (GUI), and #99 (classic auto-tone). This is a
findings doc, not an ADR; the RapidRAW adopt/fork decision itself is
`docs/adr/0018-rapidraw-adopt-or-fork.md`.

## Method

Shallow-cloned all three projects (plus RapidRAW's `rawler` fork) into a scratch directory at
pinned commits, read primary sources directly for every architectural claim (no claim taken from a
README alone without a source-file citation), and ran two decisive checks against RapidRAW's real
dependency graph rather than estimating from its `Cargo.toml` alone:

- `cargo tree --manifest-path rapidraw/src-tauri/Cargo.toml` — full resolved dependency graph.
- `cargo deny --manifest-path rapidraw/src-tauri/Cargo.toml --config <nicti>/deny.toml check
  licenses` — Nicti's own license allowlist run directly against RapidRAW's real dependency graph,
  not a manual per-crate lookup.

Pinned commits (all fetched 2026-09-25/26): RapidRAW `772c76b7`, RapidRAW-DngLab (rawler fork)
`934af4b2`, vkdt `c4c9ce54`, Ansel `33ee50db`.

## RapidRAW

**License**: AGPL-3.0 — but **the exact grant is ambiguous, same category of gap as `rawler`'s bare
`LGPL-2.1` below, not resolved here**. GitHub's API (`gh api repos/CyberTimon/RapidRAW --jq
.license.spdx_id`) reports a bare `"AGPL-3.0"`, which is GitHub's own license-detection matching
the committed `LICENSE` file's boilerplate text, not a per-project grant statement. Checked
directly for an explicit election, the way the doc already does for Ansel below: no
`SPDX-License-Identifier` header exists in any source file, and the README's own license section
(`README.md:1013`) just says "GNU Affero General Public License v3.0 (AGPL-3.0)" with no
`-only`/`-or-later` language. **Practical effect on ADR-0018's reasoning: none** — GPL-family
licenses of the same base version are compatible with each other regardless of the `-only`/
`-or-later` distinction (an `-or-later` grant is only an added permission on top of the same v3
text, so a combined work can always be distributed under plain version 3, satisfying both sides)
— but the doc should say this was checked and found ambiguous, not imply it was confirmed.
**Activity**: ~10.2k stars, pushed same day as this research (2026-09-25) — active.
**Stack**: Rust backend (`src-tauri/`, ~37.4k lines across 27 files) + TypeScript/React frontend
via Tauri 2.11 (`src-tauri/Cargo.toml:17`), **not egui/eframe** — see Correction below.

### Dependency graph / license check (decisive result)

`cargo tree` resolves **583 unique crates** (626 crate@version pairs) for `src-tauri`. Running
Nicti's own `deny.toml` allowlist against that full graph produced exactly **one rejection**:
`rawler v0.7.1` — `license = "LGPL-2.1"` (deprecated bare SPDX id, no `-only`/`-or-later` suffix),
pulled in via RapidRAW's own fork of dnglab's `rawler`
(`git = "https://github.com/CyberTimon/RapidRAW-DngLab.git"`, Cargo.toml:31). Every other crate in
RapidRAW's ~583-crate graph clears Nicti's existing allowlist without a single addition. Confirmed
by reading actual source headers (not just the crate's `Cargo.toml` line, which could reasonably be
read as shorthand for either grant): every checked `rawler` source file
(`rawler/src/tiles.rs:1`, etc.) carries `// SPDX-License-Identifier: LGPL-2.1`, no `-or-later`. This
is the **same unresolved ambiguity ADR-0013 already flagged for upstream `rawler`** — RapidRAW's
fork does not resolve it, since the fork only patched a highlights-clamping behavior
(`934af4b2`, commit message "let highlights remain unbounded") and left every license header
untouched. **#37 still needs to resolve this before adopting rawler in any form**, forked or not.

### Corrections to existing ADR claims

- **ADR-0006 (`docs/adr/0006-gui-framework.md`) is wrong**: its Prior art section states RapidRAW
  uses `egui`/`eframe`, "confirmed via the project's own dependency manifest — verified 2026-09-23."
  The actual `src-tauri/Cargo.toml:17` shows `tauri = "2.11"`, and the `src/` tree (117 `.tsx`/`.ts`
  files) is a React frontend. There is no `egui`/`eframe` dependency anywhere in the manifest.
  Corrected directly in this PR — see that file's own Amendments note.
- **ADR-0005's "RapidRAW uses wgpu" claim is correct** (`gpu_processing.rs`), but understate one
  detail: RapidRAW pins `wgpu = "29.0"` (Cargo.toml:26), one major version behind Nicti's own
  `wgpu` 30 choice, with an explicit comment in the manifest: `# Downgraded to prevent P3 color
  shifts on Apple devices`. Worth knowing if Nicti ever hits the same issue on macOS (v2, #73).
- **The issue's premise "SAM2/Depth-Anything-V2/U-2-Net/LaMa/CLIP masking" is half right**: no
  `sam2`/`SAM2`/`MobileSAM` reference exists anywhere in the source (`grep -rn` empty). The actual
  model is Meta's original **SAM ViT-B** (`ai_processing.rs:22-25`, filename
  `sam_vit_b_01ec64_encoder/decoder.onnx` — the official 2023 SAM-v1 checkpoint suffix), split into
  separate encoder/decoder ONNX sessions. Depth-Anything-V2 (vits, line 58-59), U-2-Net (general +
  a `skyseg` variant, lines 31-38), LaMa (fp16, inpainting, lines 54-55), and CLIP (model +
  tokenizer, lines 42-46, the only one with a committed SHA256 pin, line 47) are all confirmed as
  named.

### Architecture (backend)

Deep-dive over `lib.rs`/`gpu_processing.rs`/`image_processing.rs`/`file_management.rs` (~12,300
lines combined, routed to `ask-gemini` per this session's token-conservation rule), with the
highest-stakes claims spot-checked directly against the source afterward (struct shape, sidecar
naming, surface-creation `cfg` gate, and shader line count all confirmed exact).

- **Model distribution**: every ONNX model is downloaded on demand from the author's own
  HuggingFace repo (`CyberTimon/RapidRAW-Models`, `ai_processing.rs:22-59`) at runtime
  (`download_and_verify_model`, line 408), **not bundled** in the binary or repo. No weights are
  vendored, so there is no "does this repo's own license apply to weights" question — only the
  weights' own upstream licenses matter, same open question ADR-0007 already has for LaMa's
  Places2 training-data provenance.
- **Edit storage — per-image JSON sidecar, no catalog DB.** `<source>.rrdata`
  (`file_management.rs:406-413`, path built by `parse_virtual_path`, lines 387-417); virtual copies
  get `<source>.<copy_id>.rrdata` from a `?vc=<id>` suffix on the path (lines 388-405). Written by
  `save_metadata_and_update_thumbnail` via `serde_json::to_string_pretty` (lines 2544-2545).
  Verified struct (`image_processing.rs:52-61`):
  ```rust
  pub struct ImageMetadata {
      pub version: u32,
      pub rating: u8,
      pub adjustments: Value,   // untyped serde_json::Value — every slider/curve/mask/crop param
      pub tags: Option<Vec<String>>,
      pub exif: Option<HashMap<String, String>>,
  }
  ```
  `adjustments` is a single untyped blob, not a typed fixed-order stage map — the opposite of
  Nicti's own ADR-0002 design (typed per-stage params, blake3-hashed individually for Tapetum's
  cache key). A `.rrexif` cache file and an optional `.xmp` sidecar (if configured) exist alongside
  it (lines 2547-2552). **Not directly portable to Nicti's catalog-authoritative model** — there is
  no DB row to map `.rrdata` onto; adopting RapidRAW's edit engine would mean rebuilding its
  metadata layer from scratch, not carrying this one over.
- **Pixel delivery to the UI — platform-split, not one path.** Two real, hard-coded schemes:
  1. **Windows/macOS (default)**: a `wgpu::Surface` bound directly to the Tauri webview window
     (`instance.create_surface(window)`, `gpu_processing.rs:185-207`, confirmed gated by
     `#[cfg(not(any(target_os = "android", target_os = "linux")))]`) — the GPU output texture is
     drawn straight into the swapchain and presented (`gpu_processing.rs:2124-2160`); the frontend
     is only told via a `"wgpu-frame-ready"` event plus a `b"WGPU_RENDER"` sentinel return value
     from `apply_adjustments` (`lib.rs:587-591`) — no pixels cross the IPC boundary at all.
  2. **Linux/Android or `use_wgpu_renderer: false`**: a CPU readback (`read_texture_data_roi`,
     `gpu_processing.rs:435-504`), JPEG-encoded (`mozjpeg_rs`/TurboJPEG, `lib.rs:611-615`) with a
     24-byte binary ROI header prepended for interactive drags, returned as a raw
     `tauri::ipc::Response` (`lib.rs:776`). A separate aux command
     (`generate_uncropped_preview`, `lib.rs:782-915`) instead returns a base64 data-URI string.
  This confirms the structural question the plan flagged: on the platform RapidRAW actually
  optimizes for (Windows/macOS), it avoids the full-frame host↔device round-trip Nicti's own
  ADR-0005 rules out — **no conflict there**. The CPU-readback fallback path exists only for
  platforms/settings where the direct-surface path isn't available.
- **GPU pipeline — GPU-resident once uploaded, but with a real pre-GPU CPU stage.** Flare, blur
  (ping-pong across `ping_pong_view`/`sharpness_blur_view`/etc., `gpu_processing.rs:1466-1546`),
  and the main adjustment pass (lines 1558-1647) all bind directly to GPU texture views with zero
  intermediate host readback when `output_to_display: true` (lines 1652-1692). **But** geometric
  ops — distortion warp, crop, coarse rotation, flips, lens blur, AI inpainting — run on the host
  CPU via `image::DynamicImage` *before* the result is uploaded to `GpuImageCache`
  (`lib.rs:412-441`, `gpu_processing.rs:1917-1947`). So "no full-frame host↔device round-trip" is
  true for the color/tone stages only, not the geometry stages — a real architectural difference
  from Nicti's own decided all-GPU-resident stage chain (ADR-0002/ADR-0005), not a match.
- **Render graph — none. Confirmed absent, not just unfound.** No DAG, no per-stage cache, no
  blake3-style memoization. Every slider tweak re-runs the *entire* GPU pipeline top-to-bottom
  through `GpuProcessor::run` (`gpu_processing.rs:1185-1742`, reached via
  `process_and_get_dynamic_image_inner`, lines 1813-2200, from `process_preview_job`, `lib.rs:
  371-665`) — one monolithic 1,997-line WGSL compute shader
  (`shaders/shader.wgsl`, line count confirmed directly) dispatched once per tile
  (`gpu_processing.rs:1639-1647`) evaluating exposure, white balance, curves, HSL, color grading,
  tonemapping, LUTs, and vignette together in a single pass. The only caching that exists is coarse
  and pre-pipeline: a geometry-transform hash skip (`calculate_transform_hash`, `lib.rs:394-431`)
  and an input-texture-reupload skip when dimensions/geometry are unchanged
  (`GpuImageCache`, `gpu_processing.rs:1940-1946`). **This is the single most consequential finding
  for #44/ADR-0018**: RapidRAW's actual render strategy is architecturally the opposite of
  Tapetum's planned design (a real per-stage-cached DAG). Adopting RapidRAW's GPU pipeline code
  would mean adopting a design Nicti's own ADR-0002 already reasoned past, not a shortcut to it.
- **Module structure — monolithic, no extension-point abstraction.** Free functions and concrete
  structs in flat per-concern files (confirmed: only four small utility traits exist in the entire
  backend — `IntoCowImage`, `FrameSource` (focus-stacking only), `FillChannel`/`SolidFill`
  (export watermarks only) — none of them a Claw-style `Module`/`Registry` extension point). All
  60+ Tauri commands are registered imperatively in one `tauri::generate_handler![...]` block
  (`lib.rs:2144-2244`). No equivalent of Nicti's own `nicti-claw` lazy-registry design exists to
  adopt.

## vkdt

**License**: BSD-2-Clause (`gh api repos/hanatos/vkdt --jq .license.spdx_id`) — permissive, no
copyleft interaction to think about at all, the least license-constrained of the three.
**Language**: **C, not Rust** (`gh api … --jq .language`) — vkdt is a from-scratch Vulkan-native
rewrite of darktable's C codebase, not a Rust project. Its own README (`vkdt/readme.md:11-14`)
confirms: "the processing pipeline is a generic node graph (DAG) … all processing is done in
glsl shaders/vulkan… the gui profits from this scheme as well and can display textures while they
are still on GPU" — the closest prior-art match to Nicti's own already-decided GPU-resident,
no-full-frame-round-trip design (ADR-0005) and Tapetum's planned stage-cache DAG (#44), even though
the implementation language doesn't transfer.

- **RAW decode**: does use `rawler` — but via a thin WTFPL C-binding crate the vkdt author wrote
  himself (`src/pipe/modules/i-raw/rawloader-c/Cargo.toml`, `rawler = { git =
  "https://github.com/dnglab/dnglab", branch = "main" }` — **upstream dnglab directly, not
  RapidRAW's fork**), compiled to a `staticlib` and linked from C. Its own readme
  (`i-raw/readme.md:3-4`) says the module "uses eithor the rawspeed library (c++) or the rawloader
  library (rust)" — it's a dual/optional path, not a hard rawler dependency. The license question
  is identical: upstream `dnglab`'s `rawler` crate itself is the same bare `LGPL-2.1` (no
  `-or-later`), confirmed by the same source-header check done above.
- **Edit storage**: per-directory `*.cfg` sidecar files ("these are the actual input files to a
  loaded directory", `src/db/readme.md:7-8`) plus a minimal per-directory `vkdt.db` for
  rating/labels only (line 11-12) — no central catalog DB. Thumbnails cache to
  `.cache/vkdt/<murmur3-hash>.bc1`, GPU-native BC1-compressed on disk (line 17-19). This is a much
  lighter-weight model than Nicti's own catalog-authoritative design (ADR-0002) and not directly
  reusable, but the "write processing params as a flat ascii key:value sidecar, no binary format
  needed" idea (`src/pipe/readme.md:57-67`, e.g. `exposure:ev:2.0`) is a plausible reference for
  Nicti's own XMP `nicti:` namespace projection (ADR-0002).
- **Render graph**: real DAG with topological-sort scheduling directly to a Vulkan command buffer
  (`src/pipe/readme.md:1-45`) — one compute shader per node, with a distinct "self-configuring node
  layer" for cases where the graph shape depends on the current region-of-interest (line 34-42,
  e.g. a preview pipe with a smaller ROI). This is the single most directly relevant prior-art
  reference for #44 (Tapetum's stage-cached render graph design) of any project studied here — a
  real, shipping Vulkan DAG scheduler to compare design choices against, even though vkdt is C and
  wgpu-vs-raw-Vulkan is already a settled question (ADR-0005).

## Ansel

**License**: GPL-3.0 — repo-level metadata says just "GPL-3.0", but source file headers carry the
full grant: `src/win/strptime.c:11` (and every other checked header) reads "(at your option) any
later version" — confirmed **GPL-3.0-or-later**, not `-only`. GPL-3.0-or-later combines cleanly
with Nicti's own AGPL-3.0-or-later: AGPLv3 §13 exists specifically to make it license-compatible
with GPLv3 (and, by the `-or-later` grant on both sides, with future versions of either) — see the
FSF's own "GPL-Compatible Free Software Licenses" list, which names GNU AGPLv3 explicitly[^ansel1]
— so, in principle, Ansel code could be reused without a license conflict, same as RapidRAW.
**Language**: C/GTK, a hard fork of darktable — **not Rust**, and its own architecture is
darktable's well-known reorderable IOP module stack: `doc/history-split.md` (measuring an in-flight
internal refactor, not upstream-facing docs, but load-bearing evidence of the real shape) confirms
each history item holds "a `dt_iop_module_t *` and a params blob typed by module"
(`history-split.md:9`) — this is exactly the **reorderable op-stack model Nicti's ADR-0002 already
evaluated and rejected** in favor of a fixed-order stage-parameter map. Ansel is architecturally the
least relevant of the three to Nicti's own decided design, matching the issue's own expectation
("less architecturally relevant, not Rust").

**Borrow candidates**: none identified as directly portable given the language and architecture
mismatch; worth a UX/workflow-only look (masking UI, history-stack presentation) if #48/#51 ever
need a reference for interaction design, not for backend structure.

## Cross-cutting takeaways (by downstream ticket)

- **#37 (RAW decoder)**: both RapidRAW and vkdt reach for the same underlying crate family
  (`rawler`/dnglab), by two different maintainers, independently confirming it's the community's
  leading pure-Rust decoder option — but **neither resolves the `LGPL-2.1` bare-identifier
  ambiguity** ADR-0013 already flagged. That's still #37's own open question to close, not
  something this research settles.
- **#38/#39 (color pipeline, lens corrections)**: RapidRAW evaluates color grading, curves, HSL,
  tonemapping, and LUTs together inside one monolithic per-tile shader
  (`gpu_processing.rs:1639-1647`) rather than as separable stages — not directly reusable given
  Nicti's fixed-order stage-map design (ADR-0002), but a real data point that a single-pass
  combined shader is a viable perf strategy if Tapetum's staging ever needs a "collapse contiguous
  color-only stages into one dispatch" optimization.
- **#44/#45 (Tapetum)**: vkdt's Vulkan DAG (topological sort, per-ROI self-configuring subgraphs) is
  the standout reference of the three. **RapidRAW is the opposite data point, not a second
  reference**: confirmed no DAG, no per-stage cache, full top-to-bottom re-run per slider tweak
  (see RapidRAW section above) — useful mainly as evidence that Tapetum's stage-cache design is a
  real differentiator against the closest architectural peer, not a solved problem elsewhere.
- **#47 (crop/auto-level)**, **#48/#51 (masking/healing)**, **#56 (export)**, **#99 (auto-tone)**:
  RapidRAW is confirmed to ship SAM-ViT-B masking, LaMa inpainting, and a CLIP-based
  search/tagging path (not masking) — real, shipping implementations worth studying at the
  algorithm level once the deep-dive names the actual entrypoint functions.
- **#68/ADR-0006 (GUI)**: the corrected finding (Tauri+React, not egui) removes what was previously
  cited as supporting evidence for egui — see the ADR-0006 Amendments note for whether this changes
  anything material (it shouldn't: the citation was in Prior art, not Decision, and egui's lead
  came from its own wgpu-30 match and license, not from RapidRAW's stack).

**Status: complete.** All three projects' architecture, license, and reuse questions raised by #69
are answered above with file:line citations, each spot-checked against the actual source after the
`ask-gemini` deep-dive returned. See `docs/adr/0018-rapidraw-adopt-or-fork.md` for the resulting
go/no-go recommendation.

[^ansel1]: FSF, "Various Licenses and Comments about Them" (GPL-Compatible Free Software Licenses
    section), listing the GNU Affero General Public License version 3 as GPLv3-compatible —
    https://www.gnu.org/licenses/license-list.html#AGPLv3.0 — verified 2026-09-26
