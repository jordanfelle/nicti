# ADR-0434: Agent-drivable command table, loopback control channel and MCP shim for nicti-pelt

**Status:** Proposed (go, phased; slice 1 built in #542, see "Slice 1 as built" below)
**Ticket:** #434
**Part of:** #424
**Related:** #426 (headless harness, landed), #233, #368, #294, #295, #272

## Context

LightCraft (`storytold/lightcraft` at `265248c`, MIT OR Apache-2.0) routes every action through one
command table (`crates/engine/src/cmd/mod.rs`: id, label, menu path, shortcut, params doc, `enabled`
predicate, `run`). The UI, shortcuts, CLI, a `127.0.0.1` JSON-lines socket
(`crates/ui-egui/src/control.rs`, `docs/control-protocol.md`) and a hand-written stdio MCP server
(`crates/mcp`, ~1140 lines, no async runtime) all dispatch through it. An agent can list commands,
run them, read UI state, click a widget by name, press keys, and take a screenshot of the real app.

Why Nicti would want that: GPU/UI verification today needs a human at a real window (#233 is the open
example: validating egui against ADR-0068's measured targets in an interactive session; #364, #365, #390
and #409 were GPU/UI test failures and a wake-loop regression found the same way). An agent that can
drive and screenshot the real app can run those checks. This dev host has no GPU, so it would drive a
*remote* GPU machine the operator started with the flag (see section 3 for how the token gets across).

This is design only: no LightCraft code is ported, so there is no `docs/licensing.md` entry. The
LightCraft sources below are the reference read for the questions, at the pinned commit.

### What nicti-pelt looks like today (verified in the tree)

- **No command table, no menu bar.** There is no `MenuBar`; the only `menu_button`s are the Color menu (`color_mgmt.rs`) and the
  Apply-preset menu (`knead/ui.rs`).
  Actions are methods on `PeltApp` (`request_export`, `handle_knead_action(PanelAction)`,
  `save_develop_edits`, ...) called from widgets.
- **Shortcuts are inline.** About 40 `consume_shortcut` / `key_pressed` / `consume_key` sites in 13
  files (`app.rs`, `grid/view.rs`, `cull/*`, `mask_panel.rs`, `heal_tool.rs`, ...). So the issue's
  "menus generated from one table (`menus.rs`, `shortcuts.rs`)" has no pelt equivalent to replace:
  those files do not exist here.
- **All state is on `PeltApp`** (`view`, grid selection, `develop`, `cull`, ...), driven from
  `eframe::App::ui` once per frame. There is no `Session`-style engine object a command could take.
- **#426 landed `swat/`** (`#[cfg(test)]` only): egui_kittest + a CPU softpaint rasteriser, finding
  widgets by AccessKit label. It cannot draw the wgpu photo viewport (paint callbacks), by design, and
  it did **not** deliver the named-widget registry its title mentions; section 4 adds one.
- `PeltApp` implemented only `eframe::App::ui`, not `logic` (slice 1, #542, added `logic`). eframe 0.37 runs **no egui pass** while the
  window is hidden or minimised (it calls only `logic`), and throttles invisible windows to 100 ms.
- The layering check (`scripts/check-repo-policy.py`) allows egui/eframe/accesskit only in
  `nicti-pelt` and the root `nicti` binary. Anything GUI-aware has to live in pelt.

## Decision

**Go, in the lightweight form, built in four independently shippable slices.** Not the 90-command
Session-centric surface: pelt's table starts with the handful of commands that unblock verification
and grows only when a ticket needs one.

### 1. A pelt-local command table

`crates/nicti-pelt/src/cmd.rs` (module name to taste):

```rust
pub struct Command {
    pub id: &'static str,                       // "view.set", "grid.select", "photo.rate", "export.run"
    pub label: &'static str,
    pub params: &'static str,                   // human/agent-readable, rendered into MCP tool schemas
    pub risk: Risk,                             // Read | Mutate | Destructive
    pub enabled: fn(&PeltApp) -> Result<(), String>,
    pub run: fn(&mut PeltApp, &serde_json::Value) -> Result<serde_json::Value, String>,
}
```

Entries wrap the existing `PeltApp` methods; they do not move logic. `Risk` is the one addition over
LightCraft: it gates what the channel may do (section 3). `enabled` returning the *reason* (as
LightCraft does) is kept, because "why is Export greyed out" is the question an agent keeps hitting.

Shortcut and menu metadata are **not** in the table in slice 1. They are a later migration (section 6).

### 2. Execution model: the UI thread, between frames

A control connection thread parses a line and pushes `(request, reply_tx)` onto an `mpsc` channel
and calls `ctx.request_repaint()`. `PeltApp::ui` drains the channel at the top of the frame, runs the
command on the UI thread (so it sees and mutates `PeltApp` with no locking), and sends the reply.
LightCraft does the same and caps a request at 60 s.

**Drain in `logic` as well as `ui`.** eframe runs no egui pass for a hidden or minimised window, so a
drain only in `ui` would leave every request to time out. `PeltApp` therefore implements
`eframe::App::logic` to drain the queue and run `Read`/`Mutate` *commands* (which touch `PeltApp`, not
egui input), and the app sends `ViewportCommand::RequestPaintWhileHidden` while the channel is on so
frame-dependent work still happens. Injected pointer/key input and screenshots need a painted frame;
they reply with an explicit "window not visible" error rather than waiting out the timeout.

Latency: a visible idle window is woken by `request_repaint`, so the cost is one frame (~16 ms) plus
the command. A long catalog or delete job running inside `ui` stalls every reply until the 30 s cap.
Replies for frame-dependent requests are sent after the frame that shows their effect, not from the
top of it. A command that must wait for a frame to *play out* (injected clicks, a render finishing)
replies after that, which is why `ui.settle` exists in LightCraft. We copy that, with a deadline loop
(poll until idle or timeout), never a fixed sleep.

### 3. Transport and security: loopback, opt-in, tokened

LightCraft's channel has no auth; it relies on loopback plus a line cap and "first line must be a JSON
object with a `method`", which stops a web page's cross-origin `fetch` from smuggling a command in an
HTTP body. We keep all of that and add:

- **Opt-in.** Off unless `--control[=PORT]` or `NICTI_CONTROL_PORT` is set (parsed in the root
  `nicti` binary's `main.rs` and passed into `nicti_pelt::run`). Bind `127.0.0.1` only (never
  `0.0.0.0`). `PORT` defaults to 0 (OS-assigned); the **actual bound port is written into the token
  file**, so a predictable port cannot be pre-bound by another local user to harvest the token.
- **Random token.** 128 bits from the OS RNG, compared in constant time, in a file created atomically
  (`O_EXCL`, mode `0600` at creation, not chmod afterwards) under `$XDG_RUNTIME_DIR` (Windows: the
  per-user local app data dir, relying on its default ACL; Unix `0600` has no Windows meaning), removed
  on exit; a stale file from a crashed run is replaced, and a second instance gets its own file named by
  PID. The first line on a connection must be `{"auth":"<token>"}`, received within 5 s; a wrong token,
  a timeout or any non-object first line closes the connection. This closes the case LightCraft's
  framing check does not: another local user or process connecting to the port.
- **Remote use.** The token file lives on the machine running the app. Over `ssh -L` the operator
  copies the file (`scp`, or `ssh host cat ...`) next to the shim, or passes it with
  `--token-file`/`NICTI_CONTROL_TOKEN`; the shim does not read the remote filesystem by itself.
- **Caps.** Line length 1 MiB (LightCraft: 4 MiB; we have no 4 MiB request), 4 concurrent
  *authenticated* connections, plus at most 4 unauthenticated ones each held for no more than the 5 s
  auth deadline (so idle sockets cannot lock the agent out), per-request timeout 30 s.
- **Risk gate.** `Read` always runs. `Mutate` is limited to the catalog and develop state; it never takes
  a filesystem path from the channel. `Destructive` needs `--control-allow-destructive`: deleting or
  shredding files, writing anything to a channel-supplied path (export destination, screenshot `path`),
  and confirming any delete dialog. `export.run` is `Destructive` unless it targets a destination the
  operator passed on the command line (`--control-export-dir`). `develop.set` writes XMP sidecars via
  the normal autosave path; that is the app's existing, undoable edit path (Ctrl+Z), and is `Mutate`.
  **Raw input injection (click, key, drag) is gated like `Destructive` commands for any widget on a
  delete/confirm dialog**: those dialogs do not register their buttons while the flag is off, so an agent
  cannot click through them. Channel-supplied paths, where allowed, are canonicalised and opened with
  no-follow semantics where the OS supports it; a command never takes a raw shell string. Screenshots
  return the image bytes (base64) by default and write to a path only under the flag.

A threat we do not mitigate: anything running as the same user can read the token file and drive the
app. That is the same trust boundary as the user's own session and catalog, and is why the channel is
off by default.

### 4. Seeing and poking the real UI

- **`ui.inspect`**: view, selection, active asset, window size, frame time, in-flight jobs. Cheap and
  the first thing an agent needs.
- **Widgets by name: a registry, not AccessKit.** `register(ctx, "button:export", rect)` (about 20
  lines, as in LightCraft's `widgets.rs`) called from the widgets worth driving, populated only while
  the channel is on. Clicks and drags are synthesised as real egui `Event`s at the rect. We do not
  turn on eframe's `accesskit` feature in the shipped binary just for this: it is off here
  (`default-features = false`), costs the `egui-winit` AccessKit adapter at runtime, and the registry covers the
  custom-painted widgets (grid cells, sliders) AccessKit would need extra annotation for anyway.
  `swat` keeps using AccessKit in tests; the two do not need to agree.
- **Screenshot**: `egui::ViewportCommand::Screenshot` (delivers a `ColorImage` on a later frame; works
  on eframe's wgpu backend) so the captured image includes the wgpu photo viewport. `swat`'s softpaint
  stays the no-GPU, no-photo path and is out of scope for the channel.

### 5. The MCP shim

A **new non-GUI crate with its own `[[bin]]`** (a `[[bin]]` inside `nicti-pelt` would link egui), not
part of the GUI process. It needs a `LAYERS` entry in `scripts/check-repo-policy.py`; the shared wire
types (request/response structs, token-file format) live in that crate and are depended on *by*
`nicti-pelt` (layer 6), so the crate sits below it. Name per the feline convention, e.g. `nicti-whisker`
is taken by `bench/whisker`, so pick another at implementation time. stdio JSON-RPC in, the control socket out, no
async runtime. `tools/list` is generated from the app's own `commands` reply (id, `params` doc,
`risk`), plus fixed `inspect`, `widgets`, `click`, `key`, `screenshot`. It reads the token file (or `--token-file`) and
reconnects if the app restarts (LightCraft's connect mode). It needs only `serde_json`, so it can sit
in a non-GUI layer; it must not depend on egui.

### 6. Does this unlock rebindable shortcuts and a command palette?

A command palette: **yes, cheaply**, once the table exists (fuzzy-match `label`, call `run`, show the
`enabled` reason). Rebindable shortcuts: **only after a separate migration** of the ~40 inline key
sites to table-declared shortcuts dispatched in one place. That is a refactor with its own regression
risk (the cull keys have careful key-repeat handling, `cull/input.rs`) and is **not** part of this
decision; it gets its own ticket if wanted. The control channel does not depend on it.

## Alternatives rejected

- **Port LightCraft's engine-level `Session` command surface.** Pelt has no Session; this would be a
  large new abstraction for ~90 commands nobody has asked for.
- **AccessKit as the widget addressing scheme.** Covered above.
- **Unix socket instead of TCP.** Better access control, but not portable to the Windows installer
  (#249) and not forwardable the way `ssh -L` handles a TCP port. Revisit if Windows is dropped.
- **Authenticated-by-obscurity port number.** Not a control.

## Consequences

- **Slices (estimates are relative effort, not calendar):**
  1. Command table + `inspect` + a first set of commands (`view.set`, `grid.select`, `photo.rate`,
     `develop.get/set` for a few params, `export.run`), exercised in-process through `swat`. **M**
  2a. Loopback listener, token file, caps, auth deadline, `logic`-drain, risk gate. **M**
  2b. Widget registry, click/key/drag injection, `settle`, confirm-dialog gating. **M**
  3a. Screenshot via `ViewportCommand::Screenshot` (needs a visible frame). **S**
  3b. The MCP shim crate + `docs/` page + a `.mcp.json` example. **M**
  4. Command palette over the table. **S** after slice 1. Shortcut migration: **L**, optional.
  Slice 1 alone is shippable and useful (it is the test seam for the headless harness).
- **Cost of keeping the registry honest:** a widget not registered is invisible to an agent; there is no
  completeness check. Acceptable; register on demand when a verification ticket needs one.
- **New attack surface,** off by default and gated as in section 3. The follow-up implementation PRs
  need the adversarial review and a manual security pass (external input, file paths, auth).
- **Caveat on the benefit:** this speeds up GPU/UI verification only where an agent can reach a GPU
  display. On this headless host that means driving a remote instance over `ssh -L`.
- **Deferred:** shortcut/menu unification, rebinding, remote (non-loopback) control, a UI test recorder.

## Slice 1 as built (#542)

- `crates/nicti-pelt/src/cmd.rs`: `Command` / `Risk` / `Policy`, `dispatch`, `list`, `CommandQueue`
  (drained by `PeltApp::drain_commands` from both `eframe::App::logic` and `ui`, at most 16 per drain, so up to 32 requests a frame; `Arc`-shared so a command may itself submit).
- **Deviation from section 1:** `run`/`enabled` take `&mut dyn CommandHost`, not `&mut PeltApp`.
  `PeltApp::new` needs eframe's wgpu `RenderState`, so a `PeltApp` cannot be built in a test; the
  trait is mocked there and `PeltApp` implements it. The Develop get/set core is a free function
  over a real `DevelopView`, tested on it (lavapipe where there is no GPU).
- Commands: `ui.inspect`, `view.set` (library | loupe), `grid.select`, `photo.rate` (null/-1/1..5,
  at most 1000 photos), `develop.get` / `develop.set` (stages wb, exposure, tone, vibrance,
  presence; unknown fields refused; goes through the undo history), `export.open`. Unknown
  parameters are refused everywhere; no command takes a path.
- **`export.run` is not built.** The export flow starts inside the dialog's own Start button; a
  headless run needs the dialog's settings/destination step to become a callable, which is
  `Destructive` per section 3. `export.open` (opens the dialog) is the Mutate stand-in. Filed as a
  follow-up with the transport slice.
- `Risk::Destructive` is enforced by `Policy::allow_destructive` (tested with a synthetic entry);
  no destructive command exists yet. Nothing feeds the queue until slice 2a.
- Hardening from the pre-PR review: `develop.set` refuses out-of-range numbers (the documented slider
  ranges; an f32 parse of `1e300` is infinity and would serialize to `null`, poisoning the document),
  can set/clear the as-shot-omitted `temp_k`, and each call is its own undo step
  (`DevelopView::without_gesture_merging`). `grid.select` clears the multi-selection and only works
  in the library view; `photo.rate` rates only catalog photos, each once, and reports that count;
  `export.open` and `view.set loupe` report failure instead of silently succeeding; `ui.inspect`
  lists at most 1000 selected ids (plus the exact count). Still for slice 2a: a request-size cap
  before parsing, and waking an idle window when a request arrives.

## Slice 2a as built (#543)

- `crates/nicti-pelt/src/control.rs`; started by `PeltApp::enable_control` when the root binary was
  run with `--control[=PORT]` (or `NICTI_CONTROL_PORT`); `--control-allow-destructive` lifts the risk
  gate (nothing destructive exists yet; `ControlOptions::policy` is what reaches the gate). An unknown
  `--control-*` flag is an error, not ignored. macOS has no per-user runtime directory fallback, so
  `--control` fails there with a clear message (v2 target).
- **Handshake: the token never crosses the wire** (this replaces section 3's "first line is
  `{"auth":"<token>"}`", because a cleartext token is stolen by whoever binds the port first; on
  Windows a second socket can bind a listening 127.0.0.1 port unless `SO_EXCLUSIVEADDRUSE` is set):
  1. server -> `{"hello":"nicti-control","protocol":2,"nonce":N}`
  2. client -> `{"auth":MAC_c,"cnonce":C}` within 5 s *total* (a trickled line is cut off at the
     deadline, not just at a per-read timeout), `MAC_c = blake3_keyed(blake3(token), "client"|N|C)`
     (each part length-prefixed)
  3. server -> `{"ok":true,"proof":MAC_s}`, `MAC_s` the same with `"server"`; the client must check it.
  Then `{"id":..,"method":"<command id | commands>","params":{..}}` answered by
  `{"id":..,"ok":true,"result":..}` or `{"id":..,"ok":false,"error":".."}`. An impostor on the port
  sees only N and MAC_c, can't forge MAC_s and so fails the client's check (tested). The first
  non-answering line (HTTP, a bare string, an array) closes the connection.
- Token: 128 bits from the OS RNG (`getrandom`, already in the lockfile), compared in constant time, in
  `$XDG_RUNTIME_DIR/nicti-control-<pid>.json` (Windows: `%LOCALAPPDATA%\\nicti`) holding the *bound*
  port and the pid, created `O_EXCL` with mode `0600` at creation; a stale file is replaced (a planted
  symlink is replaced, its target untouched); on Unix the directory must not be group/other accessible
  or the channel refuses to start; there is no `/tmp` fallback. Removed on a clean exit; a crash or
  SIGKILL leaves a stale file, so a client should check the pid in it is alive (the handshake would
  fail anyway: nothing listens, or the proof is wrong).
- Caps: 1 MiB per line (rejected without buffering the excess), 4 authenticated connections, 4
  unauthenticated ones where **a new connection evicts the oldest silent one** (so another local user
  holding sockets open cannot lock the agent out; they would have to open four new connections inside
  the milliseconds the real client takes to answer), 30 s per request, 5 min idle timeout, 16 requests
  per UI drain. A request the client was told timed out is cancelled and never runs later. The accept
  loop backs off on errors. `logic` drains too, the app sends `RequestPaintWhileHidden`, and a queued
  request wakes an idle window (`request_repaint`).
- Known limits: dropping `Control` wakes the accept thread with a best-effort connect and does not
  join it; connection threads outlive it. The idle timeout applies per read, so an authenticated client
  can trickle. Neither crosses the trust boundary (the token holder is already trusted).
- **Not built (follow-up):** the widget-rect registry, click/key/drag injection, `settle`, confirm-dialog
  gating (slice 2b); screenshot (3a); the MCP shim (3b), which must implement the handshake above. The
  risk gate is therefore only the policy flag for now, and no command can reach a delete/confirm dialog.
- Verified here: token/handshake/cap/eviction/deadline/size/cancel guards by unit and loopback-socket
  tests, each guard broken in turn to see a test fail (the peer-address check is belt and braces and
  unobservable on loopback). Not verified: Windows ACL behaviour of `%LOCALAPPDATA%`, the
  `SO_EXCLUSIVEADDRUSE`-free bind on Windows beyond the protocol argument, and a real GUI session.

## `export.run` and the slice-1 leftovers as built (#647)

- **`export.run`** starts an export without the dialog and returns at once; **`export.status`**
  (Read) reports progress (`done`/`total`, the planning phase) and the last finished report (counts,
  up to 50 output files, failures, warnings). The run is the dialog's own `ExportRun::start` through
  `ExportUi::start_headless` (same validation, same Pounce pipeline, Cancel in the status line); the
  remembered "last settings" are left alone so a script's one-off does not become the user's next
  default. Params: `assets` (default: the selection or cursor, at most 1000, each must be in the
  catalog), `preset` (a built-in or saved name; default the dialog's last settings), `subfolder`,
  `destination`, `overwrite`.
- **Risk is per parameters** (`Command::risk_for`; the listed `risk` is the base): with neither
  `destination` nor `overwrite` the run writes new files into the operator's
  `--control-export-dir DIR` and is `Mutate`; naming a `destination` folder or `overwrite: true` makes it
  `Destructive` (needs `--control-allow-destructive`). `--control-export-dir` is parsed with the other
  `--control-*` flags (a value or `=VALUE`; must exist and be a directory, stored canonical; needs
  `--control`; naming a folder does not lift the destructive gate). Without it and without a
  destination `export.run` says what to start Nicti with.
- **What the channel cannot choose:** the collision policy is forced to the `-2` suffix (a preset's
  "overwrite" is replaced), GPS is never included, and a `subfolder` is plain relative folders only
  (no `..`, empty or hidden parts, backslashes, drive letters, wildcards or `{templates}`), checked
  again against the real filesystem: an existing part that is a link or not a folder is refused.
- **`develop.set` names its photo.** Develop edits the photo it has loaded, which is not necessarily
  the one the grid cursor or loupe shows. Decision: `develop.set` requires `asset` and is refused
  when it is not the loaded photo; `develop.get` takes it optionally with the same check; both
  results report the `asset` they acted on. `ui.inspect` already shows `develop_asset`.
- **`export.open` stays `Mutate`:** it takes no path and only opens the dialog; the dialog's own Start
  is not reachable from the channel until widget injection (slice 2b) exists, and `export.run` is
  where the gate lives.
- Already done in slice 2a (#543) and not touched here: the 1 MiB request cap before parsing
  (`MAX_LINE`) and waking an idle window when a request is queued.

Review of #647 changed three things. **The subfolder check models what preen writes:** preen renders
every part through `sanitize_component` (trims leading spaces and trailing dots/spaces, drops parts
that come out empty, prefixes reserved device names, maps bidi controls), so a raw-string link check
could be walked past -- `link.`, ` link` or ` ./link` all write into a pre-existing `link` symlink in
the export folder. `validate_subfolder` now accepts only parts the sanitiser leaves unchanged, which
also makes the inspected path the written path. **The 1000-photo cap applies to the default selection
too**, not just an explicit `assets` list. **`export.status` lists output file names, not paths** (it
is a Read command; where the user exports is theirs); failure and warning strings can still carry a
path. The spec the channel exports with is built by the pure `channel_export_spec` (destination
replaced, clash policy decided by the channel, GPS off) and unit-tested. Known and not changed: with
the default `UniqueSuffix` policy repeated `export.run` calls keep adding files (no byte quota; the
operator owns the folder and the disk), and a local process that can write to the export folder can
still race the check (a link planted between check and write).

## Slice 2b as built (#649): widgets by name, injected input, settle, dialog gating

- **`crates/nicti-pelt/src/widgets.rs`.** The registry lives in egui's context data, installed only
  when the channel starts (`widgets::install`), so with the channel off a `reg(...)` is one failed
  lookup and grid cells check `widgets::enabled` before building a name. Names are `kind:name`,
  lower-case ASCII, at most 80 characters, at most 1024 per frame. The frame being drawn replaces the
  last one at `begin_frame`, so `ui.widgets` always lists a complete recent frame. Registered today:
  the Library toolbar (`button:select-all`, `survey`, `compare`, `export`), the Library controls
  (`button:import`, `sync`, `open-in-loupe`) and every visible grid cell as `cell:<photo id>` (the
  on-screen part of the cell is what gets clicked).
- **Commands:** `ui.widgets` (Read), `ui.click {widget, button?, modifiers?}`, `ui.key {key,
  modifiers?}`, `ui.drag {widget, dx, dy}` (Mutate) and `ui.settle {timeout_ms?, jobs?}` (Read).
  Input is queued as steps of real `egui::Event`s and handed to egui from `eframe::App::raw_input_hook`,
  one step per frame: a click is move+press, then release a frame later; a key is press then release; a
  drag is press, 8 intermediate moves, release. **A click can only name a registered widget** (no
  coordinates) and **there is no text entry**, so a path field cannot be typed into. A request is
  refused with a reason when the window has not painted a frame in 2 s ("not visible"), the widget is
  unknown (the error lists similar names), it is off screen, or 64 steps are already queued.
- **`ui.settle`** replies once nothing is queued and two frames have drawn since the last delivery
  (and, with `jobs: true`, no Pounce job is queued or running). The reply is *deferred* in the
  `CommandQueue` and re-checked on every drain, with a deadline (default 5 s, 50 ms to 30 s): a timeout
  answers `{settled: false, reason}` rather than leaving the client to the transport's 30 s cap, a
  client that gave up is dropped, and the app keeps painting while one waits. Never a fixed sleep.
- **The confirm-dialog boundary, two layers.** (1) *Naming:* widgets that confirm something
  destructive are registered with `reg_destructive`, which records nothing unless the channel was
  started with `--control-allow-destructive`: the delete dialog's three buttons, the Export dialog's
  Export and Cancel, and the toolbar's Delete. (2) *Opening:* naming alone is not enough (a registered
  widget behind a modal can share a position with a dialog button, and Enter or Space can activate a
  focused one), so while the delete or Export dialog is open `ui.click/key/drag` are refused outright,
  steps already queued are discarded at delivery, **and the frame's injected events are scrubbed back
  out of egui's input at the top of `ui`** using the live dialog state. That last step closes a real
  hole found writing the tests: the input hook runs *before* the frame's commands, so a command that
  opens the dialog in the same frame would otherwise let an Enter or click delivered a moment earlier
  land on it. Only events the hook itself injected are removed, never a person's. `ui.inspect` reports
  `input_blocked`, the count discarded.
- `export.open` is not nameable-from-input-only: opening the dialog by clicking `button:export` is
  allowed (it only opens a dialog), after which the channel can do nothing in it without the flag;
  `export.run` remains the headless route.
- **Not built:** screenshot (3a) and the MCP shim (3b) are separate PRs of this ticket.

### Slice 2b review hardening (#649)

- The hook drains waiting commands before it reads the live dialog state, so a command that opens the delete or Export dialog is seen by the gate before that frame's input is delivered.
- At most one step is carried while no `ui` pass has run (hidden or minimised window), so the scrub always covers everything carried.
- `scrub_injected` removes key events only; egui builds pointer state before `ui`, so the gate relies on delivery-time refusal for the mouse. A press cannot confirm a dialog on its own (the release is a separate step and is discarded).
- Out of scope, stated plainly: Tab then Enter/Space can activate any focusable button, and only the delete and Export dialogs are gated. Other confirm-style buttons (purge previews, clear positions, repair thumbnails/models, "Use file") are reachable by keys with `--control` on. The channel is loopback + token only.

### Screenshot as built (#649, slice 3a)

- `ui.screenshot` (`Risk::Read`) asks egui for `ViewportCommand::Screenshot` via `Context::request_screenshot`; the reply is deferred like `ui.settle` and carries `{width, height, png_base64}`. The capture includes the wgpu photo viewport. egui paints a pending capture even for an occluded window; a minimised (zero-sized) surface cannot be captured and times out with an error instead of hanging. Unverified on a real window.
- `max_width` (default 1600, 64-4096) scales a wider window down so a 4K window cannot make a multi-megabyte reply; `timeout_ms` is 500-25000 (default 10000, under the transport's 30 s). A window past 16.8 Mpx is refused (the encode runs on the UI thread). Pixels are un-premultiplied for the PNG.
- One capture at a time (a second is refused); a timed-out or cancelled request frees the slot.
- **`Read` risk, deliberately:** this is the first `Read` command that returns the library's pixels (plus any visible filename, metadata or dialog); loopback + token is the boundary. The reply line can be several MiB, so a reader must not apply the 1 MiB request cap to replies (the MCP shim must not).
- **No `path` parameter yet.** The ADR's rule (write to a channel-supplied path only under `--control-allow-destructive`) is not built; bytes only.
- Not verified against a real window (this host is headless): the egui/eframe screenshot path is exercised by the encoder and queue tests only.

### MCP shim as built (#649, slice 3b)

- New crate `nicti-vibrissa` (layer 0, depends on no workspace crate and no GUI; only `blake3`, `getrandom`, `serde_json`) with the `nicti-mcp` binary. `nicti-pelt` depends on it for the handshake `mac`, so the two ends cannot drift.
- `tools/list` is the app's own `commands` reply (name = id with `.` -> `_`, description = label + risk + params doc + the "unavailable" reason); there are no fixed extra tools. A screenshot result becomes an MCP image block.
- The client checks the server's proof (an impostor on the port fails; tested against the real server and a fake one). It reads replies up to 64 MiB (the 1 MiB cap is for requests only), reconnects lazily and re-reads the token file, and never repeats a sent request.
- Token discovery: `--token-file`, `NICTI_CONTROL_TOKEN_FILE`, or the only `nicti-control-*.json` in the runtime directory; several is an error. `--port` overrides the file's port (for `ssh -L`).
- Guide: `docs/agent-control.md`; client config example: `docs/agent-control.mcp.json`.
- Not verified against a real MCP client or a running window (headless host); the JSON-RPC layer and the socket client are covered by unit tests and an end-to-end test against the real server.
- With this the ADR's slices 1-3 are built; the command palette (slice 4) and shortcut migration stay deferred.
