#!/usr/bin/env python3
"""Writes prop.bdf: a tiny proportional 5x7-ish test font (hand-drawn glyphs,
public domain), used by the font golden tests. Run: python3 make_prop_font.py"""
import os

GLYPHS = {
    "i": ["#", ".", "#", "#", "#", "#", "#"],
    "l": ["##.", ".#.", ".#.", ".#.", ".#.", ".#.", "###"],
    "1": [".#.", "##.", ".#.", ".#.", ".#.", ".#.", "###"],
    "0": [".###.", "#...#", "#..##", "#.#.#", "##..#", "#...#", ".###."],
    "2": [".###.", "#...#", "....#", "...#.", "..#..", ".#...", "#####"],
    "3": ["####.", "....#", "...#.", "..##.", "....#", "#...#", ".###."],
    "5": ["#####", "#....", "####.", "....#", "....#", "#...#", ".###."],
    ":": [".", "#", ".", ".", "#", ".", "."],
    ".": [".", ".", ".", ".", ".", ".", "#"],
    "W": ["#.....#", "#.....#", "#..#..#", "#..#..#", "#.#.#.#", "##...##", "#.....#"],
    "m": [".....", "##.#.", "#.#.#", "#.#.#", "#.#.#", "#.#.#", "#.#.#"],
    "H": ["#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#"],
    "C": [".###.", "#...#", "#....", "#....", "#....", "#...#", ".###."],
    "°": ["##.", "##.", "...", "...", "...", "...", "..."],
    "?": [".###.", "#...#", "....#", "..##.", "..#..", ".....", "..#.."],
    " ": [],  # empty bitmap, advance only
}
SPACE_ADVANCE = 3

out = [
    "STARTFONT 2.1",
    "COMMENT Hand-drawn proportional test font for paneld golden tests (public domain).",
    "FONT -paneld-prop-medium-r-normal--8-80-75-75-P-50-ISO10646-1",
    "SIZE 8 75 75",
    "FONTBOUNDINGBOX 7 8 0 -1",
    "STARTPROPERTIES 2",
    "FONT_ASCENT 7",
    "FONT_DESCENT 1",
    "ENDPROPERTIES",
    f"CHARS {len(GLYPHS)}",
]
for ch, rows in sorted(GLYPHS.items(), key=lambda kv: ord(kv[0])):
    w = len(rows[0]) if rows else 0
    adv = SPACE_ADVANCE if not rows else w + 1
    out += [
        f"STARTCHAR U+{ord(ch):04X}",
        f"ENCODING {ord(ch)}",
        f"SWIDTH {adv * 125} 0",
        f"DWIDTH {adv} 0",
        f"BBX {w} {len(rows)} 0 0",
        "BITMAP",
    ]
    for r in rows:
        v = 0
        for i, c in enumerate(r):
            if c == "#":
                v |= 0x80 >> i
        out.append(f"{v:02X}")
    out.append("ENDCHAR")
out.append("ENDFONT")
here = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(here, "prop.bdf"), "w") as f:
    f.write("\n".join(out) + "\n")
