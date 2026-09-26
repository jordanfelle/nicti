## GPU, GUI, and healing

Covers the GPU compute API choice, the (proposed) GUI framework decision, and the (proposed) healing/removal design.

- **GPU compute API**: `docs/adr/0005-gpu-compute-api.md` — `wgpu` (WGSL), Vulkan backend on
  Windows (not Dx12 — Dx12 doesn't expose `SHADER_F16` on wgpu 30/current driver, Vulkan does, and
  Tapetum's cache tiers need f16). Measured on the reference RTX 5080 in `spikes/glint/`: live-stage
  chain at 4K and 45MP both clear the decision rule (within 2x of an equivalent CUDA kernel, one
  wgpu backend actually faster); `max_buffer_size`/`max_storage_buffer_binding_size` clear the 45MP
  RGBA16F (~360MB) hero-frame size by 5–10x on real hardware. Found and fixed a real
  dispatch-dimensioning bug along the way: a naive 1D dispatch overflows wgpu's 65535-per-dimension
  workgroup limit at hero-scenario resolution — any future wgpu compute-stage code must dispatch as
  a 2D grid (`gpu.rs::workgroup_grid`), not assume 1D is safe. Never do a full-frame host↔device
  round-trip in the hot path (confirmed expensive, 0.8–1.5s, by the spike's own harness) — baked
  stage output must stay GPU-resident, per ADR-0002/#44. Unblocks #20, #41, #45; feeds #68 (GUI
  framework)'s wgpu-interop question.
- **GUI framework**: `docs/adr/0006-gui-framework.md` — **Proposed, pending a reference-machine
  measurement pass**; the hard-gate findings are final. **Correction, 2026-09-26** (#69's prior-art
  research): the Prior-art section had wrongly claimed RapidRAW uses egui/eframe — it's actually
  Tauri + React (see the ADR's own Amendments section). Doesn't change this ADR's Decision, which
  rests on egui's own wgpu-30 match, license, and `CallbackTrait` maturity, independent of that
  citation. GPUI is eliminated outright: its Windows
  backend is a bespoke Direct3D11 renderer (`windows-rs`), with no `wgpu`/Vulkan path in its own
  dependency graph on that platform at all (`blade-graphics` is Linux/macOS-only) — no
  `spikes/pelt-gpui` was built. Of the other three, egui (via eframe) currently leads: the only
  candidate whose own `wgpu` dependency (30.0.0) matches ADR-0005's choice exactly, with a clean
  MIT/Apache-2.0 license and a mature `egui_wgpu::CallbackTrait` custom-viewport story. Iced works
  but pins `wgpu` 27, not 30 (a real version-compatibility cost). Slint's GPU-resident
  `Image::try_from(wgpu::Texture)` integration is the cleanest of the three mechanically, but its
  own license (`GPL-3.0-only OR LicenseRef-Slint-*`) only passes today under a spike-scoped
  `deny.toml` exception — shipping it needs its own ADR-0003 amendment. Neither Iced nor Slint has
  egui's/GPUI's built-in virtualized-list primitive, so both had to hand-roll grid-windowing math
  (`spikes/pelt/src/virtualize.rs`) for #68's grid gate. Final selection waits on
  `bench/pelt/pelt.ahk`+`run-pelt.ps1` numbers from the reference machine.
- **Healing/removal**: `docs/adr/0007-healing-and-removal.md` — **Proposed, pending a
  reference-machine measurement pass**. Ships both classic clone/heal (CPU Poisson-Jacobi solve +
  a `wgpu` compute-shader twin, proven correct against each other in `spikes/groom/`) and AI
  removal (MobileSAM+LaMa via `ort`/`load-dynamic`, per ADR-0004 §3's already-decided pattern) as
  two `SpotKind` variants of one `HealStage`, not competing alternatives. No real ONNX weights
  exist in this sandbox — the AI-removal wrappers prove the loading/error-handling shape only.
  Re-verified LaMa's Places2 training-data flag (still unresolved — the primary source stays
  unreachable, a mirror confirms Places2's own non-commercial/no-redistribution terms) and
  researched MI-GAN as an alternative, which turned out **not** to be cleaner (same Places2
  exposure, plus its own unresolved weights-license-legitimacy question) — see
  `docs/research/groom-healing-removal.md`. Measured CPU-only timings (clone_stamp 0.12ms/op,
  spot_heal 0.25ms/op, auto_source_pick 0.05ms/op) and `HealStage` serialized sizes (120/1,511/
  7,531 bytes at 1/10/50 spots); GPU/CUDA numbers deferred to the reference machine. Proposes
  (not commits) heal/remove's stage-order placement for #44: after lens correction, before global
  tone, in linear space.
