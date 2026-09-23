# ADR-0003: Third-party license policy

- **Status:** Accepted
- **Date:** 2026-09-23
- **Ticket:** #18 Research: dependency & ML-model license audit

## Context

Nicti may eventually be released as open source (#4/#66). The requirements already state:
"License redistribution compatibility required if open-sourced (LibRaw, lensfun, model weights);
no bundled proprietary data (e.g. Adobe DCPs)." No outbound license has been chosen yet (README:
"Not yet decided") — that's #66's decision, and #66 is explicitly blocked on this ticket. This ADR
doesn't pick Nicti's own license; it sets the policy for what Nicti may depend on and bundle,
evaluated against both realistic outbound-license families, so the #66 choice isn't constrained
later by a dependency already baked in. The full audit backing every claim below is
`docs/licensing.md`.

## Decision

### Rust crates
Allow: MIT, Apache-2.0, BSD-2/3-Clause, CC0-1.0, Unlicense, MIT-0, Unicode-3.0 (as a data-license
arm), and any crate whose license is an OR-list containing at least one of these. Deny: GPL-*,
AGPL-*, and any crate whose *only* option is copyleft. LGPL crates consumed as a Cargo dependency
(effectively static-linked in Rust's compilation model) are **not** auto-allowed — see the LGPL
rule below; they need a case-by-case sign-off, not a blanket allow.

### C/C++ native libraries
LGPL is acceptable **only** via dynamic linking (shared library/DLL, replaceable without
relinking Nicti itself) — this is the standard safe-harbor pattern for a permissively-licensed
project consuming an LGPL library, and it's the intended arrangement for LibRaw and lensfun's
`libs/`. **This is a requirement to enforce, not yet an accomplished fact**: #37 (RAW decode) and
#39 (lens correction) haven't picked a specific FFI crate yet, and the common pattern for Rust
`*-sys` bindings to a C library is to vendor and statically compile the C source by default unless
the build script is explicitly configured to link a system-installed shared library. Neither
`cargo deny` nor any other automated check in this repo inspects actual linker output, so whoever
lands #37/#39 must confirm — by reading the chosen crate's `build.rs` — that it produces a genuine
dynamically-linked `.dll`/`.so` for the LGPL code, not a statically-vendored copy; the LGPL
safe-harbor analysis in `docs/licensing.md` depends on that being true and is invalidated if it
isn't. Prefer a permissive OR-arm when the library offers one (e.g. LibRaw's CDDL-1.0 option). GPL
native libraries (e.g. exiv2) are denied outright, full stop — dynamic linking does not neutralize
GPL as it does LGPL. Rust crates that themselves link a GPL C library (e.g. rexiv2 → exiv2/gexiv2)
inherit that denial; `docs/licensing.md` names the permissive alternative to use instead
(kamadak-exif + little_exif for EXIF/XMP).

`rawler` and `lensfun-rs` are flagged, not auto-denied: their LGPL terms are fine in principle, but
Rust's static-link compilation model doesn't cleanly satisfy LGPL's dynamic-linking safe harbor,
and there's no dynamic option today. Whoever picks one for #37/#39 must get explicit sign-off (or
isolate it behind a `cdylib`/out-of-process boundary via Claw's module architecture, #19) before it
ships as a linked-in dependency — don't treat "it's on the Rust crate registry" as pre-cleared.

