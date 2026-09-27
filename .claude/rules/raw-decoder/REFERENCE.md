---
paths:
  - "spikes/retina/**"
  - "crates/nicti-cornea/**"
---

# RAW Decoder — Quick Reference

Full reasoning/history: `docs/decisions/raw-decoder.md`.

- **RAW decoder (#37)** — `docs/adr/0037`: **Accepted** — build #41 on the unoptimized PR #826
  as-is; optimize later. `retina.exe peek` measured ~1-2.4s/file end-to-end, including launch,
  read, decode, and output, rather than gating on profiling/vectorization first
  (the LGPL question is resolved — LGPL-2.1 §§5-6 permit combining `rawler`/LibRaw into Nicti's
  AGPL-3.0-or-later work, no "or-later" grant needed, see `licensing` topic). **LibRaw patched
  with the still-open
  [LibRaw/LibRaw#826](https://github.com/LibRaw/LibRaw/pull/826)** (Nikon HE/HE\* decoder), vendored
  as a git submodule pinned to `yogthos/LibRaw@nikon-he-decoder` — the only candidate that decodes
  the real library's HE/HE\* files (no released LibRaw/rawler/rawspeed version handles HE/HE\*,
  88% of the real Z8 files).
- **Measured 100% decode success (261/261)** across a real subset pulled from the live library
  (HE/HE\*/Lossless, two camera bodies), zero crashes either decoder side. This measures decode
  success only, not independently re-verified pixel correctness for HE/HE\* — treat that as
  production-ready only after an oracle check (Deferred item 8).
- **rawler stays only as the Lossless-path correctness cross-check** — structurally can't read
  HE/HE\* (clean typed rejection, never a crash). Use `retina diff`'s **±1 LSB tolerance
  histogram, not hash equality** — the two decoders' Lossless output never hash-matches (a real,
  characterized, one-directional rounding difference in curve-inversion, not a bug).
- **RapidRAW (#69) doesn't solve HE either** — its own rawler fork still rejects HE/HE\*; on
  Windows it silently falls back to the embedded JPEG instead of a real RAW decode.
- **Decode cost is real and high**: ~1-2.4s/file, `retina.exe peek`'s full end-to-end process
  time (launch + read + decode + output, isolated, via WSL interop, not a pure decoder timer) —
  5-12x over the 200ms cold-switch target, 10-24x over the 100ms 1:1-zoom target. Expected for
  PR #826's unoptimized reference code; a real input to #44's render-graph cache design.
- **The frozen `ref-10k` reference set (393GB NVMe + HDD copies) vanished mid-research** —
  unmaintainable per-machine at that size; needs multi-person/multi-machine access, tracked
  separately (#136), not solved by #37. `retina scan` (no manifest CSV needed) runs directly
  against the live library or any ad hoc subset instead.

## Package contents

- **`crates/nicti-cornea`** (#41, landed) — the real `RawDecoder` implementation: LibRaw's FFI
  wrapper (`libraw_ffi.rs`, `LibRawHandle`), `shim.cpp`/`shim.h`, `build.rs` (compiles LibRaw's C++
  via the `cc` crate, no bindgen), and the vendored `LibRaw` git submodule (needs
  `git submodule update --init crates/nicti-cornea/vendor/LibRaw` before it builds) all live here
  now, promoted out of `spikes/retina`. `LibRawDecoder::decode_linear` demosaics one file with
  white balance/color-matrix/gamma all disabled via LibRaw's own params, returning a `LinearFrame`
  (linear camera RGB + the metadata `spikes/calico`'s color pipeline needs) directly, no TIFF/JSON
  round-trip required for a production caller.
  - **All of the above is gated behind a non-default `libraw` Cargo feature** (`LibRawDecoder`,
    `LibRawHandle`, `DemosaicQuality`, and `build.rs`'s entire C++ compile step are `#[cfg]`-gated
    on it, `cc`/`clap` are optional deps activated by it). `embedded` (below) is pure Rust and
    always built — `nicti-lair` depends on this crate for `embedded` alone with the feature
    off, so it never needs `vendor/LibRaw` checked out or a C++ compiler. Discovered the hard way
    on #41's own PR: a workspace `--exclude nicti-cornea` on the always-on CI jobs doesn't actually
    keep the C++ build out of them, since `nicti-lair` (unexcluded) still pulls `nicti-cornea`
    in as a dependency and its `build.rs` used to run unconditionally — the feature gate is the
    real fix; the CI `--exclude` is redundant belt-and-suspenders on top of it, not sufficient on
    its own. `spikes/retina` and CI's own `decode-linux`/`decode-windows` job both request the
    feature explicitly (`features = ["libraw"]` / `--all-features`).
- **`spikes/retina`** (#37/ADR-0037's RAW decoder comparison, plus #40's own comparison tooling) —
  now a thin CLI depending on `crates/nicti-cornea` for its decode step. Subcommands:
  `sweep`/`compare`/`diff` against rawler 0.8.0, `scan` for a manifest-free directory walk, `watch`
  for #24's `notify` research, `dump-linear` (writes `nicti-cornea`'s `LinearFrame` to a
  linear-camera-RGB TIFF + metadata JSON sidecar, kept for `spikes/calico`'s own file-based
  tooling — see `linear_input.rs`, which still reads that pair without depending on
  `nicti-cornea`'s FFI/submodule directly), and `dump-classic`/`dump-cfa` (#40/ADR-0040's
  classic-pipeline and Bayer-plane dumps for `spikes/rods` — see the `denoise` topic's
  REFERENCE.md). See `docs/research/retina-raw-decoder.md`.
