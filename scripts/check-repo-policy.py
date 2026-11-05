#!/usr/bin/env python3
"""Repo policy checks (#433, adopted from LightCraft's xtask layers/assets checks).

1. Layering: every crates/* member must appear in LAYERS, and a crate may only depend (normal,
   build or dev) on workspace crates in the same or a lower layer. GUI crates (egui/eframe/winit
   family) may only be depended on by nicti-pelt and the root `nicti` binary.
2. Assets: every tracked font/icon/image/model file outside spikes/ and test snapshots needs a
   `path` row in assets/ATTRIBUTION.md, and every row must point at a tracked file.
3. Notices (#695): lensfun, rawler or the vendored LibRaw (nicti-cornea) anywhere in the shipped
   `nicti` package's dependency closure needs its name in assets/notices/THIRD-PARTY-NOTICES.txt.

Run from anywhere: python3 scripts/check-repo-policy.py
"""
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

LAYERS = {
    "nicti-claw": 0, "nicti-collar": 0, "nicti-haw": 0, "nicti-pounce": 0, "nicti-pawprint": 0,
    "nicti-shed": 0, "nicti-scent": 0, "nicti-prowl": 0, "nicti-vibrissa": 0,
    "nicti-calico": 1, "nicti-iris": 1, "nicti-stalk": 1, "dewclaw": 1,
    "nicti-cornea": 2, "nicti-preen": 2,
    "nicti-tapetum": 3, "nicti-lair": 3,
    "nicti-groom": 4, "nicti-siamese": 4,
    "nicti-stray": 5,
    "nicti-pelt": 6,
    "nicti": 7,
}
# Existing production -> spikes/ edges (nicti-groom builds on the promoted-in-place spike crates).
# Grandfathered so the check can land; remove entries as the spikes graduate into crates/.
KNOWN_SPIKE_DEPS = {("nicti-groom", d) for d in ("crouch", "litter", "rods", "rosette")}
GUI_PREFIXES = ("egui", "eframe", "winit", "epaint", "emath", "accesskit", "iced", "slint")
GUI_EXACT = {"rfd", "raw-window-handle"}
GUI_ALLOWED = {"nicti-pelt", "nicti"}
ASSET_EXT = re.compile(
    r"\.(ttf|otf|ttc|eot|woff2?|png|jpe?g|gif|svg|ico|icns|webp|avif|bmp|tiff?"
    r"|onnx|ort|pt|pth|safetensors|gguf|bin)$",
    re.I,
)
# Only per-crate egui_kittest snapshots and the spikes/ research tree are exempt.
ASSET_SKIP = re.compile(r"^spikes/|^crates/[^/]+/tests/snapshots/")


def is_gui(dep):
    return dep.startswith(GUI_PREFIXES) or dep in GUI_EXACT


def sh(*cmd):
    r = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    if r.returncode:
        sys.exit(f"error: {' '.join(cmd)} failed:\n{r.stderr}")
    return r.stdout


def check_layers():
    meta = json.loads(sh("cargo", "metadata", "--no-deps", "--format-version", "1"))
    members = {p["name"]: p for p in meta["packages"]}
    errs = []
    for name, pkg in members.items():
        in_crates = os.path.relpath(pkg["manifest_path"], ROOT).startswith("crates" + os.sep)
        if in_crates and name not in LAYERS:
            errs.append(f"{name}: crates/* member missing from LAYERS in scripts/check-repo-policy.py")
        for dep in pkg["dependencies"]:
            d = dep["name"]
            if is_gui(d) and name not in GUI_ALLOWED:
                errs.append(f"{name} depends on GUI crate {d}; only {sorted(GUI_ALLOWED)} may")
            if name in LAYERS and d in LAYERS and LAYERS[d] > LAYERS[name]:
                errs.append(
                    f"{name} (layer {LAYERS[name]}) depends on {d} (layer {LAYERS[d]}): upward dependency"
                )
            if d in GUI_ALLOWED and name not in GUI_ALLOWED and name != "nicti":
                errs.append(f"{name} depends on app crate {d}; only the shell may")
            if name in LAYERS and d in members and d not in LAYERS \
                    and (name, d) not in KNOWN_SPIKE_DEPS and not is_gui(d):
                errs.append(f"{name} depends on unlayered workspace crate {d}")
    return errs


