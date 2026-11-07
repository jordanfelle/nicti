# Headless UI audit

How to run the real Nicti GUI on a machine with no display or GPU (a CI box, a server, a
container), look at it, and drive it with real clicks and keys. This is how issues #756-#761 and
the round-2 audit (#768-#778) were found. Everything lives in `scripts/ui-audit/`.

It is a screenshot-and-poke tool, not a test framework: use it to reproduce what a person sees
(layout, truncation, a missing state), then pin the fix with a unit or kittest test. For driving
the app programmatically (reading widget names, injected input, `ui.screenshot`) see
[agent-control.md](agent-control.md).

## What you need (Linux)

- **Xvfb**: `brew install xorg-server` (or `apt install xvfb`).
- **Software Vulkan** (lavapipe): the Mesa `lvp_icd.json` (`/usr/share/vulkan/icd.d/` on most
  distros; set `VK_ICD_FILENAMES` if yours differs).
- **libxkbcommon-x11** for winit: `brew install libxkbcommon` (set `UI_AUDIT_LIB_DIR` if its
  `lib` directory is not `/home/linuxbrew/.linuxbrew/lib`).
- **Python 3 with `python-xlib` and `pillow`** (XTEST input and screenshots):

  ```bash
  python3 -m venv /tmp/ui-audit-venv
  /tmp/ui-audit-venv/bin/pip install -r scripts/ui-audit/requirements.txt
  ```

- A debug build of the app: `cargo build --bin nicti` (the first build is long).

## Fixtures: there is no real RAW on the dev host

```bash
python3 scripts/ui-audit/make_fixtures.py /tmp/fx --jpegs 6 --dngs 3
python3 scripts/ui-audit/make_fixtures.py /tmp/fx-nomatrix --jpegs 0 --dngs 2 --no-matrix
```

The JPEGs are gradients (Pillow needed for them; the DNG writer is standard library only). The DNGs
are tiny uncompressed 16-bit RGGB files with **no embedded preview** (they exercise the decode
fallback, #770) and decode through LibRaw (a port of the builder in
`crates/nicti-cornea/tests/real_dng.rs`; check one with
`NICTI_TEST_REAL_DNG_DIR=/tmp/fx cargo test -p nicti-cornea --features libraw --test real_dng`).
`--no-matrix` writes an all-zero colour matrix to exercise the sRGB-primaries notice (#756,
#771). None of it is a real camera file.

## Taking a screenshot

```bash
/tmp/ui-audit-venv/bin/python scripts/ui-audit/ui_audit.py \
    --binary target/debug/nicti --size 900x600 \
    wait:20 shot:/tmp/library.png
```

This starts a private Xvfb (`:99`), launches the app under it with an isolated `HOME`/`XDG_*`
(your real settings are never touched; the catalog is a fresh one in that profile unless you pass
`--catalog`), runs the steps in order, and kills everything. Software rendering is slow: give the
app time (`wait:`) after startup and after every action.

Steps: `wait:S`, `shot:PATH`, `click:X,Y`, `rclick:X,Y`, `dclick:X,Y`, `move:X,Y`,
`drag:X1,Y1,X2,Y2[,SECONDS]`, `key:NAME[+MOD]` (`key:Return`, `key:a+Control_L`), `type:TEXT`,
`resize:WxH`. Coordinates are window pixels. There is no window manager, so they equal screen
pixels. Take a screenshot first and read the coordinates off it; they move when the layout does.

A worked example: import the fixtures and look at the grid at the app's minimum size.

```bash
/tmp/ui-audit-venv/bin/python scripts/ui-audit/ui_audit.py \
    --binary target/debug/nicti --size 900x600 \
    wait:20  click:457,201  type:/tmp/fx  click:711,201  wait:25  shot:/tmp/imported.png
```

(`457,201` is the Folder field and `711,201` the Import button in the Library tools section at
900x600; open the "Library tools" section first if it is collapsed.)

## Gotchas

- **The catalog path must be the app's first argument.** A leading flag is taken as the catalog
  (#758). `ui_audit.py` does this for you; `--control` is added after it.
- **Use real X clicks, not injected ones.** The control channel's `ui_click` did not apply under
  software rendering (#716); XTEST input does.
- **Keyboard input needs window focus.** `key:` and `type:` focus the window first.
- **The window opens at 800x600.** `--size` resizes it (the app's minimum is 900x600).
- **Slider drags need to be slow.** Use the `SECONDS` argument of `drag:` (2 s or more); lavapipe
  frames take a while and a fast drag is seen as a jump.
- **`pkill -f` patterns can match your own shell.** The script keeps process handles and kills
  exactly those.
- A first launch with a stray flag argument leaves `--control*` files in the working directory.

## Reading the UI without pixels

Start the app with `--control` (`ui_audit.py --control ...`) and talk to it through `nicti-mcp`
over stdio as described in [agent-control.md](agent-control.md): `ui_screenshot` returns a PNG
without X, `ui_widgets` lists the named widgets. Prefer X clicks for anything that has to actually
fire under software rendering.
