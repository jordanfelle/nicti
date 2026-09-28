# ADR-0249: Windows installer, code signing, and auto-update

- **Status:** Proposed
- **Date:** 2026-09-27
- **Ticket:** #249 Build: Windows installer + auto-update channel

## Context

Nicti has no distribution path today beyond building from source (`cargo build --release -p
nicti`, `.github/workflows/ci.yml`'s `build-windows` job, which never uploads an artifact). #249
asks for three things: a real installer, code signing so Windows SmartScreen doesn't block first
runs, and an auto-update mechanism so users aren't stuck on manual rebuild-and-copy.

Two comparison models the user raised while this was being scoped: Lightroom Classic and Chrome.
Both install **per-machine** (Program Files) and update via a **privileged background updater**
(a Windows service or scheduled task running as SYSTEM) so an update can apply without asking the
signed-in user to elevate every time. Chrome also ships a **per-user** mode (installs under
`%LOCALAPPDATA%`, no admin rights ever) specifically so it can update itself silently without any
privileged component at all — this is the model this ADR adopts for v1.

## Decision

**Per-user NSIS install, no privileged updater service.** `RequestExecutionLevel user`, installed
to `%LOCALAPPDATA%\Programs\Nicti`, HKCU-only registry entries. The in-app updater
(`crates/nicti-shed`) downloads a new installer and re-runs it with `/S /UPDATE`, which never
needs a UAC prompt because nothing under `%LOCALAPPDATA%` requires elevation. This gets Chrome's
actual property (fully silent, no service to build/maintain/harden) without the LRC/Chrome
admin-mode's own biggest cost: a persistent background service is a bigger attack surface and
outside a two-person project's v1 scope. Per-machine install + a privileged updater service is
filed as a v2 follow-up if a shared-machine deployment ever needs it.

**Code signing: SignPath Foundation**, free for qualifying OSS projects (AGPL-3.0-or-later,
public repo). This requires an application and approval outside this repo's control, so the
release workflow (`.github/workflows/release.yml`) signs conditionally on a `SIGNPATH_ENABLED`
repository variable — v1's first releases publish **unsigned**, and will show a SmartScreen
"unknown publisher" warning until approval lands and the variable flips. Authenticode signing
only affects SmartScreen/user trust, not the update-integrity guarantee below — treating it as a
soft, addable-later layer rather than a blocking dependency avoided coupling #249 to an external
approval timeline.

**Release channel: GitHub Releases.** `docs/licensing.md`'s existing analysis already established
GitHub Releases satisfies LGPL §6(d)'s same-place source-availability condition for `rawler`'s
LGPL-2.1 grant — no separate release-hosting research needed here.

**Update integrity: minisign, independent of Authenticode.** Every published installer is signed
with a minisign key held only in a GitHub Actions secret; the public key is compiled into
`nicti.exe` (`crates/nicti-shed`). `nicti-shed` refuses to run any downloaded installer whose
minisign signature doesn't verify — this holds whether or not that release also carries an
Authenticode signature, so update security doesn't regress to "trust whatever SignPath said was
okay" or degrade further to nothing while SignPath approval is pending. Chose minisign
(`minisign-verify` crate, pure Rust, MIT) over rolling a raw Ed25519 check by hand: it's a
purpose-built, widely-used format for exactly this (release-artifact signing) with an existing
verifier crate that's trivial to license-clear.

**Version source of truth: the root `nicti` package's `Cargo.toml` version.** Every other
workspace crate stays pinned at `0.0.0` (internal, `publish = false` per ADR-0019 §8's package
map) — only the shipped binary carries a real version. The release workflow fails a tag push if
the tag doesn't match that version, so "what version did we ship" always has one answer.

## Consequences

- Unblocks distributing a real installer instead of asking every user to build from source.
- v1 ships unsigned until the SignPath Foundation application (tracked as a follow-up issue, not
  part of #249's own scope) is approved — a real, disclosed UX cost (a SmartScreen click-through)
  accepted deliberately rather than blocking the whole feature on it.
- No privileged updater service exists — an update always needs the app itself, or a scheduled
  background check while the app isn't running, to trigger. A true "update while the app is
  closed and the user isn't looking" story is deferred to the per-machine v2 follow-up.
- The minisign secret key is the actual trust root for every future auto-update; its GitHub Actions
  secret (`NICTI_UPDATE_MINISIGN_KEY_B64`, an unencrypted key -- `rsign2`'s signing step has no
  non-interactive way to unlock a password-protected one, see `docs/releasing.md`) is the single
  point of compromise that would let an attacker ship a malicious "update" — this is the same
  trust model any auto-updater has (Chrome's own Omaha protocol root key, Squirrel's code-signing
  requirement), not a weaker one, but worth stating plainly since nothing else in this repo holds
  a comparably sensitive secret today.
- New crates in the dependency graph (`crates/nicti-shed`): `semver`, `minisign-verify`, `ureq`
  (Windows-only, `native-tls` feature). See `docs/licensing.md`'s dated entry for this ticket.
