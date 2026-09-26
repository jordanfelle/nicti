# ADR-0018: RapidRAW as an adopt/fork candidate — not adopted, study-only

- **Status:** Proposed
- **Date:** 2026-09-26
- **Ticket:** [#69](https://github.com/jordanfelle/nicti/issues/69) Research: prior art — RapidRAW,
  vkdt, Ansel

## Context

ADR-0013 (outbound license AGPL-3.0-or-later) reopened RapidRAW as a genuine fork/adopt candidate
rather than study-only prior art, since it's also AGPL-3.0 — no license conflict in reusing its
code directly. #69 asked for a real go/no-go on that question, not just a comparison. The full
per-project findings, with file:line citations, are in
`docs/research/stalk-prior-art.md`; this ADR is the decision that findings doc feeds.

## Decision rule

Adopt (fork whole, or lift specific modules wholesale) only if RapidRAW's architecture in the area
being considered is compatible with Nicti's already-decided ADRs (0001/0002/0004/0005/0006) without
requiring a rewrite that erases most of the value of starting from its code. Where a specific
subsystem's architecture conflicts with an already-decided ADR, that subsystem is study-only —
useful as a reference implementation to read, not code to import.

## Decision

**Not adopted, whole or by module. Study-only.** RapidRAW is real, shipping, well-tested code from
a project with directly overlapping scope, and reading it is worth doing when implementing the
matching Nicti tickets — but every load-bearing architectural choice it makes conflicts with a
decision Nicti has already made for reasons specific to Nicti's own goals, not because RapidRAW's
choices are wrong for RapidRAW's own goals (a solo/small-team GUI-first Tauri app, not a
catalog-scale DAM with a 380k-asset real library and a stage-cached render-graph requirement).

### Per-ADR compatibility check

