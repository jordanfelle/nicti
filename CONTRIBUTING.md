# Contributing to Nicti

Thanks for your interest in Nicti. This project is early — architecture and requirements are
still being worked out (see the README's Status section) — but outside contributions are welcome
and this doc is the canonical guide for making one.

## Prerequisites

- **Rust**, stable channel ([rustup](https://rustup.rs/)). No pinned MSRV yet; `edition = "2021"`.
- **[nasm](https://www.nasm.us/)** — needed to build `rav1e`/`rav1d` (an AVIF codec candidate used
  by `spikes/sniff`). `brew install nasm` (macOS/Linuxbrew), `choco install nasm` (Windows),
  `apt-get install nasm` (Debian/Ubuntu).
- **A C/C++ toolchain** — MSVC on Windows (the Visual Studio Build Tools), a standard
  gcc/clang setup elsewhere. Needed for `den`'s bundled native catalog-engine libraries and for
  `retina`'s vendored LibRaw build.
- **libclang/LLVM** — only if you're touching `den`'s RocksDB feature (`bindgen` needs it).
- **Linux GUI headers** (`libxkbcommon-dev`, `libwayland-dev`, `libx11-dev`, `libxi-dev`,
  `libxrandr-dev`, `libgl1-mesa-dev`, `libfontconfig1-dev`) — only if you're touching
  `spikes/pelt-egui`/`pelt-iced`/`pelt-slint` on Linux.
- **The `retina` submodule** — only if you're touching `spikes/retina`:
  ```bash
  git submodule update --init spikes/retina/vendor/LibRaw
  ```
- **[pre-commit](https://pre-commit.com/)**:
  ```bash
  pip install pre-commit
  pre-commit install
  ```

You don't need all of the above for every change — most day-to-day work only needs Rust + nasm.
The extras are called out above so you know when they're missing (a build failure mentioning
`bindgen`/`libclang`, a missing `libraw.h`, or a Linux windowing-backend link error will point back
here).

## Workflow

```bash
git checkout main && git pull origin main
git worktree add ../nicti-wt-myfeature -b feat/myfeature   # or just git checkout -b
```

Windows is v1's actual target platform and the required (blocking) CI check; Linux CI runs too
(catches platform-specific bugs early) but isn't required to merge.

## Building, testing, linting

**Always pass `--workspace`/`--all`** — the root `Cargo.toml` is both the workspace root and a
real package (`nicti`), not a virtual manifest, so a bare `cargo test`/`cargo clippy` (no
`-p`/`--workspace`) or a bare `cargo fmt --check` (no `--all`) silently only checks the root crate
and skips `spikes/*`/`bench/whisker` entirely. This caused a real CI gap once; don't reintroduce
it.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --exclude den --exclude pelt-egui --exclude pelt-iced --exclude pelt-slint --exclude retina --all-targets --all-features -- -D warnings
cargo test --workspace --all-targets --all-features --exclude den --exclude pelt-egui --exclude pelt-iced --exclude pelt-slint --exclude retina
```

The `den`/`pelt-*`/`retina` excludes above mirror `.github/workflows/ci.yml` exactly — those
crates are path-gated into their own CI jobs (`den (linux)`/`den (windows)`, etc.) since they pull
in heavy native builds (eight bundled catalog engines, three GUI-framework stacks, a full LibRaw
C++ compile) that most PRs don't touch. If your change does touch `spikes/den/**`,
`spikes/pelt-*/**`, or `spikes/retina/**`, also run that crate's own commands — see `ci.yml`'s
`den-linux`/`pelt-linux`/`retina-linux` jobs for the exact feature-flag combinations (some engines
can't be enabled together in the same binary; the workflow file's comments explain why).

`pre-commit`'s hooks (fmt, clippy, secret detection, YAML lint on every commit; `cargo test` on
push) mirror the same exclude list, so a clean local commit should mean clean CI for these jobs.

## Package layout

- `crates/*` — real production crates. See `CLAUDE.md`'s Package map section for what each one is
  and which ticket owns it.
- `spikes/*` and `bench/whisker` — throwaway research crates backing a specific ADR. **Don't build
  on top of a spike** — it's expected to be deleted once its own ticket promotes the working
  approach into a real `crates/*` crate.

## Naming convention: feline references

Name new crates, modules, internal tools, and subsystems with a feline-anatomy/behavior angle
rather than a purely descriptive name — the project itself is named after the nictitating
membrane (a cat's third eyelid), and that theme continues throughout. Examples: `Tapetum`
(stage-cached render graph), `Claw` (module/plugin registry), `Pounce` (job scheduler), `Sniff`
(embedded-JPEG fast preview path).

## Architecture Decision Records (ADRs)

Significant technical decisions get an ADR in `docs/adr/`, numbered sequentially
(`NNNN-title.md`). See `docs/adr/README.md` for the index and template. A per-topic summary (the
actionable conclusion without the full research trail) lives in `docs/decisions/<topic>.md`; see
`CLAUDE.md`'s Architecture decisions section for the topic map.

If your PR makes a decision worth recording — a library choice, a rejected alternative, a
non-obvious tradeoff — write an ADR. Small implementation choices don't need one.

## Licensing

Update [`docs/licensing.md`](docs/licensing.md) in the same PR as any new dependency, native
library, or ML model — it's the third-party license audit backing Nicti's AGPL-3.0-or-later
outbound license (see `docs/adr/0003-third-party-license-policy.md`). `cargo deny check licenses`
runs in CI against `deny.toml`'s allowlist, but that only covers Cargo dependencies, not native
libraries or data files — those need `docs/licensing.md` updated by hand.

## Issue conventions

- A bare `#N` in a commit/PR/ADR/issue body means a GitHub issue or PR in this repo.
- **`**Part of:** #N`** in an issue body links a child issue to its parent epic.
- **`**Blocked by:** #N`** in an issue body marks a dependency on another open issue — the
  `/nicti-backlog`-style tooling used to find pickable work treats this as "not ready yet" until
  #N closes.
- Issues carry `epic`/`requirements`/`research`/`build` labels where relevant. An epic is a
  container, not a pickable unit of work.

## Benchmarks

Most of `bench/` and the `ref-10k` reference dataset assume a Windows machine with Lightroom
Classic installed, PowerShell 7+, and AutoHotkey v2 — see `docs/benchmarks.md` and
`bench/lrc/README.md` for the full methodology. **`ref-10k` itself is a private dataset** (real
event photos) and isn't distributed with the repo; see issue #136 for its current storage-model
status. What you *can* run without it:

- `den gen --seed N --scale <n>` — a synthetic catalog generator for Library-scale benchmarks, no
  real image content needed.
- `spikes/glint`'s and `spikes/groom`'s throughput/correctness tests (`#[ignore]`d where they need
  real hardware or model weights — run with `-- --ignored` and the env var each test names).
- `nicti-prowl`'s golden-image tests, which currently run against synthetic renders only (no real
  NEF→render path exists yet).

If you're working on decoder/catalog/perf-sensitive code and need a small non-private RAW sample
set to test against, open an issue — there's no documented "bring your own NEFs" mode yet.

## PR conventions

- **PR titles**: a plain summary sentence.
- Include the linked issue in the PR body (the PR template does this for you).
- Run `cargo fmt`/`clippy`/`test` (see above) before opening.
- Anything substantial (not a copy tweak, comment, or one-line config edit) should go through a
  review pass — self-review your own diff for the areas the PR template's checklist calls out
  before requesting review.

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).

## Security

See [`SECURITY.md`](SECURITY.md) for how to report a vulnerability privately.
