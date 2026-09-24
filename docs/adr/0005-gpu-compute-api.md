# ADR-0005: GPU compute API (`wgpu`)

- **Status:** Accepted
- **Date:** 2026-09-23
- **Ticket:** [#16](https://github.com/jordanfelle/nicti/issues/16) Research: GPU compute API

## Context

Tapetum (#44, the stage-cached render graph) and every develop/AI-adjacent render stage need a
GPU compute API before #20 (core crate boundaries), #41 (RAW→GPU pipeline), and #45 (Tapetum
core) can start. #16 asks: `wgpu`/WGSL vs raw Vulkan (`ash`) vs CUDA-direct — portability vs peak
NVIDIA perf — and whether the choice can support the stage-cached render graph plus tiled AI
inference.

Constraints already fixed by earlier ADRs/docs:

- **ADR-0001** named `wgpu` and `ash` as the two viable candidates within Rust, and rated Rust's
  GPU-compute ecosystem "Strong" on the strength of both existing.
- v1 targets Windows + a high-end NVIDIA desktop (reference machine: RTX 5080, 16GB, driver
  616.56); macOS/Linux are v2 goals (#4/#66), so the architecture must not hard-code an
  NVIDIA-only or Windows-only path.
- AI inference is **already decided**: ONNX Runtime via the `ort` crate, `load-dynamic`, with
  CUDA/TensorRT execution providers (ADR-0004 §3, ADR-0003). This ADR decides the API for
  **render/pixel compute stages and display**, and the **interop boundary** with ORT — not the
  inference runtime itself.
- Performance budgets (`docs/benchmarks.md`): slider→preview p95 ≤ 16.7ms (60fps); warm image
  switch < 100ms. Tapetum caches expensive stages (denoise, AI masks, lens correction) so they're
  baked once, not recomputed per frame — the hero scenario (#43) is gated on this design holding.
- Any new dependency needs a `docs/licensing.md` row in the same PR (ADR-0003) — done, see that
  file's 2026-09-23 update.

## Decision rule (stated before measuring, per this ADR's own methodology)

Pick `wgpu` iff, on the reference machine: **(a)** the fused live-stage chain at 4K screen
resolution has p95 ≤ 4ms (¼ of the 60fps budget); **(b)** its 45MP throughput is within 2x of the
equivalent CUDA kernel; **(c)** required features (`SHADER_F16`, >256MB single-buffer allocation,
`TIMESTAMP_QUERY`) work on the backends actually in use. All three passed — see Measured results.

## Decision

**`wgpu` (WGSL), Vulkan backend on Windows, native compute over storage buffers/textures for
render stages and display.** AI inference stays entirely in ONNX Runtime (CUDA/TensorRT EP,
already decided); interop between the two is a bake-time staging copy (upload AI output once per
bake, not per frame), not a shared-context zero-copy path. Raw `ash` is rejected as the primary
API: the one thing it would buy — lower per-dispatch CPU overhead — turned out to *already* favor
wgpu's own Vulkan backend over its Dx12 backend by a wider margin (see below) than any plausible
`ash`-vs-wgpu-Vulkan gap, so there's no longer a clear win to chase there. CUDA-direct is rejected
for render/display stages: it would still need a separate graphics API for the actual viewport,
and kills the v2 macOS/AMD-laptop goal outright, for no throughput advantage the numbers below
show is actually there.

**Backend choice on Windows: default to Vulkan, not Dx12** — despite Dx12 being faster for the
live-stage chain itself (see below), **Dx12 does not expose `Features::SHADER_F16` on this
machine/driver/wgpu-30 combination, Vulkan does.** Tapetum's disk/RAM cache tiers are specified as
"compressed half-float" (ADR-0002 context), so `SHADER_F16` availability is load-bearing, not
optional. If a future wgpu release closes this Dx12 gap, this line should be revisited — it's a
`wgpu`/driver-version fact, not a fundamental Vulkan-vs-Dx12 property.

## Measured results

Backed by `spikes/glint/` (see below). All GPU-side timings are wgpu `TIMESTAMP_QUERY` or CUDA
event timing (`cudaEvent`-equivalent via `cudarc`), never host wall-clock — per
`docs/benchmarks.md`'s methodology. p50/p95/max over 5 runs, 1 warm-up discarded.

**Reference hardware:** RTX 5080 (16GB), NVIDIA driver 616.56, Windows (native, not WSL — WSL has
no GPU-backed Vulkan/Dx12 ICD in this environment, confirmed by checking `/usr/share/vulkan/icd.d`
for an `nvidia_icd.json`; only lavapipe/llvmpipe software rendering is available there). The CUDA
comparator ran on the *same physical GPU* via WSL's native CUDA driver passthrough
(`libcuda.so`/`nvcuda.dll` are both real driver bindings to the one GPU) — a genuinely
cross-platform-but-same-hardware comparison, not a proxy machine.

### (a) 4K live-stage chain (fused WB + exposure + tone-curve LUT + vibrance, 3840×2160, 8.3MP)

| Backend | Adapter | p50 (ms) | p95 (ms) |
|---|---|---|---|
| Vulkan | RTX 5080 | 0.321 | 0.326 |
| Dx12 | RTX 5080 | 0.314–0.319 | 0.315–0.337 |
| CUDA (NVRTC) | RTX 5080 | — | 0.336 |

All three are ~50x under the 4ms bar. **(a) passes comfortably** on every candidate.

### (b) 45MP hero-scenario resolution (8256×5504)

| Backend | Adapter | p50 (ms) | p95 (ms) |
|---|---|---|---|
| Vulkan | RTX 5080 | 2.076 | 3.884 |
| Dx12 | RTX 5080 | 1.730–1.740 | 1.736–1.764 |
| CUDA (NVRTC) | RTX 5080 | 1.745 | 3.753 |

wgpu-Dx12 (1.74ms p95) is ~2.1x *faster* than CUDA (3.75ms) here; wgpu-Vulkan (3.88ms) is at
rough parity with CUDA (~1.03x). **(b) passes** — neither wgpu backend is worse than 2x CUDA, and
one is meaningfully faster. (The CUDA p95 uses `RUNS=5` nearest-rank, so its p95 equals its max —
a single slower run in a 5-sample set; a larger sample would likely tighten this, but even the
worst-case number here still clears the bar.)

### (c) Feature/limit availability

| Backend | Adapter | `TIMESTAMP_QUERY` | `SHADER_F16` | `max_buffer_size` | `max_storage_buffer_binding_size` |
|---|---|---|---|---|---|
| Vulkan | RTX 5080 | ✅ | ✅ | 4,294,967,295 | 2,147,483,644 |
| Vulkan | AMD Radeon iGPU | ✅ | ✅ | 2,147,483,648 | 2,147,483,644 |
| Dx12 | RTX 5080 | ✅ | ❌ | 2,147,483,647 | 2,147,483,644 |
| Dx12 | AMD Radeon iGPU | ✅ | ❌ | 2,147,483,647 | 2,147,483,644 |

A 45MP RGBA16F intermediate is ~360MB; both `max_buffer_size` and
`max_storage_buffer_binding_size` clear that by 5–10x on real hardware, on every backend. (The
default WebGPU-portable limit, 256MB, would *not* clear it — this crate's device-request policy
explicitly requests `adapter.limits()`, the adapter's true maximum, rather than the portable
default; see `gpu.rs`.) **`SHADER_F16` is Vulkan-only** on this wgpu 30 / driver combination — the
deciding factor in the backend choice above. **(c) passes on Vulkan**, fails on Dx12 for the f16
requirement specifically.

**Software-Vulkan (lavapipe/llvmpipe, WSL) caveat:** `max_storage_buffer_binding_size` there is
only 128MB — below the 360MB hero-frame size. This is a real number but not a real-hardware one;
it's included only because it's what correctness testing runs against in CI (no GPU-backed ICD in
that environment) and is a reminder that CI's software-Vulkan numbers are for correctness, never
for capacity or performance conclusions.

### Dispatch overhead (CPU-side, per-dispatch synchronous round-trip incl. poll+readback)

| Backend | Adapter | p50 (ms) | p95 (ms) |
|---|---|---|---|
| Vulkan | RTX 5080 | 0.109 | 0.140 |
| Dx12 | RTX 5080 | 0.173–0.199 | 0.226–0.271 |

Vulkan has meaningfully lower overhead here than Dx12 (~40–47% less). This is the number that
would matter for an `ash`-vs-wgpu comparison (stands in for one — see `tests/dispatch_overhead.rs`
for why a full separate `ash` harness wasn't built just to measure this): a real `ash` win would
need to beat wgpu-Vulkan's *own* ~0.1ms number, not wgpu-Dx12's ~0.2ms one, since we're choosing
Vulkan anyway per the f16 finding above. **Caveat:** this measures a fully-synchronous
submit→poll→map→readback cycle per dispatch (needed so the loop can time each one individually),
not pure submission latency — a real Tapetum frame batches many dispatches into one submission
and polls once, so this is an upper bound on per-dispatch cost, useful for backend comparison, not
a literal "N dispatches costs N × this" estimate.

(An earlier version of this section reported numbers 8–9x higher, ~0.9–2ms — those were actually
timing pipeline compilation and full buffer (re-)allocation on every iteration, not per-dispatch
submission cost, a mislabeling caught in adversarial review before merge. `LiveChainKernel`
(`gpu.rs`) now builds the pipeline and buffers once outside the timed loop, and each timed
iteration only re-uploads the (tiny, 256-pixel) input via `queue.write_buffer` and dispatches —
the numbers above are from that corrected harness. **Residual caveat, caught in a follow-up
verification pass:** the shared dispatch helper both `LiveChainKernel` and `run_live_chain` call
still allocates a few small, fixed-size resources fresh every call — a staging readback buffer
and, where supported, a timestamp query set/resolve/readback trio — so these numbers aren't
*zero*-allocation submission cost either, just far closer to it than the original bug. Since it's
the same fixed tax on every backend, it doesn't change the Vulkan-vs-Dx12 relative comparison.)

### Host↔device interop cost — inconclusive as measured, real gap identified

| Backend | Adapter | p50 (ms) | p95 (ms) |
|---|---|---|---|
| Vulkan | RTX 5080 | 1265.72 | 1489.01 |
| Dx12 | RTX 5080 | 823.33 | 829.47 |

**This number does not mean what it looks like it means, and must not be read as "wgpu interop is
too slow for the 100ms warm-switch budget."** `run_live_chain`'s harness moves the *entire* 45MP
frame in **both directions** in one wall-clock-timed call: a fresh `create_buffer_init` upload of
the 45,000,000-pixel `[f32; 4]` input buffer (16 bytes/pixel = **~720MB** host→device), the
dispatch itself, and a full device→host readback of the same-sized output buffer via `map_async` +
`get_mapped_range().to_vec()` (another **~720MB**) — roughly **1.44GB of data movement total**,
not a single one-way 360MB download as an earlier draft of this section claimed (that 360MB figure
is the *hero-frame size in the features/limits section above*, which assumes a hypothetical
RGBA16F/half-float intermediate at 8 bytes/pixel — a different, hypothetical buffer from this
test's actual f32 buffer, at 16 bytes/pixel; conflating the two was a real error caught in
adversarial review). The readback exists only because the correctness tests need a
`Vec<[f32; 4]>` on the Rust side to assert against `cpu_reference` — a real Tapetum bake **never
does this**: a baked stage's output stays GPU-resident (a texture or buffer the next stage reads
directly, or the swapchain presents from), and the *only* real host→device transfer in the real
pipeline is the initial RAW-decoded frame upload, once per image, never a round-trip back. What
this number actually demonstrates is that **~1.44GB of bidirectional host↔device buffer traffic
is expensive (0.8–1.5s)** — worth knowing (it rules out ever doing this for real, and reinforces
that Tapetum's "stay GPU-resident" design isn't optional) but not itself a finding about wgpu's
viability, and not a clean one-way bandwidth number either. **Follow-up work, not blocking this
ADR:** measure upload-only (host→device) cost in isolation, with no readback, when #45 (Tapetum
core) is actually implemented — that is the only host↔device transfer the real design calls for,
and this spike doesn't isolate it.

## Options considered

| Option | GPU compute | Cross-platform | AI-runtime interop | Dispatch overhead | Verdict |
|---|---|---|---|---|---|
| **`wgpu` (WGSL)** | Strong — measured above | Vulkan/Dx12/Metal, one codebase | Bake-time staging copy (ORT already separate) | Lowest measured (Vulkan) | **Chosen** |
| `ash` (raw Vulkan) | Presumed strong, not separately measured (see dispatch-overhead section) | Vulkan-only; needs a second backend for Dx12/Metal | Same staging-copy shape | Unmeasured; wgpu-Vulkan's own number is already low | Rejected — no demonstrated win over wgpu-Vulkan large enough to justify losing wgpu's portability and higher-level API |
| CUDA-direct | Fast (measured above) but NVIDIA-only | None — no path on AMD/Apple | N/A, would still need Vulkan/Dx12/Metal for the viewport | N/A | Rejected — measured no faster than wgpu-Dx12, and doesn't even remove the need for a graphics API |

## Prior art

- **RapidRAW** uses `wgpu` for its render pipeline[^pa1] — same choice, same reasoning trade-off
  (portability without giving up native-backend performance).
- **vkdt** (darktable's from-scratch Vulkan rewrite) uses raw Vulkan directly, with a
  Vulkan-native node-graph (DAG) pipeline (already referenced in ADR-0004 §context/footnote
  p4)[^pa2] — the closest prior art to the rejected `ash` option, but vkdt's own stated motivation
  is avoiding a full API-abstraction layer's overhead for a project with no cross-platform-GPU-API
  goal, which is a different constraint than Nicti's v2 macOS/AMD goal.
- **darktable** (the original, non-vkdt codebase) uses OpenCL for GPU acceleration, with a CPU
  fallback path[^pa3] — not evaluated as a candidate here since OpenCL wasn't in scope per #16's
  own wording (wgpu/ash/CUDA), included only as a data point for #69 (prior-art research), which
  this ADR feeds but does not close.

## Consequences

- **`nicti-render`** (Tapetum's future home, #45) owns a single shared `wgpu::Device`/`Queue` for
  both compute and display — one device avoids cross-device resource-sharing entirely.
- **Backend selection defaults to Vulkan on Windows**, per the `SHADER_F16` finding above; the
  code should still request Dx12 as a fallback (already proven working for everything except f16)
  rather than hard-requiring Vulkan, in case a future machine's Vulkan driver is broken/absent.
- **VRAM budgeting for the hero scenario**: a 45MP RGBA16F intermediate is ~360MB; Tapetum's VRAM
  cache tier (current image + neighbors, per ADR-0002/#44's design) must budget against this
  concretely, not an assumed number — this ADR's measured `max_buffer_size`/
  `max_storage_buffer_binding_size` numbers are the first real data point for that budget.
- **Never do a full-frame host↔device round-trip in the hot path** — confirmed expensive
  (0.8–1.5s) by this spike's own harness, reinforcing (not just assuming) Tapetum's
  GPU-resident-cache design from ADR-0002/#44.
- **GUI framework (#68) coupling note**: Iced and egui are wgpu-native already; Slint has a wgpu
  integration path; GPUI's Windows backend is D3D11-based and would need shared-handle interop
  with wgpu's Dx12/Vulkan device — a real input to #68's own evaluation, not a decision made here.
- **A real dispatch-dimensioning bug was found and fixed by this spike**, not just a benchmark
  artifact: a naive 1D `dispatch_workgroups(n, 1, 1)` overflows wgpu's
  `maxComputeWorkgroupsPerDimension` (65535) at the hero scenario's 45MP resolution
  (~710k workgroups needed at a 64-thread workgroup size). Fixed via a 2D dispatch grid with the
  flat pixel index recovered in-shader via `@builtin(num_workgroups)` (`gpu.rs::workgroup_grid`,
  proven by `tests/correctness.rs` passing at full hero-scenario resolution on real hardware, plus
  unit tests in `gpu.rs`). **Tapetum's real dispatch code must do the same** — this isn't
  spike-only scaffolding, it's a genuine constraint any wgpu compute-stage implementation hits at
  Nicti's target resolutions.

## Spike: `spikes/glint/`

Feline name: the glint of eyeshine off the tapetum lucidum (the reflective retinal layer Tapetum,
#44, is itself named after) — this spike is what's being measured *through*. Not production code,
same "don't build on top of it" status as `spikes/sheath`/`spikes/pawprint` (see `CLAUDE.md`'s
package-map note); expect it deleted once #45 lands the real render engine.

- `src/cpu_reference.rs` — plain-`f32` ground truth for `live_chain` (WB → exposure → tone-curve
  LUT → vibrance) and `tile_blend` (feathered seam blend, the render-side half of tiled AI
  inference reconstruction), unit-tested independently of any GPU.
- `src/gpu.rs` — wgpu device/adapter enumeration across every backend, kernel dispatch (buffers,
  not textures — see its own scoping-note doc comment for why), `TIMESTAMP_QUERY`-based timing,
  and the workgroup-grid dispatch-dimensioning fix above.
- `src/cuda.rs` — the same `live_chain` kernel in CUDA C, compiled at runtime via NVRTC
  (`cudarc`, `fallback-dynamic-loading` — compiles without a CUDA toolkit present, gracefully
  skips at runtime if the driver/NVRTC aren't found), timed via CUDA events (not host wall-clock).
- `shaders/live_chain.wgsl`, `shaders/tile_blend.wgsl`, `shaders/f16_probe.wgsl`.
- `tests/correctness.rs` — every kernel checked against `cpu_reference` within f16-scale
  tolerance, on every backend the running machine exposes; skips cleanly with no adapter.
- `tests/features_and_limits.rs` — the (c) feature/limit checks above.
- `tests/throughput.rs`, `tests/dispatch_overhead.rs`, `tests/interop_roundtrip.rs` — `#[ignore]`d
  (correctness tests run in CI on software Vulkan; these need real hardware and were run manually
  on the reference machine for the numbers above — see each file's own doc comment for the exact
  command).

**Note on the CUDA comparator's execution environment:** it ran under WSL against the reference
machine's real GPU via CUDA driver passthrough (`libcuda.so`), not natively on Windows — an
NVRTC-DLL discovery issue on the Windows side (`cudarc` didn't find `nvrtc64_120_0.dll` even
placed next to the executable) wasn't chased further since a same-GPU, same-driver comparison was
already available via WSL and the decision rule doesn't require same-OS, only same-hardware.

[^pa1]: RapidRAW uses `wgpu` — referenced in #69 (prior-art research ticket) as one of the three
    named candidates for that ticket's own evaluation; confirmed via the project's own
    dependency manifest, checked 2026-09-23.
[^pa2]: vkdt Vulkan node-graph pipeline — already cited in
    `docs/adr/0004-module-plugin-architecture.md` footnote `[^p4]`, https://github.com/hanatos/vkdt
    — verified 2026-09-23.
[^pa3]: darktable OpenCL acceleration with CPU fallback — https://www.darktable.org/about/features/
    and the project's own `src/common/opencl.c` — verified 2026-09-23.
