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

**Update (2026-09-24, [#106](https://github.com/jordanfelle/nicti/issues/106)'s `redb` evaluation,
`docs/adr/0010-redb-evaluation.md`):** the `redb` crate (v4.3.0, default-off feature, kept for
reference after evaluating-not-adopting per the ADR) is `MIT OR Apache-2.0`, confirmed from
crates.io's version-level API response — already on `deny.toml`'s allowlist, no edit needed.
`redb` has **zero dependencies of its own** (confirmed via `cargo tree -i redb`), so this update
adds nothing new to the resolved dependency graph beyond the crate itself — `cargo deny
--workspace --all-features check licenses` passes clean, same pre-existing `cfg_block`
(Turso-only) warning as before, nothing new from `redb`.

**Update (2026-09-24, [#115](https://github.com/jordanfelle/nicti/issues/115)'s RocksDB
evaluation, `docs/adr/0015-rocksdb-evaluation.md`):** the `rocksdb` crate (v0.25.0, default-off
feature) is **`Apache-2.0` only**, confirmed from its own `Cargo.toml`'s `license` field and its
repository's top-level `LICENSE` file (`rust-rocksdb/rust-rocksdb`) — already on `deny.toml`'s
allowlist, no edit needed. Its `librocksdb-sys` companion crate (the FFI/build-script layer, not
the vendored native library) is `MIT/Apache-2.0/BSD-3-Clause`, also already allowed. **The vendored
native RocksDB C++ core itself (a git submodule of facebook/rocksdb, invisible to `cargo deny` —
same "native code isn't in the Rust crate graph" gap this file's Native libraries section exists
to catch) is dual-licensed `Apache-2.0` OR `GPL-2.0-only`**, confirmed by reading both license
files directly from `facebook/rocksdb`'s own repository root (`LICENSE.Apache` = Apache License
2.0 text; `COPYING` = GNU GPL v2 text, not a summary or a third-party claim) — see the Native
libraries table below. **The Apache-2.0 arm is genuinely selectable, not merely present as an
unused alternative**: a dual `X OR Y` license lets the recipient choose either arm unilaterally,
and nothing in this crate's build (`librocksdb-sys`'s `build.rs`) requires accepting GPL-2.0 terms
— the Rust binding crate that everything in Nicti actually depends on and links against is
Apache-2.0 *only* (no GPL arm at all), so there is no GPL-2.0 exposure to elect out of even before
reaching the dual-licensed native core. `cargo deny --workspace --all-features check licenses`
passes clean with this feature enabled — same pre-existing `cfg_block` warning as before, nothing
new from `rocksdb` (its own dependency tree — `libc`, `bindgen`, `cc`, the optional compression
codec `-sys` crates — resolves entirely within the existing allowlist).

**Update (2026-09-24, [#66](https://github.com/jordanfelle/nicti/issues/66)/
[ADR-0013](adr/0013-outbound-license-agpl.md)): Nicti's outbound license is decided — AGPL-3.0-
or-later.** This is the biggest change to this file since it was created, since most of the
copyleft-related exclusions below existed *because* the outbound license was undecided. Concretely,
this update:

- Adds `GPL-3.0-only`, `GPL-3.0-or-later`, `AGPL-3.0-only`, `AGPL-3.0-or-later`, and
  `GPL-2.0-or-later` to `deny.toml`'s allowlist (see ADR-0003's 2026-09-24 amendment for the full
  reasoning). **`GPL-2.0-only` deliberately stays denied** — no "or later" arm means no combining
  into Nicti's own GPL-3.0-family license; this is a real per-crate license-text check, not a
  blanket "any GPL now passes" change.
- Un-excludes **Ultralytics YOLO** (AGPL-3.0) and **exiv2/rexiv2** (GPL-2.0-or-later/
  GPL-3.0-or-later respectively — both grants verified precisely, not assumed from a short label)
  — see their updated rows below and the Flags section.
- **Removes** the special sign-off/`cdylib`-isolation requirement for **LGPL-as-Cargo-dependency**
  — resolved for **`lensfun-rs`** (its `LGPL-3.0-or-later OR GPL-3.0` dual license has a confirmed
  or-later arm) and, **corrected 2026-09-25 by #37**, for **`rawler`/LibRaw too**: LGPL-2.1 §§5-6
  permit the combination directly regardless of the "-or-later" question, no isolation/sign-off
  needed for either — see the Flags section below for the full reasoning and the real remaining
  distribution-mechanics checklist (not a compatibility question) before shipping.
