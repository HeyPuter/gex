#!/usr/bin/env python3
"""ppm2png.py IN.ppm OUT.png: a P6 frame (gexc --wayland-out) as a PNG."""
import re, struct, sys, zlib

data = open(sys.argv[1], 'rb').read()
m = re.match(rb'P6\s+(\d+)\s+(\d+)\s+255\s', data)
w, h = int(m[1]), int(m[2])
px = data[m.end():]
raw = b''.join(b'\0' + px[y * w * 3:(y + 1) * w * 3] for y in range(h))


def chunk(kind, body):
    return struct.pack('>I', len(body)) + kind + body + struct.pack('>I', zlib.crc32(kind + body))


open(sys.argv[2], 'wb').write(b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', w, h, 8, 2, 0, 0, 0))
                              + chunk(b'IDAT', zlib.compress(raw, 6)) + chunk(b'IEND', b''))
