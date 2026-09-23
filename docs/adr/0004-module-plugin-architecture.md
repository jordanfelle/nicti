# ADR-0004: Module/plugin architecture (Claw)

- **Status:** Accepted
- **Date:** 2026-09-23
- **Ticket:** [#19](https://github.com/jordanfelle/nicti/issues/19) Research: module/plugin
  architecture (Claw)

## Context

Nicti's requirements (#4) call for "stable interfaces for RAW decoder backends, camera color
profiles, lens-correction data, render stages, AI model providers, exporters, catalog store" and
"heavy modules (AI models, ONNX/CUDA runtime) load on demand, not at startup" — v1 stays
Nikon-only but the architecture must not hard-code Nikon assumptions, since a wider open-source
release with more camera brands is a long-term goal (#66). #19 gates #20 (core crate boundaries +
extension-point traits + module registry), which in turn gates #22 (catalog), #41 (RAW pipeline),
#45 (Tapetum core), #57 (export), and #36 (AI culling) — this is the highest-leverage unblocked
research ticket in the backlog.

Three prior ADRs already constrain this decision:

- **ADR-0001** named `libloading` (C-ABI dylib) and `wasmtime` (WASM host) as the concrete
  mechanism candidates within Rust, and flagged that `wasmtime`'s plugin story was one of the four
  criteria where Rust rated strictly stronger than C++.
- **ADR-0002** already committed the *data* shape a plugin render stage uses: a namespaced
  `vendor.stage_name` id with opaque `params: serde_json::Value`, `schema_version`-tagged, so a
  build without a given plugin installed round-trips its entry byte-for-byte rather than dropping
  or guessing at it (proven in `spikes/pawprint/tests/unknown_stage_roundtrip.rs`). This ADR
  extends that shape to a *registry* that resolves an id to a real implementation, and to the
  other six extension points beyond render stages.
- **ADR-0003** requires that an LGPL-as-Cargo-dependency case (a future `rawler`/`lensfun-rs`-style
  crate) be isolated behind a `cdylib`/out-of-process boundary "via Claw" if it's ever going to
  ship — Rust's static-link compilation model doesn't cleanly satisfy LGPL's dynamic-linking safe
  harbor otherwise. This ADR is where that promise gets a concrete mechanism.

Exit criterion for #19 is this ADR. Following the ADR-0002/#21 precedent (PR #75), it's
accompanied by a throwaway spike (`spikes/sheath` + fixture `spikes/dewclaw`) proving every
load-bearing claim below with a runnable test, plus a measured (not assumed) comparison of a WASM
guest kernel against native Rust for the specific question of whether WASM is viable for
third-party *render-stage* pixel processing.

## Decision

**A hybrid, one-mechanism-per-job design, not a single plugin system.** Different extension
points have genuinely different requirements (v1 vs. v2, hot-path vs. cold-path, first-party vs.
untrusted third-party), and forcing them through one mechanism would either over-engineer the v1
path or under-serve the v2 one.

### 1. v1 first-party modules: in-process Rust traits

Every v1 module (the Nikon NEF decoder, the render stages, the exporter) is first-party Rust code
implementing a shared trait, compiled directly into the binary. `dyn Trait` has no ABI concerns
across a single compilation, and per-stage (not per-pixel) dispatch overhead is on the order of a
few nanoseconds[^c6] — negligible against the hero-scenario's 16.7ms/frame budget. Optional heavy
dependencies (an alternate decoder backend, a specific AI runtime) are gated behind Cargo
features, so a build that doesn't need them doesn't pay their compile or binary-size cost.

### 2. "Sheathed" lazy loading through a registry

Every module registers a cheap, always-available `Descriptor` (id, version) at startup; building
the actual expensive instance is deferred to a `OnceLock`-backed factory that runs on first use,
at most once even under a concurrent race to be the first caller. This is the literal mechanism
behind the "Claw" name (claws stay sheathed until needed) and the concrete meaning of "heavy
modules load on demand, not at startup." Proven in `spikes/sheath/tests/lazy_registry.rs`
(`factory_does_not_run_before_first_get`, `factory_runs_exactly_once_under_concurrent_first_use`)
and `dylib_on_demand.rs` (the same laziness claim for a dylib-backed module specifically).

### 3. Dynamic native-runtime loading for heavy AI/GPU runtimes

ONNX Runtime is loaded via the `ort` crate's `load-dynamic` feature (`ort::init_from(path)`,
verified as a real, documented feature — the crate's own guide states its rationale as "won't
hard-crash at binary launch if the dylib is absent")[^c5], not linked at build/startup time. This
is the concrete implementation of the PRD's "AI runtime loads on demand" for the ONNX/CUDA case
specifically, distinct from #2's Rust-level registry laziness — it defers the underlying native
runtime library's own load, not just Nicti's own wrapper construction.

### 4. A checked C-ABI `cdylib` boundary, for LGPL isolation

For ADR-0003's LGPL-as-Cargo-dependency case, a stage can instead be compiled as a `cdylib`
exporting a single `#[repr(C)]` vtable, loaded via `libloading` (ISC license[^c1], added to
`deny.toml`'s allowlist in this PR — see ADR-0003's Amendments section). Rust's own struct/vtable
layout is not guaranteed stable across separate compilations — the Rust Reference is explicit that
"type layout can be changed with each compilation... we only document what is guaranteed today,"
and the default representation does not guarantee field order[^c4] — so the vtable's **first
field is an explicit `abi_version: u32`, checked before any other field is trusted**. A mismatch, a
missing export, or a present-but-null export (a symbol that exists but returns a garbage pointer,
distinct from a missing symbol) is rejected with a clean error, never UB or a crash — the null
check specifically was a gap an earlier draft of this ADR's spike had missed (`DylibStage::load`
dereferenced the vtable pointer before validating it was non-null), caught and fixed during this
PR's own adversarial review. Proven end-to-end in `spikes/sheath/src/dylib.rs`
(`DylibStage::load`) and `spikes/sheath/tests/abi_handshake.rs` (matching version loads and calls
correctly; a deliberately-wrong version is rejected as `AbiMismatch`; a missing export is rejected
as `MissingSymbol`; a present-but-null export is rejected as `NullVtable`) against the
`spikes/dewclaw` fixture crate (a toy `cdylib` stage, feature-gated to produce all three bad
variants for testing).

