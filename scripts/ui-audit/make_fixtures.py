#!/usr/bin/env python3
"""Synthetic photo fixtures for UI audits on a host with no real RAW files (#777).

    make_fixtures.py OUT_DIR [--jpegs N] [--dngs N] [--no-matrix]

Writes N gradient JPEGs (Pillow) and N tiny uncompressed 16-bit RGGB DNGs (standard library only;
a port of the builder in crates/nicti-cornea/tests/real_dng.rs) into OUT_DIR. The DNGs carry no
embedded preview, so the grid shows the decode fallback (#770); `--no-matrix` writes an all-zero
ColorMatrix1 to exercise the sRGB-primaries notice (#756, #771). Nothing here is a real camera
file: the pixels are a smooth gradient, the make/model say "Synthetic".
"""
import argparse
import os
import struct


class Dng:
    """Little-endian TIFF/DNG writer: one uncompressed 16-bit image in IFD0."""

    def __init__(self):
        self.entries = []  # (tag, type, count, bytes | None-for-strip-offset)

    def put(self, tag, ty, count, data):
        self.entries.append((tag, ty, count, data))

    def short(self, tag, vals):
        self.put(tag, 3, len(vals), struct.pack(f"<{len(vals)}H", *vals))

    def long(self, tag, vals):
        self.put(tag, 4, len(vals), struct.pack(f"<{len(vals)}I", *vals))

    def raw(self, tag, ty, data):
        self.put(tag, ty, len(data), data)

    def rationals(self, tag, ty, pairs):
        fmt = "<ii" if ty == 10 else "<II"
        self.put(tag, ty, len(pairs), b"".join(struct.pack(fmt, n, d) for n, d in pairs))

    def strip_offsets(self):
        self.put(0x0111, 4, 1, None)

    def finish(self, strip):
        entries = sorted(self.entries, key=lambda e: e[0])
        ifd_len = 2 + len(entries) * 12 + 4
        cursor = 8 + ifd_len
        out_of_line = []
        for _, _, _, data in entries:
            if data is not None and len(data) > 4:
                out_of_line.append(cursor)
                cursor += len(data) + len(data) % 2
        strip_off = cursor
        out = bytearray(b"II*\x00" + struct.pack("<I", 8) + struct.pack("<H", len(entries)))
        offs = iter(out_of_line)
        for tag, ty, count, data in entries:
            out += struct.pack("<HHI", tag, ty, count)
            if data is None:
                out += struct.pack("<I", strip_off)
            elif len(data) <= 4:
                out += data.ljust(4, b"\x00")
            else:
                out += struct.pack("<I", next(offs))
        out += struct.pack("<I", 0)
        for _, _, _, data in entries:
            if data is not None and len(data) > 4:
                out += data + (b"\x00" if len(data) % 2 else b"")
        assert len(out) == strip_off
        return bytes(out) + strip


def synthetic_dng(w=128, h=96, seed=0, with_matrix=True):
    pixels = bytearray()
    for y in range(h):
        for x in range(w):
            v = 4000 + (x * 200 + y * 120 + seed * 900) % 50000 + ((x & 1) + (y & 1) * 2) * 50
            pixels += struct.pack("<H", min(v, 65000))
    d = Dng()
    d.long(0x00FE, [0])  # NewSubfileType: full-resolution image
    d.long(0x0100, [w])
    d.long(0x0101, [h])
    d.short(0x0102, [16])  # BitsPerSample
    d.short(0x0103, [1])  # Compression: none
    d.short(0x0106, [32803])  # Photometric: CFA
    d.raw(0x010F, 2, b"Synthetic\x00")  # Make
    d.raw(0x0110, 2, b"Fixture\x00")  # Model
    d.strip_offsets()
    d.short(0x0115, [1])  # SamplesPerPixel
    d.long(0x0116, [h])  # RowsPerStrip
    d.long(0x0117, [w * h * 2])  # StripByteCounts
    d.short(0x828D, [2, 2])  # CFARepeatPatternDim
    d.raw(0x828E, 1, bytes([0, 1, 1, 2]))  # CFAPattern: RGGB
    d.raw(0xC612, 1, bytes([1, 4, 0, 0]))  # DNGVersion
    d.raw(0xC614, 2, b"Synthetic Fixture\x00")  # UniqueCameraModel
    d.long(0xC61D, [65535])  # WhiteLevel
    matrix = (
        [(6, 10), (2, 10), (1, 10), (2, 10), (7, 10), (1, 10), (1, 10), (1, 10), (8, 10)]
        if with_matrix
        else [(0, 1)] * 9
    )
    d.rationals(0xC621, 10, matrix)  # ColorMatrix1 (XYZ -> camera)
    d.rationals(0xC628, 5, [(1, 2), (1, 1), (3, 5)])  # AsShotNeutral
    d.short(0xC65A, [21])  # CalibrationIlluminant1: D65
    return d.finish(bytes(pixels))


def write_jpegs(out, n):
    from PIL import Image

    for i in range(n):
        w, h = (960, 640) if i % 3 else (640, 960)
        img = Image.new("RGB", (w, h))
        px = img.load()
        for y in range(h):
            for x in range(w):
                px[x, y] = ((x * 255 // w + i * 40) % 256, (y * 255 // h) % 256, (i * 70) % 256)
        img.save(os.path.join(out, f"synthetic_{i:03}.jpg"), quality=88)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    ap.add_argument("out_dir")
    ap.add_argument("--jpegs", type=int, default=6)
    ap.add_argument("--dngs", type=int, default=3)
    ap.add_argument("--no-matrix", action="store_true", help="DNGs with an all-zero ColorMatrix1")
    a = ap.parse_args()
    os.makedirs(a.out_dir, exist_ok=True)
    if a.jpegs:
        write_jpegs(a.out_dir, a.jpegs)
    for i in range(a.dngs):
        path = os.path.join(a.out_dir, f"synthetic_{i:03}.dng")
        with open(path, "wb") as f:
            f.write(synthetic_dng(seed=i, with_matrix=not a.no_matrix))
    print(f"wrote {a.jpegs} JPEG(s) and {a.dngs} DNG(s) to {a.out_dir}")


if __name__ == "__main__":
    main()
