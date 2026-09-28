# Release, signing, and auto-update

Tracks GitHub issue [#249](https://github.com/jordanfelle/nicti/issues/249). See
[ADR-0249](../adr/0249-windows-installer-and-updates.md) for the formal record — this file exists
per this repo's usual pattern of a terse `.claude/rules/<topic>/REFERENCE.md` plus this longer
prose companion; that REFERENCE.md is `.claude/rules/release/REFERENCE.md`.

## Why per-user, not per-machine

The user raised Lightroom Classic and Chrome as the natural comparison points while this was being
scoped. Both install per-machine (`Program Files`) and update via a privileged background
component: LRC via Adobe's own updater service, Chrome (in its default, non-enterprise deployment)
via the `GoogleUpdate` service running as `SYSTEM`. That buys silent updates without ever
prompting the signed-in user for UAC — the service itself already has the rights it needs.

Chrome, though, also ships a **user-level install** mode (used automatically when the installing
account isn't an administrator, or explicitly via `--chrome-sxs`/user-level installers): installed
under `%LOCALAPPDATA%`, no service, no elevation ever needed for anything including updates,
because nothing it touches requires admin rights in the first place. That's the model this ticket
adopted. A two-person open-source project maintaining a background Windows service — a real,
ongoing security-hardening and support burden — for what a per-user install gets for free wasn't a
trade worth making for v1. If Nicti later needs shared-machine (multi-user, one shared install)
deployment, that's the per-machine + privileged-updater-service model, filed as its own v2 issue
rather than retrofitted here.

## Signing: SignPath Foundation, gated not blocking

SignPath Foundation signs OSS projects' binaries for free, which matters because an unsigned
Windows executable triggers a SmartScreen "Windows protected your PC" interstitial on first run —
a real adoption cost for a new user. Getting that removed needs an application to SignPath and
their approval, which is outside this repo's control and has no committed timeline.

Rather than block the whole installer/auto-update feature on that external approval, the release
workflow signs conditionally: a `SIGNPATH_ENABLED` repository variable gates the signing steps
entirely. Until it's flipped, releases publish unsigned — a known, disclosed cost, not a silent
gap. The follow-up issue to actually apply and configure SignPath is filed separately from #249,
since its critical path (SignPath's own review process) has nothing to do with anything in this
repo's control.

## Update integrity doesn't depend on Authenticode

A subtlety worth being explicit about: Authenticode (SignPath) signing and update-integrity
verification are two different trust questions, and this decision keeps them separate on purpose.
Authenticode answers "does Windows/SmartScreen trust this binary's publisher" — purely a UX/OS
question. It says nothing about whether *this specific downloaded file* is the one Nicti's own
release process actually produced, versus e.g. a compromised CDN, a MITM'd HTTP connection, or (if
GitHub itself were ever compromised) a tampered release asset.

`crates/nicti-shed` therefore verifies every downloaded installer against a **minisign** signature
before ever executing it, using a public key compiled directly into the running `nicti.exe` — not
fetched at update-check time, which would let a compromised update server also spoof the
verification key. minisign was chosen over hand-rolling a raw Ed25519 check: it's a small,
widely-used, purpose-built tool/format for exactly this (BSD-licensed daemontools-style tooling
uses it, so does rustup's own installer-integrity story), and its Rust verifier
(`minisign-verify`, MIT) is trivial to license-clear against `deny.toml`. Signing happens in CI
with `rsign2` (a Rust reimplementation of minisign, MIT), keeping the whole toolchain in
Cargo-installable Rust rather than needing a separately-vendored native minisign binary.

The private minisign signing key lives only as a base64-encoded GitHub Actions secret
(`NICTI_UPDATE_MINISIGN_KEY_B64`) — it never touches the repo or a build artifact. The key itself
is generated **unencrypted** (`rsign generate -W`): `rsign2`'s `sign` subcommand has no
non-interactive way to unlock a password-protected key (it reads straight from `/dev/tty`, and a
GitHub Actions runner has no tty at all — confirmed this fails outright, not just "less
convenient"), so a password would only ever have blocked CI from signing anything, never actually
protected the key. Compromise of that one secret is the single point of failure for the
entire update channel's integrity, same as it would be for any auto-updater (Chrome's Omaha root
key, Squirrel.Windows' code-signing requirement); this is the normal shape of that risk, not a
weaker one, and is worth stating plainly since nothing else in this repo currently holds a
comparably sensitive secret.

## What `nicti-shed` actually does

- **Check** (cross-platform logic, `cfg(windows)`-gated network call): fetch
  `https://api.github.com/repos/jordanfelle/nicti/releases/latest`, parse the JSON with
  `serde_json` (already in the lockfile), compare its tag against the running binary's
  `env!("CARGO_PKG_VERSION")` with `semver` (also already in the lockfile), and find the
  `Nicti-Setup-*.exe` asset.
- **Verify**: download the asset and its `.minisig` sidecar, verify with `minisign-verify` against
  the compiled-in public key. Any mismatch is a hard refusal — never run an unverified binary.
- **Apply**: run the verified installer with `/S /UPDATE`, which (per `nicti.nsi`) waits for any
  running `nicti.exe`, overwrites the install in place, and relaunches — no UAC prompt, matching
  the per-user install model above.
- **State**: `%LOCALAPPDATA%\Nicti\update.json` holds `auto_check`/`last_check` so a background
  check runs at most once per 24h; no separate config crate (`dirs`) is needed since this is the
  only per-user state Nicti currently persists.

## Why `ureq` + `native-tls` instead of `reqwest`/`rustls`

Neither an HTTP client nor a TLS stack existed anywhere in the workspace before this ticket.
`ureq` was chosen for its small dependency footprint (this is one GET request and one file
download, not a general HTTP client need) and its `native-tls` feature, which uses Windows'
own SChannel/CNG trust store on the only platform this code actually runs on (`cfg(windows)`).
That specifically avoids pulling in `rustls` + `webpki-roots` — `webpki-roots`' license
(`CDLA-Permissive-2.0` in recent versions) is not on `deny.toml`'s allowlist, and adding it would
be a needless license-review detour for a dependency whose entire job (bundling a root CA store)
is redundant with what Windows already provides.

## Versioning

The root `nicti` package's `Cargo.toml` `version` field is the single source of truth for what
ships. Every other workspace crate (`nicti-pelt`, `nicti-tapetum`, etc.) stays pinned at `0.0.0`
per the package map's `publish = false` convention — they're internal, never independently
versioned or released. `release.yml` fails a `v*` tag push outright if the tag doesn't match that
version, so there's never a real ambiguity about "what version is this release."

## In-app Edge channel (#282)

The Stable channel's whole update-identity story leans on semver: `release.yml` refuses to
publish a tag whose version doesn't match `Cargo.toml`, so "is `latest` newer than `current`" is
just `semver::Version` comparison. The Edge channel (#267) has no equivalent to compare — it
republishes the same `edge` tag/release at whatever commit `main` is at, and doesn't bump
`Cargo.toml`'s version on every commit (most edge builds ship the exact same installer filename
and `CARGO_PKG_VERSION` as the one before it). Semver comparison would report "up to date"
forever after the first edge install, which defeats the entire point of an in-app edge-channel
prompt.

The fix is to compare identity, not ordering: the edge release's `target_commitish` (which
`release.yml`'s own publish step sets to the exact `GITHUB_SHA` it built from) against the
*running binary's own build commit*. That value has to come from somewhere real, not just
`CARGO_PKG_VERSION` — `crates/nicti-shed/build.rs` embeds it at compile time, preferring
`GITHUB_SHA` (already set on every Actions run, no extra process spawn) and falling back to `git
rev-parse HEAD` for a local `cargo build`, with `"unknown"` as a last resort so a source tarball
with no `.git` still builds (an "always looks out of date" degrade is acceptable there — it's an
opt-in channel's own UI nicety, not the minisign trust boundary, and never blocks a build).

`UpdateState` gained a persisted `channel: Channel` field (`#[serde(default)]`'s `Stable`, so an
existing install's state file never silently opts itself onto unsigned, unreviewed edge builds
just because this field didn't exist yet). Switching channel in `nicti-pelt`'s picker force-
rechecks immediately rather than waiting for the next 24h auto-check — landing on Edge with no
prompt until tomorrow would look like the switch silently did nothing.