This test proves the **handshake protocol** — a host that checks a declared version number before
trusting anything else — not a literal cross-rustc-version struct-layout break (constructing one
deliberately would require building the fixture with a different compiler than `sheath` itself,
out of scope for a spike; `dewclaw` depends on `sheath` as an ordinary path dependency to share the
`StageVTable` definition, so both sides are in practice built by the same toolchain here).

**Considered and rejected for v1:** `abi_stable`[^c2] and `stabby`[^c3], both of which give
compiler-version-independent safety via their own FFI-safe wrapper types (`abi_stable`'s `RBox`,
`RVec`, etc.) plus load-time layout validation — genuinely more robust than a hand-rolled vtable if
the two sides of the boundary might be built by *different* Rust compiler versions. Nicti doesn't
need that: it controls the toolchain on both sides of its own plugin dylibs (there's no third-party
dylib author yet — see the v2 discussion below), so the added dependency weight and macro-heavy API
surface aren't worth it today. Revisit if #66's open-source release ever produces a real
third-party native-plugin ecosystem building against a different Rust version than Nicti's own CI.

### 5. Registry-level fallback for an unrecognized module id

Looking up a stage id with no installed module returns cleanly (`Option::None`), mirroring
ADR-0002's "a version this build doesn't recognize stays read-only rather than being dropped or
guessed at." Proven in `spikes/sheath/tests/unknown_module.rs`.

### 6. v2 third-party plugins: WASM is the right *direction*, not a v1 commitment

