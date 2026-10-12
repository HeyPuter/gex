# What an X window holds, from the X server: `xwd` of the window named argv[1],
# reduced to a signature tests/demo/desktop.mjs computes the same way from what
# the compositor composed -- per cell of a COLSxROWS grid, how many pixels are
# dark. Prints "xsize W H HASH" (the hash of the whole dark bitmap), then each row of cells as "xsR n,n,...".
import struct, subprocess, sys

COLS, ROWS = 16, 8
title = sys.argv[1]
d = subprocess.run(['xwd', '-name', title, '-silent', '-nobdrs'], capture_output=True, check=True).stdout
h = struct.unpack('>25I', d[:100])
(size, _ver, _fmt, _depth, w, ht, _xoff, order, _unit, _bo, _pad, bpp, bpl,
 _cls, rm, gm, bm, _bits, _ents, ncolors) = h[:20]
if bpp != 32:
    sys.exit(f'xsig: {bpp} bits per pixel')
px = d[size + ncolors * 12:]
end = '<' if order == 0 else '>'
shift = lambda m: (m & -m).bit_length() - 1
rs, gs, bs = shift(rm), shift(gm), shift(bm)
# The outermost EDGE pixels are left out on both sides: where the window's
# border meets its contents is the compositor's business, not the client's.
EDGE = 2
iw, ih = w - 2 * EDGE, ht - 2 * EDGE
cells = [0] * (COLS * ROWS)
exact = 2166136261  # FNV-1a over the dark bitmap, row by row
for y in range(ih):
    row = struct.unpack_from(f'{end}{w}I', px, (y + EDGE) * bpl)
    cy = y * ROWS // ih * COLS
    for x in range(iw):
        v = row[x + EDGE]
        dark = max((v & rm) >> rs, (v & gm) >> gs, (v & bm) >> bs) < 128
        if dark:
            cells[cy + x * COLS // iw] += 1
        exact = ((exact ^ dark) * 16777619) & 0xffffffff
# A row of cells a line: the terminal that carries this back wraps long ones.
print('xsize', w, ht, exact)
for r in range(ROWS):
    print(f'xs{r}', ','.join(map(str, cells[r * COLS:(r + 1) * COLS])))
