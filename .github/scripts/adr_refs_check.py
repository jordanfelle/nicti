#!/usr/bin/env python3
"""Fails if any ADR-NNNN / docs/adr/NNNN reference doesn't resolve to a real ADR file.

Run from the repo root. Excludes spikes/retina/vendor (a git submodule) and Cargo.lock, same as
the #183 rekey sweep that motivated this check. Scans every tracked text file (not an extension
allowlist) since the rekey sweep itself touched references in .rs/.md/.toml/.json/.yml/.ps1/.wgsl/
.slint/.h/.ahk files -- a narrower allowlist would miss stale references in file types not yet
seen.
"""
import re
import subprocess
import sys

# Matches a whole ADR citation run after one leading anchor: the "ADR-" word-prefix, a "docs/adr/"
# path prefix, or a bare relative "adr/" path segment (as used by a markdown link's href one
# directory up from docs/ -- distinct from the "docs/adr/" form that appears in link *text*).
# LOCAL_LINK_PATTERN below is a separate, additional check applied only within docs/adr/ itself,
# for a markdown link target with no prefix at all ("](NNNN-slug.md)") -- a same-directory
# relative link from one ADR file to another. It's kept as its own pattern, gated to that one
# directory in main() below, rather than folded into RUN_PATTERN's anchor set, since its zero-width
# "immediately after '](' " lookbehind has no path-scoping of its own -- applied repo-wide it would
# treat any future unrelated markdown link elsewhere whose target happens to start with 4 digits
# (e.g. a dated changelog entry) as an ADR citation.
#
# Two mutually exclusive shapes, tried in this order:
#   1. A bare numeric list: several ticket numbers chained by "/" (or "," or an en-dash range)
#      after one anchor. The list can wrap onto the next source line after its separator -- this
#      repo's own prose does that -- so optional whitespace is allowed between a separator and the
#      next number. The separator set deliberately excludes the ASCII hyphen: a hyphen right after
#      a number is always the start of a filename slug (shape 2 below) in every real citation form
#      this repo uses, never a bare-list join -- treating hyphen as a list separator here would
#      misparse an all-digits slug component as a second citation number.
#   2. A single number optionally followed by a real filename slug (which must start with a
#      letter, precisely so a digit-only slug segment is never swallowed as part of the slug) and
#      an optional ".md".
# Each number is captured as \d+ (not a fixed 4 digits) and required to not be immediately
# followed by another word character, so:
#   - a wrong-digit-count reference is reported as invalid below rather than silently un-matched
#     (a fixed 4-digit pattern would partial-match just the first 4 digits of a longer run and
#     could accidentally equal a real, unrelated ADR number, hiding the mistake entirely); and
#   - a generic placeholder like this file's own docstring's all-caps letter suffix never gets
#     misread as a truncated real number, since digits directly followed by a letter never match.
_SHAPES = r"(?:\d+(?!\w)(?:[/,–]\s*\d+(?!\w))+|\d+(?!\w)(?:-[a-z][a-z0-9-]*)?(?:\.md)?)"
RUN_PATTERN = re.compile(r"(?:ADR-|docs/adr/|(?<!\w)adr/)" + _SHAPES)
LOCAL_LINK_PATTERN = re.compile(r"(?<=\]\()" + _SHAPES)
# A filename-shaped token (a number, optionally a real slug, and ".md") is checked against the
# exact set of tracked filenames, not just its leading number -- otherwise "](0143-wrong-slug.md)"
# or a slug-less "](0143.md)" would both pass just because ADR-0143 exists under a *different*
# real filename (#195's review).
FILENAME_TOKEN = re.compile(r"\d+(?:-[a-z][a-z0-9-]*)?\.md")
NUM_PATTERN = re.compile(r"\d+")


def git_files():
    out = subprocess.run(["git", "ls-files"], check=True, capture_output=True, text=True).stdout
    return [
        f
        for f in out.splitlines()
        if not f.startswith("spikes/retina/vendor/") and f != "Cargo.lock"
    ]


def is_text_file(path):
    try:
        with open(path, "rb") as f:
            chunk = f.read(4096)
        return b"\0" not in chunk
    except (FileNotFoundError, IsADirectoryError):
        return False


