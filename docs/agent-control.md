# Driving nicti from an agent (control channel + MCP)

Design and security model: [ADR-0434](adr/0434-agent-control-channel.md). This page is how to use it.

## 1. Start the app with the channel on

```text
nicti --control                                  # loopback only, OS-assigned port
nicti --control --control-export-dir ~/exports   # lets export.run write there
nicti --control --control-allow-destructive      # also lets an agent confirm delete/Export dialogs
```

The app writes `nicti-control-<pid>.json` (port, token, pid) to `$XDG_RUNTIME_DIR` (Windows:
`%LOCALAPPDATA%\nicti`), mode `0600`, and removes it on exit. The token never crosses the socket:
client and app prove they both know it (mutual handshake), so a process that grabbed the port first
can neither read the token nor pass for the app.

## 2. Point an MCP client at `nicti-mcp`

`nicti-mcp` (crate `nicti-vibrissa`, no GUI) speaks MCP on stdio and forwards to the socket. Its tools
are generated from the app's own command table (`ui.click` becomes `ui_click`, and so on), so a new
command appears with no change to the shim.

```text
nicti-mcp [--token-file PATH] [--port N]
```

- No `--token-file`: the one `nicti-control-*.json` in the runtime directory. Several (two instances,
  or stale files from a crash) is an error that lists them; guessing which app to drive is refused.
  `NICTI_CONTROL_TOKEN_FILE` works too.
- It connects lazily and reconnects on the next call after the app restarts. It never repeats a
  request that was already sent.
- Example client config: [`agent-control.mcp.json`](agent-control.mcp.json).

### Remote (`ssh -L`)

The token file lives on the machine running the app. Forward the port, copy the file, and give the
shim the *local* end of the forward:

```text
ssh -L 5555:127.0.0.1:<port from the file> host
scp host:/run/user/1000/nicti-control-<pid>.json .
nicti-mcp --token-file ./nicti-control-<pid>.json --port 5555
```

## 3. A first session

1. `ui_inspect`: view, selection, what is open.
2. `ui_widgets`: the names you may click (`button:...`, `cell:<id>`).
3. `ui_click` / `ui_key` / `ui_drag`, then `ui_settle` (waits for the input to play out; a deadline,
   never a fixed sleep).
4. `ui_screenshot`: an image block (PNG). `max_width` scales it down; it needs a painted window.
5. `develop_get` / `develop_set`, `photo_rate`, `export_run` (into `--control-export-dir`).

Destructive commands and the delete/Export dialogs are refused unless the operator started the app
with `--control-allow-destructive`. A tool the table lists as unavailable says why in its description.
