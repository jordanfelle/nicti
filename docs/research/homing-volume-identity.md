# #71: Volume identity + drive remapping (`spikes/homing`)

See `docs/adr/0020-volume-identity-and-remapping.md` for the decision and its rationale. This
document is the write-up: what was built, what was measured, and what's still pending.

## Sandbox constraint

This research pass ran in a Linux/WSL sandbox: no Windows toolchain, no mountable NTFS volume, no
way to attach/detach a real or virtual drive. `spikes/homing`'s Windows-only modules
(`volume::windows_impl`, `mount_events::windows_impl`) are **written but unverified** — they
type-check against `windows-sys`' documented API shapes (`FindFirstVolumeW`/`FindNextVolumeW`,
`GetVolumeInformationW`, `FSCTL_GET_NTFS_VOLUME_DATA` via `DeviceIoControl`,
`IOCTL_DISK_GET_PARTITION_INFO_EX`, `GetDriveTypeW`) but have never been compiled, let alone run
against a real volume. This is the same shape as ADR-0006/0007/#90's own "spec + tooling merged,
baseline measurement deferred" precedent — flagged explicitly here rather than glossed over.

What *is* real, from this sandbox:

- `spikes/homing`'s cross-platform modules (`path.rs`, `schema.rs`, `fingerprint.rs`, `relink.rs`,
  and `volume::identity_key`'s pure selection logic) compile and pass **20/20 unit tests**.
- `cargo clippy -p homing --all-targets --all-features -- -D warnings`: clean.
- The full workspace sweep with `homing` added — `cargo fmt --all -- --check`, `cargo clippy
  --workspace --exclude den --exclude pelt-egui --exclude pelt-iced --exclude pelt-slint --exclude
  retina --all-targets --all-features -- -D warnings`, and the matching `cargo test` — all clean.
  `homing` is **not** path-gated into its own CI job the way `den`/`pelt-*`/`retina` are: it needs
  no heavy native build (no bundled C++ library, no GUI-framework stack), the same reasoning that
  keeps `sniff` in the normal `clippy`/`test` jobs.
- `cargo deny --workspace --all-features check licenses`: `licenses ok`, no new `deny.toml` entry
  needed for any of `homing`'s dependencies (see `docs/licensing.md`'s 2026-09-26 update).

## What the spike measures (design, not yet run against hardware)

### Volume-identity candidates (`volume.rs`)

`VolumeInfo` collects every candidate field: the mount-manager's `\\?\Volume{GUID}` path, current
mount point(s), the 32-bit `GetVolumeInformationW` serial, the 64-bit NTFS serial
(`FSCTL_GET_NTFS_VOLUME_DATA`), the GPT partition GUID or MBR signature+offset
(`IOCTL_DISK_GET_PARTITION_INFO_EX`), label, size, removable flag, and an optional
`.nicti-volume` marker-file UUID. `identity_key()` implements ADR-0020's chosen key (NTFS
serial + GPT GUID, falling back to MBR signature+offset+32-bit serial) — this selection logic
itself is pure and fully unit-tested (see `volume::tests`), independent of whether the Windows FFI
that populates `VolumeInfo` has been validated.

### Mount-change detection (`mount_events.rs`)

`poll_for` wraps `sysinfo::Disks` at a configurable interval, diffing the mount-point set between
polls. `watch_push` (the `CM_Register_Notification` path) is an explicit unimplemented stub, not a
silent gap — see ADR-0020's Consequences for why this wasn't finished in this pass (a message pump
or careful callback-lifetime handling is needed, deferred to the reference-machine pass rather
than blocking the schema/fingerprint work this ticket also covers).

### Schema (`schema.rs`)

Three tables — `volume`, `root`, `asset` — with `resolve()` mapping an asset id to a live absolute
path through the currently-mounted-volumes map, returning `None` when the owning volume is
offline. Tested: idempotent volume upsert across a simulated letter change, offline volumes
retaining their `asset` rows (ratings/keywords/edits are never deleted just because a drive is
unplugged), and `resolve()` correctly following a remapped mount point.

### Fingerprint tiers (`fingerprint.rs`)

Four tiers as designed in ADR-0020. Tested: partial-hash determinism, the *expected* partial-hash
blind spot (two files with identical 64KB edges but a differing middle above the window size are
indistinguishable by partial hash alone — asserted as a documented limitation, not a false
guarantee), full-hash determinism and sensitivity to that same differing middle, and
`natural_key()` correctly returning `None` for a file with no EXIF data at all.

### Relink scenarios (`relink.rs`)

`build()` walks a folder tree into the schema; `move_root_to_volume()` re-parents a `root` row
between two known volumes (the "moved from SSD to archive" case) leaving every `asset` row
untouched — tested directly, confirming the schema's three-level design actually delivers the
single-row-update property #72 needs. `relink_against_unknown_volume()` matches offline assets
against a freshly-scanned unrecognized volume's file listing by partial-hash fingerprint (not by
path, since the scenario is explicitly "moved to a different relative path on the new volume") —
tested for both the successful-match and no-match-found (`Lost`) cases.

## Pending: the reference-machine pass

Everything below needs the RTX 5080/Windows box — see ADR-0020's Measured-results section for the
exact tables to fill in:

1. **Volume-identity survival table** — run `spikes/homing/scripts/remap-test.ps1` (VHDX-only, per
   this session's scope decision — no physical-drive step) and record which fields survive a
   letter change, detach/reattach, a folder-mount-point remount, a reformat, and a duplicated
   (cloned) VHDX attached twice.
2. **Mount-detection latency/CPU** — `homing watch --backend poll --interval-secs 1` and `--
   interval-secs 5` during a real attach/detach; `CM_Register_Notification` push detection is
   unimplemented this pass (see above), so only the polling numbers can be filled in from this
   ticket alone.
3. **Fingerprint cost** — `homing bench <dir>` against a stratified sample walked directly from
   the live library (not a `ref-10k` copy — that reference set was retired mid-research, #136),
   on both the NVMe and HDD, plus a check of whether a real LRC-edited DNG's full-file hash
   actually goes unstable across a metadata rewrite as hypothesized.

Once those are filled in, move ADR-0020 from Proposed to Accepted (or revise the Decision if a
measurement contradicts it — same discipline as every reference-machine-pending ADR in this repo).

## Follow-ups filed, not solved inline

- Real per-body Nikon MakerNote shutter-count extraction (this pass's EXIF natural key uses the
  standard `ImageNumber` tag, 0xA306, as the closest available proxy — not a body-specific
  MakerNote offset table).
- An explicit ambiguity guard for two mounted volumes reporting the same identity key (a disk
  clone, or two attached copies of the same VHDX) — `schema::upsert_volume`'s current
  `ON CONFLICT DO UPDATE` collapses them by construction today.
- `CM_Register_Notification` push-based mount detection (stubbed, not implemented).
- ADR-0011's facet-count cache excluding offline volumes — flagged for #22/#23 to pick up.
