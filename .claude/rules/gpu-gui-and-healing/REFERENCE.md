---
paths:
  - "spikes/glint/**"
  - "spikes/pelt/**"
  - "spikes/pelt-egui/**"
  - "spikes/pelt-iced/**"
  - "spikes/pelt-slint/**"
  - "spikes/groom/**"
  - "bench/pelt/**"
  - "crates/nicti-render/**"
---

# GPU, GUI, and Healing — Quick Reference

Full reasoning/history: `docs/decisions/gpu-gui-and-healing.md`.

- **GPU compute API** — `docs/adr/0005`: `wgpu` (WGSL), Vulkan backend on Windows (not Dx12 — no
  `SHADER_F16` there, Tapetum's cache tiers need f16). Measured on RTX 5080: within 2x of CUDA at
  4K/45MP. **Always dispatch as a 2D grid** (`gpu.rs::workgroup_grid`) — naive 1D overflows wgpu's
  65535-per-dimension workgroup limit. **Never a full-frame host↔device round-trip in the hot
  path** (0.8–1.5s, confirmed expensive) — baked stage output stays GPU-resident.
- **GUI framework** — `docs/adr/0006`: **Proposed, pending reference-machine pass**; hard-gate
  findings final. **Correction (2026-09-26)**: Prior-art section wrongly claimed RapidRAW uses
  egui/eframe — it's Tauri+React; doesn't change the Decision. GPUI eliminated (Windows backend has
  no wgpu/Vulkan path). egui currently leads
  (wgpu 30.0.0 match, MIT/Apache-2.0). Iced pins wgpu 27 (compat cost). Slint's GPU integration is
  cleanest but its license (`GPL-3.0-only OR LicenseRef-Slint-*`) needs its own ADR-0003 amendment
  to ship. Final pick waits on `bench/pelt/pelt.ahk`+`run-pelt.ps1` reference-machine numbers.
- **Healing/removal** — `docs/adr/0007`: **Proposed, pending reference-machine pass**. Ships both
  classic clone/heal (CPU Poisson-Jacobi + `wgpu` compute-shader twin) and AI removal
  (MobileSAM+LaMa via `ort`/`load-dynamic`) as two `SpotKind` variants of one `HealStage`. **No
  real ONNX weights exist in this sandbox** — the AI-removal wrappers prove only the
  loading/error-handling shape, not real inference. LaMa's Places2 training-data license status is
  still unresolved (unreachable primary source); MI-GAN investigated as an alternative, not
  cleaner (same exposure). Proposed stage order for #44: after lens correction, before global
  tone, in linear space.
