---
paths:
  - "spikes/glint/**"
  - "spikes/groom/**"
  - "crates/nicti-tapetum/**"
  - "crates/nicti-pelt/**"
---

# GPU, GUI, and Healing — Quick Reference

Full reasoning/history: `docs/decisions/gpu-gui-and-healing.md`.

- **GPU compute API** — `docs/adr/0016`: `wgpu` (WGSL), Vulkan backend on Windows (not Dx12 — no
  `SHADER_F16` there, Tapetum's cache tiers need f16). Measured on RTX 5080: within 2x of CUDA at
  4K/45MP. **Always dispatch as a 2D grid** (`gpu.rs::workgroup_grid`) — naive 1D overflows wgpu's
  65535-per-dimension workgroup limit. **Never a full-frame host↔device round-trip in the hot
  path** (0.8–1.5s, confirmed expensive) — baked stage output stays GPU-resident.
- **Production `GpuContext` landed in #45** (`crates/nicti-tapetum::gpu`), adapted from
  `spikes/glint`'s: one shared `wgpu::Device`/`Queue` per ADR-0016, requesting `adapter.limits()`
  (not the 256MB default) and `TIMESTAMP_QUERY`/`SHADER_F16` when the adapter supports them. Frame
  storage is `Rgba16Float` **textures** (`crates/nicti-tapetum::frame::FrameTexture`), not the
  `array<vec4<f32>>` storage **buffers** `spikes/glint`/`spikes/loaf` use — a texture format needs
  no `SHADER_F16` feature at all, so it works identically on Vulkan, Dx12, WARP and lavapipe.
- **GUI framework** — `docs/adr/0068`: **Accepted — egui** (2026-09-27, #90), on hard-gate evidence
  alone; the reference-machine benchmark pass was waived, see the ADR's Amendments. **Correction
  (2026-09-26)**: Prior-art section wrongly claimed RapidRAW uses egui/eframe — it's Tauri+React;
  doesn't change the Decision. GPUI eliminated (Windows backend has no wgpu/Vulkan path). egui wins
  (wgpu 30.0.0 match, MIT/Apache-2.0). Iced pins wgpu 27 (compat cost). Slint's GPU integration is
  cleanest but its license (`GPL-3.0-only OR LicenseRef-Slint-*`) needs its own ADR-0018 amendment
  to ship. `spikes/pelt*`/`bench/pelt/` deleted in #232; real perf validation against the actual UI
  crate tracked in #233.
- **Production UI crate: `crates/nicti-pelt`** (#241, landed) — device sharing implemented as
  `nicti_tapetum::gpu::device_descriptor_for` passed to eframe's `WgpuSetup::CreateNew` (so
  eframe's own device gets Tapetum's own limits/features, not `wgpu::Limits::default()`'s
  conservative 8192px/no-optional-features default), then `GpuContext::from_device(adapter,
  device, queue)` wraps the resulting `cc.wgpu_render_state`'s device — one real device shared
  between egui's render pass and every Tapetum compute dispatch, exactly ADR-0016's "one shared
  device" rule. Displaying a `FrameTexture` (linear ProPhoto RGB, `Rgba16Float`) needs its own
  small fragment shader (`nicti-pelt/shaders/display.wgsl`, `viewport.rs`'s `ViewportCallback`) —
  the same `color::prophoto_to_srgb_linear_matrix()`/sRGB-OETF pair
  `geometry::output_encode`'s CPU reference already uses, with the OETF skipped when the render
  target itself is an `*Srgb` format (hardware already applies it on write then — applying it
  twice double-gammas the image).
- **Healing/removal** — `docs/adr/0050`: **Accepted** (2026-09-26, #97's reference-machine pass).
  Ships both classic clone/heal (CPU Poisson-Jacobi + `wgpu` compute-shader twin) and AI removal
  (MobileSAM+LaMa via `ort`/`load-dynamic`) as two `SpotKind` variants of one `HealStage`.
  **GPU Poisson-solve validated on real RTX 5080 hardware: 0.386ms p50 (Vulkan) vs. the <16ms/
  update target** — ~40x headroom; see #97's WSL-has-no-NVIDIA-Vulkan-ICD gotcha below. **AI
  removal latency/quality still TBD** — real ONNX weights are #51's scope, not obtained here; the
  wrappers prove only the loading/error-handling shape. LaMa's Places2 training-data license
  status is still unresolved (unreachable primary source); MI-GAN investigated as an alternative,
  not cleaner (same exposure). Proposed stage order for #44: after lens correction, before global
  tone, in linear space. **Model loading must follow ADR-0218**: offline inference by default, no
  telemetry, no hosted API; weight fetch only as an explicit user-initiated + checksummed
  download, never a silent auto-fetch.
  - **Gotcha (#97)**: this WSL sandbox has no NVIDIA Vulkan ICD registered at all — `wgpu` here
    only reaches the software `llvmpipe` adapter (88ms p50, not real hardware), even though
    `libcuda.so`/D3D12 interop libs under `/usr/lib/wsl/lib/` give real CUDA/D3D12 access.
    `mesa-vulkan-drivers` on this distro also has no `dzn` (D3D12-translation) ICD. Fix: cross-
    compile for `x86_64-pc-windows-gnu` (`rustup target add`, needs `x86_64-w64-mingw32-gcc` for
    the linker — **and must use the rustup-managed `cargo`/`rustc`, not a Homebrew-installed one
    that shadows it on `PATH` first and has no Windows target installed**) and run the real `.exe`
    directly on the reference machine's Windows side via WSL interop
    (`powershell.exe -Command "& '<unc-path-from-wslpath--w>' --ignored --nocapture"`) — same
    pattern `spikes/retina`/`spikes/sniff` already used for their own real-hardware passes.

## Package contents

- **`spikes/glint`** (#16/ADR-0016) — wgpu-vs-CUDA measured comparison: correctness, feature/limit
  availability, throughput, dispatch overhead, host↔device interop cost.
- **`spikes/pelt` + `spikes/pelt-egui`/`pelt-iced`/`pelt-slint`** (#68/ADR-0068, deleted in #232) —
  GUI-framework research. `pelt` was the toolkit-agnostic shared fixture/math crate; each `pelt-*`
  was one candidate's virtualized-grid + loupe + custom-wgpu-viewport spike. No `spikes/pelt-gpui`
  ever existed (ADR-0068's Hard-gate-1 early exit).
- **`crates/nicti-pelt`** (#241, landed) — the production app shell: `lib.rs` (`run`, the
  `WgpuSetup::CreateNew` device-descriptor wiring), `app.rs` (`PeltApp`, view routing), `render.rs`
  (`DevelopView` — wires a synthetic gradient `LinearFrame` through the real Tapetum pipeline;
  a real NEF is #31's job), `viewport.rs` (`ViewportResources`/`ViewportCallback`, the
  `egui_wgpu::CallbackTrait` display pass) and `catalog.rs` (opens a `nicti-lair` `SqliteCatalog`).
  Supersedes `spikes/pelt-egui` as the real, non-throwaway crate ADR-0068 points to.
- **`spikes/groom`** (#50/ADR-0050) — healing/removal research: CPU clone-stamp/Poisson-heal +
  auto-source-pick reference, a `wgpu` compute-shader Poisson twin proven correct against it,
  `ort`/`load-dynamic` MobileSAM+LaMa wrapper scaffolding (no real ONNX weights in this sandbox,
  and any real weight fetch must follow ADR-0218's user-initiated + checksummed download rule),
  crop/resize/feather compositing, and the `HealStage`/`Spot` edit-model representation with a
  pawprint-style `cache_key()`. See `docs/research/groom-healing-removal.md` for the LaMa/MI-GAN
  licensing findings.