The working hypothesis — WASM (`wasmtime`) is viable for non-hot-path extension points but not for
a third-party render stage's per-pixel loop — was tested directly, not assumed. `spikes/sheath`
runs an identical linear-scale kernel (`buf[i] *= k`, representative of a cheap per-pixel stage
like white balance) both as native Rust and as a `wasmtime`-hosted guest written in inline WAT
(`spikes/sheath/tests/wasm_vs_native.rs`), over a 32MB buffer (scaled down from a real ~360MB
45MP RGBA16F frame to keep CI time/memory bounded — see the test's own doc comment). Measured
locally (release build, three runs, after this PR's own adversarial review caught and fixed a
methodology bug where the output buffer's allocation was counted inside the `copy_out` timing
window on the WASM side with no equivalent cost charged to the native side — the numbers below are
post-fix): the WASM path's **total** time (host→guest copy + guest call + guest→host copy) was
**11–19x slower than the native loop** (native: 1.9–5.1ms; WASM total: 26.0–56.3ms), and that gap
is **almost entirely the buffer copy**, not the compute: the guest call itself took 2.0–6.4ms,
close to native, while `copy_in`+`copy_out` alone accounted for 24.0–49.8ms of the total. This
matches the research finding that no authoritative zero-copy path
exists yet for a buffer this large crossing the WASM linear-memory boundary — the closest
authoritative discussion (a 2026 wasmtime/WASI thread on `wasi:http` resource streaming) concludes
the state of the art is still "essentially copying via guest memory pointers," not a solved
problem[^w4]. Extrapolated linearly to a real ~360MB hero-scenario frame (11x this test's buffer),
the copy cost alone would land somewhere in the hundreds of milliseconds — hopelessly over the
16.7ms/frame budget regardless of how fast the guest compute itself is.

A second, independent data point reinforces this: a real-world wasmtime user (bytecodealliance
issue #8428) reported ~2ms timing jitter under WASI preview2 for a latency-sensitive control-system
workload — jitter alone, before any real per-pixel work, that already eats a meaningful fraction of
the 16.7ms budget[^w5].

**Decision: WASM/`wasmtime` is the recommended v2 mechanism for non-hot-path extension points**
(exporters, catalog-adjacent logic, stage *parameter*/config logic — anything that doesn't move a
whole-frame buffer across the boundary) **but not for third-party render-stage pixel processing**,
which — if opened to third parties at all in v2 — would need to be GPU shaders (consistent with
#16's GPU compute API direction) rather than a WASM CPU loop, with WASM at most handling the
shader's parameter logic. This is a *direction for v2*, not a v1 commitment: v1 ships first-party
modules only (§1), so no plugin-hosting code needs to exist yet. When v2 third-party plugins are
actually built, `wasmtime`'s sandboxing is the right property to lean on regardless of the
hot-path finding above — bounds-checked linear memory, no arbitrary code execution, a formally
verified (VeriWasm) compiled-output safety property, and fuel/epoch-based CPU-time bounding via
`Store::set_fuel`/`Engine::increment_epoch()`[^w3][^w6] — properties `libloading`-style dylib
loading gives no equivalent of. `extism`[^w2] is worth evaluating at that point as a
marshaling-boilerplate layer over raw `wasmtime`, rather than hand-rolling `Store`/`Linker`/WIT
bindings from scratch, if the number of extension points warrants it.

### 7. Extension-point trait shape (identity, not execution)

All seven extension points (RAW decoder, camera color profile, lens-correction data, render
stage, AI model provider, exporter, catalog store) share the same minimal trait shape for
*identity and versioning*, deliberately **not** their execution signature:

```rust
pub trait Module: Send + Sync {
    /// Namespaced id (e.g. "nicti.decoder.libraw", or "vendor.stage_name" for a v2 plugin —
    /// ADR-0002's stage-id convention, generalized to every extension point).
    fn id(&self) -> &str;
    fn schema_version(&self) -> u32;
    /// Migrates an older params blob forward (ADR-0002's `legacy_params()`-style pattern).
    /// `None` means this build doesn't recognize the version at all — the caller must treat
    /// the data as read-only rather than dropping or guessing at it.
    fn migrate_params(&self, from_version: u32, params: serde_json::Value)
        -> Option<serde_json::Value>;
}
```

A render stage's actual per-frame execution signature (GPU buffer bindings, cache-key
interaction with Tapetum's stage cache) is deliberately left as a placeholder here, owned by #16
(GPU compute API) and #44 (Tapetum) — this ADR only settles how a module is *found, versioned, and
loaded*, not how a render stage specifically executes.

### 8. Proposed crate layout for #20

- **`nicti-claw`**: `Module`/`Descriptor`/`LazyModule` registry (generalizing
  `spikes/sheath/src/registry.rs`) and the `DylibStage` C-ABI loader (generalizing
  `spikes/sheath/src/dylib.rs`) — the load-bearing crate this ADR is really about.
- One crate per domain implementing `nicti-claw`'s traits: `nicti-decode` (RAW decoders),
  `nicti-color` (camera color profiles), `nicti-lens` (lens-correction data), `nicti-render`
  (render stages — later home of Tapetum, #44), `nicti-ai` (AI model providers — the
  `ort`/`load-dynamic` boundary lives here), `nicti-export` (exporters), `nicti-catalog`
  (catalog store — #22's home).
- This is a proposal for #20 to adopt or revise, not a binding commitment of this ADR.

## Prior art

**darktable**'s IOP modules genuinely *are* dynamically loaded at runtime via GLib's `GModule`
(`dt_iop_module_so_t` holds a live `GModule*` handle)[^p1] — closer to this ADR's §4 than a
first-party-only design might suggest. But despite that, darktable does not appear to support real
out-of-tree third-party IOP plugins in practice: every `.so` under `src/iop/` ships in-tree from
the same source tree, and no darktable doc advertises an out-of-tree IOP workflow. darktable's
actual, officially documented third-party extension surface is **Lua scripting**, a separate,
stable, versioned API[^p2] — a useful reminder that "dynamically loaded" and "open to third
parties" are different properties, and that this ADR's v1 dynamic-loading mechanism (§4) existing
doesn't by itself answer the v2 third-party-plugin question (§6). **RawTherapee** has no plugin
system of any kind — a monolithic codebase[^p3]. **vkdt** (a Vulkan-based rewrite by darktable's
own author) takes a structurally different approach worth noting for #44/Tapetum specifically: a
node-graph (DAG) pipeline rather than a linear IOP stack, with the DAG scheduler itself aware of
dependencies and memory allocation[^p4] — not directly this ADR's decision, but relevant prior art
for whoever designs Tapetum's stage graph.

Two Rust-ecosystem precedents anchor this ADR's split design. **Bevy**'s `Plugin` trait
(`fn build(&self, app: &mut App)`, registered via `App::add_plugins()`)[^p5] is exactly this ADR's
§1 shape: compile-time, in-process, first-party. **Zed**'s extension system — third-party code
compiled to `wasm32-wasip2`, implementing a `zed::Extension` trait, with the host/extension
contract defined via WIT for a versioned stable ABI across host updates[^p6] — is the closest
real-world precedent for this ADR's §6 v2 direction, and is worth re-reading in detail whenever v2
third-party plugins actually get built.

## Consequences

- **Unblocks #20**: the crate layout in §8, and `nicti-claw`'s traits/registry (§2, §7) plus its
  dylib loader (§4), are ready to generalize from `spikes/sheath` into real crates.
- **Feeds #37/#39**: if `rawler`/`lensfun-rs` end up used, ADR-0003's LGPL-isolation requirement is
  satisfied via §4's `cdylib` boundary — a concrete mechanism now exists, not just a promise to
  find one later.
- **Feeds #16/#44**: §7 deliberately leaves the render-stage execution signature open for those
  tickets to settle; §6 sets a direction (GPU shaders, not WASM CPU loops, for any future
  third-party render stage) they should design against.
- **Sets, but does not commit, v2's third-party plugin mechanism** (§6): WASM/`wasmtime` for
  non-hot-path extension points, informed by a measured (not assumed) timing result. This is a
  recommendation for whoever picks up that work in v2, not a decision this ADR is authorized to
  make final given v1 ships no plugin-hosting code at all.
- **`deny.toml` amended**: `ISC` added to the Rust-crate license allowlist for `libloading` (see
  ADR-0003's Amendments section and `docs/licensing.md`).
- **A pre-existing CI gap was found and fixed in the same PR, unrelated to this ADR's content but
  discovered while verifying it**: `.github/workflows/ci.yml`'s `clippy`/`test` jobs ran `cargo
  clippy`/`cargo test` without `-p`/`--workspace`. Because `nicti`'s root manifest is both the
  workspace root *and* a real package (not a virtual manifest), that silently restricted both jobs
  to the root `nicti` placeholder crate — `spikes/pawprint`'s 20 tests (and now `spikes/sheath`'s
  8) were never actually exercised by CI. Fixed by adding `--workspace` to both jobs (matching the
  `cargo-deny` job, which already needed and documented the same fix for its own purposes) and to
  `CLAUDE.md`'s local dev instructions.

---

## Verified findings

All claims fetched/verified 2026-09-23 by three parallel research passes (Rust dylib/ABI
mechanisms; WASM hosting; prior-art plugin architectures), each citing a primary source with a
verification date where one exists, matching the citation discipline of ADR-0001/0002.

### Rust dylib/ABI mechanisms

[^c1]: `libloading` v0.9.0, ISC license — https://docs.rs/libloading/latest/libloading/ and
    crates.io API — **primary-source-verified**. `Symbol<T>`'s docs state it "will not outlive
    the `Library` from which it comes" (lifetime-bound, borrow-checker-enforced); all
    loading/lookup calls are `unsafe` since the compiler can't verify a dynamically loaded
    symbol's actual signature.
[^c2]: `abi_stable` v0.11.3 (released 2026-08-17), MIT/Apache-2.0 —
    https://docs.rs/abi_stable/latest/abi_stable/ — **primary-source-verified** for
    version/license/purpose (Rust-to-Rust FFI safe across *different* compiler versions, via
    FFI-safe wrapper types plus load-time layout validation); **best-available-secondary** for
    "actively maintained" specifically (recent release date, but commit cadence/issue-response
    time not independently checked).
[^c3]: `stabby` v72.1.16 (date/CI-based versioning, not a semver-maturity signal), EPL-2.0 OR
    Apache-2.0, github.com/ZettaScaleLabs/stabby — **primary-source-verified** for
    version/license/tagline via crates.io API; **not independently verified** for
    maturity/adoption.
[^c4]: Rust type-layout instability — https://doc.rust-lang.org/reference/type-layout.html —
    **primary-source-verified** for the general claim ("type layout can be changed with each
    compilation... we only document what is guaranteed today"; the default representation
    doesn't guarantee field order). The specific `dyn Trait`/vtable-layout-across-compilations
    claim rests on this general instability plus community knowledge, not an explicit quoted
    sentence — **best-available-secondary** for that narrower sub-claim.
[^c5]: `ort` crate's `load-dynamic` feature — https://docs.rs/ort/latest/ort/environment/fn.init_from.html
    and https://ort.pyke.io/setup/linking — **primary-source-verified**. `ort::init_from(path)`
    loads the ONNX Runtime shared library from an arbitrary runtime path, must be called before
    any other `ort` API.
[^c6]: `dyn Trait`/vtable vs. FFI call overhead — no single authoritative primary-source
    benchmark found; a Godot-Rust FFI benchmarking writeup
    (https://godot-rust.github.io/dev/ffi-optimizations-benchmarking/) measures both in the
    single-digit-nanosecond range once warmed — **best-available-secondary**, directionally
    reliable but not rigorously benchmarked for this specific comparison.

### WASM hosting

[^w1]: `wasmtime` v49.0.0 (2026-09-21 release), Apache-2.0 WITH LLVM-exception —
    https://github.com/bytecodealliance/wasmtime/releases — **primary-source-verified** for
    version/license. WASI Preview 2/Component Model is production-usable (wasmCloud 2.5 ships
    WASI P3-by-default on wasmtime 46, per a secondary source) but still shows active feature
    churn in the same release notes — **not verified** as "frozen, no more breaking changes."
[^w2]: `extism`, BSD-3-Clause, github.com/extism/extism — **primary-source-verified**. A
    plugin-framework layer over `wasmtime` (or other wasm runtimes) handling host↔guest
    data-marshaling boilerplate; no formal 1.0/stability declaration found —
    **best-available-secondary** on maturity.
[^w3]: Fuel (`Store::set_fuel`, deterministic instruction-count budget) and epoch-based
    (`Store::set_epoch_deadline`/`Engine::increment_epoch()`, coarser wall-clock-driven)
    interruption — https://docs.wasmtime.dev/api/wasmtime/struct.Store.html and
    https://docs.wasmtime.dev/examples-interrupting-wasm.html — **primary-source-verified**.
[^w4]: Host↔guest large-buffer copy cost at hundreds-of-MB scale — no authoritative quantified
    benchmark found at this specific scale. A live 2026 wasmtime/WASI discussion on whether
    `wasi:http` resource streaming enables a direct-memory-view path concludes the state of the
    art is still "essentially copying via guest memory pointers" — **not independently
    verifiable** at the specific scale this ADR cares about; the absence of a published solved
    zero-copy path is itself meaningful evidence, not proof.
[^w5]: bytecodealliance/wasmtime issue #8428: a control-systems user reported ~2ms timing jitter
    under WASI preview2 (vs. microsecond-level on older wasmtime), root-caused to a WASI-layer
    difference and later patched — **primary-source-verified** for the incident;
    **best-available-secondary** for generalizing it into "WASM unsuitable for real-time media"
    broadly. No DAW/game-engine case study with hard accept/reject numbers for audio/pixel inner
    loops specifically was found — the weakest-cited part of this ADR's hot-path conclusion,
    compensated for by this ADR's own direct measurement (`wasm_vs_native.rs`) rather than relying
    on precedent alone.
[^w6]: `wasmtime` security model — bounds-checked linear memory + guard pages, all control
    transfers to type-checked destinations, no raw syscall access outside explicit
    imports/WASI's capability model, Cranelift-generated code formally verified sandbox-safe by
    VeriWasm (UCSD/Stanford/Fastly), continuous fuzzing, a dedicated security/CVE process —
    https://docs.wasmtime.dev/security.html and
    https://bytecodealliance.org/articles/security-and-correctness-in-wasmtime — both
    **primary-source-verified**.

### Prior art

[^p1]: darktable IOP modules loaded via GLib's `GModule` (`dt_iop_module_so_t.module`,
    `dt_iop_load_module_so()`/`dt_iop_load_module_by_so()` in `src/develop/imageop.c`), version
    check for parameter introspection (`DT_INTROSPECTION_VERSION`) plus `dt_iop_legacy_params()`
    for cross-version param migration — https://github.com/darktable-org/darktable —
    **primary-source-verified**.
[^p2]: No darktable doc or repo evidence of a supported out-of-tree third-party IOP plugin
    workflow found (every in-tree `.so` builds from the same source tree) —
    **best-available-secondary** (absence-of-evidence, not an explicit "unsupported" statement).
    Lua scripting as the real, officially documented third-party extension surface —
    https://docs.darktable.org/lua/stable/ and https://github.com/darktable-org/lua-scripts —
    **primary-source-verified**.
[^p3]: RawTherapee has no plugin/module system — a monolithic C++/GTK codebase; the only
    "plugin"-adjacent feature found is a one-way GIMP handoff, not an extension point of its
    own — **best-available-secondary** (absence-of-evidence from a docs/repo skim, not a quoted
    "no plugins" statement).
[^p4]: vkdt's Vulkan node-graph (DAG) pipeline, dependency- and memory-allocation-aware
    scheduling, allows feedback/cyclic connectors for iterative processing —
    https://github.com/hanatos/vkdt/blob/master/src/pipe/readme.md — **primary-source-verified**.
[^p5]: Bevy's `Plugin` trait (`fn build(&self, app: &mut App)`, `App::add_plugins()`, a plain
    `fn(&mut App)` also auto-implementing `Plugin`) —
    https://docs.rs/bevy_app/latest/bevy_app/trait.Plugin.html and
    `examples/app/plugin.rs` in bevyengine/bevy — **primary-source-verified**. Compile-time,
    in-process — a precedent for this ADR's §1, not for dynamic/out-of-process loading.
[^p6]: Zed's third-party extensions: `wasm32-wasip2` modules implementing `zed::Extension` from
    the `zed_extension_api` crate, host/extension contract defined via WIT for a versioned stable
    ABI across host updates — https://zed.dev/docs/extensions/developing-extensions,
    https://zed.dev/blog/zed-decoded-extensions, crates.io/crates/zed_extension_api —
    **primary-source-verified**.

*Not independently verifiable with a primary source: `stabby`'s real-world maturity/adoption
beyond its own crates.io metadata; the exact scaling of this ADR's `wasm_vs_native.rs` numbers to
a literal 45MP/360MB hero-scenario frame (extrapolated linearly from a 32MB measurement, not
independently measured at full scale); whether a `dyn Trait`'s vtable layout specifically (as
opposed to struct field layout generally) is guaranteed unstable across compilations by an
explicit Rust-team statement, as opposed to the general type-layout instability the Reference does
state outright.*
