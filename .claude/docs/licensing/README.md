## Licensing

Covers the third-party license policy (and its 2026-09-24 amendment) and the outbound AGPL-3.0-or-later decision.

- **Third-party license policy**: `docs/adr/0003-third-party-license-policy.md`, backed by the
  full per-dependency/per-model audit in `docs/licensing.md` — Rust crate allowlist, ML-model
  bundle-vs-on-demand-download criteria, and the "no Adobe DCP/LCP data" rule. **Amended
  2026-09-24** (see ADR-0003's own Amendments section, load-bearing not historical): the original
  LGPL-native-lib dynamic-linking rule and the blanket GPL/AGPL denial are both superseded now that
  Nicti's own outbound license is decided (see the ADR-0013 bullet below) — read the amendment,
  not just the original 2026-09-23 Decision text. Update `docs/licensing.md` in the same PR as any
  new dependency or model.
- **Outbound license: AGPL-3.0-or-later** — `docs/adr/0013-outbound-license-agpl.md`, resolving
  #66 (open-source release prep) early because ADR-0003's permissive-only default was already
  actively constraining in-flight decisions. Chosen specifically over plain GPL-3.0 for the
  network-use clause (§13) — closes the "run it as a hosted service, never share the source"
  loophole plain GPL leaves open, which matters given #58 (web gallery/upload) and #64
  (multi-machine catalog) are real planned v2 network-facing features, not hypothetical ones.
  Un-excludes Ultralytics YOLO (culling/detection) and exiv2/rexiv2 (EXIF/XMP/IPTC, though
  kamadak-exif/little_exif remain the current unforced choice) on license grounds; removes the
  LGPL-as-Cargo-dependency sign-off/`cdylib`-isolation requirement **for `lensfun-rs` specifically**
  (its confirmed `LGPL-3.0-or-later OR GPL-3.0` dual license combines cleanly now that Nicti's own
  license is already copyleft) — **but not for `rawler`**, whose bare `license = "LGPL-2.1"` (no
  `-only`/`-or-later` suffix, and no project-specific evidence either way beyond that) could still
  mean GPL-2.0-only if relicensed, which this same amendment denies; #37 still needs to resolve
  that before treating rawler as pre-cleared. **#37 found LibRaw itself has the identical
  ambiguity** (its own per-file header is a bare "version 2.1" too) — its CDDL-1.0 arm is
  unambiguous but GPL-incompatible, so it can't substitute; see `raw-decoder` topic's own doc for
  the full account. Reopens RapidRAW (#69) as a potential adopt/fork candidate, not just prior-art
  study, since it's also AGPL-3.0 — see the new tickets filed alongside this ADR for follow-up.
  **#37 found RapidRAW doesn't actually decode HE/HE\* either** (its own rawler fork still rejects
  it, silently falling back to the embedded JPEG on Windows), so it isn't a shortcut past #37's
  own decoder work.
