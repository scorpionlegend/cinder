#!/usr/bin/env python3
"""Regenerate `src/font.bin`, the bitmap font the window chrome draws with.

The window layer has no text stack: it blits pixels into a softbuffer surface
and nothing else. Rather than pull a font rasteriser and a font file into the
build, the chrome uses one small pre-rasterised ASCII font, generated here from
DejaVu Sans and committed as `src/font.bin`.

Layout, little-endian, for each of the 95 printable ASCII characters from 0x20
to 0x7e in order:

    u8          advance in pixels
    u8[14*16]   14 rows of 16 pixels, one byte of coverage per pixel

Run from this directory:  python3 gen_font.py
"""

import os

from PIL import Image, ImageDraw, ImageFont

CELL_W = 16
CELL_H = 14
BASELINE = 11
SIZE = 13
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf"

out = bytearray()
font = ImageFont.truetype(FONT, SIZE)
for code in range(0x20, 0x7F):
    ch = chr(code)
    advance = int(round(font.getlength(ch)))
    advance = max(1, min(CELL_W, advance))
    image = Image.new("L", (CELL_W, CELL_H), 0)
    draw = ImageDraw.Draw(image)
    draw.text((0, BASELINE), ch, fill=255, font=font, anchor="ls")
    out.append(advance)
    out.extend(image.tobytes())

path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "src", "font.bin")
with open(path, "wb") as handle:
    handle.write(out)
print(f"wrote {path}: {len(out)} bytes")
