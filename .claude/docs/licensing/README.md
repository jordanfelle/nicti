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
  LGPL-as-Cargo-dependency sign-off/`cdylib`-isolation requirement **for `lensfun-rs`** (its
  confirmed `LGPL-3.0-or-later OR GPL-3.0` dual license combines cleanly now that Nicti's own
  license is already copyleft) — **and, corrected 2026-09-25 by #37, for `rawler`/LibRaw too.** An
  earlier draft of this amendment treated their bare `LGPL-2.1` grants (no `-only`/`-or-later`
  suffix) as blocking, reasoning that LGPL-2.1 §3's relicense-to-GPL option would force a
  GPL-2.0-only result absent an explicit "or-later" grant, which this project's AGPL-3.0-or-later
  license can't combine with. **That reasoning doesn't hold up against the actual license text**:
  §3 lets whoever exercises it pick *any* GPL version that exists at the time (its own text: "if a
  newer version than version 2 of the ordinary General Public License has appeared, then you can
  specify that version instead if you wish"), independent of the code's own "-only"/"-or-later"
  wording. More importantly, **§3 isn't even the applicable mechanism** — it's an opt-in act for
  redistributing a *modified copy of the library itself* under GPL terms, not something linking
  triggers. The actual provision, **LGPL-2.1 §§5–6, directly permits combining an LGPL-2.1 library
  into a differently-licensed larger work** (this is exactly what LGPL is designed for), 
  conditioned only on a notice + source-availability obligation for the LGPL'd portion — already
  satisfied, since Nicti vendors full source of both. FSF's own license-compatibility page
  corroborates: LGPLv2.1 is "compatible with GPLv2 and GPLv3." No relicensing, no GPL-version
  question, no "-or-later" grant needed. See `docs/licensing.md`'s Flags §2 for the full
  gnu.org-sourced citation trail; `raw-decoder` topic's own doc has the RAW-decoder-specific
  account. What remains is packaging mechanics (satisfying §6's condition before shipping), not a
  licensing blocker. Reopens RapidRAW (#69) as a potential adopt/fork candidate, not just prior-art
  study, since it's also AGPL-3.0 — see the new tickets filed alongside this ADR for follow-up.
  **#37 found RapidRAW doesn't actually decode HE/HE\* either** (its own rawler fork still rejects
  it, silently falling back to the embedded JPEG on Windows), so it isn't a shortcut past #37's
  own decoder work.
