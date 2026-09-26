# #71: Volume identity + drive remapping (`spikes/homing`)

See `docs/adr/0020-volume-identity-and-remapping.md` for the decision and its rationale. This
document is the write-up: what was built, what was measured, and what's still pending.

## Sandbox constraint

This research pass ran in a Linux/WSL sandbox: no Windows machine, no mountable NTFS volume, no
way to attach/detach a real or virtual drive. `spikes/homing`'s Windows-only modules
(`volume::windows_impl`, `mount_events::windows_impl`) were initially **written but unverified**,
until this session found the sandbox does have a `x86_64-pc-windows-gnu` cross-compile path
(rustup's own toolchain, separate from the Homebrew `rustc` otherwise used) and used it as a real
compiler check, not just a type-shape guess: `cargo check`/`cargo clippy --target
x86_64-pc-windows-gnu -p homing --all-targets --all-features -- -D warnings` are both clean.

This caught real bugs on the first attempt — before this cross-compile check existed, real GitHub
Actions CI on this PR's `windows-latest` runner failed with the same two compile errors this
section describes, confirming the cross-compile check reproduces CI faithfully rather than being a
weaker local proxy:

- `windows_sys::Win32::Storage::FileSystem::DRIVE_REMOVABLE` doesn't exist — that constant lives
  in `Win32::System::WindowsProgramming`, a different module (and Cargo feature) than
  `GetDriveTypeW` itself, which is defined in `FileSystem`.
- `PARTITION_INFORMATION_EX`'s MBR arm (`PARTITION_INFORMATION_MBR`) has no `Signature` field at
  all — only `PartitionType`/`BootIndicator`/`RecognizedPartition`/`HiddenSectors`/`PartitionId`.
  The MBR disk signature is a **whole-disk** property (`DRIVE_LAYOUT_INFORMATION_MBR::Signature`),
  returned by a different IOCTL (`IOCTL_DISK_GET_DRIVE_LAYOUT_EX`) against the owning
  `\\.\PhysicalDriveN` device handle, found via `IOCTL_STORAGE_GET_DEVICE_NUMBER` on the volume
  handle — not a per-partition field on `IOCTL_DISK_GET_PARTITION_INFO_EX`'s response. Fixed by
  adding `volume::windows_impl::disk_number_for`/`mbr_disk_signature`.

**What cross-compiling still cannot prove**: none of this exercises the actual Win32 API calls
against real hardware — no volume was ever enumerated, no `DeviceIoControl` call ever executed, no
drive was ever attached/detached/reformatted. The type/shape-correctness gap (does this code even
compile against the real windows-sys API) is now closed; the runtime-behavior gap (does
`FindFirstVolumeW` actually enumerate what's expected, does the survival table hold, does the new
two-IOCTL `mbr_disk_signature` chain actually return the right disk's signature) is not, and stays
exactly the "spec + tooling merged, baseline measurement deferred" shape ADR-0006/0007/#90
describe — flagged explicitly here rather than glossed over.

What *is* real, from this sandbox:

- `spikes/homing`'s cross-platform modules (`path.rs`, `schema.rs`, `fingerprint.rs`, `relink.rs`,
  and `volume::identity_key`'s pure selection logic) compile and pass **28/28 unit tests**.
- `cargo clippy -p homing --all-targets --all-features -- -D warnings`: clean on both the native
  Linux target and the `x86_64-pc-windows-gnu` cross-compile.
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

## Adversarial review

A fresh review pass (before opening the PR, per this repo's standing rule) attacked the code that
*was* compiled and tested here, not just the flagged-unverified Windows FFI, and found six real
issues, all fixed before merge:

- **Reconnect bug**: `schema::mark_offline_except` only ever cleared `volume.online`, never set it
  back — a volume that went offline once stayed stuck offline after a real reconnect, since
  `cmd_resolve`'s poll-and-resolve loop never calls `upsert_volume` (the only other path that set
  `online = 1`). Fixed to flip both directions in one call; covered by a new test
  (`mark_offline_except_brings_a_reconnected_volume_back_online`).
- **False-positive relink**: `relink_against_unknown_volume` had no candidate de-duplication — two
  offline assets sharing a fingerprint+size (a genuine duplicate photo) could both silently resolve
  to the same candidate file. Fixed with a `claimed`-paths set, processed in a deterministic
  `ORDER BY a.id`; covered by a new test.
- **`schema::resolve`'s error handling** conflated "asset not found" with a real DB error (both
  collapsed to `Ok(None)` via `.ok()`) — fixed to match specifically on
  `rusqlite::Error::QueryReturnedNoRows` and propagate everything else.
- **`ntfs_volume_data`'s FFI was structurally wrong**, independent of being unverified: it opened a
  directory (the mount point) rather than a volume-device handle, which `FSCTL_GET_NTFS_VOLUME_DATA`
  requires per its own documentation. Fixed to open the trimmed volume-GUID path instead, matching
  `partition_info`'s already-correct pattern.
- **Silent fingerprint-hash failures** (a locked/corrupt file during import) were indistinguishable
  from "no fingerprint requested" — added `BuildStats::fingerprint_failures` to track them
  separately.
- **`fingerprint::SizeNameKey` (tier a) was dead code**, never actually wired into the relink path
  despite the design intent described in `fingerprint.rs`'s own doc comment. Wired in as the
  last-resort match for assets with no fingerprint at all (never computed, or a hashing failure),
  and as a size-bucketing index that also narrows the fingerprint-tier fallback scan instead of a
  full linear scan over every candidate file. A new `ResolveOutcome::RelinkedBySizeName` variant
  keeps this weaker signal distinguishable from a fingerprint-confirmed match; covered by a new
  test.

Not fixed, correctly left as an open follow-up (see below): the volume-level identity-ambiguity
guard (two *mounted volumes* sharing an identity key) is a different gap from the relink-level
candidate-dedup bug above, and remains unimplemented.

## CodeRabbit review

CodeRabbit's automated PR review found 9 actionable issues against the PR as it stood after the
adversarial-review fixes above (plus the real Windows CI compile failures this session found and
fixed separately — see the Sandbox constraint section). Each was independently verified against
the current code before fixing, not applied blindly:

- **`current_volume_for`'s mount-point matching** (`main.rs`) had two real bugs: a bare
  `starts_with` matched sibling paths sharing a prefix that wasn't a real path-component boundary
  (e.g. mount point `C:/Mount` would also match `C:/MountOther`), and `Path::canonicalize` on
  Windows can return a verbatim path (`\\?\H:\...`) that never lined up with the enumerated mount
  points at all. Fixed: strip the verbatim prefix, require a full path-component match, and pick
  the longest matching mount point among any genuine ties.
- **`homing relink` never persisted a match** — it only printed the result, so a subsequent
  `homing resolve` still read each asset's stale (offline volume's) `root_id`/`rel_path` and
  reported it unresolved forever, defeating the whole point of relinking. Fixed:
  `schema::relink_asset` re-points a matched asset at a new `root` (registered lazily for
  `scan_dir`, under whichever volume currently owns it) and the candidate's relative path.
