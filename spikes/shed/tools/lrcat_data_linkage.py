#!/usr/bin/env python3
"""Research #156: `.lrcat-data`-to-catalog blob linkage (see ADR-0156).

Not a Cargo subcommand -- `.lrcat-data` is a RocksDB database, and `rocksdb` cannot be added as a
workspace dependency without re-triggering the same Cargo `links = "sqlite3"`-style uniqueness
conflict ADR-0061 Q8 already found for `lrcat-extractor` (see `.claude/rules/lrc-migration/
REFERENCE.md`). This is a read-only, stdlib-only probe run against a *closed* LRC backup (never
the live/locked catalog -- `shed`'s own `open_backup` guard is the enforcement point for that
rule; this script has no equivalent guard of its own, so only ever point it at an already-closed
backup extraction).

Requires the `rocksdb_sst_dump` CLI (`brew install rocksdb`) on PATH, or pass --sst-dump-bin.

Usage:
    python3 lrcat_data_linkage.py \
        --lrcat /path/to/extracted/Catalog.lrcat \
        --lrcat-data-dir /path/to/extracted/Catalog.lrcat-data

Prints aggregate counts only -- no key, digest, path, or catalog content is ever printed, per
ADR-0061's privacy note (this repo is public and real backups may contain real people's names).
"""

import argparse
import glob
import mmap
import os
import re
import subprocess
import sys
from collections import defaultdict

LINE_RE = re.compile(r"^'([0-9A-Fa-f]+)' seq:(\d+), type:(\d+) => ([0-9A-Fa-f]*)")
FIELD_RE = re.compile(rb'(\w+)\s*=\s*"([0-9A-F]{32})"')


def read_varint(b: bytes, i: int) -> tuple[int, int]:
    shift = 0
    result = 0
    while True:
        byte = b[i]
        i += 1
        result |= (byte & 0x7F) << shift
        if not (byte & 0x80):
            break
        shift += 7
    return result, i


def scan_sst_files(sst_dump_bin: str, lrcat_data_dir: str):
    """Returns (num_keys, distinct_file_numbers, sizes, real_keys_set)."""
    real_keys = set()
    file_numbers = set()
    sizes = []
    num_keys = 0

    for sst_path in sorted(glob.glob(os.path.join(lrcat_data_dir, "*.sst"))):
        out = subprocess.run(
            [sst_dump_bin, f"--file={sst_path}", "--command=scan", "--output_hex"],
            capture_output=True, text=True, check=True,
        ).stdout
        for line in out.splitlines():
            m = LINE_RE.match(line)
            if not m:
                continue
            key_hex, _seq, _typ, val_hex = m.groups()
            num_keys += 1
            key_bytes = bytes.fromhex(key_hex)
            try:
                key_str = key_bytes.decode("ascii")
            except UnicodeDecodeError:
                continue
            if len(key_str) != 32 or not re.fullmatch(r"[0-9A-F]{32}", key_str):
                continue  # e.g. the internal "rocksdbIntegrityId" bookkeeping key
            real_keys.add(key_str)

            if val_hex:
                val = bytes.fromhex(val_hex)
                try:
                    i = 1  # val[0] is the blob-index type byte (1 = kBlobType)
                    fn, i = read_varint(val, i)
                    _off, i = read_varint(val, i)
                    sz, i = read_varint(val, i)
                    file_numbers.add(fn)
                    sizes.append(sz)
                except (IndexError, ValueError):
                    pass

    return num_keys, file_numbers, sizes, real_keys


def scan_catalog_fields(lrcat_path: str):
    """Returns {field_name: set_of_32char_hex_values} for every `Field = "HEX32"` literal
    found anywhere in the raw catalog file (these live inside the Lua-literal develop-settings
    text blobs, e.g. Adobe_imageDevelopSettings.text)."""
    field_values = defaultdict(set)
    with open(lrcat_path, "rb") as f:
        mm = mmap.mmap(f.fileno(), 0, access=mmap.ACCESS_READ)
        try:
            for m in FIELD_RE.finditer(mm):
                field_values[m.group(1).decode("ascii")].add(m.group(2).decode("ascii"))
        finally:
            mm.close()
    return field_values


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--lrcat", required=True, help="Path to the extracted (closed) .lrcat SQLite file")
    ap.add_argument("--lrcat-data-dir", required=True, help="Path to the extracted .lrcat-data/ RocksDB dir")
    ap.add_argument("--sst-dump-bin", default="rocksdb_sst_dump")
    ap.add_argument("--min-overlap", type=int, default=10,
                     help="Only print catalog fields whose blob-key overlap is >= this (cuts "
                          "regex-boundary noise from truncated field-name matches)")
    args = ap.parse_args()

    print("== RocksDB .lrcat-data scan ==")
    num_keys, file_numbers, sizes, real_keys = scan_sst_files(args.sst_dump_bin, args.lrcat_data_dir)
    print(f"total SST entries: {num_keys}")
    print(f"real (32-char hex) blob keys: {len(real_keys)}")
    print(f"distinct blob file numbers referenced: {len(file_numbers)}")
    if file_numbers:
        print(f"blob file number range: {min(file_numbers)}..{max(file_numbers)}")
    if sizes:
        print(f"blob value sizes: min={min(sizes)} max={max(sizes)} total={sum(sizes)}")

    print()
    print("== Catalog field -> blob-key overlap ==")
    field_values = scan_catalog_fields(args.lrcat)
    covered = set()
    for field, vals in sorted(field_values.items(), key=lambda kv: -len(kv[1])):
        overlap = real_keys & vals
        if len(overlap) >= args.min_overlap:
            covered |= overlap
            print(f"{field:30s} distinct={len(vals):8d} overlap={len(overlap):8d}")

    print()
    print(f"total blob keys: {len(real_keys)}")
    print(f"covered by a catalog field above threshold: {len(covered)}")
    print(f"unaccounted: {len(real_keys - covered)}")


if __name__ == "__main__":
    sys.exit(main())