def known_adr_files():
    out = subprocess.run(
        ["git", "ls-files", "docs/adr"], check=True, capture_output=True, text=True
    ).stdout
    numbers = set()
    filenames = set()
    for path in out.splitlines():
        m = re.match(r"docs/adr/((\d{4})[a-z]?-.+)", path)
        if m:
            filenames.add(m.group(1))
            numbers.add(m.group(2))
    return numbers, filenames


def main():
    known, filenames = known_adr_files()
    if not known:
        print("ERROR: found zero ADR files under docs/adr/ — refusing to check anything")
        return 1

    bad = []
    for path in git_files():
        if not is_text_file(path):
            continue
        try:
            with open(path, "r", encoding="utf-8") as f:
                content = f.read()
        except UnicodeDecodeError:
            continue
        runs = list(RUN_PATTERN.finditer(content))
        if path.startswith("docs/adr/"):
            runs += list(LOCAL_LINK_PATTERN.finditer(content))
        for run in runs:
            run_text = run.group(0)
            line_start = content.rfind("\n", 0, run.start()) + 1
            line_end = content.find("\n", run.start())
            line_text = content[line_start : line_end if line_end != -1 else None]
            before = line_text[: run.start() - line_start]
            # Whether this run is immediately preceded by the "**Formerly:**" marker at all --
            # whether it's actually SAFE to exempt anything is decided further below, once we
            # know how many citations this run holds.
            formerly_marker = bool(re.search(r"\*\*Formerly:\*\*\s*$", before))

            # Filename-shaped tokens ("0143-slug.md") are checked against the exact set of
            # tracked filenames, not just their leading number -- a correct number with a wrong
            # or nonexistent slug must still fail (see #195's review).
            filename_spans = [fn_m.span() for fn_m in FILENAME_TOKEN.finditer(run_text)]

            def _inside_filename_token(pos, spans=filename_spans):
                return any(start <= pos < end for start, end in spans)

            # One citation-check per filename token and per bare number not already covered by
            # one, merged and ordered by position so "the first citation in this run" means the
            # same thing regardless of which shape it is.
            checks = [("file", m) for m in FILENAME_TOKEN.finditer(run_text)]
            checks += [
                ("num", m)
                for m in NUM_PATTERN.finditer(run_text)
                if not _inside_filename_token(m.start())
            ]
            checks.sort(key=lambda kv: kv[1].start())

            # Exempt the retired-number pointer ONLY when the run holds exactly one citation,
            # and that citation is a bare number (never a filename-shaped one -- every real
            # "**Formerly:**" line in this repo is exactly "ADR-NNNN", never a filename, so
            # there's no legitimate reason to exempt a filename-shaped token here at all). A run
            # with a "**Formerly:**" marker but more than one citation is never exempted at all:
            # two successive attempts to guess "which one is the real pointer" from its position
            # in the run each broke on a different adversarial ordering (whole-run skip, then
            # first-by-position skip, which wrongly exempts a bogus number placed before the real
            # one). Failing loud here -- forcing every citation in an ambiguous multi-citation run
            # to validate normally -- is deliberately conservative rather than inventing a third
            # position-based guess.
            exempt_first = formerly_marker and len(checks) == 1 and checks[0][0] == "num"

            for i, (kind, m) in enumerate(checks):
                is_exempt_pointer = i == 0 and exempt_first
                abs_pos = run.start() + m.start()
                line_no = content.count("\n", 0, abs_pos) + 1
                if kind == "file":
                    if not is_exempt_pointer and m.group(0) not in filenames:
                        bad.append(
                            f"{path}:{line_no}: reference to a nonexistent ADR file "
                            f"{m.group(0)!r} (in {run_text!r})"
                        )
                else:
                    num = m.group(0)
                    if len(num) != 4:
                        # Even the exempt retired-number pointer must be well-formed -- the
                        # exemption only ever meant "don't require this to resolve to a
                        # currently-known number," never "skip basic shape validation too"
                        # (#195's review).
                        bad.append(
                            f"{path}:{line_no}: malformed ADR reference {num!r} "
                            f"(not 4 digits, in {run_text!r})"
                        )
                    elif not is_exempt_pointer and num not in known:
                        bad.append(
                            f"{path}:{line_no}: unresolved reference to ADR-{num} "
                            f"(in {run_text!r})"
                        )

    if bad:
        print(f"Found {len(bad)} unresolved ADR reference(s):")
        for line in bad:
            print(" ", line)
        return 1

    print(f"OK: all ADR references resolve against {len(known)} known ADR numbers.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