- **One unreadable candidate aborted the entire relink scan** — `cmd_relink` used `?` on
  `fingerprint::partial_hash`, so a single locked/corrupt file on the unrecognized volume prevented
  every other candidate from getting a chance to match. Fixed: skip that candidate and continue.
- **Relink could never match a full-tier asset** — `homing build --tier full` stores a full-file
  hash on the asset, but `cmd_relink` only ever computed a partial hash for candidates, so the two
  values could never be equal. Fixed: compute both hash tiers per candidate (relink-time-only
  cost, not routine import) and match against either.
- **`mount_events.rs`'s doc comment** named a nonexistent `--backend notify` flag; the real flag
  value is `push` (`WatchBackend::Push`). Fixed, text-only.
- **`by_size_name`'s `or_insert_with` collapsed genuine duplicates to one winner**, non-
  deterministically (`HashMap` iteration order decided which asset "won" the shared slot) — two
  fingerprint-less assets sharing size+filename (e.g. the same shot's filename from two camera
  bodies) could see one falsely reported `Lost` even when a second real candidate existed. Fixed:
  store every candidate path per `SizeNameKey`, sorted, and pick the first unclaimed one.
- **`schema::migrate` wasn't idempotent** — plain `CREATE TABLE`/`CREATE INDEX` fail with "table
  already exists" on a second call against an existing catalog file, which `cmd_build` does on
  every invocation. This would have broken `remap-test.ps1`'s own repeated-command workflow
  against one `homing.sqlite3`. Fixed: `IF NOT EXISTS` on every statement.
- **`remap-test.ps1` two script-robustness gaps**: native command failures (`diskpart`,
  `homing.exe`) weren't checked (`$ErrorActionPreference = "Stop"` doesn't cover native exit
  codes), and Step 6 (folder-only mount test) never removed the volume's existing drive-letter
  access path, so `homing resolve` could pass via the leftover letter without actually proving
  folder-only resolution — with Step 8 needing a drive letter restored afterward, since it filters
  partitions by `DriveLetter`. Both fixed; the script remains otherwise unrun (see Sandbox
  constraint above).

Every fix above is covered by a new or extended unit test (26 total, up from 20 after the
adversarial-review round). None of CodeRabbit's 9 findings were dismissed as invalid.

**Second pass** (against the fix commit above) found 2 more, both real, both fixed:

- **The new `root` registered for a relink's `scan_dir` always used an empty relative path** —
  correct only when `scan_dir` *is* the volume root. If it's a subfolder (e.g. `H:\Photos`), the
  stored chain would resolve to `H:\<rel>` instead of `H:\Photos\<rel>`, a wrong path. Fixed with a
  new `root_rel_path_for` helper (mirrors `current_volume_for`'s own path-normalization logic),
  covered by two new tests.
- **`mbr_disk_signature`'s buffer had two real bugs**, not just an insufficient-size guess:
  `IOCTL_DISK_GET_DRIVE_LAYOUT_EX` doesn't reliably report the buffer size actually needed on
  failure (per Microsoft's own documentation — no dependable `ERROR_INSUFFICIENT_BUFFER`-plus-size
  contract), so a fixed 4-partition-slot guess could fail outright on a larger real MBR disk; and
  the `Vec<u8>` buffer being cast to `*const DRIVE_LAYOUT_INFORMATION_EX` was never alignment-safe
  in the first place (`Vec<u8>` only guarantees byte alignment). Fixed: a growing retry loop
  (double the buffer on any failure, up to 8 attempts) over a properly-aligned
  `Vec<DRIVE_LAYOUT_INFORMATION_EX>` buffer instead.

## Follow-ups filed, not solved inline

- Real per-body Nikon MakerNote shutter-count extraction (this pass's EXIF natural key uses the
  standard `ImageNumber` tag, 0xA306, as the closest available proxy — not a body-specific
  MakerNote offset table).
- An explicit ambiguity guard for two mounted volumes reporting the same identity key (a disk
  clone, or two attached copies of the same VHDX) — `schema::upsert_volume`'s current
  `ON CONFLICT DO UPDATE` collapses them by construction today.
- `CM_Register_Notification` push-based mount detection (stubbed, not implemented).
- ADR-0011's facet-count cache excluding offline volumes — flagged for #22/#23 to pick up.
