#!/usr/bin/env python3
"""Drive the real nicti GUI headless and take named screenshots (#777).

Starts a private Xvfb, launches the app under it with software Vulkan (lavapipe), then runs the
steps given on the command line in order and cleans everything up. See docs/ui-audit.md.

    ui_audit.py --binary target/debug/nicti --size 900x600 wait:15 shot:/tmp/library.png \
        click:756,124 wait:2 shot:/tmp/menu.png

Steps (coordinates are window pixels; there is no window manager, so they equal screen pixels):
    wait:SECONDS         sleep (software rendering is slow: give the app time after each action)
    shot:PATH            save a PNG of the window
    click:X,Y            left click        rclick:X,Y   right click      dclick:X,Y   double click
    move:X,Y             move the pointer
    drag:X1,Y1,X2,Y2[,SECONDS]   press, move slowly, release (sliders need slow drags)
    key:NAME[+MOD...]    one key, e.g. key:Return  key:a+Control_L  key:Escape
    type:TEXT            type text (printable ASCII)
    resize:WxH           resize the window

Injected `ui_click` through the control channel does not apply under software rendering (#716);
these are real X events (XTEST), which do.
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time

from PIL import Image
from Xlib import X, XK, display
from Xlib.ext import xtest


def find_xvfb():
    for cand in (shutil.which("Xvfb"), "/home/linuxbrew/.linuxbrew/bin/Xvfb", "/usr/bin/Xvfb"):
        if cand and os.path.exists(cand):
            return cand
    sys.exit("Xvfb not found (brew install xorg-server, or apt install xvfb)")


class Session:
    def __init__(self, args):
        self.args = args
        self.procs = []
        self.home = args.home or tempfile.mkdtemp(prefix="nicti-ui-audit-")
        self.d = None
        self.win = None

    def env(self):
        lib = os.environ.get("UI_AUDIT_LIB_DIR", "/home/linuxbrew/.linuxbrew/lib")
        h = self.home
        return dict(
            os.environ,
            DISPLAY=self.args.display,
            VK_ICD_FILENAMES=os.environ.get(
                "VK_ICD_FILENAMES", "/usr/share/vulkan/icd.d/lvp_icd.json"
            ),
            WGPU_BACKEND="vulkan",
            LD_LIBRARY_PATH=lib + ":" + os.environ.get("LD_LIBRARY_PATH", ""),
            # Isolated profile so the audit never touches the real settings.
            HOME=h,
            XDG_CONFIG_HOME=h + "/config",
            XDG_DATA_HOME=h + "/data",
            XDG_CACHE_HOME=h + "/cache",
            XDG_RUNTIME_DIR=h,
        )

    def start(self):
        a = self.args
        self.procs.append(
            subprocess.Popen(
                [find_xvfb(), a.display, "-screen", "0", a.screen + "x24"],
                stderr=subprocess.DEVNULL,
            )
        )
        time.sleep(2)
        # The catalog path MUST be the first argument: a leading flag becomes the catalog (#758).
        cmd = [a.binary, a.catalog] + (["--control"] if a.control else [])
        log = open(os.path.join(self.home, "app.log"), "w")
        self.procs.append(
            subprocess.Popen(cmd, env=self.env(), cwd=self.home, stdout=log, stderr=subprocess.STDOUT)
        )
        self.d = display.Display(a.display)
        root = self.d.screen().root
        for _ in range(90):
            time.sleep(1)
            kids = root.query_tree().children
            if kids:
                self.win = kids[-1]
                break
        if self.win is None:
            sys.exit("the app never opened a window; see " + os.path.join(self.home, "app.log"))
        w, h = a.size
        self.win.configure(x=0, y=0, width=w, height=h)
        self.d.sync()

    def stop(self):
        for p in reversed(self.procs):
            p.kill()
            p.wait()
        if not self.args.home:
            shutil.rmtree(self.home, ignore_errors=True)

    # -- input ---------------------------------------------------------------------------------
    def _fake(self, kind, detail=0, x=0, y=0):
        xtest.fake_input(self.d, kind, detail, x=x, y=y)
        self.d.sync()

    def move(self, x, y):
        self._fake(X.MotionNotify, x=x, y=y)

    def button(self, x, y, button=1, count=1):
        self.move(x, y)
        time.sleep(0.2)
        for _ in range(count):
            self._fake(X.ButtonPress, button)
            time.sleep(0.08)
            self._fake(X.ButtonRelease, button)
            time.sleep(0.12)

    def drag(self, x1, y1, x2, y2, secs=2.0):
        self.move(x1, y1)
        time.sleep(0.3)
        self._fake(X.ButtonPress, 1)
        steps = max(10, int(secs * 10))
        for i in range(1, steps + 1):
            self.move(x1 + (x2 - x1) * i // steps, y1 + (y2 - y1) * i // steps)
            time.sleep(secs / steps)
        self._fake(X.ButtonRelease, 1)

    def _keycode(self, name):
        sym = XK.string_to_keysym(name)
        if sym == 0:
            sys.exit(f"unknown key {name!r}")
        return self.d.keysym_to_keycode(sym)

    def key(self, spec):
        # Keyboard input needs the window focused first.
        self.win.set_input_focus(X.RevertToParent, X.CurrentTime)
        self.d.sync()
        parts = spec.split("+")
        codes = [self._keycode(p) for p in reversed(parts)]  # modifiers first
        for c in codes:
            self._fake(X.KeyPress, c)
        for c in reversed(codes):
            self._fake(X.KeyRelease, c)
        time.sleep(0.15)

    def type_text(self, text):
        shift_code = self._keycode("Shift_L")
        for ch in text:
            # Latin-1 printable characters are their own keysym; Return/Tab by name.
            sym = {"\n": XK.string_to_keysym("Return"), "\t": XK.string_to_keysym("Tab")}.get(ch, ord(ch))
            code = self.d.keysym_to_keycode(sym)
            if code == 0:
                sys.exit(f"cannot type {ch!r}")
            # Shifted when the unshifted keysym of that key is not the one asked for.
            shift = self.d.keycode_to_keysym(code, 0) != sym
            self.win.set_input_focus(X.RevertToParent, X.CurrentTime)
            if shift:
                self._fake(X.KeyPress, shift_code)
            self._fake(X.KeyPress, code)
            self._fake(X.KeyRelease, code)
            if shift:
                self._fake(X.KeyRelease, shift_code)
            time.sleep(0.08)

    def shot(self, path):
        w, h = self.args.size
        img = self.d.screen().root.get_image(0, 0, w, h, X.ZPixmap, 0xFFFFFFFF)
        Image.frombytes("RGB", (w, h), img.data, "raw", "BGRX").save(path)
        print("saved", path)

    def run(self, step):
        name, _, arg = step.partition(":")
        nums = [int(v) for v in arg.replace("x", ",").split(",") if v.lstrip("-").isdigit()]
        if name == "wait":
            time.sleep(float(arg))
        elif name == "shot":
            self.shot(arg)
        elif name == "click":
            self.button(*nums[:2])
        elif name == "rclick":
            self.button(*nums[:2], button=3)
        elif name == "dclick":
            self.button(*nums[:2], count=2)
        elif name == "move":
            self.move(*nums[:2])
        elif name == "drag":
            secs = float(arg.split(",")[4]) if arg.count(",") >= 4 else 2.0
            self.drag(*nums[:4], secs=secs)
        elif name == "key":
            self.key(arg)
        elif name == "type":
            self.type_text(arg)
        elif name == "resize":
            self.win.configure(width=nums[0], height=nums[1])
            self.args.size = (nums[0], nums[1])
            self.d.sync()
        else:
            sys.exit(f"unknown step {step!r}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("--binary", default="target/debug/nicti")
    ap.add_argument("--catalog", default=None, help="catalog .sqlite (default: a fresh one in the profile dir)")
    ap.add_argument("--size", default="900x600", help="window size WxH")
    ap.add_argument("--screen", default="1600x1000", help="Xvfb screen WxH")
    ap.add_argument("--display", default=":99")
    ap.add_argument("--home", default=None, help="keep the isolated profile here (default: a temp dir)")
    ap.add_argument("--control", action="store_true", help="start the app with --control")
    ap.add_argument("steps", nargs="+")
    args = ap.parse_args()
    args.size = tuple(int(v) for v in args.size.split("x"))
    args.binary = os.path.abspath(args.binary)
    s = Session(args)
    if args.catalog is None:
        args.catalog = os.path.join(s.home, "catalog.sqlite")
    args.catalog = os.path.abspath(args.catalog)
    try:
        s.start()
        for step in args.steps:
            s.run(step)
    finally:
        s.stop()


if __name__ == "__main__":
    main()
