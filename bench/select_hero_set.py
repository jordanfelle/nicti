#!/usr/bin/env python3
"""Deterministically select the hero-scenario's 50-image working set.

Selects only from the `z8` bucket of docs/ref-10k-manifest.csv (the bucket the
performance targets in docs/benchmarks.md are actually measured against), stratified
across four ISO bands so denoise load is representative of a real event mix, then
writes the sorted `id` column to docs/benchmarks/hero-set.txt.

Re-running this script must always produce the same output (fixed seed, deterministic
tie-breaking) so the hero set stays stable across benchmark runs.
"""

import csv
import random
from pathlib import Path

MANIFEST = Path(__file__).resolve().parent.parent / "docs" / "ref-10k-manifest.csv"
OUTPUT = Path(__file__).resolve().parent.parent / "docs" / "benchmarks" / "hero-set.txt"
SEED = 43  # ticket number, arbitrary but fixed
TOTAL = 50

# (label, inclusive-lower, inclusive-upper-or-None, target count)
BANDS = [
    ("<=800", 0, 800, 13),
    ("801-3200", 801, 3200, 13),
    ("3201-6400", 3201, 6400, 12),
    (">6400", 6401, None, 12),
]


def band_for(iso: int) -> str:
    for label, lo, hi, _ in BANDS:
        if iso >= lo and (hi is None or iso <= hi):
            return label
    raise ValueError(f"iso {iso} matched no band")


def main() -> None:
    rows = []
    with MANIFEST.open(newline="") as f:
        for row in csv.DictReader(f):
            if row["bucket"] == "z8":
                rows.append(row)

    by_band: dict[str, list[dict]] = {label: [] for label, *_ in BANDS}
    for row in rows:
        by_band[band_for(int(row["iso"]))].append(row)

    rng = random.Random(SEED)
    selected: list[str] = []
    for label, _, _, count in BANDS:
        pool = sorted(by_band[label], key=lambda r: r["id"])
        if len(pool) < count:
            raise ValueError(f"band {label} has only {len(pool)} files, need {count}")
        selected.extend(r["id"] for r in rng.sample(pool, count))

    assert len(selected) == TOTAL, len(selected)
    selected.sort()

    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text("\n".join(selected) + "\n")
    print(f"wrote {len(selected)} ids to {OUTPUT}")


if __name__ == "__main__":
    main()
