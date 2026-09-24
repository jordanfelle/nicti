# Groom: healing/removal — classic clone/heal measurements + AI-removal licensing findings

Findings for [#50](https://github.com/jordanfelle/nicti/issues/50), feeding
[#51](https://github.com/jordanfelle/nicti/issues/51)'s AI-removal shipping decision and
[docs/adr/0007-healing-and-removal.md](../adr/0007-healing-and-removal.md)'s design. Tooling:
`spikes/groom/` — see that crate's own module docs. This is a findings doc, not an ADR; the
shipping decision itself belongs to #51.

## Method

Two independent halves, measured differently:

- **Classic clone/heal** was implemented and measured directly in this sandbox: a CPU reference
  (clone stamp, gradient-domain/Poisson spot heal via Jacobi iteration, SSD-over-a-ring
  auto-source-pick) and a `wgpu` compute-shader twin of the Poisson solver, checked against each
  other for correctness and timed on CPU (this sandbox has no GPU-backed Vulkan/Dx12 adapter).
- **AI-removal licensing** was desk research only: WebSearch/WebFetch against public sources for
  the Places2 dataset's own terms (which `docs/licensing.md` had already flagged as a live,
  unverified concern for LaMa) and for one alternative inpainting model (MI-GAN) with different
  claimed provenance, to see whether it's actually cleaner. No inference was run for either model
  — no real ONNX weights exist in this sandbox, and obtaining them is explicitly out of scope for
  this spike.

## Classic clone/heal: measured

CPU-only (no GPU-backed adapter in this sandbox), via `cargo test -p groom --test throughput
--release -- --ignored --nocapture`: one discarded warm-up run + 20 measured runs per operation,
512×512 synthetic checkerboard image, `radius = 20`, `feather = 4`, 50 Jacobi iterations for the
heal case.

| Operation | Mean time |
|---|---|
| `clone_stamp` | 0.1209 ms/op |
| `spot_heal` (50 Jacobi iterations) | 0.2458 ms/op |
| `auto_source_pick` (24 candidates) | 0.0542 ms/op |

All three are comfortably inside a 16.7ms (60fps) interactive budget *on CPU, at this small patch
size and iteration count* — not a claim about GPU throughput at hero-scenario resolution, which
this sandbox cannot measure (see ADR-0007's Measured results section for the explicit "TBD —
reference machine" markers on every GPU/CUDA number).

The `wgpu` compute-shader Poisson solver (`shaders/poisson_jacobi.wgsl`) was checked for
**correctness** against the CPU reference in `tests/correctness.rs` — both implement the exact
same per-pixel Jacobi update rule, and matched within `1e-3` absolute tolerance on a 16×16
synthetic patch across every iteration tested (including the zero-iteration edge case, which
proved the ping-pong buffer-selection logic handles an even iteration count correctly). This test
ran against a real wgpu adapter in this sandbox (a software/lavapipe-class Vulkan backend under
WSL, not a hardware GPU) — it proves the shader's *logic* is correct, not its throughput on real
hardware.

`HealStage` serialized size (canonical `serde_json`, via `cargo test -p groom
spot::tests::spot_list_sizes_at_1_10_and_50 -- --nocapture`), a representative mix of `Clone`/
`Remove` spots:

| Spot count | Serialized bytes |
|---|---|
| 1 | 120 bytes |
| 10 | 1,511 bytes |
| 50 | 7,531 bytes |

## AI-removal: LaMa/Places2 licensing

`docs/licensing.md` already flagged LaMa's `big-lama` checkpoint: Apache-2.0 code license, but
trained on the **Places2** dataset, whose own terms were previously unreachable during that
earlier audit pass (`places2.csail.mit.edu` returned a connection error).

**This pass's finding: still not a clean resolution, but real new information.** A direct fetch
of `places2.csail.mit.edu` was attempted again and again returned a connection error in this
session — the primary source remains genuinely unreachable from this sandbox, not just
inconvenient to reach. A WebSearch did surface a working page
(`places2.csail.mit.edu/download-private.html`) stating Places2's terms plainly: **"you will use
the data only for non-commercial research and educational purposes and will NOT distribute the
images."** This is the dataset's own stated position, found via search rather than a direct fetch
of the canonical URL in this exact session — treat it as best-available-secondary, not a
freshly-verified primary-source read, and re-verify with a direct fetch before treating it as
settled.

**What this does *not* resolve**: whether a non-commercial, no-redistribution restriction on the
*training data* legally propagates to a *model trained on it*, when that model's own weights carry
no separate restriction of their own (LaMa's repository states Apache-2.0 for its code and states
nothing separately for the `big-lama` checkpoint specifically). This is a genuinely unsettled
question this research pass has no authority to answer, and did not find a definitive legal
answer for anywhere in the sources checked. The practical, conservative reading — and the one
`docs/licensing.md`'s flag already assumed — is to treat LaMa's `big-lama` checkpoint as **not
clear to bundle**, while remaining legally usable for research/evaluation (this spike's own use)
and shippable as a user-initiated **on-demand download** rather than something Nicti's installer
redistributes itself.

## AI-removal: alternative model researched (MI-GAN)

MI-GAN (Picsart AI Research, ICCV 2023) was researched as a candidate alternative to LaMa with
potentially different training-data provenance. **Finding: it is not a cleaner alternative — on
both axes this research checked, it is either equally flagged or actively murkier than LaMa, not
better:**

- **Code license**: MIT (`github.com/Picsart-AI-Research/MI-GAN`'s own `LICENSE` file).
- **Weights license**: the repository ships a *separate* `LICENSE-WEIGHTS` file specifically for
  the pretrained checkpoints — a direct fetch of that file's own text (not a fetch of the repo's
  main `LICENSE`) shows it is *also* written as a permissive MIT-style grant, not an explicit
  non-commercial license. A GitHub issue (`Picsart-AI-Research/MI-GAN#25`) raised the question of
  whether that MIT grant over the *weights* is even legitimate: MI-GAN's training pipeline
  distills from a Co-Mod-GAN teacher model, and Co-Mod-GAN's own license is the **NVIDIA Source
  Code License-NC**, whose §3.2 requires the non-commercial term to carry over to derivative
  works. **The issue is closed** (2026-09-14, ten days before this pass) — the maintainer
  (`AndranikSargsyan`, a repo collaborator) confirmed the weights (`.pt` and ONNX) are released
  "under the same MIT license included in this repository," and added the `LICENSE-WEIGHTS` file
  *in direct response to this issue*. On the harder legitimacy question the maintainer explicitly
  declined to rule: "I am not qualified to answer that question. If you have any concerns, I
  recommend consulting a lawyer." So this isn't "unanswered" — the license-grant question is
  answered — but it also isn't "confirmed clean": it's **"the weights are labeled permissive, the
  maintainer stands behind that label, but the deeper legitimacy question was explicitly punted to
  legal counsel rather than resolved."** That residual, maintainer-acknowledged uncertainty is
  still arguably a *harder* thing to get comfortable shipping than an openly-restrictive license
  would be, even though it's a materially weaker claim than "unanswered."
- **Training data**: **Places2 and FFHQ** — the exact same Places2 exposure LaMa already carries,
  with no improvement there at all.

**Conclusion: MI-GAN is not a way around LaMa's flag, and may be a worse choice, not a better
one.** It carries the exact same Places2 exposure LaMa does, *plus* a specifically-named legal
question that its own maintainer declined to resolve and punted to legal counsel — a strictly
murkier position than LaMa's "Apache-2.0 code, presumed-but-not-separately-stated weights license,
flagged Places2 provenance." This is a
genuinely useful negative result: it means the "just find an alternative with cleaner provenance"
instinct doesn't automatically pay off, and the search for a truly clean checkpoint should look
elsewhere (a model whose training corpus avoids Places2/FFHQ-lineage data and NVIDIA-NC-licensed
teacher models entirely) rather than stopping at the first alternative found.

No other alternative was evaluated in this pass beyond MI-GAN — a fuller survey (AOT-GAN, CM-GAN,
MAT, or a diffusion-based inpainter) is worth a dedicated pass if #51 decides LaMa's flag is a
real blocker rather than an acceptable on-demand-download risk.

## Reproducing / extending this run

```bash
# Classic clone/heal timings (CPU-only in this sandbox):
cargo test -p groom --test throughput --release -- --ignored --nocapture

# wgpu-vs-CPU Poisson correctness (runs against whatever adapter is available, skips cleanly if none):
cargo test -p groom --test correctness

# HealStage serialized-size numbers:
cargo test -p groom spot::tests::spot_list_sizes_at_1_10_and_50 -- --nocapture

# AI-removal graceful-failure path (no model file needed):
cargo test -p groom ai::tests
```

Re-verifying the licensing findings: retry a direct fetch of `https://places2.csail.mit.edu/` and
its `download-private.html` page (both returned a connection error from this sandbox during this
pass); re-read `github.com/Picsart-AI-Research/MI-GAN`'s own `LICENSE-WEIGHTS` file directly
(this pass relied on a GitHub issue's summary of it, not a direct read of the file's own text).

## Recommendations for #51

- **Ship classic clone/heal now** — it's proven correct (CPU vs. GPU-shader parity) and fast
  enough on CPU alone that even a conservative GPU estimate should clear the interactive budget;
  the only remaining unknown is real hero-scenario-resolution GPU throughput, which needs the
  reference machine, not further sandbox work.
- **Ship LaMa as an on-demand download, not bundled, until Places2's terms are directly
  re-verified** (a fresh fetch of the primary source, not this pass's search-engine-mediated
  finding) or a genuinely clean-provenance checkpoint is found. Do not adopt MI-GAN as a
  workaround — this pass found it strictly worse on licensing, not better.
- **Budget a dedicated, wider model survey before committing to LaMa long-term** if the on-demand-
  download workaround turns out to be a poor user experience — this pass only checked one
  alternative and found it wanting; that's evidence LaMa's flag is worth taking seriously, not
  evidence LaMa is definitely the only option.
- **Real MobileSAM/LaMa ONNX weights and a real `ort`/ONNX-Runtime-dylib environment are needed**
  before #51 can validate this spike's `ai.rs` wrappers past their graceful-failure path, or
  measure real inference latency against the <2s/removal budget ADR-0007 states as a hypothesis.