def check_assets():
    tracked = set(sh("git", "ls-files", "-z").split("\0")) - {""}
    assets = {f for f in tracked if ASSET_EXT.search(f) and not ASSET_SKIP.search(f)}
    path = os.path.join(ROOT, "assets", "ATTRIBUTION.md")
    if not os.path.exists(path):
        return ["assets/ATTRIBUTION.md is missing"]
    errs, rows = [], set()
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            m = re.match(r"^\|\s*`([^`]+)`\s*\|(.*)$", line.rstrip("\r\n"))
            if not m:
                continue
            cells = [c.strip() for c in m.group(2).strip().rstrip("|").split("|")]
            if m.group(1) in rows:
                errs.append(f"assets/ATTRIBUTION.md: duplicate row for {m.group(1)}")
            if len(cells) < 3 or not all(cells[:3]):
                errs.append(f"assets/ATTRIBUTION.md: {m.group(1)} needs source, license and license text")
            rows.add(m.group(1))
    errs += [f"{a}: no row in assets/ATTRIBUTION.md" for a in sorted(assets - rows)]
    errs += [f"assets/ATTRIBUTION.md lists {r}, which is not a tracked asset" for r in sorted(rows - assets)]
    return errs


# Shipped components whose licences require a notice (#695). A Cargo crate in the dependency closure
# of the shipped `nicti` package (read from Cargo.lock, so it is offline and transitive) must be
# named, as a whole word, in the notices file. `nicti-cornea` statically compiles the vendored
# LibRaw. Cargo.lock also lists dev-dependencies, so this is conservative: a dev-only dependency on
# one of these would ask for a notice it does not need.
NOTICE_CRATES = {"lensfun": "LensFun", "rawler": "rawler", "nicti-cornea": "LibRaw"}
NOTICES = os.path.join("assets", "notices", "THIRD-PARTY-NOTICES.txt")
SHIPPED_ROOT = "nicti"


def lock_closure(lock_text, root):
    """Names of every package reachable from `root` in a Cargo.lock."""
    deps = {}
    for block in lock_text.split("[[package]]")[1:]:
        m = re.search(r'^name = "([^"]+)"', block, re.M)
        if not m:
            continue
        d = re.search(r"^dependencies = \[(.*?)^\]", block, re.M | re.S)
        names = re.findall(r'"([^" ]+)(?: [^"]*)?"', d.group(1)) if d else []
        deps.setdefault(m.group(1), set()).update(names)
    seen, todo = set(), [root]
    while todo:
        n = todo.pop()
        if n not in seen:
            seen.add(n)
            todo.extend(deps.get(n, ()))
    return seen


def check_notices():
    """Every shipped component that needs a notice is named in assets/notices/THIRD-PARTY-NOTICES.txt."""
    path = os.path.join(ROOT, NOTICES)
    if not os.path.exists(path):
        return [f"{NOTICES} is missing"]
    with open(path, encoding="utf-8") as fh:
        text = fh.read()
    with open(os.path.join(ROOT, "Cargo.lock"), encoding="utf-8") as fh:
        closure = lock_closure(fh.read(), SHIPPED_ROOT)
    errs = []
    for crate, needle in NOTICE_CRATES.items():
        if crate in closure and not re.search(rf"\b{re.escape(needle)}\b", text):
            errs.append(f"{SHIPPED_ROOT} ships {crate}; {NOTICES} must mention {needle}")
    return errs


def main():
    errs = check_layers() + check_assets() + check_notices()
    for e in errs:
        print(f"error: {e}", file=sys.stderr)
    if not errs:
        print("repo policy: layering, asset attribution and third-party notices OK")
    return 1 if errs else 0


if __name__ == "__main__":
    sys.exit(main())