| Nicti ADR | RapidRAW's actual choice | Compatible? |
|---|---|---|
| ADR-0001 (Rust) | Rust backend (`src-tauri`), but paired with a TypeScript/React frontend via Tauri | **Partial.** The backend language matches; the UI layer is dead weight if ADR-0006 lands on egui (a native Rust immediate-mode UI, no webview, no JS frontend at all). Forking would mean either keeping Tauri/React (contradicts #68's own scope, which names four Rust-only GUI candidates) or discarding ~117 `.tsx`/`.ts` files and the entire Tauri IPC layer — at which point the backend crates are what's left, and those have their own conflicts below. |
| ADR-0002 (catalog-authoritative, fixed-order stage map, blake3 per-stage hash) | Per-image `.rrdata` JSON sidecar, no catalog DB, `adjustments: Value` — one untyped blob, not typed per-stage params | **Conflicts.** There is no catalog row to fork into; RapidRAW's metadata model would need to be rebuilt from scratch to fit ADR-0002, at which point none of `file_management.rs`'s sidecar-path logic survives. |
| ADR-0004 (Claw lazy-registry module/plugin architecture) | Monolithic free functions in flat per-concern files; only 4 small utility traits in the whole backend, none an extension point | **Conflicts.** No `Module`/`Registry` shape to adopt; RapidRAW's structure is the "before" case ADR-0004 was written to avoid. |
| ADR-0005 (wgpu 30/Vulkan, GPU-resident, no full-frame host↔device round-trip in the hot path) | wgpu 29 (pinned down for a macOS P3 color-shift issue); color/tone stages are GPU-resident with no round-trip, but geometry ops (crop, warp, lens blur, AI inpaint) run CPU-side via `image::DynamicImage` before GPU upload | **Partial.** The color/tone half is a real match; the geometry half is architecturally different from Nicti's all-GPU-resident stage chain. Adopting the shader set would mean adopting this CPU/GPU split too, or extracting only the WGSL and rewriting the surrounding Rust dispatch code — most of the "reuse" value disappears either way. |
| ADR-0006 (GUI framework, egui leading) | Tauri + React (see ADR-0001 row) | **Conflicts** with the leading candidate; RapidRAW's own choice isn't in #68's candidate list at all. |
| #44/Tapetum (stage-cached render graph) | **No DAG, no per-stage cache — confirmed absent.** Full top-to-bottom pipeline re-run on every parameter tweak, into one 1,997-line monolithic WGSL shader | **Direct conflict**, the most consequential one. This is precisely the design Tapetum exists to avoid. Adopting RapidRAW's GPU pipeline code as a starting point for Tapetum would mean adopting the anti-pattern and then re-deriving a stage cache on top of it — no shortcut over building it from scratch against Nicti's own edit model. |
| ADR-0003/0013 (license policy) | AGPL-3.0, same family as Nicti — but see the dependency-graph result below | **Compatible at the license-family level**, with one real exception (`rawler`, see below). |

### Dependency-graph / license result (the one clean, adoptable data point)

Running Nicti's own `deny.toml` against RapidRAW's full resolved dependency graph (`cargo deny
check licenses`, 583 unique crates) produced exactly **one rejection**: `rawler v0.7.1` (bare
`LGPL-2.1`, no `-or-later` suffix, confirmed in source headers not just `Cargo.toml`). This was the
same open question ADR-0013 originally flagged for upstream `rawler` — RapidRAW's own fork
(`RapidRAW-DngLab`) doesn't resolve it; it only patches a highlights-clamping behavior, license
headers untouched. **Resolved since (2026-09-25/26, #37/#138, see ADR-0019's Licensing section):**
LGPL-2.1 §§5–6 permit combining a bare-`LGPL-2.1` library into Nicti's AGPL-3.0-or-later work
directly, no "or-later" grant or upstream answer needed. **This is real, useful evidence for #37**,
independent of the adopt/fork question: RapidRAW's own real-world use of `rawler` (in production,
10.2k stars, active) is corroborating evidence it's a viable v1 decoder choice. Every other crate
in RapidRAW's graph — all 582 remaining, including `ort`/`load-dynamic`, `image`, `imageproc`,
`jxl-oxide`, `image_hasher` — clears Nicti's existing allowlist with zero additions needed.

### AI model stack — real value, but as reference not as code

RapidRAW ships working integrations for SAM-ViT-B (masking, not SAM2 as #69 assumed), LaMa
(inpainting), Depth-Anything-V2 (vits), U-2-Net (general + sky-segmentation variant), and CLIP
(search/tagging). Every model is downloaded on demand from the author's own HuggingFace mirror at
runtime, not bundled — so there's no license-of-the-repo-vs-license-of-the-weights entanglement to
resolve, only each model's own upstream license (the same category of open question ADR-0007
already has for LaMa/Places2). The `ort`/`load-dynamic` integration pattern itself matches what
ADR-0004 §3 already decided for Nicti — worth reading `ai_processing.rs`'s
download/verify/session-creation code (`download_and_verify_model`, `ai_processing.rs:408`) as a
reference implementation when #48-#53 build the equivalent, but not importing the file wholesale
given the monolithic-module conflict above.

## What to actually do with this research

- **#37 (RAW decoder)**: cite RapidRAW's and vkdt's independent convergence on `rawler`/dnglab as
  evidence it's the community's leading pure-Rust option. The LGPL-2.1 compatibility question is
  now resolved (see above) — not an open item for #37 anymore. The per-release §6 distribution
  checklist still applies before `nicti-decode` ships either dependency: §6(d) (binary and
  complete source available from the same place), plus the separate notice and shipped
  license-text requirements — see ADR-0019's deferred item 2.
- **#44/#45 (Tapetum)**: use vkdt's Vulkan DAG (`docs/research/stalk-prior-art.md`'s vkdt section)
  as the real prior-art reference for the stage-cache design, not RapidRAW's.
- **#48/#51 (masking/healing)**: read RapidRAW's SAM/LaMa/Depth-Anything integration code as a
  reference for the ONNX session lifecycle and mask-generation flow, not for adoption.
- **#68/ADR-0006**: correct the existing "RapidRAW uses egui" citation (wrong — see that ADR's own
  Amendments note added in this same PR); no other change to ADR-0006's own decision, since the
  citation was in Prior art, not Decision.
- No new dependency, model, or code is adopted by this ADR. `deny.toml`/`docs/licensing.md` are
  unchanged.

## Consequences

- Closes #69's own "adopt-or-fork, not just prior-art" ask with a clear no, backed by a per-ADR
  compatibility table rather than a general impression.
- RapidRAW stays a valuable reference implementation to consult while building #37/#44/#48, just
  not a codebase to fork or a dependency to add.
- If a future architectural decision reverses course on Tapetum's stage-caching requirement (very
  unlikely, but the honest caveat), RapidRAW's simpler monolithic-shader model would become a much
  more attractive adoption candidate — worth remembering rather than re-researching from scratch if
  that ever comes up.