### ML model weights
Bundle a model's weights directly in Nicti's installer only if **both** the weights license and
its known training-data provenance permit redistribution. If either is non-commercial-only,
research-only, or unclear (per `docs/licensing.md`'s per-model verdict), the model is not bundled —
instead it becomes an on-demand user download at first use, consistent with #4's requirement that
"heavy modules (AI models, ONNX/CUDA runtime) load on demand, not at startup." Every model actually
wired into a feature needs a row in `docs/licensing.md` recording code license, weights license,
and training-data provenance separately — they are frequently not the same license, and
conflating them is the single most common audit mistake this ADR is meant to prevent.

Denied for bundling under this policy today: InsightFace/RetinaFace (non-commercial only,
explicit maintainer statement) and Ultralytics YOLO (AGPL-3.0 — would force the combined work
AGPL or require a paid commercial license). Flagged for a closer look before shipping: LaMa (its
`big-lama` checkpoint's Places2 training-data provenance) and CLIP (OpenAI's model card explicitly
discourages any deployed use, though not a legal restriction).

### No bundled proprietary data
Adobe DCP (camera profile) and LCP (lens profile) files are never bundled — no redistribution
grant exists for Adobe/Lightroom-authored profile data. Where Nicti needs a camera color profile
or lens-correction data without Adobe data, use LibRaw's built-in camera color matrices (already
inside an existing dependency, no new license surface) or generate one with `dcamprof` (GPL-3.0,
acceptable because it's invoked as an external CLI tool, never linked into Nicti's own binary).

### Out of scope by design — not forgotten
This audit does not cover the GPU compute API choice ([#16](https://github.com/jordanfelle/nicti/issues/16)),
the embedded database engine ([#67](https://github.com/jordanfelle/nicti/issues/67), e.g.
DuckDB/pglite-rs), or the GUI framework ([#68](https://github.com/jordanfelle/nicti/issues/68)) —
those are separate, still-open research tickets with their own licensing dimension (notably, some
strong Rust GUI candidates carry GPL-3.0/dual-commercial terms, which would matter a great deal for
an open-source-release track). #66 is unblocked by *this* ticket closing, but whoever resolves
#16/#67/#68 must add a row to `docs/licensing.md` for the option they pick before it ships — the
"new dependency needs a row in the same PR" rule in the Process section below applies to those
picks too, not just to the components this audit already enumerated.

### NVIDIA runtime (CUDA/cuDNN/TensorRT)
The GPU driver itself is never bundleable — always a user-installed prerequisite Nicti's
on-demand ML loader must detect before initializing a GPU execution provider, falling back to a
CPU execution provider if absent. The specific redistributable CUDA runtime libraries (CUDA
Runtime, cuFFT, cuBLAS, cuSPARSE, cuSOLVER, cuRAND, NPP, NVRTC) may ship inside Nicti's own
installer. cuDNN and TensorRT runtime libraries may also ship inside the installer, conditioned on
keeping them private to Nicti's own process (never exposed as a general shared library other
applications could load) and including NVIDIA's required attribution notice in Nicti's
third-party notices file.

### Process
A new dependency, crate, native library, or ML model gets a row in `docs/licensing.md` **in the
same PR** that introduces it — not deferred to a follow-up. `cargo deny check licenses` (CI-gated,
see `deny.toml`) catches a disallowed Rust crate license automatically; it does not (and cannot)
catch a native-library link, an ML model weight, or a data file, so those still rely on this
process being followed at review time.

## Amendments

- **2026-09-23 (#19/ADR-0004):** added `ISC` to the Rust-crate allowlist (`deny.toml` and the
  section above), for `libloading` — a short permissive license, OSI-approved and FSF Free/Libre,
  functionally MIT-equivalent. No other allow-list change was needed for that ADR's other new
  dependencies (`wasmtime`/`wat`, both `Apache-2.0 WITH LLVM-exception`, already allowed).

## Consequences

- Blocks #66 unblocking is now unblocked (`docs/licensing.md` + this ADR satisfy #18).
- `rawler`/`lensfun-rs` and any future LGPL-as-Cargo-dependency case carries an open sign-off step
  that #37/#39 must resolve explicitly, not silently inherit from "it's on crates.io."
- Face/subject grouping (#35) should target DINOv2 or OpenCLIP embeddings rather than a literal
  face-recognition model — this also happens to better serve the "must handle fursuiters, not
  just human faces" requirement from #4, since InsightFace-style face recognition is excluded
  anyway on license grounds.
- A YOLO-family detector for culling needs a non-Ultralytics implementation/weights, or a budget
  line for Ultralytics' commercial license, if one is used at all.
