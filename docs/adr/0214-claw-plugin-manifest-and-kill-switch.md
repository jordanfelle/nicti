# ADR-0214: Claw v2 plugin manifest, disclosure UX, and kill switch (Collar/Hiss)

- **Status:** Proposed
- **Date:** 2026-09-27
- **Ticket:** [#214](https://github.com/jordanfelle/nicti/issues/214) Research: third-party plugin
  sandbox manifest + kill switch (Claw v2 security model)

## Context

ADR-0019 (Claw) settled v1 (first-party modules only, in-process Rust traits, no plugin-hosting
code shipped) and named `wasmtime`/WASM the recommended *direction* for v2 third-party plugins —
measured, not assumed, to be viable for non-hot-path extension points (exporters, catalog-adjacent
logic, stage parameter/config logic) but not for a third-party render stage's per-pixel loop
(§6, `wasm_vs_native.rs`). It deliberately left the security/consent model unscoped.

ADR-0218 (local-only AI) then made this ticket load-bearing, not merely a nice-to-have: it requires
that "network access must be an action-scoped capability the manifest declares... denied by
default during local inference/training, and a module that doesn't declare it must not get it at
all" (§4), scoped specifically to two actions — a user-initiated, checksummed weight download, and
a separately opted-in cloud-AI feature. This ADR is where that requirement gets a concrete shape.

Per the ticket's own non-goals: **this is not a sandboxing implementation.** It doesn't commit to
WASM vs. a cdylib capability check vs. something else — the manifest schema, disclosure UX, and
kill-switch *policy* here must hold regardless of which mechanism v2 eventually builds, though the
Enforcement section below shows a WASM-shaped mapping since that's ADR-0019's current direction.

## Decision

### 1. Manifest ("Collar")

Every third-party Claw module ships a manifest (`collar.toml`) in its package root, following
ADR-0019 §7's `Module` identity shape:

```toml
id = "vendor.module_name"          # namespaced, ADR-0019/0021 convention
version = "1.2.0"
host_api_version = 1               # Claw host contract this module was built against
extension_points = ["ModelProvider"]  # one or more of ADR-0019 §7's seven, EXCEPT RenderStage

[[capabilities]]
kind = "network"
scope = "weight-download"          # ADR-0218 §2/§3: "weight-download" | "cloud-feature"
allowed_hosts = ["cdn.vendor.example.com"]
justification = "Downloads the BiRefNet segmentation weights on first enable."

[[capabilities]]
kind = "filesystem"
scope = "user-selected"            # or "catalog-managed" — never ambient/whole-disk
justification = "Lets the user pick a folder of reference images for style matching."
```

**Every capability entry requires both a scope and a plain-language `justification` string**,
shown verbatim at consent time — not just for edge cases. This goes further than Figma's manifest
(the closest real precedent, but its `reasoning` field is only *conditionally* required — for a
wildcard `"*"` domain or an undeclared dev server, not for an explicit host allowlist entry[^1]);
Nicti requires it universally because a photo library is a materially more sensitive dataset than
a design file, and reviewer/user time is scarcer here than at Figma's scale.

**Capability kinds**, each host-enforced at a different boundary (see Enforcement below):
- `network` — action-scoped (`weight-download` | `cloud-feature` only, per ADR-0218), host-scoped
  allowlist. No general "internet access" grant exists.
- `filesystem` — `catalog-managed` (a handle the catalog already tracks) or `user-selected`
  (a path the user explicitly picked via an OS file dialog for this module); never an ambient
  path or whole-volume grant.
- `gpu` — needed for any module implementing a render-adjacent extension point.
- extension-point hooks — implicit from `extension_points`, not separately declared.
- resource budget — optional `fuel_limit`/`epoch_budget_ms`/`memory_mb_max` overrides tightening
  (never loosening) the host's own defaults.

**No entry, no access.** A capability the manifest doesn't declare is denied unconditionally —
there is no ambient authority, no "ask forgiveness" fallback.

**`RenderStage` is not a third-party-declarable extension point.** ADR-0019 §6 found a
third-party per-pixel render stage isn't viable over WASM (the host↔guest buffer-copy cost), and
any future opening of that extension point to third parties would need GPU shaders instead — a
different mechanism this ADR doesn't cover. A manifest declaring `RenderStage` is rejected by the
host at install time; the other six extension points in ADR-0019 §7 remain third-party-eligible.

### 2. Disclosure UX

- **Install/enable time**: a consent screen lists every declared capability with its
  `justification` shown inline, grouped by kind. The module cannot be enabled without this screen
  being shown and accepted — no silent auto-enable.
- **Update that adds a new capability kind, or widens an existing one's scope, disables the
  module until re-consented** — Chrome's model[^2], chosen over Firefox's "block the update
  outright until approved"[^3]: Nicti is a single-vendor desktop app with no staged-rollout
  complexity to protect against, so disabling the already-updated module (rather than pinning the
  user to a stale version) is the simpler, safer default — a security-relevant update still
  installs, it just doesn't silently gain new authority. **"Widens" is evaluated field-by-field
  within a capability, not just at the kind level**: an update that adds a host to an existing
  `network` capability's `allowed_hosts`, adds a filesystem scope, or relaxes a resource-budget
  override is exactly as much a widening as declaring a wholly new capability kind, and disables
  the module the same way — mirroring Chrome's own `host_permissions` re-consent behavior[^2],
  which this ADR would otherwise under-transcribe if read as kind-level only.
- Capabilities may also be requested **lazily** at first actual use rather than only declared
  upfront in the install screen — Deno's `Deno.permissions.request()` pattern[^4] and Android's
  contextual rationale[^5] are the precedents — but the manifest must still pre-declare every
  capability a lazy request can draw from; a lazy prompt can only *activate* a declared-but-not-
  yet-granted entry, never introduce an undeclared one.

### 3. Enforcement (mapped onto the current WASM direction, non-binding on the eventual mechanism)

- **`network`**: only `wasi:http`'s `outgoing-handler` import is linked into a module's world when
  `network` is granted — `wasi:sockets` (raw TCP/UDP) is never linked at all for a third-party
  module[^6]. The host's own `outgoing-handler` implementation checks the request's authority
  against `allowed_hosts` and the currently-active action scope, and requires HTTPS with
  certificate validation (a plain-HTTP request is rejected the same as an unlisted host), before
  issuing it; WASI's own primitive is only the binary link-time switch, the hostname/scope/
  transport filtering is Claw's responsibility layered on top — matching how Zed's `download_file`
  capability is host-scoped to specific hosts, not merely present/absent[^7].
- **`filesystem`**: WASI preopened directories only, never ambient path access[^8] — but a preopen
  is directory-granular, coarser than Nicti needs by itself (a known, acknowledged WASI gap[^8]),
  so `user-selected` grants preopen exactly the one directory the user's file-picker returned, not
  a parent.
- **`gpu`** / extension-point hooks: gated the same way — the corresponding host import is absent
  from a module's linked world unless declared.

### 4. Kill switch ("Hiss")

- **Reachable two ways**: a menu item, and a keyboard shortcut. Candidate binding:
  `Ctrl+Shift+Escape` (Windows) / `Cmd+Shift+Escape` (macOS) — no Lightroom Classic default
  shortcut using an Escape-modifier chord was found in any source checked, so this looks
  low-collision, but Adobe's own shortcut reference blocked direct fetch (HTTP 403); **this
  specific binding needs a manual cross-check against Adobe's live page or in-app Help before
  it's locked**, not treated as ADR-final.
- **Immediate, no restart required, for the module's own WASM call stack**: `Engine::increment_epoch()`
  advances a single counter shared by every `Store` on that `Engine`, not a per-module switch by
  itself — each `Store` traps independently once its own `set_epoch_deadline()` value is crossed
  by that shared counter. Isolating a kill to one module therefore needs either a separate
  `Engine` per third-party module (simplest, at the cost of one `Engine`'s worth of overhead per
  module) or per-`Store` deadlines on a shared `Engine` spaced so only the targeted module's
  deadline is crossed by a given increment — an implementation detail for whoever builds this, not
  resolved further here. Once the target module's deadline is crossed, wasmtime's own worked
  example confirms this interrupts code already mid-execution, not just blocks new calls —
  including a tight compute loop with no host-call boundary, since the compiler inserts epoch
  checks at both function entries *and loop back-edges*[^9]. The module's registry entry flips to
  quarantined (persisted) so it isn't reinstantiated until explicitly re-enabled.
- **`gpu` constraint — epoch interruption does not stop already-submitted GPU work**: if a killed
  module already called a host import that submitted a GPU command buffer (a compute-shader
  dispatch, per ADR-0019 §6's direction for any future third-party render-adjacent work),
  `increment_epoch()` stops the module's *WASM* call stack but has no effect on work already
  queued to the device — the GPU keeps executing until that dispatch completes on its own. This
  ADR doesn't resolve that gap; whoever implements the `gpu` capability's enforcement needs its
  own cancellation path (e.g. a device-level fence the host can drop/ignore, or bounding dispatch
  size so a kill's worst-case latency stays acceptable), tracked as an open constraint the same
  way the cdylib case below is.
- **cdylib constraint**: epoch interruption is a WASM-specific primitive. If a future third-party
  module were ever hosted via ADR-0019 §4's cdylib boundary instead, in-process preemption of a
  runaway call isn't available the same way — true interruption there would require running that
  module out-of-process. This ADR doesn't resolve that; it's a constraint on whichever sandbox
  mechanism is eventually chosen for a cdylib-hosted third-party module, flagged for whoever picks
  that up.
- **Edits stay intact**: a killed module's stage entries stay read-only and round-trip
  byte-for-byte via ADR-0019 §5 / ADR-0021's unknown-module handling — killing a plugin never
  loses or corrupts edit data referencing it.
- **Safe-mode fallback**: a persisted, file-backed "all third-party modules disabled" flag, read
  and enforced *before* any third-party module executes at launch — Obsidian's Restricted Mode is
  the precedent[^10]: a settings-file flag checked pre-load, not an in-app toggle that itself
  depends on a working UI. This is the escape hatch when a bad module has made the normal UI
  unusable. (An earlier draft of this research considered an Obsidian "hold Shift at startup"
  shortcut as an additional trigger; that claim could not be confirmed against an official Obsidian
  source and is deliberately not carried into this decision — the persisted-flag mechanism alone
  is the verified precedent.)

### 5. Bug vs. violation

- **Bug** (soft): a trap, panic, fuel/epoch timeout, or malformed output from an otherwise
  correctly-scoped call. After repeated failures within a short window, the module auto-disables
  with a plain notice; one click re-enables it. This is corrective, not punitive — it assumes
  overconfident code, not malice.
- **Violation** (hard): the module's own code attempts to use a capability it never declared,
  caught at the host-side boundary — for `network`/`filesystem`/`gpu` this is structurally
  impossible to trigger accidentally, since the corresponding import simply isn't linked into an
  undeclared module's world (§3) — so reaching this path at all means the module tried to call
  something that doesn't exist in its own environment, or exceeded a granted scope (wrong host,
  path outside the preopen). This is an immediate quarantine, a distinct (non-dismissable-by-
  default) warning UI naming exactly what was attempted, and re-enabling requires explicit
  acknowledgment — never auto-cleared by a subsequent module update, unlike §2's escalation case
  (which is an honest declared change, not an attempted undeclared one).

### 6. Explicitly deferred

- Publisher signing/identity and any trust-tier above "the user explicitly enabled this."
- A distribution registry or a remote/server-side blocklist (VS Code's marketplace-removal +
  forced-uninstall model[^11] is a strong precedent for later, once Nicti has any kind of central
  distribution point — out of scope while v2 plugins don't exist yet).
- The sandbox implementation itself (WASM vs. cdylib vs. other) — ADR-0019's scope, not this one.
- Exact consent-screen/kill-switch UI layout — depends on ADR-0068's GUI framework pick.
- Whether lazy/runtime capability *requests* (§2) are built at all in v1 of this system, vs.
  upfront-only — left to whoever implements this ADR's follow-up ticket.

## Prior art

**Chrome Manifest V3** distinguishes `permissions` (install-time, can trigger a warning dialog)
from `optional_permissions` (runtime-granted via `chrome.permissions`, deferring the ask until a
feature is actually used) and `host_permissions`[^2]. Adding a warning-triggering permission in an
update disables the extension until the user re-accepts[^2] — the direct precedent for this ADR's
§2 escalation-disables-until-reconsent rule. **Firefox** takes a different tack at the same moment:
it blocks the update from installing at all until approved, leaving the previous version running
in the meantime[^3] — considered and not chosen here (see §2's reasoning).

**VS Code** has no per-extension permission system at all — the extension host runs with VS Code's
own full permissions, and Workspace Trust explicitly does not stop a malicious extension from
ignoring it[^12]. Its real security lever is post-hoc: a verified-malicious extension is removed
from the Marketplace and added to a blocklist that VS Code checks client-side, automatically
uninstalling it from already-installed machines[^11] — the strongest precedent found for a kill
switch that reaches machines beyond the one that first noticed the problem, and worth revisiting
once Nicti has any central distribution channel (see §6).

**Figma** is the closest manifest-shape precedent (`networkAccess.allowedDomains` plus a
`reasoning` field)[^1], but its `reasoning` requirement is narrower than it first appears — only
mandatory for a wildcard domain or an undeclared dev server, not for a normal explicit host
allowlist entry. This ADR's universal justification requirement (§1) is a deliberate departure
from, not a copy of, Figma's narrower rule.

**Android's** `shouldShowRequestPermissionRationale`[^5] is the precedent for contextual,
just-in-time justification UX (shown when a feature is actually invoked) rather than only a static
upfront list — informing this ADR's optional lazy-request allowance in §2.

**Adobe Lightroom Classic's** Lua plugin SDK — the baseline Nicti users migrate away from — has no
documented permission model anywhere in its public API reference: `LrFileUtils` exposes
unrestricted filesystem read/write/delete/move/copy with no capability gate, and `LrHttp` similarly
appears to allow unrestricted network calls[^13]. No Adobe source explicitly states "plugins are
fully trusted"; the absence of any declared-capability documentation across the entire reference is
strong structural (not directly quoted) evidence for that conclusion.

**Zed's** extension model is the closest precedent for this ADR's overall shape: WASM
(`wasm32-wasip2`) modules with declared capability kinds (`process:exec`, `download_file`,
`npm:install`), enforced by refusing the corresponding host API call when the *user's own*
settings (`granted_extension_capabilities`) don't include a match — notably, the user's grant list
is authoritative independent of what the extension itself requests, letting a user narrow (e.g.
scope `download_file` to one host) or zero out capabilities entirely[^7]. **Deno's** permission
model is the strongest precedent for *scoped* grants (`--allow-net=host:port`, not just a boolean)
and for genuine runtime/interactive requests via `Deno.permissions.request()`/`.revoke()`[^4] — the
source for this ADR's §2 lazy-capability allowance. **WASI**'s capability model underlies the
actual link-time enforcement mechanism in §3: preopened directories for filesystem (with a known,
acknowledged coarseness at directory granularity[^8]) and an optional `outgoing-handler` import for
`wasi:http` that a host simply doesn't link if network wasn't granted[^6]. **wasmtime's** epoch
interruption (`Engine::increment_epoch()`) is confirmed, via its own worked fibonacci-recursion
example, to interrupt guest code already mid-execution — including a tight loop with no host-call
boundary, since checks are inserted at loop back-edges as well as function entries[^9] — this is
the concrete mechanism behind §4's kill switch. **Obsidian's** Restricted Mode — a persisted,
pre-plugin-load config flag, not an in-app toggle — is the precedent for §4's safe-mode fallback,
chosen deliberately over an unconfirmed "hold Shift at startup" claim that didn't hold up against
an official source[^10].

## Consequences

- Directly satisfies ADR-0218 §4's requirement: `network` is a declarable, action-scoped
  (`weight-download`/`cloud-feature`) capability, denied by default, with an undeclared module
  structurally unable to obtain it (§3's link-time absence, not a runtime check the module's code
  could route around).
- **Unblocks nothing yet** — no v1 code depends on this; it's the design ADR-0019 §6 deferred.
  Feeds whichever future ticket implements v2 third-party plugin hosting.
- A follow-up `build`/`v2` issue is filed for manifest parsing + capability enforcement + kill
  switch implementation, blocked by this ADR, since this ticket's own exit criterion is documentary
  only.
- `Ctrl+Shift+Escape` is a *candidate* kill-switch binding pending a manual check against Adobe's
  live shortcut reference (§4) — implementation should confirm before shipping, not treat this ADR
  as having locked it.
- Leaves the actual sandbox mechanism (WASM vs. cdylib vs. other) to ADR-0019/whoever implements
  v2 — this ADR's policy is written to survive that choice, with the cdylib-preemption gap (§4)
  flagged as a known open constraint rather than silently assumed away.

---

## Verified findings

Findings gathered 2026-09-27 by two parallel research passes (manifest/consent ecosystems;
capability enforcement + kill-switch mechanics), each citing a primary source with a verification
label where one exists, matching ADR-0019's citation discipline.

[^1]: Figma plugin manifest, `networkAccess.allowedDomains`/`reasoning` —
    https://developers.figma.com/docs/plugins/manifest/ — **primary-source-verified**.
    `reasoning` is conditionally required (wildcard `"*"` domain or an undeclared dev server only),
    not required for every declared capability — narrower than a first read suggests.
[^2]: Chrome Manifest V3 `permissions`/`optional_permissions`/`host_permissions` and
    disable-pending-reconsent on an update adding a warning-triggering permission —
    https://developer.chrome.com/docs/extensions/develop/concepts/declare-permissions and
    https://developer.chrome.com/docs/extensions/develop/concepts/permission-warnings —
    **primary-source-verified**.
[^3]: Firefox WebExtensions optional-permissions model and update-time approval blocking the
    update rather than disabling post-update —
    https://extensionworkshop.com/documentation/develop/request-the-right-permissions/ —
    **primary-source-verified**.
[^4]: Deno's scoped permission flags (`--allow-net=host:port` etc.), interactive TTY prompt, and
    `Deno.permissions.query()/request()/revoke()` — https://docs.deno.com/runtime/fundamentals/security/
    — **primary-source-verified**.
[^5]: Android `ActivityCompat.shouldShowRequestPermissionRationale` contextual rationale pattern —
    https://developer.android.com/training/permissions/requesting — **primary-source-verified**.
[^6]: WASI `wasi:http` optional `outgoing-handler` import as a link-time capability gate —
    https://github.com/WebAssembly/wasi-http/blob/main/wit/proxy.wit —
    **primary-source-verified** for the import structure; the general "preopen/capability" framing
    of WASI beyond this file is **best-available-secondary**.
[^7]: Zed extension capabilities (`process:exec`, `download_file`, `npm:install`), user-editable
    `granted_extension_capabilities`, host-scoped `download_file` — https://zed.dev/docs/extensions/capabilities
    — **primary-source-verified**; completeness of the capability-kind list beyond what this page
    shows is **best-available-secondary** (not cross-checked against Zed's own source).
[^8]: WASI preopened-directory filesystem model and its acknowledged directory-level (not
    file-level) coarseness — https://github.com/WebAssembly/WASI/issues/66 —
    **primary-source-verified** as a documented, acknowledged limitation (this is a discussion
    issue, not a spec page).
[^9]: wasmtime epoch interruption (`Engine::increment_epoch()`) interrupting guest code
    mid-execution, including tight loops via loop-back-edge checks, worked fibonacci example —
    https://docs.wasmtime.dev/examples-interrupting-wasm.html — **primary-source-verified** for
    the demonstrated example; whether every non-async `Store` configuration guarantees this
    unconditionally is **best-available-secondary** beyond that example.
[^10]: Obsidian Restricted Mode as a persisted, pre-plugin-load disable flag —
    https://obsidian.md/help/plugin-security — **primary-source-verified**. A "hold Shift at
    startup" safe-mode trigger surfaced in search results but was **not independently verified**
    against an official source and is not relied on by this ADR.
[^11]: VS Code Marketplace removal + client-side blocklist forcing automatic uninstall of an
    already-installed malicious extension —
    https://code.visualstudio.com/docs/configure/extensions/extension-runtime-security —
    **primary-source-verified**.
[^12]: VS Code has no per-extension permission system; Workspace Trust does not stop a malicious
    extension from ignoring it — https://code.visualstudio.com/docs/configure/extensions/extension-runtime-security
    and https://code.visualstudio.com/docs/editor/workspace-trust — **primary-source-verified**.
[^13]: Lightroom Classic Lua SDK (`LrFileUtils`, `LrHttp`) with no documented permission/capability
    gate — https://archive.stecman.co.nz/files/docs/lightroom-sdk/API-Reference/modules/LrFileUtils.html
    — **primary-source-verified** for the API surface itself (a mirrored copy of Adobe's own
    reference; Adobe's live docs are PDF-only and were not machine-fetchable this pass);
    **best-available-secondary**, via third-party plugin-dev guides, for the broader "no sandbox
    exists at all" conclusion — no explicit Adobe statement to that effect was found or quoted.

*Not independently verifiable with a primary source this pass: the exact candidate kill-switch
keybinding's collision-freedom against Lightroom Classic's live shortcut reference (blocked by an
HTTP 403 on Adobe's own page — see the Consequences section's flag); whether every
`wasi:sockets`/`wasi:http` host implementation in the wild enforces the same link-time-absence
guarantee this ADR relies on, as opposed to `wasi:http`'s own spec structure alone.*
