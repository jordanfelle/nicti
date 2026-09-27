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


# RocksDB BlobIndex::Type (db/blob/blob_index.h): the *value's* own leading type byte, distinct
# from the SST record type below. kInlinedTTL=0 and a value smaller than min_blob_size store the
# payload directly in the SST rather than a blob-index record, so only kBlob(=1)/kBlobTTL(=2)
# actually decode as (file_number, offset, size) -- treating every value as a blob index
# regardless of this byte, as an earlier draft of this script did, quietly parses a small inlined
# value's own payload bytes as bogus file/offset/size varints instead of skipping it.
BLOB_INDEX_VALUE_TYPES = {1, 2}  # kBlob, kBlobTTL (kUnknown=3 is invalid/rejected by RocksDB itself)

# RocksDB's own internal per-record type (db/dbformat.h ValueType), printed by sst_dump as
# `type:N` -- NOT the same byte as BLOB_INDEX_VALUE_TYPES above, which is *inside* the value.
# `sst_dump --command=scan` dumps every record physically present in the file, including
# tombstones (kTypeDeletion=0, kTypeSingleDeletion=7) and, for a key updated more than once before
# compaction removed the old version, more than one entry for the same key at different sequence
# numbers -- it does not resolve "what does this DB currently return for this key" the way an
# actual RocksDB read would. Requiring the record type to be kTypeBlobIndex excludes tombstones
# outright; requiring first-occurrence-per-key (RocksDB orders same-key records by descending
# sequence number within a file) keeps only the newest surviving version instead of whichever one
# happened to be scanned last. This is still an approximation, not a full leveled-compaction
# resolution across files -- adequate for a closed, already-compacted catalog backup (every entry
# this ADR measured was `seq:0`, i.e. no multiple live versions existed to resolve), not a
# guarantee against a DB with in-flight compactions.
SST_RECORD_TYPE_BLOB_INDEX = "17"  # kTypeBlobIndex (0x11)


def scan_sst_files(sst_dump_bin: str, lrcat_data_dir: str):
    """Returns (num_keys, distinct_file_numbers, blob_index_by_key, real_keys_set), where
    blob_index_by_key maps each *live* (non-tombstoned, newest-seen) key to its (file_number,
    size) -- see SST_RECORD_TYPE_BLOB_INDEX's docstring above for what "live" means here."""
    real_keys = set()
    blob_index_by_key: dict[str, tuple[int, int]] = {}
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
            key_hex, _seq, typ, val_hex = m.groups()
            num_keys += 1
            if typ != SST_RECORD_TYPE_BLOB_INDEX:
                continue  # a tombstone or any other non-blob-index record type -- not a live blob key
            key_bytes = bytes.fromhex(key_hex)
            try:
                key_str = key_bytes.decode("ascii")
            except UnicodeDecodeError:
                continue
            if len(key_str) != 32 or not re.fullmatch(r"[0-9A-F]{32}", key_str):
                continue  # e.g. the internal "rocksdbIntegrityId" bookkeeping key
            real_keys.add(key_str)

            if key_str in blob_index_by_key:
                continue  # already resolved this key's newest version -- see module docstring above
            if not val_hex:
                continue
            try:
                val = bytes.fromhex(val_hex)
                if not val or val[0] not in BLOB_INDEX_VALUE_TYPES:
                    continue  # inlined small value (below min_blob_size) -- no blob file to point at
                i = 1
                fn, i = read_varint(val, i)
                _off, i = read_varint(val, i)
                sz, i = read_varint(val, i)
                blob_index_by_key[key_str] = (fn, sz)
            except (IndexError, ValueError):
                continue  # malformed/truncated hex or varint for this one record -- skip it, don't abort the scan

    return num_keys, blob_index_by_key, real_keys


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
    num_keys, blob_index_by_key, real_keys = scan_sst_files(args.sst_dump_bin, args.lrcat_data_dir)
    file_numbers = {fn for fn, _sz in blob_index_by_key.values()}
    sizes = [sz for _fn, sz in blob_index_by_key.values()]
    print(f"total SST entries: {num_keys}")
    print(f"real (32-char hex) blob keys: {len(real_keys)}")
    print(f"real keys with a blob-type value (vs. inlined-small or unresolved): {len(blob_index_by_key)}")
    print(f"distinct blob file numbers referenced: {len(file_numbers)}")
    if file_numbers:
        print(f"blob file number range: {min(file_numbers)}..{max(file_numbers)}")
    if sizes:
        print(f"blob value sizes (deduplicated per key): min={min(sizes)} max={max(sizes)} total={sum(sizes)}")

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
