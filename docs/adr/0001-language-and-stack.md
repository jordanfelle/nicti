# ADR-0001: Implementation language and native stack

- **Status:** Accepted (see Context update, 2026-09)
- **Date:** 2026-09-23
- **Ticket:** #15 Research: language/stack choice

## Context

Nicti is a personal, non-destructive RAW photo editor and DAM meant to replace Adobe Lightroom
Classic (LRC). v1 targets Windows on a high-end NVIDIA desktop, Nikon RAW (NEF, including HE/HE*
compressed variants), a library scaling to 600k-2M assets, and a hero-scenario performance bar
(bulk WB + vibrance + 2 AI masks + AI denoise, then navigate/crop between edited images at <100ms
switch / 60fps crop). macOS/Linux release builds are deferred to v2 (#73). Most code will be
written by one person plus Claude Code, not a team.

The E0 PRD (#4) states a preference for memory safety but does not mandate a specific language.
This ADR is the gating decision for #16 (GPU compute API), #19 (module/plugin architecture, Claw),
and #68 (GUI framework, whose candidate list — GPUI/Iced/egui/Slint — assumes Rust).

## Decision drivers

Six candidates — **Rust, C++, C#/.NET, Go, Zig, Swift** — were scored against 12 criteria drawn
from the E0 PRD:

1. RAW decode reach (LibRaw bindings; Nikon HE/HE* NEF support)
2. GPU compute (wgpu/Vulkan/D3D12/CUDA maturity on Windows+NVIDIA)
3. ML inference (ONNX Runtime + CUDA/TensorRT execution providers)
4. UI + GPU viewport availability (custom viewport, ~2M-item virtualized grid — availability only,
   the actual toolkit pick is #68)
5. Embedded DB reach (#67's candidates: SQLite, embedded Postgres, DuckDB, LMDB)
6. Latency predictability against the 16.7ms slider-drag budget and <50ms cull-keypress budget
7. Memory safety
8. Concurrency (cooperative cancellation, priority scheduling — feeds the Pounce scheduler, #54)
9. Plugin story (C ABI/dylib, WASM host, out-of-process — feeds Claw, #19)
10. Windows-first tooling, with a credible macOS/Linux path for v2
11. Open-source viability (contributor pool, toolchain licensing)
12. Solo + agent (Claude Code) productivity — how well the compiler/type system catches
    agent-introduced mistakes

Full research (three parallel passes, one per language pair) is in the [Verified matrix](#verified-matrix)
appendix below, with a primary-source citation and verification date on every claim.

## A cross-cutting risk, independent of this decision

**LibRaw does not yet publicly ship a Nikon HE/HE*-format NEF decoder**, regardless of language,
though there's a concrete near-term path to one. Community PR
[LibRaw/LibRaw#826](https://github.com/LibRaw/LibRaw/pull/826) is **open** (not rejected as an
earlier draft of this ADR incorrectly stated) and demonstrates a working HE decoder: after two
rounds of fixes, an independent tester matched it exactly against Adobe DNG Converter across
336.7M samples on 11 NEFs (Z9, Z8, Z6III, Zf, Z5II). LibRaw's own maintainer (Alexey Danilchenko)
says the library has had a separately-developed HE/HE* decoder in production use inside
FastRawViewer and RawDigger since 2024, and on 2026-09-12 committed to shipping it "along with
the next public snapshot this fall" — 2026-09-12 comment on the same PR thread, verified
2026-09-23. This PR itself won't be merged (the maintainer's own decoder will ship instead), but
the net signal is a public-facing decoder within months, not "no ETA." (An earlier LibRaw forum
post, [libraw.org/node/2766](https://www.libraw.org/node/2766), said "no estimated completion
date" — that post predates the PR#826 thread by several months and is now stale; don't rely on
it.) This is tracked as a project risk under #37 (RAW decoder research) regardless — v1 may still
need a stopgap (Adobe DNG Converter or the manufacturer SDK as a bridge) if the fall snapshot
slips. It does not factor into the language choice below since every binding candidate sits on
top of the same LibRaw C library.

## Decision

**Rust**, using C FFI bindings to LibRaw (and to lensfun if lens-correction data is needed).

Rust and C++ are the two strongest candidates overall, and the matrix is genuinely close: 5 of 12
criteria are ties (GPU compute, ML inference, embedded-DB reach, latency predictability,
open-source viability), C++ rates strictly stronger on 3 (RAW decode reach, UI+GPU viewport
availability, Windows-first tooling — all three directly relevant to this project's stated v1
priorities), and Rust rates strictly stronger on 4 (memory safety, concurrency, plugin story,
solo+agent productivity). This is a real tradeoff, not a clean sweep for either side. The
C++ wins are worth taking seriously: LibRaw's own C++ API is more complete than Rust's
early-stage bindings; Qt/Dear ImGui have decades of production use at exactly the
DAM/virtualized-grid scale this project targets; and MSVC/PIX/WinDbg are the deeper, more
battle-tested Windows toolchain. None of them are dismissed here — #37, #68, and general Windows
tooling risk should be read with C++'s stronger baseline in mind, and revisited if Rust's younger
bindings (`libraw-rs`, egui's virtualization story) prove inadequate in practice.

Rust is still the pick because the three criteria where it wins outright matter more for how
this project will actually be built: solo, with heavy AI-agent involvement, and with no second
human reviewer to catch memory-safety bugs before they ship.

- **Memory safety (criterion 7):** compiler-enforced (borrow checker, `Send`/`Sync`), vs. C++'s
  discipline-and-tooling-dependent safety. This matters more here than usual: a solo project
  without a second reviewer has less safety net against the class of bug (use-after-free,
  data races) that a borrow checker eliminates at compile time.
- **Plugin story (criterion 9, feeds #19):** `wasmtime` is a first-class native Rust embedding
  API from the Bytecode Alliance with active fuzzing infrastructure. C++'s WASM-host story
  (Extism's C++ SDK) is workable but thinner. Go's native `plugin` package is **explicitly
  unsupported on Windows** ([pkg.go.dev/plugin](https://pkg.go.dev/plugin), tracking issue
  [golang/go#19282](https://github.com/golang/go/issues/19282), both open, verified 2026-09-23) —
  but this is *not* a hard blocker the way an earlier draft of this ADR framed it: HashiCorp's
  `go-plugin` (subprocess + net/rpc/gRPC, Windows-compatible, battle-tested in Terraform/Vault/
  Nomad) covers the "out-of-process" branch of this criterion's own definition. Go's gap here is
  the same tier as C++'s — workable with a different architecture, not blocked — so it isn't
  treated as disqualifying on its own; Go is ruled out below for a different reason.
- **Solo + agent productivity (criterion 12):** exhaustive `Result`/`match` handling and the
  borrow checker catch a large class of agent-introduced mistakes (forgotten error handling,
  dangling references, data races) at compile time, before they ever run. C++ catches
  meaningfully fewer of these; dangling pointers and UB routinely compile cleanly.

C#/.NET rates strong across ML (the only *official* Microsoft ONNX Runtime GPU package), UI, and
Windows tooling, and is the strongest non-Rust/C++ option. Its GC does have a live open issue,
[dotnet/runtime#65850](https://github.com/dotnet/runtime/issues/65850), discussing GC pauses too
large for 60fps — but that issue's headline 55ms figure is measured on **ARM64 Raspberry Pi 4**,
not desktop hardware; a comment on the same thread states "on PC processor, getting 600fps with
SustainedLowLatency is achievable." Against this project's actual target (a high-end NVIDIA
desktop), the GC-latency risk is real but weaker evidence than the earlier draft implied — this
was softened after the fact-check pass below. Go is not ruled out by the plugin-loading gap
(softened above); it trails Rust because its GPU-compute (`gogpu/wgpu`, described by its own repo
as a young 2026-era implementation) and ML-inference (`yalue/onnxruntime_go`, community-only, no
bundled CUDA build) bindings are markedly less mature than Rust's `wgpu`/`ort`, which have
production users and official Microsoft backing respectively. Zig and Swift are both ruled out
primarily on Windows-tooling immaturity: Zig is pre-1.0 with disruptive breaking changes every
release (the 0.16 `std.Io` rework touched every I/O-touching function), and Swift's Windows
institutional support (the Windows Workgroup) is only ~8 months old as of this research date,
with no first-party GPU-compute or ONNX Runtime path on Windows+NVIDIA.

None of Rust's competitors are hard-blocked; each loses on a different combination of ecosystem
maturity, GC-latency risk, or Windows-tooling youth. Rust is preferred, not the only option that
would have worked.

## Context update (2026-09)

Nicti is now public under AGPL-3.0-or-later and accepts outside contributors (#133) — the
"most code will be written by one person plus Claude Code, not a team" framing in Context above,
and criterion 12's "solo + agent productivity" framing, described the situation at the time this
ADR was written, not a permanent constraint.

Re-checking criterion 12 against that change: the underlying language decision still holds. Rust's
compiler-enforced memory safety and exhaustive `Result`/`match` handling remain valuable
independent of who's writing the code — they catch mistakes regardless of whether the author is a
solo maintainer, an AI agent, or an outside contributor's first PR. What has changed is the
"no second human reviewer" framing under criterion 7 (Decision, above): outside contributions now
mean real human review is possible on at least some changes, which was the specific gap that
argument was compensating for. This doesn't change the Rust decision — it was never solely
justified by the absence of reviewers — but it does mean that gap is now partially closed by
something other than the compiler.

The original scoring and matrix below are left as written; they're a record of the decision as
made, not something to re-litigate here.

## Consequences

- **Unblocks #16** (GPU compute API): `wgpu` (cross-platform, Vulkan/D3D12/Metal, in W3C
  CR-draft alignment) or `ash` (raw Vulkan) are both viable within Rust; the specific choice
  between them is #16's own decision, not this ADR's.
- **Unblocks #19** (Claw, module/plugin architecture): `libloading` for C-ABI dylib loading,
  `wasmtime` for a WASM host — both first-class Rust crates.
- **Unblocks #68** (GUI framework): its Rust-only candidate list (GPUI/Iced/egui/Slint) is now a
  valid constraint, not an assumption to revisit.
- **Expected cross-language FFI:** LibRaw is a C/C++ library — Rust bindings are early-stage/WIP
  (`libraw-rs`, `libraw` crates) rather than mature, and will need `bindgen`-generated or
  hand-written FFI, tracked in #37. The `lensfun` crate (`vdavid/lensfun-rs` v0.7.0) is a
  pre-alpha pure-Rust port (correcting an earlier brief's claim that it was a C-binding wrapper) —
  treat as unverified-in-production pending #37's own evaluation.
- **Explicitly out of scope for this ADR** (deferred to their own tickets): the specific GPU
  compute API (#16), the embedded DB engine (#67), and the GUI toolkit (#68). This ADR only
  established that Rust has a *reachable* path to each — not which option within that path wins.

---

## Verified matrix

All claims below were fetched/verified 2026-09-23 by three parallel research passes (Rust+C++,
C#+Go, Zig+Swift), each citing a primary source (crate/package index, GitHub repo, or official
docs — not blog posts or Stack Overflow) with a URL and verification date. Ratings: **Strong** /
**Adequate** / **Weak** / **Blocker**.

### Rust vs. C++

| # | Criterion | Rust | C++ |
|---|---|---|---|
| 1 | RAW decode reach | Adequate — `libraw-rs`/`libraw` are early-stage FFI bindings[^r1]; pure-Rust `rawloader` (v0.37.x) has no HE path[^r2]. Shared HE/HE* gap (see above). | Strong — native first-party C++ API via vcpkg `libraw` port, full surface, thread-safe variant[^r3]. Shared HE/HE* gap. |
| 2 | GPU compute | Strong — `wgpu`[^r4], `ash` (raw Vulkan)[^r5], `windows-rs` (Microsoft's own crate, covers D3D12)[^r6]. | Strong — native Vulkan SDK, DX12 SDK, CUDA toolkit are all first-party C/C++ APIs; the reference implementation language for all three. |
| 3 | ML inference | Strong — `ort` crate, maintained safe wrapper for ONNX Runtime 1.28 with `cuda`/`tensorrt` features, used in production (Twitter, Bloop)[^r7]. | Strong — ONNX Runtime's official C++ API (`onnxruntime_cxx_api.h`) is Microsoft's primary supported surface[^r8]. |
| 4 | UI + GPU viewport | Adequate — egui/iced/Slint can host a custom GPU viewport (egui via wgpu paint callbacks); `egui_virtual_list` exists, though egui's own docs favor built-in `ScrollArea` virtualization for 1M+ items[^r9]. Younger, less battle-tested ecosystem. | Strong — Qt embeds custom OpenGL/D3D content under QML via `QQuickItem`/scenegraph hooks[^r10]; Dear ImGui's `ImGuiListClipper` is a proven pattern for virtualizing huge tables[^r11]. Decades of production use at DAM/NLE scale. |
| 5 | Embedded DB reach | Strong — `rusqlite`[^r12], `duckdb` crate (Arrow interchange)[^r13], `heed` (typed LMDB, maintenance status not independently re-verified). | Strong — direct native access; DuckDB is itself implemented in C++, SQLite3/LMDB both ship as vcpkg ports[^r14]. |
| 6 | Latency predictability | Strong — no GC; RAII/ownership gives deterministic drop timing at zero runtime cost[^r15]. | Strong — no GC; RAII deterministic destructors. Effectively a tie with Rust on this criterion alone. |
| 7 | Memory safety | **Strong** — safe by default; borrow checker statically prevents use-after-free/double-free/data races at compile time; `unsafe` is explicit and greppable[^r15]. | **Weak** — no default safety; relies on discipline (smart pointers, RAII) + external tooling (ASan/UBSan/Valgrind); raw-pointer misuse is silent UB by default. |
| 8 | Concurrency | Strong — `tokio`/`rayon`; `Send`/`Sync` traits give compiler-enforced thread safety; cancellation via dropped futures/`CancellationToken`. | Adequate — `std::jthread`/`stop_token` (C++20), oneTBB for priority/task scheduling, but no compiler-enforced race safety. |
| 9 | Plugin story | **Strong** — `libloading` for C-ABI dylib loading; `wasmtime` is a first-class, heavily-fuzzed native Rust embedding API from the Bytecode Alliance[^r16]. | Adequate — native `dlopen`/`LoadLibrary` is trivial; Extism's C++ SDK for WASM hosting works but pulls in extra deps and is a thinner surface than wasmtime's[^r17]. |
| 10 | Windows-first tooling | Adequate — `cargo` builds cleanly on Windows/MSVC; `windows-rs` is Microsoft's own crate[^r6]; native profiler (PIX/VTune) and PDB integration is less battle-tested than for C++. Cross-platform v2 port is straightforward via `wgpu`'s existing backends. | Strong — MSVC/Visual Studio, PIX, WinDbg, vcpkg: the native first-party Windows toolchain with decades of depth. Cross-platform port requires more manual backend work. |
| 11 | Open-source viability | Strong — crates.io ecosystem is large, mostly MIT/Apache-2.0, growing contributor pool. | Strong — vastly larger existing graphics/gamedev C++ contributor pool; more fragmented build tooling (CMake variants) raises contribution friction. Roughly a wash. |
| 12 | Solo + agent productivity | **Strong** — the borrow checker and exhaustive `Result`/`match` handling catch a large class of agent mistakes at compile time, before they ever run. | Weak/Adequate — the compiler catches far fewer classes of agent-introduced bugs; dangling pointers and UB routinely compile cleanly. |

[^r1]: `libraw-rs`/`libraw` crates — https://crates.io/crates/libraw-rs , https://crates.io/crates/libraw — verified 2026-09-23
[^r2]: `rawloader` crate (v0.37.1) — https://crates.io/crates/rawloader — verified 2026-09-23
[^r3]: vcpkg `libraw` port — https://github.com/microsoft/vcpkg/blob/master/ports/libraw/portfile.cmake — verified 2026-09-23
[^r4]: `wgpu` — https://crates.io/crates/wgpu , https://github.com/gfx-rs/wgpu — verified 2026-09-23
[^r5]: `ash` — https://crates.io/crates/ash , https://github.com/ash-rs/ash — verified 2026-09-23
[^r6]: `windows` crate (Microsoft official) — https://github.com/microsoft/windows-rs — verified 2026-09-23
[^r7]: `ort` crate — https://crates.io/crates/ort — verified 2026-09-23
[^r8]: ONNX Runtime TensorRT EP C++ API — https://onnxruntime.ai/docs/execution-providers/ — verified 2026-09-23
[^r9]: `egui_virtual_list` — https://crates.io/crates/egui_virtual_list ; egui repo — https://github.com/emilk/egui — verified 2026-09-23
[^r10]: Qt "OpenGL Under QML" — https://doc.qt.io/qt-6/qtquick-scenegraph-openglunderqml-example.html — verified 2026-09-23
[^r11]: Dear ImGui `ImGuiListClipper` (`imgui_tables.cpp`) — https://github.com/ocornut/imgui/blob/master/imgui_tables.cpp — verified 2026-09-23
[^r12]: `rusqlite` — https://crates.io/crates/rusqlite — verified 2026-09-23
[^r13]: `duckdb` Rust client — https://duckdb.org/docs/current/clients/rust/overview — verified 2026-09-23
[^r14]: vcpkg ports (`duckdb`, `lmdb`, `sqlite3`) — https://vcpkg.link/ports/duckdb , https://vcpkg.link/ports/lmdb — verified 2026-09-23
[^r15]: Rust ownership/borrowing — https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html — verified 2026-09-23
[^r16]: `wasmtime` — https://crates.io/crates/wasmtime , https://docs.wasmtime.dev/contributing-fuzzing.html (fuzzing/OSS-Fuzz specifically), https://bytecodealliance.org/security (security process) — verified 2026-09-23
[^r17]: Extism C++ SDK — https://github.com/extism/cpp-sdk — verified 2026-09-23

*Not independently re-verified: exact crates.io publish dates/download counts for several crates
above (direct API access was blocked in the research sandbox; facts are sourced from WebSearch
snippets of crates.io/docs.rs/GitHub content instead — treat version numbers as reasonably
current, not byte-for-byte re-checked). `heed`'s current maintenance status and Rust's native
Windows PDB/profiler maturity (row 10) are asserted from general ecosystem knowledge, not a single
fetched primary source — worth a targeted follow-up if row 10 ever becomes deciding.*

### C#/.NET vs. Go

| # | Criterion | C#/.NET | Go |
|---|---|---|---|
| 1 | RAW decode reach | Adequate — `Sdcb.LibRaw` is a feature-rich NuGet LibRaw wrapper, but its GitHub repo shows no push since 2025-01-28 (~8 months stale as of this research date) — "actively-maintained" overstates it[^c1]. Shared HE/HE* gap. | Adequate — several thin community `go-libraw` cgo wrappers exist, none as mature[^c2]. Shared HE/HE* gap. |
| 2 | GPU compute | Strong — `Vortice.Windows`/`Vortice.Direct3D12`/`Vortice.Vulkan` are mature, actively-versioned low-level bindings with a proven Windows+NVIDIA compute path[^c3]. | Adequate — `gogpu/wgpu` is a young (2026-era) pure-Go WebGPU implementation; thinner, less battle-tested than Vortice[^c4]. |
| 3 | ML inference | **Strong** — `Microsoft.ML.OnnxRuntime`/`.Gpu` are **official** Microsoft NuGet packages with CUDA/TensorRT EPs[^c5]. | Adequate — `yalue/onnxruntime_go` is community-only; CUDA EP works but you must supply your own ORT build[^c6]. |
| 4 | UI + GPU viewport | Strong (availability) — Avalonia/WPF/WinUI3 support custom D3D/Vulkan-interop viewports and virtualized large lists; large, Windows-proven ecosystem. | Adequate (availability) — Gio is a genuine GPU-rendered toolkit on Windows with community virtualized-grid patterns, but a much smaller ecosystem[^c7]. |
| 5 | Embedded DB reach | Strong — SQLite (`Microsoft.Data.Sqlite`), DuckDB (`DuckDB.NET.Data.Full`, 1.5M+ downloads)[^c8], LMDB (LightningDB.NET). | Strong — SQLite (`mattn/go-sqlite3`, `modernc.org/sqlite`), DuckDB (`duckdb/duckdb-go`)[^c9], LMDB (`bmatsuo/lmdb-go`). |
| 6 | Latency predictability | Adequate — Server GC ~10ms vs Workstation ~25ms; `SustainedLowLatency` suppresses foreground gen2 collections, but a live `dotnet/runtime` issue explicitly discusses GC pauses too large for 60fps[^c10]. | Strong — sub-ms/low-single-digit-ms STW pauses since Go 1.8's concurrent mark phase, comfortably inside both budgets under normal load[^c11]. |
| 7 | Memory safety | Adequate — GC'd by default; heavy LibRaw/GPU/ONNX interop (P/Invoke) will live in unsafe code regardless. | Adequate — GC'd by default; cgo (required for LibRaw/DuckDB/ONNX bindings) drops safety at the same native boundaries. Materially equivalent to C#. |
| 8 | Concurrency | Strong — `Task`/`async`-`await` + built-in `CancellationToken`, `Channels`, `PriorityQueue<T,P>` — a richer built-in toolkit for a foreground-preemptive scheduler. | Adequate — goroutines + `context.Context` give excellent cooperative cancellation, but the runtime has no built-in priority concept; must be hand-built. |
| 9 | Plugin story | Strong — `AssemblyLoadContext` gives an officially-documented in-process load/unload story, plus `DllImport`/`NativeLibrary` and Wasmtime .NET bindings[^c12]. | **Weak** — Go's native `plugin` package is **explicitly unsupported on Windows** (Linux/FreeBSD/macOS only), confirmed on its own pkg.go.dev page and a long-open tracking issue[^c13]. In-process module loading is off the table on Windows. |
| 10 | Windows-first tooling | Strong — Visual Studio + MSBuild + dotnet-trace/PerfView give first-class, deeply-integrated Windows tooling; credible macOS/Linux port via Avalonia. | Adequate — cross-compiles to Windows trivially, and the macOS/Linux port is easier than for .NET, but Windows-side debugging (delve, pprof) is less IDE-integrated. |
| 11 | Open-source viability | Strong — MIT-licensed runtime/SDK under the .NET Foundation since 2014 with an explicit patent promise; governance is still Microsoft-controlled, not a neutral foundation[^c14]. | Strong — BSD-3-Clause, huge contributor base; governance is single-vendor (Google) — a comparable risk profile, different vendor. |
| 12 | Solo + agent productivity | Strong (judgment call, no citable primary source) — strong static typing, nullable-reference-types, and Roslyn analyzers give agents more compile-time guardrails. | Strong (same caveat) — smaller language surface and a strict compiler (unused imports/vars are hard errors) catch agent slop; slightly weaker type-level guardrails than C#'s generics/nullability. |

[^c1]: `Sdcb.LibRaw` — https://github.com/sdcb/Sdcb.LibRaw — verified 2026-09-23
[^c2]: Go LibRaw wrappers — https://pkg.go.dev/github.com/seppedelanghe/go-libraw , https://pkg.go.dev/github.com/inokone/golibraw — verified 2026-09-23
[^c3]: `Vortice.Windows` — https://www.nuget.org/packages/Vortice.Direct3D12 — verified 2026-09-23
[^c4]: `gogpu/wgpu` — https://github.com/gogpu/wgpu — verified 2026-09-23
[^c5]: `Microsoft.ML.OnnxRuntime.Gpu` (v1.30.0) — https://www.nuget.org/packages/Microsoft.ML.OnnxRuntime.gpu — verified 2026-09-23
[^c6]: `yalue/onnxruntime_go` — https://pkg.go.dev/github.com/yalue/onnxruntime_go — verified 2026-09-23
[^c7]: Gio UI — https://gioui.org/ — verified 2026-09-23
[^c8]: `DuckDB.NET.Data.Full` (v1.5.5) — https://www.nuget.org/packages/DuckDB.NET.Data.Full — verified 2026-09-23
[^c9]: `go-duckdb` (moved to `duckdb/duckdb-go` at v2.5.0) — https://github.com/duckdb/duckdb-go — verified 2026-09-23
[^c10]: `dotnet/runtime` GC-pause-for-60fps issue — https://github.com/dotnet/runtime/issues/65850 — verified 2026-09-23
[^c11]: Go GC tuning guide — https://tip.golang.org/doc/gc-guide — verified 2026-09-23
[^c12]: `AssemblyLoadContext.Unload` — https://learn.microsoft.com/en-us/dotnet/api/system.runtime.loader.assemblyloadcontext.unload — verified 2026-09-23
[^c13]: Go `plugin` package Windows support — https://pkg.go.dev/plugin ; tracking issue https://github.com/golang/go/issues/19282 — verified 2026-09-23
[^c14]: .NET open-source history — https://weblogs.asp.net/scottgu/announcing-open-source-of-net-core-framework-net-core-distribution-for-linux-osx-and-free-visual-studio-community-edition/ — verified 2026-09-23

*Not independently verifiable with a primary source: criterion 12 for both languages — no
package-index/GitHub/official-docs page states agent-coding reliability; both ratings above are
reasoning, not citation, and should be weighted accordingly.*

### Zig vs. Swift

| # | Criterion | Zig | Swift |
|---|---|---|---|
| 1 | RAW decode reach | Weak — no dedicated Zig LibRaw binding exists; would require hand-written `@cImport` FFI[^z1]. Shared HE/HE* gap. | Adequate — `SwiftLibRaw` (0.1.4) is an informal wrapper built against Homebrew's LibRaw; Windows path unverified/likely absent[^z2]. Shared HE/HE* gap. |
| 2 | GPU compute | Strong — `zgpu`/`wgpu-native-zig` explicitly support Windows via D3D12; Zig's C interop makes raw CUDA/Vulkan binding straightforward[^z3]. | Adequate, real caveat — no official Apple GPU-compute path applies on Windows; community `SwiftCU` (unofficial, single-maintainer) wraps CUDA and claims Windows testing[^z4]. |
| 3 | ML inference | Weak — only informal/experimental community bindings (`recursiveGecko/onnxruntime.zig`, self-described "incomplete experimental")[^z5]. | Weak-to-adequate, Apple-skewed — Microsoft's `onnxruntime-swift-package-manager` is packaged as an Apple-platforms `.xcframework`; Windows support and CUDA/TensorRT EP wiring are undocumented[^z6]. |
| 4 | UI + GPU viewport | Adequate (exists, immature) — `Capy` has confirmed Windows support (native Win32 widgets); `zgui`/`dvui` suit a custom-owned render loop[^z7]. No evidence of 2M-item-scale proof. | Adequate (exists, immature, non-native look) — `SwiftCrossUI` supports Windows via a GTK4 backend; v0.7.0 notes "200MB smaller Windows builds," implying prior builds were heavy[^z8]. |
| 5 | Embedded DB reach | Strong — `zig-sqlite`, `zuckdb.zig` (DuckDB), multiple LMDB bindings[^z9]. | Strong — official `duckdb-swift` explicitly supports Windows; LMDB via `SwiftLMDB`; SQLite via `swift-sqlcipher`[^z10]. |
| 6 | Latency predictability | Strong — no GC, manual allocator control, deterministic frame timing by design. | Adequate, real risk — ARC retain/release traffic is a known jitter source in hot per-frame paths; needs disciplined value-type/`unowned` use to hit 16.7ms reliably. |
| 7 | Memory safety | Weak/discipline-required — no borrow checker; debug-mode checks are compiled out in ReleaseFast. | Adequate/strong by default — ARC + strict optionals + bounds checking on by default in all build configs. |
| 8 | Concurrency | Adequate, in flux — Zig 0.16's `std.Io` gives cooperative cancellation via `future.cancel()`, but only the thread-pool-backed `std.Io.Threaded` ships; the event-driven backend is explicitly WIP[^z11]. No built-in priority scheduling. | **Strong** — structured concurrency (`TaskGroup`) with automatic cooperative-cancellation propagation and built-in `TaskPriority` with priority escalation — directly matches a foreground-preemptive scheduler[^z12]. |
| 9 | Plugin story | Strong — first-class C ABI, `wasmtime-zig`, official `extism/zig-sdk` (host) + `extism/zig-pdk` (plugin) covering both sides[^z13]. | Adequate — C ABI interop has documented C++-stdlib linking friction; WASM hosting via community `swift-wasmtime` (vendors Windows artifacts) and an official Extism Swift SDK[^z14]. |
| 10 | Windows-first tooling | **Weak/unproven for production** — cross-compiles well, but Zig is explicitly pre-1.0 with disruptive breaking changes every release (0.16's `std.Io` rework touched every I/O-touching function)[^z15]. | Adequate but young — an official toolchain exists (WinGet, signed installers), but Swift.org only announced a dedicated Windows Workgroup in January 2026 — first-class Windows support is a very recent institutional commitment, not a mature one[^z16]. |
| 11 | Open-source viability | Adequate — MIT license; 10,000+ commits, but governed by a small core team still steering toward 1.0 with authority to break APIs[^z17]. | Strong — Apache 2.0 with patent grant; ~69.5k stars/10.6k forks, large corporately-backed (Apple) pool — though the Windows-specific slice is much smaller and newer[^z18]. |
| 12 | Solo + agent productivity | Adequate, real caveat — markedly less training-data representation than Rust/C++, and the pre-1.0 breaking-change cadence means agents will confidently emit outdated API patterns unless carefully steered. | Adequate, real caveat — a 2026 benchmark paper (44 LLMs) found all models score lower on Swift than Python/Java, attributed to limited Swift training data[^z19]. |

[^z1]: Zig C-interop — https://ziglang.org/documentation/master/#C — verified 2026-09-23 (no dedicated LibRaw binding found)
[^z2]: SwiftLibRaw 0.1.4 — https://www.libraw.org/node/2858 — verified 2026-09-23
[^z3]: `zgpu` — https://github.com/zig-gamedev/zgpu — verified 2026-09-23
[^z4]: SwiftCU — referenced via search result only, no separate primary fetch — lower confidence
[^z5]: `onnxruntime.zig` — https://github.com/recursiveGecko/onnxruntime.zig — verified 2026-09-23
[^z6]: `microsoft/onnxruntime-swift-package-manager` — https://github.com/microsoft/onnxruntime-swift-package-manager — fetched 2026-09-23, Windows/CUDA support not stated
[^z7]: Capy — https://github.com/capy-ui/capy — verified 2026-09-23
[^z8]: swift-cross-ui — https://github.com/moreSwift/swift-cross-ui — verified 2026-09-23
[^z9]: `zuckdb.zig` — https://github.com/karlseguin/zuckdb.zig — verified 2026-09-23
[^z10]: `duckdb-swift` — https://duckdb.org/docs/lts/clients/swift — verified 2026-09-23
[^z11]: Zig 0.16 `std.Io` — https://lalinsky.com/2026/05/11/async-io-in-zig-016-today.html — verified 2026-09-23
[^z12]: Swift structured concurrency — https://github.com/swiftlang/swift-evolution/blob/main/proposals/0304-structured-concurrency.md — verified 2026-09-23
[^z13]: `extism/zig-sdk` / `extism/zig-pdk` — https://github.com/extism/zig-sdk , https://github.com/extism/zig-pdk — verified 2026-09-23
[^z14]: swift-wasmtime — https://github.com/OpenCow42/swift-wasmtime — verified 2026-09-23
[^z15]: Zig devlog (1.0 status/breaking changes) — https://ziglang.org/devlog/2026/ — verified 2026-09-23
[^z16]: Swift Windows Workgroup announcement — https://www.swift.org/blog/announcing-windows-workgroup/ — verified 2026-09-23
[^z17]: Zig LICENSE/contributors — https://github.com/ziglang/zig/blob/master/LICENSE — verified 2026-09-23
[^z18]: Swift LICENSE/contributors — https://github.com/swiftlang/swift/blob/main/LICENSE.txt — verified 2026-09-23
[^z19]: SwiftEval paper — https://arxiv.org/abs/2505.24324 — verified 2026-09-23

*Not verifiable with a primary source: whether Swift's ONNX Runtime package is buildable on
Windows with CUDA/TensorRT EPs exposed (Microsoft's repo page didn't state this either way);
Zig's `std.Io.Evented` completion timeline (WIP, no ETA in devlog sources); the anecdote of Claude
Code porting ~750k LOC Zig→Rust (sourced only via a secondary aggregator, not a primary
Anthropic/author post — included with reduced confidence, not load-bearing in the Decision above).*