- **Unaffected**: Adobe DCP/LCP (no redistribution grant exists, not a copyleft question),
  InsightFace/RetinaFace (non-commercial-only restriction, not a copyleft question), LaMa's Places2
  flag (training-data provenance, not a license-family question), CLIP's model-card caveat (a
  stated position, not a license restriction).

**Update (2026-09-24, [#113](https://github.com/jordanfelle/nicti/issues/113)'s `libSQL`
evaluation, `docs/adr/0014-libsql-evaluation.md`):** the `libsql` crate (v0.9.30, default-off
feature, kept for reference after evaluating-not-adopting-for-v1 per the ADR) is MIT, confirmed
independently from both crates.io's version-level API response (every version checked, including
the newest `0.10.0-pre.4`) and the `tursodatabase/libsql` GitHub repo's own `license` API field —
already on `deny.toml`'s allowlist, no edit needed. Unlike `redb` (zero dependencies) or Turso
(large but fully within-allowlist), `libsql`'s own **default** features (`core`, `replication`,
`remote`, `sync`, `tls` — used as-is here, not trimmed down, since that's what a real caller would
depend on) pull in a genuinely large dependency subtree (`tonic`, `tower`, `hyper`, `h2`, `prost`,
`rustls` and its ecosystem, `libsql_replication`, `libsql-hrana`, among others — `cargo tree -e
normal -p den --features libsql` resolves 433 lines vs. plain `sqlite`'s 85). Despite the size,
`cargo deny --workspace --all-features check licenses` passes clean with no new `deny.toml` entry
needed — the entire subtree resolves within the existing allowlist, same pre-existing `cfg_block`
(Turso-only) warning as every prior ADR in this series, nothing new from `libsql`.

**Update (2026-09-24, [#116](https://github.com/jordanfelle/nicti/issues/116)'s `fjall` evaluation,
`docs/adr/0016-fjall-evaluation.md`):** the `fjall` crate (v3.1.10, default-off feature, kept for
reference after evaluating-not-adopting per the ADR) is `MIT OR Apache-2.0`, confirmed directly
from crates.io's version-level API response and cross-checked against the bundled `LICENSE-MIT`/
`LICENSE-APACHE` files in the downloaded crate source. Its own dependency (`lsm-tree`, the
underlying LSM-tree implementation fjall wraps) is also `MIT OR Apache-2.0` (checked directly in
its own `Cargo.toml`, not assumed from fjall's). Unlike SQLite/DuckDB/LMDB, fjall has **no bundled
native C/C++ core at all** — 100% safe Rust (the crate itself carries `#![deny(unsafe_code)]`) —
so this update adds no new row to the Native libraries table below. One `deny.toml` edit **was**
needed, unlike redb/Turso's zero-edit updates: `varint-rs` (a transitive dependency of `lsm-tree`)
carries `0BSD`, not previously on the allowlist — added as its own entry (OSI-approved, even more
permissive than MIT/Apache-2.0, no attribution requirement) rather than assumed compatible.
`cargo deny --workspace --all-features check licenses` now passes clean, same pre-existing
`cfg_block` (Turso-only) warning as before, nothing else new from `fjall`.

**Update (2026-09-25, [#29](https://github.com/jordanfelle/nicti/issues/29)'s preview-tier-strategy
spike, `docs/adr/0017-preview-tier-strategy.md`):** new dependencies for `spikes/sniff`'s cache-
format and tier-payload-format (JPEG vs AVIF, added at the user's request rather than decided from
priors) comparison. `rusqlite` v0.40.2 (`bundled` feature) and `libsqlite3-sys` v0.38.2 are both
MIT — same crate/license `spikes/den`'s 2026-09-24 update already covers, reused here for the
`previews.db`/pack-index candidates, no new review needed. `jpeg-encoder` v0.6.1 is
`(MIT OR Apache-2.0) AND IJG` — the `IJG` arm (Independent JPEG Group License, the crate's
quantization-table code traces back to IJG's reference implementation) needed adding to
`deny.toml`, permissive/FSF-Free with no copyleft terms, same category as ISC/Zlib/NCSA/BSL-1.0/
0BSD already on the allowlist.

AVIF candidates, both pure Rust (no C-toolchain dependency, unlike `image`'s `avif-native`/`dav1d`
feature): `ravif` v0.13.0 (encode, BSD-3-Clause) and its own tree — `rav1e` v0.8.1 (BSD-2-Clause),
`av1-grain` v0.2.5 (BSD-2-Clause), `av-scenechange` v0.14.1 (MIT), `avif-serialize` v0.8.9
(BSD-3-Clause), `y4m`/`bitstream-io`/`nasm-rs` (MIT or MIT/Apache-2.0) — all already covered by the
existing BSD-2-Clause/BSD-3-Clause/MIT/Apache-2.0 allowlist entries, no edit needed. `avif-decode`
v3.0.0 (decode, BSD-3-Clause, `rav1d`-based — `rav1d` v1.1.0 itself is BSD-2-Clause, the
`memorysafety` project's Rust port of the reference `dav1d` decoder) needed one real addition:
`avif-parse` v2.1.0 (the AVIF-container/ISOBMFF demuxer `avif-decode` depends on) carries
**MPL-2.0** — file-level weak copyleft, OSI-approved, FSF Free/Libre — added to `deny.toml`'s
allowlist alongside the GPL/AGPL family already there, on the same combines-cleanly-into-
Nicti's-own-AGPL-3.0-or-later reasoning as those entries, not a permissive-only case. `rgb` v0.8.53
(MIT) and `imgref` v1.12.3 (CC0-1.0 OR Apache-2.0) are the pixel-buffer types `ravif`/`avif-decode`
share — both already covered.

Building `ravif`/`rav1e`/`rav1d` requires `nasm` (their optimized SIMD asm paths) — installed via
`brew install nasm` on this dev machine; **`.github/workflows/ci.yml`'s four general
clippy/test jobs (Linux and Windows) now install it too** (`apt-get install -y nasm` /
`choco install nasm -y`, same job `sniff` already runs in via the plain `--workspace` sweep, not a
path-gated job of its own like `den`/`pelt-*`). `cargo deny --workspace --all-features check
licenses` passes clean with the two new `deny.toml` entries (`MPL-2.0`, `IJG`) above.

## Native libraries

| Component | Used for | Code license | Data/weights license | Link model | Permissive-compatible? | Copyleft(GPL-3)-compatible? | Verdict |
|---|---|---|---|---|---|---|---|
| [LibRaw](https://github.com/LibRaw/LibRaw/blob/master/LICENSE.LGPL) | RAW decode ([#37](https://github.com/jordanfelle/nicti/issues/37)) | Dual LGPL-2.1 **or** CDDL-1.0 (licensee's choice)[^lr1] | — | Static (spike; `cc`-compiled into `spikes/retina`, see #37) | ✅ **resolved 2026-09-25** — LGPL-2.1 §§5–6 permit combining an LGPL-2.1 library into a differently-licensed larger work (here, AGPL-3.0-or-later) without any relicensing; §6(a)'s full requirement (source for the *whole combined executable*, not just the library, so a user can relink) is satisfied structurally by Nicti already being AGPL-3.0-or-later open source, not by vendoring alone — see Flags §2 | ✅ | ✅ give §6's prominent notice + include the LGPL license text for LibRaw in the shipped product (a real checklist item, not a legal question) before shipping in `nicti-decode` |
| [LibRaw/LibRaw#826](https://github.com/LibRaw/LibRaw/pull/826) (Nikon HE/HE* PR, vendored as `yogthos/LibRaw@nikon-he-decoder` in `spikes/retina/vendor/LibRaw`) | RAW decode research + spike | **Corrected 2026-09-25** (superseding the "no license grant" line below -- checked the actual submodule source this time, not just the PR thread): every new `nikon_he/*`/`nikon_he_decoder.cpp` file carries LibRaw's own standard dual LGPL-2.1/CDDL-1.0 header, copyright Dmitri Sotnikov[^lr3] -- it inherits LibRaw's own license, not an unlicensed contribution. ~~No license grant of its own; maintainers state it won't be merged, will be replaced by their own decoder~~ (the "won't be merged" part is still true and is why this is pinned to a fork, not upstream) | — | Static (spike) | ✅ same resolution as the LibRaw row above | ✅ | ⚠️ spike-only pending LibRaw's own official HE snapshot (ADR-0001) — separately: a hostile PR review found and this project patched at build time a real out-of-bounds read in this pinned commit's tone-curve table builder (`nikon_he_iqx_iqp_lut_data.h`), see `spikes/retina/build.rs`'s own `PATCHES` constant |
| [rawler](https://crates.io/crates/rawler) | RAW decode alt. ([#37](https://github.com/jordanfelle/nicti/issues/37)) | LGPL-2.1[^raw1] | — | Static (Cargo dep — the LGPL/Rust gray area) | ✅ **resolved 2026-09-25**, same LGPL-2.1 §§5–6 reasoning as the LibRaw row above | ✅ | ✅ same §6 notice/license-text checklist item as the LibRaw row before shipping in `nicti-decode`; the `deny.toml` exception (named for `rawler`, not path-scoped — see its own comment) stays a per-crate exception rather than a global `LGPL-2.1` allow entry, since cargo-deny can verify license compatibility but not §6's administrative notice requirement |
| [lensfun](https://github.com/lensfun/lensfun) — `libs/` | Lens correction ([#39](https://github.com/jordanfelle/nicti/issues/39)) | LGPL-3.0[^lf1] | — | Dynamic (DLL) | ✅ if dynamically linked | ✅ | ✅ dynamic link only; never link `apps/` (GPL-3.0) |
| lensfun **database** (calibration data) | Lens correction | — | CC BY-SA 3.0[^lf1] | Data file, unmodified | ✅ (data obligation, not code) | ✅ | ✅ — share-alike only bites if Nicti *modifies* and redistributes the database |
| [lensfun-rs](https://github.com/vdavid/lensfun-rs) | Rust binding for lensfun | Dual LGPL-3.0-or-later **or** GPL-3.0[^lf2] | — | Static (Cargo dep) | ✅ **resolved 2026-09-24** — pick the LGPL-3.0-or-later arm; Nicti's own outbound license is now AGPL-3.0-or-later (#66/ADR-0013), and this arm's confirmed "or later" grant combines cleanly | ✅ | ✅ no isolation/sign-off needed — same conclusion `rawler`/LibRaw reached too, **corrected 2026-09-25 by #37** (see Flags §2): LGPL-2.1 §§5-6 permit this regardless of an "-or-later" grant |
| [Little CMS 2](https://github.com/mm2/Little-CMS) | Color management ([#42](https://github.com/jordanfelle/nicti/issues/42)) | MIT[^lcms1] | — | Static or dynamic | ✅ | ✅ | ✅ bundle OK |
| [kamadak-exif](https://crates.io/crates/kamadak-exif) | EXIF read | BSD-2-Clause[^kx1] | — | Static | ✅ | ✅ | ✅ bundle OK (read-only — see #47 below) |
| [little_exif](https://crates.io/crates/little_exif) | EXIF/XMP write | MIT OR Apache-2.0[^le1] | — | Static | ✅ | ✅ | ✅ bundle OK |
| [exiv2](https://github.com/Exiv2/exiv2/blob/main/src/exif.cpp) | EXIF/XMP/IPTC (candidate) | **GPL-2.0-or-later** (precise grant confirmed 2026-09-24 via project-specific evidence — `SPDX-License-Identifier: GPL-2.0-or-later` headers in its own source files, e.g. `src/exif.cpp`/`src/image.cpp`, and its README's License section; note the bundled `COPYING` file is just the generic FSF LGPL/GPL template distributed unedited and isn't project-specific evidence on its own, don't cite it as the source for this)[^ex1] | — | n/a | ⛔ | ✅ (the "-or-later" arm combines into Nicti's own AGPL-3.0-or-later) | ✅ **usable as of 2026-09-24** — Nicti's outbound license is now AGPL-3.0-or-later (#66/ADR-0013); kamadak-exif + little_exif remain the current choice (nothing wrong with them, no forced switch), but exiv2 is no longer license-excluded if a real reason to prefer it comes up |
| [rexiv2](https://github.com/felixc/rexiv2) | Rust binding to exiv2/gexiv2 (candidate) | GPL-3.0-or-later — the crate's own README carries `SPDX-License-Identifier: GPL-3.0-or-later`, an explicit statement that linking against GPL exiv2/gexiv2 makes the binding itself GPL[^rx1] | — | n/a | ⛔ | ✅ (same license) | ✅ **usable as of 2026-09-24**, same reasoning as exiv2 above |
| [Adobe XMP Toolkit SDK](https://github.com/adobe/XMP-Toolkit-SDK) | XMP (candidate) | BSD-3-Clause[^xmp1] | XMP *specification* separately covered by Adobe's XMP Specification Public Patent License (patent grant, not copyright) | Static or dynamic | ✅ | ✅ | ✅ bundle OK |
| Adobe DNG SDK (code, not DCP/LCP data) | DNG format handling (candidate) | Adobe's own DNG SDK EULA — permits reproduction/redistribution/sublicensing but is a custom EULA, not OSI-approved[^dng1] | — | Static or dynamic | ⚠️ not SPDX-clean; must attribute as a separately-EULA'd third-party component, can't claim MIT/Apache for it | ⚠️ same | ⚠️ usable, but list under its own EULA in third-party notices, not folded into the project's own license |
| Adobe DCP (camera profile) files | Color profiles | Proprietary Adobe/Lightroom data; no redistribution grant found[^dcp1] | — | n/a | ⛔ | ⛔ | ⛔ **never bundle** |
| Adobe LCP (lens profile) files | Lens correction | Proprietary Adobe/Lightroom data; no redistribution grant found (weakest-sourced claim in this audit — treat as prudent inference, re-verify if this becomes a real dependency)[^lcp1] | — | n/a | ⛔ | ⛔ | ⛔ **never bundle** |
| [dcamprof](https://github.com/Beep6581/dcamprof) | Open DCP-alternative profile generator | GPL-3.0[^dc1] | — | External CLI tool (not linked) | ✅ if invoked as a separate process, not linked into Nicti's binary | ✅ | ✅ external-tool use only |
| LibRaw's built-in camera color matrices | Open DCP-alternative (fallback, no extra dependency) | Inherits LibRaw's own LGPL-2.1/CDDL-1.0 (item above) | — | Dynamic (already inside LibRaw) | ✅ | ✅ | ✅ bundle OK — no new license surface |
| [SQLite](https://www.sqlite.org/copyright.html) (bundled via `libsqlite3-sys`) | Catalog DB candidate ([#67](https://github.com/jordanfelle/nicti/issues/67)) | Public domain[^den1] | — | Static (`bundled` feature) | ✅ | ✅ | ✅ bundle OK |
| [DuckDB](https://github.com/duckdb/duckdb/blob/main/LICENSE) core (bundled via `libduckdb-sys`) | Catalog DB candidate ([#67](https://github.com/jordanfelle/nicti/issues/67)) | MIT[^den2] | — | Static (`bundled` feature) | ✅ | ✅ | ✅ bundle OK |
| [LMDB](https://www.openldap.org/software/release/license.html) (bundled via `lmdb-master-sys`) | Catalog DB candidate ([#67](https://github.com/jordanfelle/nicti/issues/67)) | OpenLDAP Public License 2.8[^den3] | — | Static | ✅ (attribution-only, no copyleft) | ✅ | ✅ bundle OK — retain the license text per its own §3 condition |
| [libSQL](https://github.com/tursodatabase/libsql) core, a SQLite C-source fork (bundled via `libsql-ffi`) | Catalog DB candidate ([#113](https://github.com/jordanfelle/nicti/issues/113)) | Public domain[^den6] — the bundled `bundled/src/sqlite3.c` retains SQLite's own standard "blessing" (public-domain dedication) notice throughout, confirmed by reading the actual bundled file, not assumed from the crate's own `license = "MIT"` Cargo.toml field (which describes the Rust binding, not the underlying forked C source — same "cargo-deny only sees what a crate declares" gap this file's LMDB row and `cfg_block` note already establish) | — | Static (`bundled` feature) | ✅ | ✅ | ✅ bundle OK |
| [RocksDB](https://github.com/facebook/rocksdb) core (vendored git submodule via `librocksdb-sys`) | Catalog DB candidate ([#115](https://github.com/jordanfelle/nicti/issues/115)) | Dual `Apache-2.0` **or** `GPL-2.0-only` (licensee's choice)[^den5] | — | Static | ✅ if the Apache-2.0 arm is elected (confirmed selectable — see the 2026-09-24 update above) | ✅ | ✅ bundle OK — elect the Apache-2.0 arm; the Rust binding crate itself (`rocksdb`) is Apache-2.0 only regardless |

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
| [Ultralytics YOLO](https://github.com/ultralytics/ultralytics/blob/main/LICENSE) | Culling/detection candidate | **AGPL-3.0**[^m10] | Same AGPL-3.0 (weights bundled under the same terms; commercial license sold separately) | COCO/Ultralytics-curated — not the issue | ✅ **bundle OK as of 2026-09-24** — Nicti's own outbound license is now AGPL-3.0-or-later (#66/ADR-0013), so this combines cleanly; no need for Ultralytics' paid commercial license or a non-AGPL swap. See ADR-0003's 2026-09-24 amendment. |

## Flags requiring a decision before shipping a real feature

1. ~~**exiv2 / rexiv2 (GPL)** — excluded outright~~ — **resolved 2026-09-24**: no longer excluded.
   Nicti's own outbound license is now AGPL-3.0-or-later (#66/ADR-0013); both crates' precise
   grants (GPL-2.0-or-later, GPL-3.0-or-later) combine cleanly into it. kamadak-exif + little_exif
   remain the current dependency — nothing wrong with them, this isn't a forced switch — but a
   real reason to prefer exiv2 (IPTC support, maturity) is no longer blocked on license grounds.
2. **rawler / lensfun-rs / LibRaw (LGPL + Rust static linking) — resolved 2026-09-25, corrected
   from an earlier overcautious framing.** An earlier draft of this section (2026-09-24) reasoned
   that rawler's and LibRaw's bare "LGPL-2.1" grants (no explicit "-or-later" suffix) were
   blocking, on the theory that LGPL-2.1 §3's relicense-to-GPL option would force a GPL-2.0-only
   result, incompatible with Nicti's AGPL-3.0-or-later. **That theory doesn't hold up against the
   actual license text, found during #37's own licensing review (2026-09-25) and independently
   confirmed against `gnu.org`'s primary sources:**[^lgpl1]
   - **LGPL-2.1 §3 itself lets the person exercising it pick *any* GPL version that exists at the
     time**, not just GPL-2.0 — its own text: "(If a newer version than version 2 of the ordinary
     General Public License has appeared, then you can specify that version instead if you wish.)"
     This is independent of whether the code's own LGPL grant says "-only" or "-or-later" — that
     phrasing (when present) only governs which *version of the LGPL itself* applies; §3's
     GPL-conversion choice is a separate right LGPL-2.1 grants unconditionally. So even a strict
     LGPL-2.1-only work could be relicensed to GPL-3.0 via §3, if that were the mechanism in play.
   - **But §3 isn't the mechanism that actually matters here, and doesn't need to be exercised at
     all.** §3 is an opt-in act (the license text calls it "irreversible for that copy" once you
     "alter all the notices") for redistributing a *modified copy of the library itself* under GPL
     terms — not something linking/combining triggers automatically. The right provision for
     "can Nicti combine an LGPL-2.1 library into a differently-licensed larger AGPL-3.0-or-later
     program" is **LGPL-2.1 §§5–6**, which is exactly what LGPL is *for*: §5 confirms a "work that
     uses the Library" isn't a derivative of the Library until linked; §6 permits distributing that
     combination under terms of your choice (here, AGPL-3.0-or-later), **provided those terms
     permit modification of the work for the user's own use and reverse engineering for
     debugging** (AGPL grants both by its own nature — it's a copyleft license, and it doesn't
     restrict reverse engineering), plus prominent notice + the LGPL license text, plus one of five
     options for the *source* obligation. **A hostile review correctly caught that an earlier draft
     of this section understated that source obligation** — for a statically-linked executable,
     §6(a) specifically requires accompanying the work not just with the LGPL'd library's own
     source, but with **the complete "work that uses the Library" — i.e. the whole combined
     executable — as object and/or source code, so the user can modify the library and relink**.
     "We vendor the library's source" alone doesn't reach that. **A second hostile review correctly
     pushed back further**: §6(a)'s own text says "*accompany* the work" — read strictly, that
     means the source travels *with* the distributed binary, not merely "exists somewhere public";
     public repo existence alone doesn't, by itself, satisfy option (a) specifically. The clean fit
     is a **different one of §6's five options: §6(d)**, "if distribution of the work is made by
     offering access to copy from a designated place, offer equivalent access to copy the [§6a]
     materials from the same place." A GitHub Release tied to a tagged commit is exactly this
     shape: the place a user gets the binary from (that Release, on this repo) is the *same place*
     that already serves the complete corresponding source of the entire executable — required
     regardless, since Nicti's own outbound license (AGPL-3.0-or-later) makes this repo the
     authoritative, complete source for anyone who receives a copy (§13 further extends the same
     source-offer obligation to remote network users specifically for a *modified* version run as
     a network service — closing the "hosted-service loophole" plain GPL leaves open, per
     ADR-0013 — not a claim that §13 covers every deployment). **This is not "already
     unconditionally satisfied, zero action needed"** — it depends on the actual release mechanism
     genuinely keeping the binary and complete source at the same place (which GitHub Releases
     does naturally, but a different distribution channel later — a standalone installer, a
     third-party download mirror — would need to independently satisfy §6(d) or another option, a
     real "verify at ship time" item, not a permanently pre-solved abstract fact). Combined with
     the notice + LGPL-license-text requirement §6 always demands regardless of which sub-option is
     used, this is the actual, complete compliance shape before `nicti-decode` ships either
     dependency — genuinely satisfiable with Nicti's existing GitHub-native distribution model, but
     a real checklist to verify per release, not something to treat as closed forever. No
     relicensing, no "-or-later" grant, and no GPL-version question ever enters into this path.
   - The FSF's own license-compatibility page (`gnu.org/licenses/license-list.en.html`) confirms
     LGPL-2.1 is compatible with both GPLv2 and GPLv3, and separately confirms GPLv3-family works
     can combine separate modules/source files with AGPLv3-family works even though the two aren't
     interchangeable as a whole-program relicense — the same cross-linking shape as LGPL's own §6.
   - **Net: no isolation/sign-off requirement is actually needed for `rawler`, `LibRaw`, or
     `lensfun-rs` on LGPL-vs-AGPL *compatibility* grounds** — the earlier `cdylib`-isolation
     requirement (ADR-0004 §3) was written to satisfy a stricter reading than LGPL-2.1 actually
     demands. LibRaw's CDDL-1.0 arm is a moot alternative either way (GPL-incompatible, and
     unneeded now that the LGPL arm is confirmed usable directly). What's left before shipping
     isn't a compatibility question, but it *is* a real distribution-mechanics checklist (see
     above): §6(d)'s "same place" condition via GitHub Releases, plus §6's prominent notice + LGPL
     license text for LibRaw/rawler — verify both hold for however `nicti-decode` actually ships,
     not a permanently pre-solved fact.
   - `lensfun-rs`'s own dual license (`LGPL-3.0-or-later OR GPL-3.0`) was already confirmed
     unambiguous before this correction and needs no change to that conclusion.
3. **Adobe DCP/LCP data** — never bundle. Use `dcamprof` (external CLI, GPL-3.0 but not linked in)
   or LibRaw's built-in color matrices instead. **Unaffected by the 2026-09-24 license change** —
   this exclusion is about no redistribution grant existing at all, not about copyleft
   compatibility; it would hold under any outbound license Nicti could pick.
4. **InsightFace/RetinaFace** — non-commercial only, excluded. Use DINOv2 or OpenCLIP embeddings
   for face/subject grouping (#35) instead of literal face-recognition models — this also better
   serves the "must handle fursuiters, not just human faces" requirement from #4. **Unaffected by
   the 2026-09-24 license change** — this exclusion is about a non-commercial-only restriction, not
   copyleft-vs-permissive compatibility.
5. ~~**Ultralytics YOLO (AGPL-3.0)** — excluded from a permissive release~~ — **resolved
   2026-09-24**: no longer excluded. Nicti's own outbound license is now AGPL-3.0-or-later
   (#66/ADR-0013), so this combines cleanly — no non-Ultralytics swap or paid commercial license
   needed if a YOLO-family detector is wanted for culling.
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
[^lr3]: License headers on the vendored fork's own new files — direct inspection of `spikes/retina/vendor/LibRaw` (git submodule pinned to `yogthos/LibRaw@nikon-he-decoder`, commit `499bfd4c…`) at `src/decoders/nikon_he/nikon_he_decode.cpp` and `src/decoders/nikon_he_decoder.cpp`, both reading "Copyright (C) 2026 Dmitri Sotnikov ... LGPL-2.1 or CDDL-1.0" — not the PR thread's own text, which `[^lr2]` cites — verified 2026-09-25
[^raw1]: rawler license field — https://crates.io/api/v1/crates/rawler (registry JSON) — verified 2026-09-23
[^lf1]: lensfun code (`libs/` LGPL-3.0, `apps/` GPL-3.0) and database (CC BY-SA 3.0) — https://github.com/lensfun/lensfun README licensing section — verified 2026-09-23
[^lf2]: lensfun-rs dual license — https://github.com/vdavid/lensfun-rs (README + LICENSE-LGPL-3.0/LICENSE-GPL-3.0) — verified 2026-09-23
[^lcms1]: Little CMS 2 MIT license — GitHub license API resolving the repo's own `LICENSE` file at https://github.com/mm2/Little-CMS/blob/master/LICENSE — verified 2026-09-23
[^kx1]: kamadak-exif BSD-2-Clause — https://crates.io/api/v1/crates/kamadak-exif (registry JSON) — verified 2026-09-23
[^le1]: little_exif MIT OR Apache-2.0 — https://crates.io/api/v1/crates/little_exif (registry JSON) — verified 2026-09-23
[^ex1]: exiv2 GPL-2.0-or-later — `SPDX-License-Identifier: GPL-2.0-or-later` headers in its own source (`src/exif.cpp`, `src/image.cpp`) and its README's License section — verified 2026-09-23, precise -or-later grant confirmed 2026-09-24 (superseding an earlier citation of `COPYING`, which is generic FSF boilerplate, not project-specific evidence)
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
[^den5]: RocksDB core dual license — `LICENSE.Apache` (Apache License 2.0 full text) and `COPYING` (GNU GPL v2 full text) at the root of https://github.com/facebook/rocksdb, both read directly, not inferred from a summary or README line — verified 2026-09-24. The `rocksdb` Rust binding crate's own `Cargo.toml` declares `license = "Apache-2.0"` only (no GPL arm), and its top-level `LICENSE` file at https://github.com/rust-rocksdb/rust-rocksdb is the Apache License text — verified 2026-09-24.
[^den6]: libSQL's bundled SQLite C fork public-domain notice — the "blessing" text repeated throughout `~/.cargo/registry/src/.../libsql-ffi-0.9.30/bundled/src/sqlite3.c`, the same standard SQLite public-domain dedication `sqlite.rs`'s own footnote (`[^den1]`) cites for plain `libsqlite3-sys` — verified 2026-09-24
[^lgpl1]: LGPL-2.1 §3 (relicense-to-GPL option, version choice is the redistributor's, not forced to GPL-2.0) and §§5–6 (permits combining/linking with a differently-licensed work without relicensing, provided that license permits modification + reverse engineering for debugging, plus notice + one of five source-availability options — for a statically-linked executable, §6(a) specifically requires the complete "work that uses the Library," i.e. the whole combined executable, as object and/or source so the user can relink, not just the LGPL'd library's own source) — https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html and https://www.gnu.org/licenses/old-licenses/lgpl-2.1.txt (full section text, §6 read in full), cross-checked against https://opensource.org/license/lgpl-2-1/ — verified 2026-09-25. FSF's license-compatibility page confirms LGPLv2.1 is "compatible with GPLv2 and GPLv3," and separately that GPLv3-family and AGPLv3-family works can combine separate modules/source files even though neither is a whole-program relicense of the other — https://www.gnu.org/licenses/license-list.en.html — verified 2026-09-25.
