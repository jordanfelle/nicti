# Cutting a Nicti release

See [ADR-0249](adr/0249-windows-installer-and-updates.md) and
[`.claude/rules/release/REFERENCE.md`](../.claude/rules/release/REFERENCE.md) for the design this
implements.

## Cutting a release

1. Bump the `version` field in the root `Cargo.toml` (the `nicti` package) — every other
   workspace crate stays `0.0.0`.
2. Open a PR with just that version bump (plus a `CHANGELOG`-style note in the PR description if
   there's anything worth calling out — `gh release create --generate-notes` in the workflow
   already generates notes from merged PR titles, so this is optional color, not required).
3. Once merged, tag the resulting commit on `main`:
   ```bash
   git tag v0.2.0  # matching Cargo.toml exactly, with a leading "v"
   git push origin v0.2.0
   ```
4. Pushing the tag triggers `.github/workflows/release.yml`, which builds `nicti.exe`, builds the
   NSIS installer, signs it (Authenticode via SignPath if `SIGNPATH_ENABLED` is on, minisign
   always), and publishes a GitHub Release with the installer + `.minisig` + `.sha256`.
5. Before tagging, it's worth a `workflow_dispatch` dry run first (same workflow, no tag) — it
   runs every step except the actual `gh release create`, and uploads the installer as a workflow
   artifact instead, so a build/signing problem surfaces before a real tag is pushed.

## One-time setup: generating the minisign signing key

This only needs to happen once per key generation/rotation — the private key then lives only in
GitHub Actions secrets, never in the repo.

```bash
cargo install rsign2 --locked
rsign generate -p nicti-update.pub -s nicti-update.key -W
```

`-W`/`--passwordless` generates an **unencrypted** secret key, deliberately: `rsign2`'s `sign`
subcommand has no non-interactive way to supply a password (it reads directly from `/dev/tty`,
ignoring redirected stdin — confirmed this fails outright in a real CI run, which has no
controlling tty at all), so a password-protected key simply cannot be used from
`release.yml`'s automated signing step. The real secret boundary is the GitHub Actions secret
store itself, not an extra password on top of it — same trust model as any other CI-held signing
key.

This produces:
- `nicti-update.pub` — a minisign public key file. Copy its base64 key string (the second line)
  into `crates/nicti-shed`'s embedded public-key constant. **This is the only artifact that gets
  committed to the repo.**
- `nicti-update.key` — the private signing key. Base64-encode it and store it as the
  `NICTI_UPDATE_MINISIGN_KEY_B64` repository secret. **Never commit this file.** Delete the local
  copy once the secret is set.

Rotating this key means: generate a new keypair, then have `crates/nicti-shed` accept a signature
from *either* the old or the new public key (`verify_installer` tried against each in turn) for
every release from the transition point on. There is no safe point to drop the old key on an
"enough clients have updated" estimate -- a client that's still on the old key and, for whatever
reason, misses the transition release entirely (skipped a check, was offline, auto-check was off)
would otherwise be stuck: every later release is signed only with the new key, which its still-old
binary can't verify, so it can never update itself again without a manual reinstall. The old key
only gets dropped once a version is reached that the update-check flow itself no longer needs to
support (e.g. a hard minimum-supported-version cutover, communicated well in advance), not on a
guess about install-base coverage. No rotation has happened yet as of this writing.

## Enabling SignPath (Authenticode) signing

Not yet done — tracked as a follow-up issue to #249, since it depends on SignPath Foundation's own
external approval process. Once approved:

1. Set up the `nicti` project and a `release-signing` signing policy in the SignPath dashboard,
   matching the `project-slug`/`signing-policy-slug` values `release.yml` already references.
2. Add the `SIGNPATH_API_TOKEN` secret and `SIGNPATH_ORG_ID` repository variable.
3. Set the `SIGNPATH_ENABLED` repository variable to `true`.

No code change is needed beyond that — the workflow's signing steps are already gated on
`SIGNPATH_ENABLED` and will start running on the next release.
