#!/usr/bin/env python3
"""Regenerate the media fixtures used by the image widget tests.

Run: python3 tests/fixtures/media/make_fixtures.py
Needs Pillow for the PNGs. The outputs are committed; tests do not run this.

The GIF is assembled by hand (explicit frame rectangles, disposal methods and
a transparent index) so the result does not depend on Pillow's frame optimiser.
"""
import os
import struct

from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
RED, GREEN, BLUE, YELLOW, WHITE = (
    (255, 0, 0),
    (0, 255, 0),
    (0, 0, 255),
    (255, 255, 0),
    (255, 255, 255),
)


def out(name):
    return os.path.join(HERE, name)


# palette.png: 4x2, 8-bit indexed. Not square, so contain/cover differ.
#   R G B Y
#   Y B G R
pal = Image.new("P", (4, 2))
pal.putpalette([*RED, *GREEN, *BLUE, *YELLOW] + [0] * (256 * 3 - 12))
pal.putdata([0, 1, 2, 3, 3, 2, 1, 0])
pal.save(out("palette.png"), optimize=False)

# alpha.png: 4x4 RGBA with partial transparency.
#   row 0: opaque red, opaque green, half-transparent blue, fully transparent
#   rows 1-2: half-transparent white
#   row 3: opaque yellow
rgba = Image.new("RGBA", (4, 4))
for x, c in enumerate([(*RED, 255), (*GREEN, 255), (*BLUE, 128), (0, 0, 0, 0)]):
    rgba.putpixel((x, 0), c)
for y in (1, 2):
    for x in range(4):
        rgba.putpixel((x, y), (*WHITE, 128))
for x in range(4):
    rgba.putpixel((x, 3), (*YELLOW, 255))
rgba.save(out("alpha.png"))


def lzw_compress(indices, min_code):
    """GIF LZW (LSB-first codes, clear code at start, reset when full)."""
    clear, eoi = 1 << min_code, (1 << min_code) + 1
    acc, nbits, buf = 0, 0, bytearray()

    def emit(code, size):
        nonlocal acc, nbits
        acc |= code << nbits
        nbits += size
        while nbits >= 8:
            buf.append(acc & 0xFF)
            acc >>= 8
            nbits -= 8

    def fresh():
        return {bytes([i]): i for i in range(clear)}, eoi + 1, min_code + 1

    table, next_code, size = fresh()
    emit(clear, size)
    cur = b""
    for i in indices:
        nxt = cur + bytes([i])
        if nxt in table:
            cur = nxt
            continue
        emit(table[cur], size)
        if next_code < 4096:
            table[nxt] = next_code
            if next_code == (1 << size) and size < 12:
                size += 1
            next_code += 1
        else:
            emit(clear, size)
            table, next_code, size = fresh()
        cur = bytes([i])
    if cur:
        emit(table[cur], size)
    emit(eoi, size)
    if nbits:
        buf.append(acc & 0xFF)
    return bytes(buf)


def gif_frame(x, y, w, h, indices, delay_cs, disposal, transparent=None):
    flags = (disposal << 2) | (1 if transparent is not None else 0)
    gce = bytes([0x21, 0xF9, 4, flags]) + struct.pack("<H", delay_cs)
    gce += bytes([transparent or 0, 0])
    desc = b"\x2c" + struct.pack("<HHHH", x, y, w, h) + b"\x00"
    min_code = 2
    data = lzw_compress(indices, min_code)
    blocks = b"".join(
        bytes([len(data[i : i + 255])]) + data[i : i + 255] for i in range(0, len(data), 255)
    )
    return gce + desc + bytes([min_code]) + blocks + b"\x00"


# anim.gif: 8x8 logical screen, global palette 0 red, 1 green, 2 blue, 3 yellow.
#   frame 0: whole screen red, 100 ms, disposal 1 (keep)
#   frame 1: 4x4 blue square at (2,2), 200 ms, disposal 2 (restore background)
#   frame 2: 4x4 at (0,0), green with a 2x2 transparent hole at (1,1)..(2,2),
#            50 ms, disposal 1
# Composited expectation (see tests in src/media.rs):
#   f0: all red
#   f1: red + blue square at (2..6, 2..6)
#   f2: the blue square was cleared to transparent by disposal 2; green covers
#       (0..4, 0..4) except the hole, where red shows at (1,1) and nothing
#       (transparent) at (2,2).
gct = bytes([*RED, *GREEN, *BLUE, *YELLOW])
header = b"GIF89a" + struct.pack("<HH", 8, 8) + bytes([0x80 | 0x01, 0, 0]) + gct
hole = [3 if (x in (1, 2) and y in (1, 2)) else 1 for y in range(4) for x in range(4)]
frames = [
    gif_frame(0, 0, 8, 8, [0] * 64, 10, 1),
    gif_frame(2, 2, 4, 4, [2] * 16, 20, 2),
    gif_frame(0, 0, 4, 4, hole, 5, 1, transparent=3),
]
with open(out("anim.gif"), "wb") as f:
    f.write(header + b"".join(frames) + b"\x3b")

check = Image.open(out("anim.gif"))
assert check.n_frames == 3, check.n_frames

# assets/example.png: 32x16 gradient with a white border, used by config/example.toml.
ex = Image.new("RGB", (32, 16))
for y in range(16):
    for x in range(32):
        ex.putpixel((x, y), (x * 8, 40 + y * 10, 255 - x * 8))
for x in range(32):
    ex.putpixel((x, 0), WHITE)
    ex.putpixel((x, 15), WHITE)
for y in range(16):
    ex.putpixel((0, y), WHITE)
    ex.putpixel((31, y), WHITE)
os.makedirs(os.path.join(ROOT, "assets"), exist_ok=True)
ex.save(os.path.join(ROOT, "assets", "example.png"))

# detail.png: 64x32 RGB with fine detail for the smooth-downscale test.
#   left half: 1px red/blue checkerboard; right half: 2px diagonal green stripes
#   on dark grey.
det = Image.new("RGB", (64, 32))
for y in range(32):
    for x in range(64):
        if x < 32:
            c = RED if (x + y) % 2 == 0 else BLUE
        else:
            c = GREEN if ((x + y) // 2) % 2 == 0 else (40, 40, 40)
        det.putpixel((x, y), c)
det.save(out("detail.png"))
