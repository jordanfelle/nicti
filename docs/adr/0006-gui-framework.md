# ADR-0006: GUI framework

- **Status:** Proposed — hard-gate findings below are final (static, code/dependency-graph
  evidence, not hardware-dependent); the measured-gate tables and final Decision are pending a
  baseline run on the reference machine (see Measured results)
- **Date:** 2026-09-23
- **Ticket:** [#68](https://github.com/jordanfelle/nicti/issues/68) Research: GUI framework (GPUI vs Iced vs egui vs Slint)

## Context

Tapetum (#44, the stage-cached render graph) and the rest of the UI need a GUI toolkit chosen
before render-engine work starts, since the toolkit determines how Tapetum exposes baked stage
output to the screen. #68's shortlist: **GPUI** (Zed's toolkit), **Iced** (Elm-style, wgpu-native),
**egui** (immediate-mode, wgpu-native via eframe), and **Slint** (declarative DSL + codegen),
merged in from an archived duplicate ticket (#13).

Constraints already fixed by earlier ADRs/docs:

- **ADR-0001** named all four as the viable Rust GUI candidates and already flagged GPUI's
  cross-platform maturity, egui's virtualization story (`ScrollArea::show_rows`/
  `egui_virtual_list`), and iced's async-offload fit as open questions for this ticket to resolve.
- **ADR-0005** decided `wgpu` 30 (Vulkan backend on Windows, `SHADER_F16` required for Tapetum's
  compressed-half-float cache tiers) as the render/compute API, with **one shared `wgpu::Device`**
  intended for both compute and display. Its Consequences section already flagged, without
  measuring it, that "GPUI's Windows backend is D3D11-based and would need shared-handle interop
  with wgpu's Dx12/Vulkan device" — this ADR is where that flag gets checked against real evidence
  (see Hard gate 1 below; the D3D11 claim holds, though the *reason* GPUI can't share a device
  turns out to be broader than just that).
- **ADR-0003**: license policy. #68 was explicitly carved out of that audit's scope, flagged as
  a live concern specifically for Slint's GPL-3.0/dual-commercial terms. This ADR's own
  license-gate finding (below) resolves that flag for the research-spike stage; shipping any
  candidate in a production crate is a separate sign-off (see `docs/licensing.md`'s #68 update).
- v1 targets Windows only; macOS/Linux are v2 goals (#4/#66), so cross-platform story is a
  tiebreaker, not a gate.
- Performance budgets (`docs/benchmarks.md`): slider→preview p95 ≤ 16.7ms (60fps); loupe next/prev
  < 50ms warm; the hero scenario (#43) demands this hold regardless of edit-stack complexity.
- **Sandbox note:** this research pass ran in a Linux/WSL sandbox with no GPU-backed Vulkan and no
  Windows, and could not drive AutoHotkey or capture a real screen. Every dependency-graph, API,
  and license finding below is real, current-crate evidence (crates.io downloads, `cargo add
  --dry-run`, `cargo deny`, and each crate's own published `Cargo.toml`/source, all checked
  2026-09-23) — but the Measured-gates tables are placeholders, exactly like
  `docs/benchmarks/hero-scenario.md`'s own "filled in after the baseline runs" precedent. **The
  final Decision below is provisional** on those measurements.

## Decision rule (stated before measuring, per ADR-0005's own methodology)

### Hard gates (static evidence — resolved in this pass)

1. **Custom GPU viewport on the shared `wgpu::Device`.** The candidate must let Nicti render a
   `wgpu` compute/render pass (glint's `live_chain` kernel, reused unchanged from ADR-0005) into
   the same scene, using the *same* `wgpu::Device`/`Queue` the candidate itself renders with — not
   a second, independent GPU context requiring cross-API interop.
2. **wgpu version compatibility.** The toolkit's own `wgpu` dependency must be able to coexist
   with ADR-0005's wgpu 30 / Vulkan / `SHADER_F16` choice, in the same binary.
3. **Native Windows 11 support**, not an experimental/community backend.
4. **License** passes ADR-0003's Rust-crate allowlist, or is explicitly flagged for sign-off.

### Measured gates (pending reference-machine run)

p95 on the reference machine (RTX 5080, Windows, ADR-0005's own reference hardware), against
`docs/benchmarks.md`'s targets:

| Interaction | Metric | Target |
|---|---|---|
| Grid scroll at 2M cells | frame interval | ≤ 16.7ms |
| Loupe next/prev, images prefetched | settled latency | < 50ms |
| Slider drag driving `live_chain` in the viewport | frame interval | ≤ 16.7ms |
| Viewport pan (hero-scenario crop/zoom proxy) | frame interval | ≤ 16.7ms |

### Tiebreakers (among candidates passing all gates)

Cross-platform story, API stability/churn, docs and agent-productivity, text/IME/accessibility,
compile time.

### Early exit

A candidate that fails a hard gate does not get a measured-gates pass or a full spike built out —
per this rule, GPUI's spike was not built (see Decision, Hard gate 1).

## Decision

**Provisional, pending the measured gates: egui (via eframe) is the leading candidate — it is the
only one of the four that clears every hard gate with no caveat.** Final selection between egui,
Iced, and Slint waits on the reference-machine numbers. GPUI is eliminated by Hard gate 1, with
concrete evidence, not a measurement — see below.

### Hard gate 1 — GPUI: **fails**, confirmed with more precision than ADR-0005's own flag

`gpui` v0.2.2's own published `Cargo.toml`[^g1] shows its GPU backend is **platform-conditional**,
not `wgpu` on any platform:

- `macos-blade`, `wayland`, and `x11` features all pull in `blade-graphics` (Zed's own GPU
  abstraction crate, built on raw `ash`/Vulkan via `gpu-alloc`[^g2]) — but only for macOS, Wayland,
  and X11.
- **Windows is conspicuously absent from that list.** Its dependency tree instead pulls
  `Win32_Graphics_Direct3D`, `Win32_Graphics_Direct3D11`, `Win32_Graphics_Direct3D_Fxc`, and
  `Win32_Graphics_DirectComposition` via `windows-rs`[^g1] — a bespoke Direct3D 11 renderer, wired
  directly to Win32 APIs, entirely independent of both `blade-graphics` and `wgpu`.

This confirms ADR-0005's flag precisely: GPUI's Windows backend is D3D11, and there is no `wgpu`
(or even Vulkan) code path on Windows at all in GPUI's own dependency graph — not a version
mismatch or an interop inconvenience, but no shared-device mechanism to reach for. GPUI exposes no
public API (as of 0.2.2) for importing an externally-created texture or device into its scene the
way Slint's `Image::try_from(wgpu::Texture)` does. Building a working interop path would mean
either patching GPUI itself to accept a foreign D3D11 device/texture (maintaining a fork) or a
D3D11↔Vulkan/Dx12 shared-handle bridge with no supported entry point in GPUI to hang it from.
**Per this ADR's Early-exit rule, no `pelt-gpui` spike was built** — the grid/loupe halves of a
spike wouldn't exercise the actual disqualifying gate, and building them would spend budget
disproving nothing.

### Hard gate 1 — Iced, egui, Slint: **all pass**, by three different mechanisms

- **Iced** (`iced::widget::shader::Primitive`/`Pipeline`, confirmed against `iced_wgpu` 0.14.0's
  own source[^i1]): a custom `Primitive::prepare`/`draw` pair runs against the *same*
  `wgpu::Device`/`Queue` iced itself creates and renders with, passed by reference into `prepare`,
  with `draw` issuing commands directly into iced's own `wgpu::RenderPass`. Structural difference
  from the other two: `prepare` is handed no shared command encoder, so a custom primitive that
  needs a compute dispatch (this spike's `live_chain`) must create and submit its own encoder
  before `draw` runs — a real, iced-imposed shape, not a design choice made by this spike (see
  `spikes/pelt-iced/src/viewport.rs`'s doc comment).
- **egui** (`egui_wgpu::CallbackTrait`, confirmed against `egui-wgpu` 0.36.2's own source[^e1]):
  the well-known `custom3d_wgpu` pattern — `prepare` gets eframe's shared device/queue *and* its
  own command encoder (so a compute dispatch can share one submission with egui's main pass),
  `paint` runs inside egui's own render pass.
- **Slint** (`slint::Image::try_from(wgpu::Texture)` + `Window::set_rendering_notifier`, confirmed
  against Slint's own doc-tested example[^s1] and `i-slint-core`'s `wgpu_30.rs`[^s2]): the
  cleanest of the three — an externally-rendered `Rgba8Unorm`/`Rgba8UnormSrgb` texture (with
  `TEXTURE_BINDING | RENDER_ATTACHMENT` usage) is imported into the scene **by value, GPU-resident,
  no CPU readback**, using the exact `wgpu::Device`/`Queue` Slint renders with (obtained via
  `set_rendering_notifier`'s `RenderingSetup` state, or supplied up front via
  `WGPUConfiguration::Manual`). Slint also has a documented `BackendSelector::require_wgpu_30(...)`
  entry point specifically for this integration shape, which neither Iced nor egui expose as a
  named, first-class API — it's assembled from more general-purpose pieces in both.

### Hard gate 2 — wgpu version compatibility: **egui and Slint pass cleanly; Iced does not**

| Candidate | Its own resolved `wgpu` version | vs. ADR-0005's `wgpu` 30 |
|---|---|---|
| egui / eframe (`egui-wgpu` 0.36.2) | **30.0.0**[^e2] | Matches exactly |
| Slint (`i-slint-renderer-femtovg`'s `wgpu-30` feature) | Named `wgpu-30`; exact resolved version not independently pinned down this pass (its own `Cargo.lock` didn't resolve the optional dependency in this sandbox's default-feature check) — but the feature's own name is a strong, direct signal | Consistent with 30, not independently confirmed to the patch version |
| Iced (`iced_wgpu` 0.14.0) | **27.0.1**[^i2] | **Does not match** |

This is a real, load-bearing, evidenced constraint, not a hypothetical: Rust's type system
requires the exact same `wgpu` crate version to unify `wgpu::Device`/`wgpu::RenderPass` types
between a candidate's internals and any code Nicti writes against them (confirmed directly —
`spikes/pelt-iced` had to depend on `wgpu = "27"`, not `"30"`, or it fails to compile against
`iced_wgpu`'s own types). Shipping Iced today means either `nicti-render` targets wgpu 27 instead
of ADR-0005's wgpu 30 (losing whatever wgpu 30 brought — `Rgba32Float`/dispatch behavior wasn't
re-audited against 27 this pass), or waiting on/forcing an iced upgrade. Not disqualifying by
itself, but a real cost egui and Slint don't carry.

### Hard gate 3 — native Windows 11 support: **all three remaining candidates pass**

Iced, egui/eframe, and Slint each have first-party Windows targets via `winit` (iced, egui) or
Slint's own `winit`-backed default backend — none experimental, none community-maintained forks.

### Hard gate 4 — license: **egui and Iced pass outright; Slint passes only under a spike-scoped
exception**

- **egui / eframe / egui-wgpu / egui-winit / epaint / emath / ecolor**: MIT OR Apache-2.0.
  `eframe`'s `default_fonts` feature bundles `epaint_default_fonts`
  (`(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0` for the actual font *data*) — both
  OFL-1.1 and Ubuntu-font-1.0 are open, redistribution-permitting font licenses. Added to
  `deny.toml`'s global allowlist; see `docs/licensing.md`'s #68 update.
- **Iced** (`iced`/`iced_wgpu`/`iced_widget`/`iced_core`/`iced_runtime`/`iced_graphics`/
  `iced_winit`/`iced_tiny_skia`/`iced_program`/`iced_renderer`/`iced_futures`/`iced_debug`): MIT.
- **Slint**: confirmed via `cargo deny` directly (not assumed from the plan) —
  `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0` on every
  Slint-published crate (`slint`, `slint-build`, `slint-macros`, and every `i-slint-*` crate)[^s3].
  None of the three arms are on ADR-0003's allowlist, and the `LicenseRef-*` arms are Slint's own
  non-SPDX-registered dual-commercial license texts. Per your instruction, this was spiked anyway
  under a **spike-scoped `[[licenses.exceptions]]` block per Slint crate name** in `deny.toml` (not
  a global allow), so `cargo deny check licenses` passes for this research pass without opening
  the allowlist to any other GPL-only crate. **This is not a shipping decision**: if Slint wins on
  the measured gates, shipping it in `nicti-render`/a future `nicti-ui` needs its own ADR-0003
  amendment and your explicit sign-off — the same standard already applied to LGPL-as-Cargo-
  dependency crates (rawler/lensfun-rs) in that ADR.

## Measured results

_Pending a baseline run on the reference machine (RTX 5080, Windows, native — not WSL, same
caveat as ADR-0005's own hardware-identity rule). `spikes/pelt-egui`, `spikes/pelt-iced`, and
`spikes/pelt-slint` each build and compile-check clean (`cargo check`/`cargo build`/`cargo test`/
`cargo clippy --workspace --all-targets --all-features -- -D warnings`/`cargo fmt --check` all
pass in this sandbox), but none has been run against a real display or driven by
`bench/pelt/pelt.ahk` — this sandbox has no GPU-backed Vulkan/Dx12 and no Windows. Filled in after
the baseline runs, same as `docs/benchmarks/hero-scenario.md`'s own Results section._

| Candidate | Grid scroll (2M cells) p95 | Loupe next/prev p95 | Slider drag p95 | Viewport pan p95 |
|---|---|---|---|---|
| egui | _pending_ | _pending_ | _pending_ | _pending_ |
| Iced | _pending_ | _pending_ | _pending_ | _pending_ |
| Slint | _pending_ | _pending_ | _pending_ | _pending_ |

## Options considered

| Option | GPU viewport on shared device | wgpu version | License | Verdict |
|---|---|---|---|---|
| **GPUI** | **Fails** — Windows backend is D3D11 via `windows-rs`, no `wgpu`/Vulkan path in its own dependency graph at all, no public texture-import API | N/A | Apache-2.0[^g3] (not the blocker) | **Eliminated** — Hard gate 1, no spike built |
| **Iced** | Passes — `Primitive`/`Pipeline` on iced's own device | **27.0.1**, not 30 | MIT | Passes gates, real wgpu-version cost |
| **egui / eframe** | Passes — `egui_wgpu::CallbackTrait` on eframe's own device | **30.0.0**, matches ADR-0005 exactly | MIT/Apache-2.0 (+ OFL-1.1/Ubuntu-font-1.0 for bundled default fonts) | Passes every hard gate cleanly |
| **Slint** | Passes — cleanest mechanism, GPU-resident `Image::try_from`, first-class `require_wgpu_30` API | Named `wgpu-30`; not independently pinned to a patch version this pass | GPL-3.0/dual-commercial — spike-scoped exception only | Passes gates under a scoped exception; shipping needs ADR-0003 amendment |

## Prior art

- **RapidRAW** uses `egui`/`eframe` for its UI[^pa1] (referenced in #69, prior-art research) —
  the same "immediate-mode + wgpu" combination this ADR's leading candidate uses, from a project
  with a directly comparable scope (a from-scratch RAW editor).
- **vkdt** builds its own from-scratch Vulkan-native UI rather than adopting any of these four
  toolkits[^pa2] — a data point that a bespoke render-first UI is a real, if much higher-effort,
  alternative to any off-the-shelf Rust GUI toolkit, not evaluated as a fifth candidate here since
  #68's own scope named these four.
- **Zed** (GPUI's origin project) is itself the strongest real-world evidence for GPUI's maturity
  and performance at scale — but Zed does not target Windows via the same code path Nicti would
  need, which is exactly what Hard gate 1 found directly in GPUI's own dependency graph rather
  than relying on Zed's reputation as a proxy.

## Consequences

- **If egui wins** (current lead per the hard gates): `nicti-render`'s viewport integration is the
  `egui_wgpu::CallbackTrait` shape already proven in `spikes/pelt-egui/src/viewport.rs` — a
  `prepare`/`paint` split sharing eframe's device, `wgpu` pinned at exactly the version ADR-0005
  already chose, no version-compatibility tax.
- **If Slint wins**: needs an ADR-0003 amendment (a GPL-3.0/dual-commercial dependency is a real
  outbound-license decision, feeding directly into #66) before it ships beyond this research spike
  — not automatic, and not implied by this ADR passing it through the hard gates.
- **If Iced wins**: `nicti-render` either targets wgpu 27 (revisiting ADR-0005's own wgpu-30-
  specific `SHADER_F16`/Vulkan findings against that older version) or blocks on an iced upgrade to
  wgpu 30 — a real, non-trivial dependency to carry forward, not a footnote.
- **GPUI is closed out for Nicti's v1 (Windows) target** on hard evidence, not a hunch — this can
  be revisited only if GPUI ships an official Windows Vulkan/wgpu-interop backend in the future
  (its own crate features show no sign of one as of v0.2.2, 2026-09-23).
- **Grid virtualization**: egui (`ScrollArea::show_rows`) and (per ADR-0001's own note) GPUI
  (`uniform_list`, now moot) both ship a built-in virtualized-list primitive; Iced and Slint do
  not, as confirmed directly by having to hand-roll the identical windowing math
  (`spikes/pelt/src/virtualize.rs`, shared and unit-tested once, driving both `pelt-iced` and
  `pelt-slint`'s own manual visible-range logic) for both of them in this pass. This is a real,
  measured (in code, not in frame-time) ergonomics cost for whichever of those two might still win
  on the pending hardware numbers.

## Spike: `spikes/pelt`, `spikes/pelt-egui`, `spikes/pelt-iced`, `spikes/pelt-slint`

Feline name: pelt, the visible outer coat — what these spikes are all about, the UI surface over
Tapetum's fur underneath. Not production code, same "don't build on top of it" status as
`spikes/glint`/`spikes/sheath`/`spikes/pawprint` (see `CLAUDE.md`'s package-map note); expect all
four deleted once #20/whatever real UI crate this ADR points to lands.

- **`spikes/pelt`** (`src/config.rs`, `thumbnails.rs`, `loupe.rs`, `live_chain.rs`,
  `virtualize.rs`): toolkit-agnostic synthetic fixtures shared by all three candidate crates —
  a 2M-cell grid over 4096 distinct synthetic tiles (hashed cell→tile mapping, realistic texture
  churn without generating 2M unique images), a 50-frame synthetic loupe set matching the
  hero-scenario working-set size, the `live_chain` WGSL kernel and CPU reference (copied from
  `spikes/glint`, not depended on — spikes don't build on spikes), and the shared virtualized-grid
  windowing math (`GridLayout`) both `pelt-iced` and `pelt-slint` drive their hand-rolled grids
  from. Holds no `wgpu` types itself, since the three candidate crates pin different `wgpu`
  versions (Hard gate 2). Every module has real unit tests (20 total, all passing in this
  sandbox).
- **`spikes/pelt-egui`**: `egui_wgpu::CallbackTrait`-based viewport (`src/viewport.rs`), egui's
  built-in `ScrollArea::show_rows` for the grid, `egui::Slider` + a drag-sensed `Ui::allocate_
  exact_size` region for exposure/vibrance/white-balance.
- **`spikes/pelt-iced`**: hand-rolled virtualized grid (`src/main.rs::view_grid`, driven by
  `pelt::virtualize::GridLayout` since Iced has no built-in equivalent), `iced::widget::shader`-
  based viewport (`src/viewport.rs`, `src/program.rs`) with its own encoder-submission shape (see
  Hard gate 1's Iced note).
  **wgpu version note:** this crate depends on `wgpu = "27"`, not `"30"` — see Hard gate 2.
- **`spikes/pelt-slint`**: a `.slint` UI (`ui/app.slint`) with a Rust-driven visible-tile slice
  (same hand-rolled virtualization as `pelt-iced`, via a `changed content-y => { root.scrolled(...)
  }` property-change handler calling back into Rust), and a `ViewportRenderer`
  (`src/viewport.rs`) built against the exact device/queue Slint itself renders with, obtained via
  `Window::set_rendering_notifier`'s `RenderingSetup` state, importing its output straight into the
  scene via `slint::Image::try_from` every `BeforeRendering` frame.
- **No `spikes/pelt-gpui`** — see Hard gate 1's Early-exit note.

**What's left before this ADR's Status can move to Accepted:** a baseline run on the reference
machine for all three remaining candidates' measured gates, using `bench/pelt/pelt.ahk` +
`bench/pelt/run-pelt.ps1` (same screen-capture method as `bench/run-hero.ps1`/`bench/whisker`,
generalized over grid/loupe/slider/pan instead of switch/crop/zoom) — see that directory's own
files for exact usage. `bench/pelt/pelt-config.ini.example` documents the indicator/ROI
calibration keys, same shape as `bench/lrc/hero-config.ini.example`.

[^g1]: `gpui` v0.2.2 `Cargo.toml` — feature list (`macos-blade`/`wayland`/`x11` pull
    `blade-graphics`; no such feature exists for Windows) and
    `[target.'cfg(windows)'.dependencies.windows]` feature list (`Win32_Graphics_Direct3D11`,
    `Win32_Graphics_Direct3D_Fxc`, `Win32_Graphics_DirectComposition`) — downloaded directly from
    `https://crates.io/api/v1/crates/gpui/0.2.2/download` and inspected — verified 2026-09-23
[^g2]: `blade-graphics` v0.9.0 `Cargo.toml` — `ash`/`ash-window`/`gpu-alloc`/`gpu-alloc-ash`
    dependencies under `cfg(any(vulkan, windows, target_os = "linux", ...))` — downloaded directly
    from `https://crates.io/api/v1/crates/blade-graphics/0.9.0/download` — verified 2026-09-23
[^g3]: `gpui` license field, `cargo info gpui` / registry metadata — verified 2026-09-23
[^i1]: `iced_wgpu` v0.14.0 `src/primitive.rs` (`Primitive::prepare`/`draw`, `Pipeline::new`) and
    `iced_widget` v0.14.0 `src/shader/program.rs` (`Program` trait) — downloaded directly from
    crates.io and inspected — verified 2026-09-23
[^i2]: `iced_wgpu` v0.14.0's own published `Cargo.lock` (`wgpu` resolved to 27.0.1) — verified
    2026-09-23; independently reconfirmed by `spikes/pelt-iced` itself only compiling against
    `wgpu = "27"`, not `"30"`, in this repo
[^e1]: `egui-wgpu` v0.36.2 `src/renderer.rs` (`CallbackTrait`, `Callback::new_paint_callback`) —
    downloaded directly from crates.io and inspected — verified 2026-09-23
[^e2]: `egui-wgpu` v0.36.2's own published `Cargo.lock` (`wgpu` resolved to 30.0.0) — verified
    2026-09-23
[^s1]: Slint's own doc-tested `wgpu_30` module example (`slint` v1.18.1 `lib.rs`,
    `BackendSelector::require_wgpu_30`/`set_rendering_notifier`/`Image::try_from`) — this is
    Slint's own maintained, CI-doctested example, not a third-party tutorial — verified 2026-09-23
[^s2]: `i-slint-core` v1.18.1 `graphics/wgpu_30.rs` (`WGPUConfiguration::Manual`/`Automatic`,
    `impl TryFrom<wgpu_30::Texture> for Image`, format/usage requirements) — verified 2026-09-23
[^s3]: Slint crate family license expression, confirmed directly via
    `cargo deny --workspace --all-features check licenses` against this repo's own resolved
    dependency graph, not assumed from a registry listing — verified 2026-09-23
[^pa1]: RapidRAW's `egui`/`eframe` UI — cited in #69 (prior-art research ticket); confirmed via
    the project's own dependency manifest — verified 2026-09-23
[^pa2]: vkdt's Vulkan-native UI (no third-party Rust GUI toolkit involved at all) — already cited
    in `docs/adr/0004-module-plugin-architecture.md` footnote `[^p4]` and
    `docs/adr/0005-gpu-compute-api.md`'s own Prior art section — https://github.com/hanatos/vkdt
    — verified 2026-09-23
