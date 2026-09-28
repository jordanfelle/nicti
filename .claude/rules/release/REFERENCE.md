---
paths:
  - "packaging/**"
  - ".github/workflows/release.yml"
  - "crates/nicti-shed/**"
  - "docs/releasing.md"
---

# Release, Signing, and Auto-Update — Quick Reference

Full reasoning/history: `docs/decisions/release.md`.

- **Windows installer (#249/ADR-0249)**: per-user NSIS (`packaging/windows/nicti.nsi`),
  `RequestExecutionLevel user`, installs to `%LOCALAPPDATA%\Programs\Nicti`. **No UAC ever** —
  this is Chrome's non-admin mode, chosen specifically so the in-app updater can re-run the
  installer silently. Not the LRC/Chrome admin-mode model (per-machine + a privileged updater
  service) — that's a v2 follow-up, not this ticket.
- **Code signing: SignPath Foundation** (free OSS), gated on a `SIGNPATH_ENABLED` repo variable
  in `.github/workflows/release.yml` — **v1 ships unsigned** (SmartScreen warning) until that
  external application is approved. Flip the variable once it lands; don't block other release
  work on it.
- **Release channel: GitHub Releases** — already established as LGPL §6(d)-compliant for
  `rawler`'s grant, see the `licensing` topic.
- **Update trust anchor: minisign, not Authenticode.** `crates/nicti-shed` verifies every
  downloaded installer against a public key compiled into the binary
  (`minisign-verify` crate) before ever executing it — this holds regardless of whether
  Authenticode signing is on yet. The private minisign key (generated **unencrypted**, `rsign
  generate -W` — `rsign2` can't unlock a password-protected key non-interactively, so CI couldn't
  sign with one at all) lives only in the `NICTI_UPDATE_MINISIGN_KEY_B64` GitHub secret; it is the
  single point of compromise for the whole update channel.
- **Version source of truth**: root `nicti` package's `Cargo.toml` `version` field. Every other
  workspace crate stays `0.0.0` (`publish = false`). `release.yml` fails a tag push if the tag
  doesn't match it.
- **`crates/nicti-shed`** (the updater, named for a cat shedding its old coat): pure
  version-compare/asset-pick/minisign-verify logic is cross-platform and unit-tested on Linux CI;
  the actual HTTP check + download + re-exec-installer path is `cfg(windows)`-only (`ureq` +
  `native-tls`, so it rides the OS trust store — no `webpki-roots`, which isn't on `deny.toml`'s
  allowlist). Auto-check state: `%LOCALAPPDATA%\Nicti\update.json`, at most once per 24h.
- **Edge channel (#267)**: every push to `main` also rebuilds and republishes a single moving
  `edge` prerelease at that commit — same installer/minisign pipeline as a real release, gated on
  `github.ref == 'refs/heads/main'` instead of a tag, with SignPath (Authenticode) signing
  explicitly excluded via `&& github.ref_type == 'tag'` on all four of its steps, so edge stays
  minisign-only even once SignPath is enabled. `--prerelease` keeps it out of
  `/releases/latest`, which is what `nicti-shed`'s auto-updater polls, so it can never get
  auto-installed over a stable install. No separate `develop` branch: `main` only ever advances
  via a reviewed, CI-passing PR merge already, so it's already the "always good, frequently
  updated" branch a `develop` branch would otherwise exist for. **The `edge` release/tag is
  force-moved and reused, never deleted** (four rounds of CodeRabbit review on PR #268 — see the
  workflow's own comments for the full list: a delete-then-recreate approach's availability gap
  and stale-tag risk, stale-asset accumulation across version bumps, a wrong plural/singular
  tag-ref GET endpoint, a concurrent-run race fixed by serializing main-push runs via
  `cancel-in-progress: false`, and asset uploads staged under a temp name then server-side
  renamed into place instead of `--clobber`, since `--clobber` itself deletes-then-uploads each
  same-named asset non-atomically — the case that hits on every ordinary push, not just a
  version bump). **Known limit**: a GitHub Actions concurrency group holds one running + one
  pending run, not an unbounded queue — a third push landing while two are already in flight
  discards the *second* push's pending run (the channel still converges on the true tip of main
  once the burst settles; it just isn't guaranteed to publish one distinct build per individual
  merge under a rapid burst). Accepted rather than building a real external queue, given how
  infrequently this repo sees multiple main merges within one ~10-15 minute Windows build.
- See `docs/releasing.md` for the actual cut-a-release runbook and the one-time minisign
  key-generation steps.

## Package contents

- `packaging/windows/nicti.nsi` — the installer script, not a crate.
- `.github/workflows/release.yml` — tag-triggered build+sign+publish; `workflow_dispatch` runs the
  same steps as a dry run (uploads a workflow artifact instead of publishing).
- `crates/nicti-shed` — `check.rs` (GitHub releases/latest parse + semver compare, cross-platform),
  `verify.rs` (minisign verification), `net.rs` (Windows-only download + verify + re-exec),
  `state.rs` (the 24h auto-check throttle).
