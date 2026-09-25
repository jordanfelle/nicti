# Nicti

[![CI](https://github.com/jordanfelle/nicti/actions/workflows/ci.yml/badge.svg)](https://github.com/jordanfelle/nicti/actions/workflows/ci.yml)

A fast, non-destructive RAW photo editor and digital asset manager -- a personal replacement for
Adobe Lightroom Classic, built around a stage-cached render pipeline so editing a heavy stack of
AI masks and denoise doesn't mean waiting on every navigation and crop.

## Status

Early bootstrap. Architecture and requirements are still being worked out; nothing here is
usable yet. Tracked as plain GitHub issues/PRs -- see [`CLAUDE.md`](CLAUDE.md) for the current
stack decisions and project conventions, and `docs/adr/` for the design record.

## Scope

- v1 targets **Nikon RAW (NEF)** on **Windows**. macOS/Linux release builds and other camera
  brands are out of scope for v1.
- Built in Rust, with a C FFI binding to [LibRaw](https://www.libraw.org/) for RAW decoding.
- Core design goal: a stage-cached render pipeline, so switching between images or dragging a
  crop stays fast even with a heavy stack of AI masks/denoise applied -- expensive stages are
  baked once and cached, cheap stages (white balance, vibrance, etc.) stay live.

## Building

Requires a recent stable Rust toolchain ([rustup](https://rustup.rs/)).

```bash
cargo build
cargo test
```

## License

[AGPL-3.0-or-later](LICENSE) — see
[`docs/adr/0013-outbound-license-agpl.md`](docs/adr/0013-outbound-license-agpl.md) for the
rationale and [`docs/licensing.md`](docs/licensing.md) for the third-party dependency and
ML-model license audit backing it.
