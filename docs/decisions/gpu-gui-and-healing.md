## GPU, GUI, and healing

Covers the GPU compute API choice, the GUI framework decision, and the healing/removal design.

- **GPU compute API**: `docs/adr/0016-gpu-compute-api.md` — `wgpu` (WGSL), Vulkan backend on
  Windows (not Dx12 — Dx12 doesn't expose `SHADER_F16` on wgpu 30/current driver, Vulkan does, and
  Tapetum's cache tiers need f16). Measured on the reference RTX 5080 in `spikes/glint/`: live-stage
  chain at 4K and 45MP both clear the decision rule (within 2x of an equivalent CUDA kernel, one
  wgpu backend actually faster); `max_buffer_size`/`max_storage_buffer_binding_size` clear the 45MP
  RGBA16F (~360MB) hero-frame size by 5–10x on real hardware. Found and fixed a real
  dispatch-dimensioning bug along the way: a naive 1D dispatch overflows wgpu's 65535-per-dimension
  workgroup limit at hero-scenario resolution — any future wgpu compute-stage code must dispatch as
  a 2D grid (`gpu.rs::workgroup_grid`), not assume 1D is safe. Never do a full-frame host↔device
  round-trip in the hot path (confirmed expensive, 0.8–1.5s, by the spike's own harness) — baked
  stage output must stay GPU-resident, per ADR-0021/#44. Unblocks #20, #41, #45; feeds #68 (GUI
  framework)'s wgpu-interop question.
- **GUI framework**: `docs/adr/0068-gui-framework.md` — **Accepted: egui** (2026-09-27, #90), on
  the hard-gate findings alone; the planned reference-machine measurement pass was waived rather
  than run (see the ADR's Amendments section for the reasoning). **Correction, 2026-09-26** (#69's
  prior-art research): the Prior-art section had wrongly claimed RapidRAW uses egui/eframe — it's
  actually Tauri + React (see the ADR's own Amendments section). Doesn't change this ADR's
  Decision, which rests on egui's own wgpu-30 match, license, and `CallbackTrait` maturity,
  independent of that citation. GPUI is eliminated outright: its Windows
  backend is a bespoke Direct3D11 renderer (`windows-rs`), with no `wgpu`/Vulkan path in its own
  dependency graph on that platform at all (`blade-graphics` is Linux/macOS-only) — no
  `spikes/pelt-gpui` was built. Of the other three, egui (via eframe) wins: the only
  candidate whose own `wgpu` dependency (30.0.0) matches ADR-0016's choice exactly, with a clean
  MIT/Apache-2.0 license and a mature `egui_wgpu::CallbackTrait` custom-viewport story. Iced works
  but pins `wgpu` 27, not 30 (a real version-compatibility cost). Slint's GPU-resident
  `Image::try_from(wgpu::Texture)` integration is the cleanest of the three mechanically, but its
  own license (`GPL-3.0-only OR LicenseRef-Slint-*`) only passes today under a spike-scoped
  `deny.toml` exception — shipping it needs its own ADR-0018 amendment. Neither Iced nor Slint has
  egui's/GPUI's built-in virtualized-list primitive, so both had to hand-roll grid-windowing math
  (`spikes/pelt/src/virtualize.rs`) for #68's grid gate. `spikes/pelt*`/`bench/pelt/` and their CI
  jobs are now dead weight, tracked for deletion in #232; real performance validation against
  this ADR's measured-gate targets moves to #233, against the actual UI crate via #43's
  hero-scenario tooling, once it exists.
- **Production UI crate, `crates/nicti-pelt` (#241, landed)**: the app shell ADR-0068 points to.
  Named `nicti-pelt`, not the ticket's own `nicti-ui` filing name, to match the feline naming
  convention every other crate follows — reusing "pelt" from the now-superseded research spikes
  above. Device sharing: `nicti-pelt::run` passes eframe's `WgpuSetup::CreateNew` a
  `device_descriptor` closure set to the newly-factored-out
  `nicti_tapetum::gpu::device_descriptor_for` (the same adapter-limits/optional-features
  descriptor `GpuContext::new` itself requests, rather than eframe's own conservative
  `wgpu::Limits::default()`), then `PeltApp::new` wraps `cc.wgpu_render_state`'s resulting
  adapter/device/queue with a new `GpuContext::from_device` constructor — one real device shared
  between egui's render pass and every Tapetum compute dispatch, exactly this section's own "one
  shared device" rule. Displaying a `FrameTexture` (linear ProPhoto RGB, `Rgba16Float`) into an
  8-bit surface needed its own small fragment shader (`nicti-pelt/shaders/display.wgsl`,
  `viewport.rs`'s `ViewportCallback`), applying the same
  `color::prophoto_to_srgb_linear_matrix()`/sRGB-OETF pair `geometry::output_encode`'s CPU
  reference already uses — skipping the OETF when the render target itself is an `*Srgb` format,
  since the hardware already applies it on write then (applying it twice would double-gamma the
  image).
- **Healing/removal**: `docs/adr/0050-healing-and-removal.md` — **Accepted** (2026-09-26, #97's
  reference-machine pass). Ships both classic clone/heal (CPU Poisson-Jacobi solve + a `wgpu`
  compute-shader twin, proven correct against each other in `spikes/groom/`) and AI removal
  (MobileSAM+LaMa via `ort`/`load-dynamic`, per ADR-0019 §3's already-decided pattern) as two
  `SpotKind` variants of one `HealStage`, not competing alternatives. Its model loading must
  follow `docs/adr/0218-local-only-ai.md`: offline inference/training by default, no telemetry, no
  hosted API; weight downloads are explicit, user-initiated, and checksum-verified, never a silent
  auto-fetch. No real ONNX weights exist
  yet — the AI-removal wrappers prove the loading/error-handling shape only, latency/quality still
  TBD pending #51's real checkpoints. Re-verified LaMa's Places2 training-data flag (still
  unresolved — the primary source stays unreachable, a mirror confirms Places2's own
  non-commercial/no-redistribution terms) and researched MI-GAN as an alternative, which turned
  out **not** to be cleaner (same Places2 exposure, plus its own unresolved
  weights-license-legitimacy question) — see `docs/research/groom-healing-removal.md`. Measured
  CPU timings (clone_stamp 0.12ms/op, spot_heal 0.25ms/op, auto_source_pick 0.05ms/op) and
  `HealStage` serialized sizes (120/1,511/7,531 bytes at 1/10/50 spots). **#97 measured the GPU
  Poisson-solve on the real RTX 5080/Windows box: 0.386ms p50 (Vulkan) against the <16ms/update
  target** — ~40x headroom, clearing the last open speed question this ADR had (AI-removal
  latency stays gated on #51's weights, not on hardware access). Proposes (not commits)
  heal/remove's stage-order placement for #44: after lens correction, before global tone, in
  linear space.
- **Healing/removal, the build (#51)**: `docs/adr/0051-healing-removal-build.md` — **Accepted**
  (2026-09-29). `spikes/groom` is deleted and promoted: classic clone/heal became the real baked
  `nicti.heal` stage in `nicti-tapetum::heal` (GPU passes on `Rgba16Float` textures, GPU-vs-CPU
  parity-tested; **1 heal spot ≈ 0.9-3 ms and 10 ≈ 8-14 ms end to end on the RTX 5080**, inside the
  16 ms budget), and AI removal became `nicti-groom` (MobileSAM + LaMa against their **real**
  tensor contracts, read from the ONNX files) producing a `RemovalPatch` the GPU blends in. A
  finished patch reaches the render graph by being *stamped into the heal params the render sees*
  (`heal::stamp_removal_state`), so it invalidates through the normal cache-key path. Models are
  fetched only by an explicit click through `nicti-stalk::models` (pinned URL + size + SHA-256,
  atomic, bounded, cancellable, per ADR-0218). **LaMa's Places2 flag was resolved by owner
  sign-off for on-demand download only, never bundled — an accepted risk, not a legal conclusion.**
  ONNX Runtime ships as the official CPU build (DirectML wasn't a single pinnable asset), so AI
  removal measures ~3-4 s and **misses ADR-0050's <2 s target** (which assumed CUDA); quality was
  verified on synthetic scenes only, never real photos. Deferred: GPU execution provider,
  real-photo evaluation, undo/persistence (Develop-wide), LRC `RetouchInfo` import (#62).
