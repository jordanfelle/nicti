# Nicti

[![CI](https://github.com/jordanfelle/nicti/actions/workflows/ci.yml/badge.svg)](https://github.com/jordanfelle/nicti/actions/workflows/ci.yml)

A fast, non-destructive RAW photo editor and digital asset manager, aiming to be a viable
open-source replacement for Adobe Lightroom Classic — built around a stage-cached render pipeline
so editing a heavy stack of AI masks and denoise doesn't mean waiting on every navigation and crop.
Named after the nictitating membrane, a cat's third eyelid; see `CONTRIBUTING.md` for the
feline-naming convention that continues throughout the project.

## Status

Early bootstrap. Architecture and requirements are still being worked out; nothing here is
usable yet. Tracked as GitHub issues/PRs in this repo, with epics (`epic` label) breaking work
into `requirements`/`research`/`build` children — `docs/adr/` is the design record.

## Scope

- v1 targets **Nikon RAW (NEF)** on **Windows**. macOS/Linux release builds (#73) and other
  camera brands (#65) are out of scope for v1, but tracked for v2 — the architecture avoids
  hard-coding Nikon-only assumptions where it doesn't cost anything now.
- Built in Rust, with a C FFI binding to [LibRaw](https://www.libraw.org/) for RAW decoding.
- Core design goal: a stage-cached render pipeline, so switching between images or dragging a
  crop stays fast even with a heavy stack of AI masks/denoise applied -- expensive stages are
  baked once and cached, cheap stages (white balance, vibrance, etc.) stay live.

## Crate map

- `crates/nicti-claw` — the module/plugin registry every other crate builds on.
- `crates/nicti-prowl` — the benchmark + golden-image harness.
- `crates/nicti-decode`, `nicti-color`, `nicti-lens`, `nicti-render`, `nicti-ai`, `nicti-export`,
  `nicti-catalog` — one crate per extension point (RAW decode, color, lens correction, render
  pipeline, AI models, export, catalog store), currently trait definitions only.
- `spikes/*` — throwaway research spikes backing specific ADRs; not production code, not a base
  to build on. `bench/whisker` is benchmark tooling in the same category.

See `CLAUDE.md`'s Package map section for the full breakdown and which ticket owns each crate.

## Building

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for prerequisites (Rust, `nasm`, platform-specific
toolchain requirements, and the LibRaw submodule) and the full command set. Quick start:

```bash
cargo build --workspace
cargo test --workspace --all-targets --all-features
```

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for build setup, coding conventions, the ADR process, and
PR expectations. `docs/adr/` holds the architecture decision record; `CLAUDE.md` holds Claude
Code-specific conventions (also useful background for a human contributor, but CONTRIBUTING is the
canonical guide).

## License

[AGPL-3.0-or-later](LICENSE) — see
[`docs/adr/0013-outbound-license-agpl.md`](docs/adr/0013-outbound-license-agpl.md) for the
rationale and [`docs/licensing.md`](docs/licensing.md) for the third-party dependency and
ML-model license audit backing it.
