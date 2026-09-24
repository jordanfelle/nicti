# ADR-0013: Nicti's outbound license — AGPL-3.0-or-later

- **Status:** Accepted
- **Date:** 2026-09-24
- **Ticket:** [#66](https://github.com/jordanfelle/nicti/issues/66) v2: open-source release prep (license-choice portion resolved early — see Context)

## Context

ADR-0003 deliberately did not pick Nicti's own outbound license — its own words: "This ADR doesn't
pick Nicti's own license; it sets the policy for what Nicti may depend on and bundle, evaluated
against both realistic outbound-license families, so the #66 choice isn't constrained later by a
dependency already baked in." That left ADR-0003's actual enforced policy (`deny.toml`, the
Rust-crate allow/deny list) defaulting to permissive-only, denying GPL/AGPL outright, since a
permissive-or-copyleft split needed *a* default to be CI-enforceable before the real choice landed.

The choice has now been made, ahead of #66's own v2 timeline, because the policy default above was
actively blocking real decisions in flight (a RapidRAW prior-art evaluation, #69; culling-detector
research; RAW/XMP library candidates already flagged copyleft-adjacent in `docs/licensing.md`).
Resolving it now, rather than waiting for #66's full release-prep pass, avoids more dependencies
getting evaluated and decided against a policy default that was never meant to be permanent.

## Decision

**Nicti ships under AGPL-3.0-or-later.** The explicit goal, in the words behind this decision: the
project should be free forever, and no one — including a hosted-service operator — should be able
to take it proprietary or ship a closed derivative. Plain GPL-3.0 only closes that door for
distributed binaries; it does not cover the case of someone running a modified Nicti as a network
service without ever distributing the binary. AGPL-3.0's §13 closes that gap. `-or-later` is kept
open (not pinned to exactly v3.0) so a future FSF license revision doesn't require a fresh relicense
decision, consistent with how most GPL-family projects grant their own license.

## Consequences for ADR-0003's policy

ADR-0003's Rust-crate/native-library allow-deny rules existed specifically to keep options open
while the outbound license was undecided. With AGPL-3.0-or-later now decided, several of those
rules are the wrong default going forward — **see ADR-0003's own Amendments section for the actual
policy changes**, made directly there rather than duplicated here, so ADR-0003 stays the single
source of truth for what's allowed. Summary of what changes, in brief:

- GPL-3.0-or-later and AGPL-3.0-or-later dependencies become allowed outright (previously denied).
- GPL-2.0-or-later dependencies become allowed (the "-or-later" grant permits combining with a
  GPLv3-family work) — but **GPL-2.0-only** (no "or later" arm, and no separate compatible OR-arm)
  remains genuinely incompatible and stays denied; this is a real per-license check, not a blanket
  "any GPL is now fine" rule. Verify the exact grant text — not just a bare "GPL-2.0" label, and
  not a project's bundled `COPYING`/`LICENSE` file either (usually the FSF's own unedited template,
  not project-specific evidence) — before clearing anything against this ADR. `docs/licensing.md`'s
  exiv2 entry is a worked example of checking this precisely (its own source's `SPDX-License-
  Identifier: GPL-2.0-or-later` headers and README confirm the `-or-later` grant).
- The LGPL-as-Cargo-dependency dynamic-linking safe-harbor requirement (ADR-0003's `rawler`/
  `lensfun-rs` flag) is **only partly resolved — the two crates are not in the same state.** That
  safe harbor exists to protect users of a *permissively-licensed or proprietary* combined work
  from having the whole program's source forced open by a statically-linked LGPL component.
  Nicti's own license is now already copyleft (AGPL-3.0-or-later) — LGPL explicitly permits
  relicensing to "the ordinary GPL," but only to whichever GPL version the library's own grant
  actually permits, not automatically to whatever version the combiner wants. `lensfun-rs`'s dual
  license (`LGPL-3.0-or-later OR GPL-3.0`) has a confirmed or-later arm, so it's genuinely
  unconditionally fine as an ordinary Cargo dependency now. **`rawler` is not resolved**: its
  `Cargo.toml` declares a bare `license = "LGPL-2.1"` (not valid SPDX without an `-only`/
  `-or-later` suffix), and its repo's `LICENSE` file is the unedited generic FSF template
  (confirmed — it still contains the template's own placeholder text), not project-specific
  evidence. If rawler's actual grant is LGPL-2.1-only, its GPL-relicensed form is GPL-2.0-only —
  denied under this same ADR. #37 must resolve that (an authoritative upstream answer, or a real
  per-file SPDX header) before dropping rawler's `cdylib`/out-of-process isolation requirement;
  #39 (`lensfun-rs`) no longer needs it.
- Ultralytics YOLO (AGPL-3.0, previously denied for ML model bundling) becomes allowed.

## New work this unblocks — see the tickets filed alongside this ADR

- **RapidRAW re-evaluation** (was prior-art-only under #69; now potentially adopt/fork-worthy,
  since it's also AGPL-3.0 — no license conflict in reusing its code directly). New ticket: see
  below.
- **Ultralytics YOLO** as a real culling/detection candidate (#34/#36's research scope).
- **exiv2 as a genuine third option** alongside kamadak-exif/little_exif for EXIF/XMP — not a
  forced switch (the existing permissive choice still works fine and isn't obviously worse), but no
  longer excluded on license grounds if a real reason to prefer it comes up.

## Options considered

| Option | Verdict |
|---|---|
| AGPL-3.0-or-later | **Chosen.** Strongest copyleft available; closes the network-service loophole plain GPL leaves open; matches the explicit "free forever" intent behind this decision. |
| GPL-3.0-or-later (no network clause) | Rejected — doesn't prevent a hosted-service fork from staying closed, which is exactly the gap AGPL exists to close, and Nicti has real planned network-facing v2 features (#58 web gallery/upload, #64 multi-machine catalog) where this gap would actually matter, not just a theoretical one. |
| Permissive (MIT/Apache-2.0) | Rejected — doesn't prevent a closed proprietary fork at all, the opposite of the stated goal. |
| Defer further (keep #66 as a pure v2 concern) | Rejected — the policy default was already actively constraining in-flight decisions (see Context); deferring further just meant more decisions getting made against a default nobody intended to keep. |

## Consequences

- `deny.toml`, ADR-0003, and `docs/licensing.md` are updated in this same PR — see their own diffs
  and ADR-0003's new Amendments entry for the specifics.
- #66 (open-source release prep) still has real remaining scope (contribution guide, plugin/
  extension-point SDK docs) — this ADR resolves only the license-choice portion of it early.
- Anyone adding a new dependency going forward checks it against ADR-0003's *amended* rules, not
  the original 2026-09-23 text alone — read the Amendments section, which is now load-bearing, not
  historical color.
