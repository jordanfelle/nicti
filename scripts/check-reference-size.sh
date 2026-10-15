#!/usr/bin/env bash
# Pre-commit: fail when a path-scoped rules file (.claude/rules/<topic>/*.md) is over 12KB.
#
# Those files load IN FULL into a Claude Code session whenever a matching file is edited, and are
# re-read on every turn after that, so a 100KB+ one costs tens of thousands of tokens per turn
# (shutterpaws-tech#4221). Keep them as short indexes (ONE line per entry, ideally <= 8KB) and put the
# detail in .claude/docs/<topic>/REFERENCE-full.md, which is never auto-loaded. Detail lives there; the
# index entry text doubles as its grep key: grep -nF '<entry text>' .claude/docs/<topic>/REFERENCE-full.md
#
# To convert an oversized file: python3 ~/.claude/bin/split-reference.py <file> (also in
# shutterpaws-claude-utils users/jordan/bin). Receives the staged filenames from pre-commit.
set -uo pipefail
HARD=${REFERENCE_HARD_BYTES:-12000}
rc=0
for f in "$@"; do
  case "$f" in .claude/rules/*/*.md) ;; *) continue ;; esac
  [ -f "$f" ] || continue
  size=$(wc -c < "$f")
  if [ "$size" -gt "$HARD" ]; then
    echo "$f is $size bytes (limit $HARD): scoped rules files load in full into context on matching edits." >&2
    echo "  Put the detail in .claude/docs/$(echo "$f" | cut -d/ -f3)/REFERENCE-full.md and keep ONE index line here." >&2
    rc=1
  fi
done
exit $rc
