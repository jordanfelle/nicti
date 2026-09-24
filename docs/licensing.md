# Third-party license audit

Tracks GitHub issue [#18](https://github.com/jordanfelle/nicti/issues/18). Feeds the outbound
license decision in [#66](https://github.com/jordanfelle/nicti/issues/66) — this document does not
choose Nicti's own license, it audits what Nicti would be allowed to redistribute *under* either
candidate outbound family. See [ADR-0003](adr/0003-third-party-license-policy.md) for the policy
this audit supports.

**Columns:** Compatible-permissive = can this dependency's terms coexist with Nicti shipping under
a pure permissive license (MIT/Apache-2.0)? Compatible-copyleft = same question if Nicti ships under
GPL-3.0/AGPL-3.0 instead. Verdict: ✅ bundle OK · ⚠️ bundle with conditions · ⛔ do not bundle.

**Update this file in the same PR as any new dependency, crate, or ML model.**

## Rust crate dependency tree (as of 2026-09-23)

Checked via `cargo metadata` against the full workspace (`nicti`, `spikes/pawprint`,
`crates/nicti-claw`, `crates/dewclaw`, `bench/whisker`). All resolved crates are permissive:

MIT OR Apache-2.0 (the large majority — anstream, anstyle*, anyhow, arrayvec, base64, bumpalo, cc,
cfg-if, clap*, colorchoice, cpufeatures, find-msvc-tools, futures-*, getrandom, heck,
is_terminal_polyfill, itoa, js-sys, libc, once_cell*, proc-macro2, quote, rustversion, serde*,
shlex, syn, utf8parse, uuid, wasm-bindgen*, windows-*), CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH
LLVM-exception (`blake3`), CC0-1.0 OR MIT-0 OR Apache-2.0 (`constant_time_eq`), Unlicense OR MIT
(`memchr`), MIT (`slab`, `strsim`, `zmij`), (MIT OR Apache-2.0) AND Unicode-3.0 (`unicode-ident` —
the `Unicode-3.0` arm is a data-license for its Unicode table, not a code copyleft), MIT OR
Apache-2.0 OR LGPL-2.1-or-later (`r-efi` — LGPL is only one arm of an OR, permissive arms exist),
**ISC** (`libloading` v0.9.0 — added for [#19](https://github.com/jordanfelle/nicti/issues/19)'s
`sheath` spike, now a real dependency of the production `nicti-claw` crate landed in
[#20](https://github.com/jordanfelle/nicti/issues/20); a short permissive license, OSI-approved
and FSF Free/Libre, functionally MIT-equivalent — added to `deny.toml`'s allowlist in the same
PR)[^s1], **Apache-2.0 WITH LLVM-exception** (`wasmtime` v49.0.0, and one arm of `wat`
v1.259.0's own OR-list — both added for the same spike's WASM-vs-native timing test, still a
dev-dependency only after #20's promotion; already on `deny.toml`'s allowlist via `blake3`'s same
license string)[^s2]. No GPL/AGPL crate is currently in the tree. **No action needed today beyond
the `ISC` addition above** — this section exists so a future `cargo deny check licenses` failure
has a "last known clean" baseline to diff against.

**Update (2026-09-23, [#16](https://github.com/jordanfelle/nicti/issues/16)'s `glint` spike,**
`docs/adr/0005-gpu-compute-api.md`): `wgpu` v30.0.1 and its own dependency tree (`wgpu-core`,
`wgpu-hal`, `wgpu-types`, `wgpu-naga-bridge`, `naga`, `naga-types`, `gpu-allocator`,
`range-alloc`, `profiling`, `renderdoc-sys`, `static_assertions`, `khronos-egl`, `glow`,
`raw-window-handle`, `ordered-float`) are all **MIT OR Apache-2.0** (a few carry a third `Zlib` OR
arm — `bytemuck`, `glow`, `raw-window-handle` — which doesn't matter since the MIT/Apache-2.0 arm
already satisfies `deny.toml`'s allowlist). `ash` (raw Vulkan, pulled in transitively by
`wgpu-hal`'s Vulkan backend, not used directly by `glint`) is also MIT OR Apache-2.0. `pollster`,
`half`, `bytemuck` (spike-only helper crates) are the same. `cudarc` v0.19.9 (the CUDA comparison
harness, compiled via its `fallback-dynamic-loading` feature so it never links against a CUDA
toolkit at build time) is MIT OR Apache-2.0.

**One new `deny.toml` entry was needed, but not caught until CI ran** (a local `cargo deny check`
without `--workspace --all-features` — the exact flags CI's job passes, per its own comment in
`ci.yml` — silently only checks the root `nicti` package's own deps, missing `spikes/*` entirely;
this was a real gap in this PR's own local verification, not a false alarm): `slotmap` v1.1.1
(pulled in transitively via `glow`, which `wgpu-hal`'s GLES backend depends on even though `glint`
never selects that backend directly — `[graph] all-features = true` in `deny.toml` resolves the
full feature-enabled graph regardless) carries **Zlib as its sole license**, no MIT/Apache-2.0
OR-arm the way `glow` itself has. Same for `foldhash` (pulled in via `hashbrown`, itself pulled in
by the `wasmtime`/`cranelift` toolchain already in the tree for the `nicti-claw` crate's
WASM-vs-native dev-dependency test, ADR-0004 — `--workspace` is what surfaces this, since it
unifies the whole workspace's dependency graph, not just this one crate's). Both added `Zlib` to `deny.toml`'s `allow` list, same category
as the existing `ISC` precedent: OSI-approved, FSF Free/Libre, no copyleft terms[^s3].

The one native/runtime component this spike touches, NVRTC, is already covered by the existing
"NVIDIA runtime (CUDA/cuDNN/TensorRT)" row below — `cudarc` dynamically loads
`libnvrtc.so`/`nvrtc64_*.dll` at runtime (never bundled by the spike itself), which is the same
"user-installed prerequisite, detected then used" pattern that row already describes for the CUDA
driver.

**Update (2026-09-23, [#68](https://github.com/jordanfelle/nicti/issues/68)'s `pelt-egui`/
`pelt-iced`/`pelt-slint` spikes, `docs/adr/0006-gui-framework.md`):** each GUI-framework
candidate's own dependency tree pulled in real new licenses beyond what `deny.toml`'s existing
allowlist covered:

- **egui/eframe** (`pelt-egui`): the crate tree itself (`egui`, `eframe`, `egui-wgpu`,
  `egui-winit`, `epaint`, `emath`, `ecolor`) is MIT OR Apache-2.0, already covered. `eframe`'s
  `default_fonts` feature bundles `epaint_default_fonts`, whose license expression is
  `(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0` — the `OFL-1.1`/`Ubuntu-font-1.0` arms
  cover the actual bundled font **data** (not code), both open font licenses explicitly designed
  to permit redistribution/bundling. Added to `deny.toml`'s allowlist[^s4].
- **iced** (`pelt-iced`): `iced`/`iced_wgpu`/`iced_widget`/`iced_core`/`iced_runtime`/
  `iced_graphics`/`iced_winit`/`iced_tiny_skia`/`iced_program`/`iced_renderer`/`iced_futures`/
  `iced_debug` are all MIT, already covered. Pulls in `wgpu` **27.0.1** (not 30 — see ADR-0006's
  gate-2 finding), itself MIT OR Apache-2.0 like ADR-0005's own `wgpu` 30 audit already covered.
- **Slint** (`pelt-slint`): `slint`, `slint-build`, `slint-macros`, and every `i-slint-*` crate
  (`i-slint-core`, `i-slint-core-macros`, `i-slint-common`, `i-slint-compiler`,
  `i-slint-backend-selector`, `i-slint-backend-winit`, `i-slint-renderer-femtovg`,
  `i-slint-renderer-skia`, `i-slint-renderer-software`) carry
  `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0` — none of
  `deny.toml`'s existing allow-list entries, and the `LicenseRef-*` arms are Slint's own
  non-SPDX-registered dual-commercial license texts, not something `cargo-deny` evaluates
  generically[^s5]. Scoped `[[licenses.exceptions]]` blocks were added per Slint-published crate
  name (not a global allow) since this is research-spike-only — **if Slint is ADR-0006's winner,
  shipping it in `nicti-render`/a future `nicti-ui` needs its own ADR-0003 amendment and explicit
  sign-off**, the same standard already applied to LGPL-as-Cargo-dependency crates
  (rawler/lensfun-rs) above; this update does not grant that sign-off.
- **Shared transitive dependency (all three candidates):** each toolkit's `image`-crate-based
  asset loading pulls in `ravif`'s AVIF encoder support, which depends on `rav1e`, which in turn
  depends on `libfuzzer-sys` (its fuzz-target harness) — `libfuzzer-sys`'s own license expression
  is `(MIT OR Apache-2.0) AND NCSA`, so the `NCSA` arm (a permissive, OSI-approved license) needed
  adding. Also common to all three: `clipboard-win`/`error-code` (arboard via egui/Slint,
  `window_clipboard` via iced) carry `BSL-1.0` (Boost Software License), also permissive and
  OSI-approved. Both added to `deny.toml`'s allowlist globally (not GUI-toolkit-specific)[^s6].

No action beyond the `deny.toml` changes above — `cargo deny --workspace --all-features check
licenses` passes clean as of this update.

**Update (2026-09-24, [#28](https://github.com/jordanfelle/nicti/issues/28)'s `sniff` spike):**
new dependencies for embedded-JPEG inventory/decode/benchmark tooling, all already covered by
`deny.toml`'s existing allowlist with no changes needed: `zune-jpeg`/`zune-core` (decode) and
`fast_image_resize` (tier resize) are MIT OR Apache-2.0 OR Zlib; `csv`/`csv-core` are
Unlicense/MIT; `clap`/`clap_builder`/`clap_derive`/`clap_lex`, `rayon`/`rayon-core`, `rand` and
its own tree, `sha2` and its `digest`/`block-buffer`/`crypto-common`/`generic-array` chain,
`thiserror`/`thiserror-impl`, and `windows-sys` (Windows-only, the `FILE_FLAG_NO_BUFFERING`
cold-read path) are all MIT OR Apache-2.0; `tempfile`/`fastrand` (dev-dependency) are
Apache-2.0 OR MIT; `zerocopy` (transitive) is BSD-2-Clause OR Apache-2.0 OR MIT. No LibRaw/rawler
dependency: `sniff` implements its own minimal TIFF/EXIF/Nikon-MakerNote IFD walker (see
`spikes/sniff/src/ifd.rs`) specifically to avoid pulling in #37's still-undecided RAW-decoder
choice and rawler's LGPL-as-Cargo-dependency review this early. `cargo deny --workspace
--all-features check licenses` passes clean as of this update, no `deny.toml` edits required.

**Update (2026-09-24, [#50](https://github.com/jordanfelle/nicti/issues/50)'s `groom` spike):**
new dependencies for the AI-removal scaffolding, all already covered by `deny.toml`'s existing
allowlist with no changes needed: `ort`/`ort-sys` v2.0.0-rc.13 (the ONNX Runtime binding, per
ADR-0004 §3's `load-dynamic` pattern) are MIT OR Apache-2.0; `ndarray`/`matrixmultiply`/
`rawpointer` (pulled in transitively by `ort`'s tensor-value helpers) are MIT OR Apache-2.0 (or
the equivalent pre-SPDX `MIT/Apache-2.0` slash-form). `wgpu` 30 and its own dependency tree (for
the Poisson-Jacobi compute shader) are already covered by ADR-0005's 2026-09-23 update above — no
new licenses there. `cargo deny --workspace --all-features check licenses` passes clean as of
this update, no `deny.toml` edits required.

**Update (2026-09-24, [#67](https://github.com/jordanfelle/nicti/issues/67)'s `den` spike,
`docs/adr/0008-catalog-database-engine.md`):** new dependencies for the catalog-database-engine
comparison. `rusqlite`, `duckdb`, `heed`/`heed-types`/`heed-traits`, and `bincode` (the Rust
binding crates) are all MIT, already covered by the existing allowlist. One new `deny.toml` entry
was needed: `webpki-roots` v1.0.9 carries **CDLA-Permissive-2.0**, pulled in as a *build-time-only*
dependency of `libduckdb-sys`'s build script (`duckdb -> ureq -> webpki-roots`) — never linked
into a shipped binary. Same bundled-permissive-data category as the existing `OFL-1.1`/
`Ubuntu-font-1.0` entries, so added globally rather than scoped[^s7]. The two embedded-Postgres
candidates named in #67's own issue body (`pglite-rs`, `pglite-oxide`) never got this far: both
hard-gate-failed (see the ADR) before either was added as a real dependency, so neither appears in
`deny.toml` or below.

Each engine's **statically-linked native C/C++ core** — invisible to `cargo deny`, which only
resolves the Rust crate graph, not vendored/bundled native source compiled by a crate's own
`build.rs` — is added to the Native libraries table below: SQLite (public domain), DuckDB (MIT,
same as its Rust binding), and LMDB (OpenLDAP Public License 2.8 — note this differs from
`lmdb-master-sys`'s own self-declared `Apache-2.0` Cargo.toml field, another instance of the same
"native code license isn't what `cargo deny` sees" gap this section exists to catch)[^s8].

**Update (2026-09-24, [#102](https://github.com/jordanfelle/nicti/issues/102)'s Turso Database
evaluation, `docs/adr/0009-turso-database-evaluation.md`):** the `turso` crate (v0.8.0-pre.12,
default-off feature, kept for reference after evaluating-not-adopting per the ADR) is MIT,
confirmed from crates.io's version-level API response. Its own dependency tree is large (`tantivy`
full-text search, `roaring` bitmaps, `prost`/protobuf, `aristo`, `bon`, `miette`, its own
`turso_core`/`turso_parser`/`turso_sync_engine`/`turso_sdk_kit` family) but resolves entirely
within `deny.toml`'s existing allowlist — `cargo deny --workspace --all-features check licenses`
passes clean, no `deny.toml` edits required. One gap worth noting, not a blocker: `cfg_block`
v0.1.1 (a transitive dependency of `turso_core`) carries no SPDX `license` field in its own
`Cargo.toml`, which `cargo-deny` only warns on rather than fails — its bundled `LICENSE` file
confirms Apache-2.0[^den4], already an allowed license, so no action needed beyond noting it here
per this file's own "cargo-deny only sees what a crate declares" pattern.

## Native libraries

| Component | Used for | Code license | Data/weights license | Link model | Permissive-compatible? | Copyleft(GPL-3)-compatible? | Verdict |
|---|---|---|---|---|---|---|---|
| [LibRaw](https://github.com/LibRaw/LibRaw/blob/master/LICENSE.LGPL) | RAW decode ([#37](https://github.com/jordanfelle/nicti/issues/37)) | Dual LGPL-2.1 **or** CDDL-1.0 (licensee's choice)[^lr1] | — | Dynamic (DLL) | ✅ if dynamically linked | ✅ | ✅ dynamic link only |
| [LibRaw/LibRaw#826](https://github.com/LibRaw/LibRaw/pull/826) (Nikon HE/HE* PR) | RAW decode research | No license grant of its own; maintainers state it won't be merged, will be replaced by their own decoder[^lr2] | — | n/a | n/a | n/a | ⛔ do not vendor this PR's code directly — no license grant |
| [rawler](https://crates.io/crates/rawler) | RAW decode alt. ([#37](https://github.com/jordanfelle/nicti/issues/37)) | LGPL-2.1[^raw1] | — | Static (Cargo dep — the LGPL/Rust gray area) | ⚠️ needs explicit review | ✅ | ⚠️ static-link-vs-LGPL friction, get sign-off before shipping |
| [lensfun](https://github.com/lensfun/lensfun) — `libs/` | Lens correction ([#39](https://github.com/jordanfelle/nicti/issues/39)) | LGPL-3.0[^lf1] | — | Dynamic (DLL) | ✅ if dynamically linked | ✅ | ✅ dynamic link only; never link `apps/` (GPL-3.0) |
| lensfun **database** (calibration data) | Lens correction | — | CC BY-SA 3.0[^lf1] | Data file, unmodified | ✅ (data obligation, not code) | ✅ | ✅ — share-alike only bites if Nicti *modifies* and redistributes the database |
| [lensfun-rs](https://github.com/vdavid/lensfun-rs) | Rust binding for lensfun | Dual LGPL-3.0-or-later **or** GPL-3.0[^lf2] | — | Static (Cargo dep) | ⚠️ same LGPL/Rust caveat as rawler | ✅ | ⚠️ pick the LGPL-3.0-or-later arm; needs same review as rawler |
| [Little CMS 2](https://github.com/mm2/Little-CMS) | Color management ([#42](https://github.com/jordanfelle/nicti/issues/42)) | MIT[^lcms1] | — | Static or dynamic | ✅ | ✅ | ✅ bundle OK |
| [kamadak-exif](https://crates.io/crates/kamadak-exif) | EXIF read | BSD-2-Clause[^kx1] | — | Static | ✅ | ✅ | ✅ bundle OK (read-only — see #47 below) |
| [little_exif](https://crates.io/crates/little_exif) | EXIF/XMP write | MIT OR Apache-2.0[^le1] | — | Static | ✅ | ✅ | ✅ bundle OK |
| [exiv2](https://github.com/Exiv2/exiv2/blob/main/COPYING) | EXIF/XMP/IPTC (candidate) | GPL-2.0[^ex1] | — | n/a | ⛔ | ✅ (same license) | ⛔ **do not use** — use kamadak-exif + little_exif instead |
| [rexiv2](https://github.com/felixc/rexiv2) | Rust binding to exiv2/gexiv2 (candidate) | GPL-3.0-or-later — the crate's own README carries `SPDX-License-Identifier: GPL-3.0-or-later`, an explicit statement that linking against GPL exiv2/gexiv2 makes the binding itself GPL[^rx1] | — | n/a | ⛔ | ✅ (same license) | ⛔ **do not use**, same root cause as exiv2 |
| [Adobe XMP Toolkit SDK](https://github.com/adobe/XMP-Toolkit-SDK) | XMP (candidate) | BSD-3-Clause[^xmp1] | XMP *specification* separately covered by Adobe's XMP Specification Public Patent License (patent grant, not copyright) | Static or dynamic | ✅ | ✅ | ✅ bundle OK |
| Adobe DNG SDK (code, not DCP/LCP data) | DNG format handling (candidate) | Adobe's own DNG SDK EULA — permits reproduction/redistribution/sublicensing but is a custom EULA, not OSI-approved[^dng1] | — | Static or dynamic | ⚠️ not SPDX-clean; must attribute as a separately-EULA'd third-party component, can't claim MIT/Apache for it | ⚠️ same | ⚠️ usable, but list under its own EULA in third-party notices, not folded into the project's own license |
| Adobe DCP (camera profile) files | Color profiles | Proprietary Adobe/Lightroom data; no redistribution grant found[^dcp1] | — | n/a | ⛔ | ⛔ | ⛔ **never bundle** |
| Adobe LCP (lens profile) files | Lens correction | Proprietary Adobe/Lightroom data; no redistribution grant found (weakest-sourced claim in this audit — treat as prudent inference, re-verify if this becomes a real dependency)[^lcp1] | — | n/a | ⛔ | ⛔ | ⛔ **never bundle** |
| [dcamprof](https://github.com/Beep6581/dcamprof) | Open DCP-alternative profile generator | GPL-3.0[^dc1] | — | External CLI tool (not linked) | ✅ if invoked as a separate process, not linked into Nicti's binary | ✅ | ✅ external-tool use only |
| LibRaw's built-in camera color matrices | Open DCP-alternative (fallback, no extra dependency) | Inherits LibRaw's own LGPL-2.1/CDDL-1.0 (item above) | — | Dynamic (already inside LibRaw) | ✅ | ✅ | ✅ bundle OK — no new license surface |
| [SQLite](https://www.sqlite.org/copyright.html) (bundled via `libsqlite3-sys`) | Catalog DB candidate ([#67](https://github.com/jordanfelle/nicti/issues/67)) | Public domain[^den1] | — | Static (`bundled` feature) | ✅ | ✅ | ✅ bundle OK |
| [DuckDB](https://github.com/duckdb/duckdb/blob/main/LICENSE) core (bundled via `libduckdb-sys`) | Catalog DB candidate ([#67](https://github.com/jordanfelle/nicti/issues/67)) | MIT[^den2] | — | Static (`bundled` feature) | ✅ | ✅ | ✅ bundle OK |
| [LMDB](https://www.openldap.org/software/release/license.html) (bundled via `lmdb-master-sys`) | Catalog DB candidate ([#67](https://github.com/jordanfelle/nicti/issues/67)) | OpenLDAP Public License 2.8[^den3] | — | Static | ✅ (attribution-only, no copyleft) | ✅ | ✅ bundle OK — retain the license text per its own §3 condition |

## ML runtime (ONNX / CUDA / TensorRT)

| Component | License/EULA | Bundle in installer, or user-installed prerequisite? | Notes |
|---|---|---|---|
| [ONNX Runtime](https://github.com/microsoft/onnxruntime/blob/main/LICENSE) | MIT[^ort1] | Bundle freely | `ThirdPartyNotices.txt` lists bundled third-party code (MKL, Eigen, oneDNN, etc.) but carries no NVIDIA/CUDA/TensorRT entries — those EP `.dll`s dynamically link the *separate* NVIDIA runtime libraries below, which carry their own EULAs independent of ORT's MIT license. |
| [`ort` crate](https://github.com/pykeio/ort) | MIT OR Apache-2.0[^ort2] | Bundle freely (crate/binding itself) | Downloads a pyke-provided prebuilt ONNX Runtime binary by default (still MIT-licensed ORT — no separate pyke license attaches). **Unverified:** whether the *default* prebuilt includes a CUDA/TensorRT EP variant vs CPU-only — check the crate's `cuda`/`tensorrt` Cargo features directly before relying on this at build time. |
| NVIDIA CUDA Toolkit/Driver | [NVIDIA CUDA EULA](https://docs.nvidia.com/cuda/eula/index.html)[^cuda1] | **Split** | The GPU **driver is never bundleable** — always a user-installed prerequisite. Specific redistributable libraries (CUDA Runtime, cuFFT, cuBLAS, cuSPARSE, cuSOLVER, cuRAND, NPP, NVRTC) **may** be bundled in Nicti's own installer as object code. Dev tools/compiler are internal-use-only, not redistributable. |
| NVIDIA cuDNN | [cuDNN EULA](https://docs.nvidia.com/deeplearning/cudnn/latest/reference/eula.html)[^cudnn1] | Bundleable, conditionally | Runtime `.dll`s distributable if: (1) Nicti provides "material additional functionality" beyond a thin wrapper, (2) the files are accessible only by Nicti's own process (no exposing as a general shared lib), (3) an NVIDIA attribution notice is included in Nicti's third-party notices. |
| NVIDIA TensorRT | [TensorRT SLA](https://docs.nvidia.com/deeplearning/tensorrt/latest/reference/sla.html)[^trt1] | Bundleable, conditionally | Same three conditions as cuDNN, plus: non-sublicensable (Nicti can't let a plugin SDK re-redistribute these files), and Nicti must notify NVIDIA in writing of any known/suspected non-compliant use it becomes aware of. |

**Packaging implication:** the driver-prerequisite requirement means the on-demand ML-module loader
(per [#4](https://github.com/jordanfelle/nicti/issues/4)'s "heavy modules load on demand") should
check driver presence/version before initializing a CUDA/TensorRT execution provider, with a CPU-EP
fallback if the prerequisite isn't met. Everything else (CUDA runtime libs, cuDNN, TensorRT) can
ship inside Nicti's own installer — no separate "install the CUDA Toolkit" step required for end
users, as long as the cuDNN/TensorRT isolation conditions above are honored.

## ML model weights

| Model | Used for | Code license | Weights license | Training data / provenance flag | Verdict |
|---|---|---|---|---|---|
| [BiRefNet](https://github.com/ZhengPeng7/BiRefNet/blob/main/LICENSE) | Masking ([#48](https://github.com/jordanfelle/nicti/issues/48)) | MIT[^m1] | MIT[^m1] | DIS-TR — no stated restriction | ✅ bundle OK |
| [MobileSAM](https://github.com/ChaoningZhang/MobileSAM/blob/master/LICENSE) | Masking ([#48](https://github.com/jordanfelle/nicti/issues/48)), AI-removal source selector ([#50](https://github.com/jordanfelle/nicti/issues/50)) | Apache-2.0[^m2] | Apache-2.0 (inherits SAM lineage) | Distilled from SAM/SA-1B | ✅ bundle OK |
| [SAM / SAM2 (Meta)](https://github.com/facebookresearch/sam2/blob/main/README.md) | Masking ([#48](https://github.com/jordanfelle/nicti/issues/48)) | Apache-2.0[^m3] | Apache-2.0 (SAM2 confirmed on GitHub README + HF card)[^m3] | SA-1B / SA-V; SA-V dataset-specific terms (`sav_dataset/README.md`) not independently re-checked | ⚠️ bundle OK on code/weights license, but re-check the SA-V dataset terms directly (same caveat class as LaMa/NAFNet below) before treating provenance as fully cleared |
| [LaMa](https://github.com/advimman/lama/blob/main/LICENSE) | Healing/removal ([#50](https://github.com/jordanfelle/nicti/issues/50)/[#51](https://github.com/jordanfelle/nicti/issues/51)) | Apache-2.0[^m4] | Not separately stated, presumed Apache-2.0 | `big-lama` checkpoint trained on **Places2** — re-checked 2026-09-24 ([#50](https://github.com/jordanfelle/nicti/issues/50)'s spike): the primary `places2.csail.mit.edu` domain again returned a connection error on a direct fetch, but a search-indexed mirror of `download-private.html` states plainly "you will use the data only for non-commercial research and educational purposes and will NOT distribute the images" — whether that restriction shadows a model merely *trained on* the data is still a live, unsettled question, not asserted as a legal conclusion here; see `docs/research/groom-healing-removal.md` | ⚠️ flag — still unverified via a direct primary-source fetch; re-verify Places2 terms directly, or find/train a checkpoint on non-Places2 data, before shipping |
| [MI-GAN](https://github.com/Picsart-AI-Research/MI-GAN) | Healing/removal candidate, researched as a LaMa alternative ([#50](https://github.com/jordanfelle/nicti/issues/50)) | MIT[^m11] | A separate `LICENSE-WEIGHTS` file, fetched directly, is itself written as MIT. A now-**closed** GitHub issue (`#25`) asked whether that grant is legitimate, since the training pipeline distills from a Co-Mod-GAN teacher licensed NVIDIA Source Code License-NC (§3.2 requires its non-commercial term to carry over to derivatives); the maintainer confirmed the MIT grant on the weights but explicitly declined the deeper legitimacy question, telling the asker to consult a lawyer[^m11] | Places2 **and** FFHQ — same Places2 exposure as LaMa above, no improvement | ⚠️ **not a clean alternative to LaMa** — same Places2 exposure, plus an explicitly maintainer-punted (not silently unanswered) provenance question over the weights license; do not adopt as a LaMa workaround without resolving that question first |
| [NAFNet](https://github.com/megvii-research/NAFNet/blob/main/LICENSE) | Denoise ([#40](https://github.com/jordanfelle/nicti/issues/40)) | MIT + Apache-2.0 (bundled BasicSR)[^m5] | Not separately stated | Training-dataset list (SIDD/GoPro/REDS) unverified this pass — README fetch failed | ✅ bundle OK (license); re-verify training-data provenance before shipping a specific checkpoint |
| [DINOv2](https://github.com/facebookresearch/dinov2/blob/main/LICENSE) (standard ViT-S/B/L/g) | Face/subject grouping ([#35](https://github.com/jordanfelle/nicti/issues/35)) | Apache-2.0[^m6] | Apache-2.0 | LVD-142M, self-supervised, provenance of source corpus not disclosed by Meta | ✅ bundle OK — **do not** substitute the XRay-DINO/Cell-DINO variants (FAIR Noncommercial Research License) |
| [CLIP (OpenAI)](https://github.com/openai/CLIP/blob/main/LICENSE) | Subject/burst grouping candidate ([#33](https://github.com/jordanfelle/nicti/issues/33)/[#35](https://github.com/jordanfelle/nicti/issues/35)) | MIT[^m7] | MIT (no separate weight license file) | OpenAI's own model card explicitly discourages *any* deployed use case, commercial or not — not a legal restriction, but a stated rights-holder position | ⚠️ flag — legally bundle-OK, but document the risk acknowledgment if shipped in a real feature |
| [OpenCLIP](https://github.com/mlfoundations/open_clip/blob/main/LICENSE) | Subject/burst grouping candidate | MIT[^m8] | Not separately stated, presumed MIT-equivalent | Varies by checkpoint (LAION-400M/2B, DataComp-1B, etc.) — no license restriction, but LAION checkpoints carry reputational/takedown history | ✅ bundle OK — prefer a non-LAION-5B/2B checkpoint if provenance matters |
| [InsightFace / RetinaFace](https://github.com/deepinsight/insightface/blob/master/README.md) | Face detection candidate | MIT (source code)[^m9] | **Non-commercial research only**, per the maintainers' own README, explicitly covering "models trained with this data" | Training data itself restricted; maintainers state the restriction carries to weights | ⛔ **do not bundle** — negotiate a commercial license or use a different embedding model (DINOv2/OpenCLIP) |
| [Ultralytics YOLO](https://github.com/ultralytics/ultralytics/blob/main/LICENSE) | Culling/detection candidate | **AGPL-3.0**[^m10] | Same AGPL-3.0 (weights bundled under the same terms; commercial license sold separately) | COCO/Ultralytics-curated — not the issue | ⛔ **do not bundle under a permissive release** — AGPL's network-use clause would force the whole combined work AGPL, or requires Ultralytics' paid commercial license, or swap to a non-Ultralytics/non-AGPL detector |

## Flags requiring a decision before shipping a real feature

1. **exiv2 / rexiv2 (GPL)** — excluded outright; kamadak-exif + little_exif already cover the same
   ground under permissive terms. No decision needed, just don't add exiv2/rexiv2 as a dependency.
2. **rawler / lensfun-rs (LGPL + Rust static linking)** — Rust's typical static-link compilation
   model is a known unresolved friction point for LGPL. Get explicit sign-off (or isolate behind a
   `cdylib`/plugin boundary, per Claw's [#19](https://github.com/jordanfelle/nicti/issues/19)
   module architecture) before shipping either as a linked-in dependency.
3. **Adobe DCP/LCP data** — never bundle. Use `dcamprof` (external CLI, GPL-3.0 but not linked in)
   or LibRaw's built-in color matrices instead.
4. **InsightFace/RetinaFace** — non-commercial only, excluded. Use DINOv2 or OpenCLIP embeddings
   for face/subject grouping (#35) instead of literal face-recognition models — this also better
   serves the "must handle fursuiters, not just human faces" requirement from #4.
5. **Ultralytics YOLO (AGPL-3.0)** — excluded from a permissive release. If a YOLO-family detector
   is still wanted for culling, use a non-Ultralytics implementation/weights not wrapped in the
   AGPL license, or budget for Ultralytics' commercial license.
6. **LaMa (Places2 provenance)** — the least clear-cut case, still unresolved after a second
   direct-fetch attempt in [#50](https://github.com/jordanfelle/nicti/issues/50)'s spike
   (`places2.csail.mit.edu` again returned a connection error; a mirror confirms Places2's own
   non-commercial/no-redistribution terms, but whether that shadows a model trained on it remains
   unsettled). **MI-GAN was researched as an alternative and found not to be cleaner** — same
   Places2 exposure, plus its own unresolved weights-license-legitimacy question (see the new
   MI-GAN row above and `docs/research/groom-healing-removal.md`). Re-verify Places2's terms
   directly via a working primary-source fetch, widen the alternative-model search beyond MI-GAN,
   or find/train an alternative checkpoint on non-Places2-provenance data before
   shipping healing/removal (#50/#51).
7. **CLIP (OpenAI)** — legally clean, but OpenAI's own model card explicitly discourages any
   deployed use. Document the acknowledgment if used in a shipped feature rather than treating it
   as fully risk-free.
8. **cuDNN/TensorRT bundling conditions** — Nicti's installer must keep these DLLs private to its
   own process (no general shared-lib exposure) and include the NVIDIA attribution notice text.

## Footnotes

[^lr1]: LibRaw dual license — https://github.com/LibRaw/LibRaw/blob/master/LICENSE.LGPL and repo README's dual LGPL-2.1/CDDL-1.0 statement — verified 2026-09-23
[^lr2]: LibRaw/LibRaw#826 PR thread — https://github.com/LibRaw/LibRaw/pull/826 — verified 2026-09-23
[^raw1]: rawler license field — https://crates.io/api/v1/crates/rawler (registry JSON) — verified 2026-09-23
[^lf1]: lensfun code (`libs/` LGPL-3.0, `apps/` GPL-3.0) and database (CC BY-SA 3.0) — https://github.com/lensfun/lensfun README licensing section — verified 2026-09-23
[^lf2]: lensfun-rs dual license — https://github.com/vdavid/lensfun-rs (README + LICENSE-LGPL-3.0/LICENSE-GPL-3.0) — verified 2026-09-23
[^lcms1]: Little CMS 2 MIT license — GitHub license API resolving the repo's own `LICENSE` file at https://github.com/mm2/Little-CMS/blob/master/LICENSE — verified 2026-09-23
[^kx1]: kamadak-exif BSD-2-Clause — https://crates.io/api/v1/crates/kamadak-exif (registry JSON) — verified 2026-09-23
[^le1]: little_exif MIT OR Apache-2.0 — https://crates.io/api/v1/crates/little_exif (registry JSON) — verified 2026-09-23
[^ex1]: exiv2 GPL-2.0 — https://github.com/Exiv2/exiv2/blob/main/COPYING — verified 2026-09-23
[^rx1]: rexiv2 GPL-3.0-or-later — `SPDX-License-Identifier: GPL-3.0-or-later` header at the top of https://github.com/felixc/rexiv2/blob/main/README.md — verified 2026-09-23
[^xmp1]: Adobe XMP Toolkit SDK BSD-3-Clause — GitHub license API for https://github.com/adobe/XMP-Toolkit-SDK — verified 2026-09-23
[^dng1]: Adobe DNG SDK EULA — https://scancode-licensedb.aboutcode.org/adobe-dng-sdk.html (quotes the EULA text) — verified 2026-09-23
[^dcp1]: No Adobe DCP redistribution grant found — absence of a redistribution grant in Adobe's DNG SDK EULA and general Adobe product-EULA norms; **not a single definitive primary-source sentence naming DCP files specifically** — treated conservatively, re-verify if this becomes a real dependency
[^lcp1]: No Adobe LCP redistribution grant found — same caveat as [^dcp1], weakest-sourced claim in this audit; third-party (Sigma) LCP manual copyright language referenced but not an Adobe primary source
[^dc1]: dcamprof GPL-3.0 — GitHub license API for https://github.com/Beep6581/dcamprof — verified 2026-09-23
[^ort1]: ONNX Runtime MIT — https://github.com/microsoft/onnxruntime/blob/main/LICENSE and ThirdPartyNotices.txt — verified 2026-09-23
[^ort2]: `ort` crate dual MIT/Apache-2.0 — https://github.com/pykeio/ort (LICENSE-MIT, LICENSE-APACHE) — verified 2026-09-23
[^cuda1]: NVIDIA CUDA EULA — https://docs.nvidia.com/cuda/eula/index.html — verified 2026-09-23
[^cudnn1]: NVIDIA cuDNN EULA — https://docs.nvidia.com/deeplearning/cudnn/latest/reference/eula.html — verified 2026-09-23
[^trt1]: NVIDIA TensorRT SLA — https://docs.nvidia.com/deeplearning/tensorrt/latest/reference/sla.html — verified 2026-09-23
[^m1]: BiRefNet MIT — https://github.com/ZhengPeng7/BiRefNet/blob/main/LICENSE and https://huggingface.co/ZhengPeng7/BiRefNet — verified 2026-09-23
[^m2]: MobileSAM Apache-2.0 — https://github.com/ChaoningZhang/MobileSAM/blob/master/LICENSE — verified 2026-09-23
[^m3]: SAM/SAM2 Apache-2.0 — https://github.com/facebookresearch/segment-anything/blob/main/LICENSE and https://github.com/facebookresearch/sam2/blob/main/README.md and https://huggingface.co/facebook/sam2-hiera-large — verified 2026-09-23
[^m4]: LaMa Apache-2.0 — https://github.com/advimman/lama/blob/main/LICENSE — verified 2026-09-23; Places2 dataset terms via secondary source (primary page unreachable this session; re-attempted and still unreachable 2026-09-24 during [#50](https://github.com/jordanfelle/nicti/issues/50)'s spike, see `docs/research/groom-healing-removal.md`)
[^m5]: NAFNet MIT + Apache-2.0 — https://github.com/megvii-research/NAFNet/blob/main/LICENSE — verified 2026-09-23
[^m6]: DINOv2 Apache-2.0 (standard checkpoints) — https://github.com/facebookresearch/dinov2/blob/main/LICENSE and README — verified 2026-09-23
[^m7]: CLIP MIT + model-card deployment caveat — https://github.com/openai/CLIP/blob/main/LICENSE and https://raw.githubusercontent.com/openai/CLIP/main/model-card.md — verified 2026-09-23
[^m8]: OpenCLIP MIT — https://github.com/mlfoundations/open_clip/blob/main/LICENSE — verified 2026-09-23
[^m9]: InsightFace non-commercial restriction — https://github.com/deepinsight/insightface/blob/master/README.md — verified 2026-09-23
[^m10]: Ultralytics YOLO AGPL-3.0 — https://github.com/ultralytics/ultralytics/blob/main/LICENSE — verified 2026-09-23
[^m11]: MI-GAN code MIT and `LICENSE-WEIGHTS` (itself MIT-style) — https://github.com/Picsart-AI-Research/MI-GAN and https://raw.githubusercontent.com/Picsart-AI-Research/MI-GAN/main/LICENSE-WEIGHTS — verified 2026-09-24. https://github.com/Picsart-AI-Research/MI-GAN/issues/25 is **closed** (closed 2026-09-14, before this pass): the maintainer (`AndranikSargsyan`) confirmed the weights are released under the same MIT license, and added the `LICENSE-WEIGHTS` file in direct response to this issue — but explicitly declined the harder question of whether that grant is legitimate given the Co-Mod-GAN teacher's NVIDIA Source Code License-NC, telling the asker "I am not qualified to answer that question... I recommend consulting a lawyer." So the license-grant question is answered; only the deeper legitimacy question was left open, by an explicit punt rather than silence. Places2+FFHQ training-data statement — the repository's own README — verified 2026-09-24, **best-available-secondary** for the exact NVIDIA license clause text (relayed via the GitHub issue's own summary, not independently re-fetched verbatim from NVIDIA's license text in this pass).
[^s1]: `libloading` v0.9.0 ISC license — `cargo metadata`'s resolved `license` field against this crate's own `Cargo.toml`, cross-checked against https://docs.rs/libloading/latest/libloading/ — verified 2026-09-23
[^s2]: `wasmtime` v49.0.0 and `wat` v1.259.0 license fields — `cargo metadata`'s resolved `license` field, cross-checked against https://github.com/bytecodealliance/wasmtime (repo-wide Apache-2.0 WITH LLVM-exception, standard for Bytecode Alliance projects) — verified 2026-09-23
[^s3]: `slotmap` v1.1.1 and `foldhash` v0.2.0 Zlib licenses — `cargo metadata`'s resolved `license` field via `cargo deny --workspace --all-features check licenses` against each crate's own `Cargo.toml`, cross-checked against https://crates.io/crates/slotmap and https://crates.io/crates/foldhash — verified 2026-09-24
[^s4]: `epaint_default_fonts` v0.36.2 license expression — `cargo deny --workspace --all-features check licenses` against its own `Cargo.toml` — verified 2026-09-23
[^s5]: Slint crate family license expression — `cargo deny --workspace --all-features check licenses` against `slint`/`slint-build`/`slint-macros`/every `i-slint-*` crate's own `Cargo.toml` (all identical), cross-checked against https://github.com/slint-ui/slint/blob/master/LICENSES — verified 2026-09-23
[^s6]: `libfuzzer-sys` v0.4.13 (`(MIT OR Apache-2.0) AND NCSA`) and `clipboard-win`/`error-code` (`BSL-1.0`) — `cargo deny --workspace --all-features check licenses` against each crate's own `Cargo.toml` — verified 2026-09-23
[^s7]: `webpki-roots` v1.0.9 `CDLA-Permissive-2.0` — `cargo deny --workspace --all-features check licenses` against its own `Cargo.toml`, cross-checked against https://crates.io/crates/webpki-roots — verified 2026-09-24
[^den1]: SQLite public-domain dedication — https://www.sqlite.org/copyright.html, and the "public domain" notice embedded directly in `libsqlite3-sys`'s bundled `sqlite3.c` — verified 2026-09-24
[^den2]: DuckDB core MIT license — https://github.com/duckdb/duckdb/blob/main/LICENSE, matching `libduckdb-sys`'s own bundled `LICENSE` file — verified 2026-09-24
[^den3]: LMDB (`liblmdb`) OpenLDAP Public License 2.8 — the `LICENSE`/`COPYRIGHT` files bundled inside `lmdb-master-sys`'s vendored `lmdb/libraries/liblmdb/` source, cross-checked against https://www.openldap.org/software/release/license.html; note this is the *bundled C source's* license, distinct from (and not accurately reflected by) `lmdb-master-sys`'s own self-declared `Apache-2.0` Cargo.toml field — verified 2026-09-24
[^den4]: `cfg_block` v0.1.1 Apache-2.0 — its own bundled `LICENSE` file at `~/.cargo/registry/src/.../cfg_block-0.1.1/LICENSE`, since its `Cargo.toml` carries no SPDX `license` field for `cargo-deny` to read directly — verified 2026-09-24
