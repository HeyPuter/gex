#!/usr/bin/env python3
"""Pinned DLL addresses for a 32-bit Wine bundle: its prefix's dll-bases file, which
Wine's server reads (server/mapping.c, "ADDRESS UNIXPATH" lines) to place
each named image, relocated if need be, at that address.

In the browser a 32-bit Wine process's memory grows only as high as it is used (the
compact space), and gex keeps
[0x11000000, 0x20000000) as an island for these pins. An image left at its
own base (w3-bases.txt puts the dist's DLLs at 0x20000000-0x7b000000) is
mapped wherever the shim's heap may already sit once the arena's cursor has
passed it: the mapping is refused, and a game that needs the room
runs out. So the DLLs a program loads go in the band, bottom-up
from 0x11000000, each at its size rounded to 64 KiB plus a 64 KiB gap, in
the order given (keep the list's order stable: a function-tier unit is keyed
by its image and its base).

    dll-bases.py assign OUT [--map GUEST=HOST]... < PATHS
    dll-bases.py check FILE

assign: PATHS is one image per line, as the server names it (the unix path
it opened: /w3/lib/wine/i386-windows/x.dll in the page, the dist's own path
natively). --map says where a path prefix is on this machine, to read the
image's header. Programs, images without relocations (they cannot move) and
paths not found are left out, each said once on stderr; a line already
given is skipped. An image the band has no room left for stays at its own
base too (said on stderr): put the hottest images first.
check: every line of FILE is in the band, granule-aligned and above the line
before it, as assign writes them (exit 1 otherwise).
"""
import os
import struct
import sys

LO = 0x11000000
HI = 0x20000000
GRANULE = 0x10000


def image(path):
    """(SizeOfImage, is_dll, relocatable) of a PE32 image, or None."""
    try:
        with open(path, 'rb') as f:
            d = f.read(4096)
    except OSError:
        return None
    if d[:2] != b'MZ':
        return None
    pe = struct.unpack_from('<I', d, 0x3c)[0]
    if d[pe:pe + 4] != b'PE\0\0' or struct.unpack_from('<H', d, pe + 24)[0] != 0x10b:
        return None
    charact = struct.unpack_from('<H', d, pe + 22)[0]
    size = struct.unpack_from('<I', d, pe + 24 + 56)[0]
    nrva = struct.unpack_from('<I', d, pe + 24 + 92)[0]
    reloc_size = struct.unpack_from('<I', d, pe + 24 + 96 + 5 * 8 + 4)[0] if nrva > 5 else 0
    return size, bool(charact & 0x2000), not (charact & 0x0001) and reloc_size > 0


def assign(out, maps, lines):
    at = LO
    seen = set()
    rows = []
    for p in lines:
        p = p.strip()
        if not p or p.startswith('#') or p in seen:
            continue
        seen.add(p)
        host = p
        for g, h in maps:
            if p.startswith(g):
                host = h + p[len(g):]
                break
        info = image(host)
        if info is None:
            print(f'dll-bases: {p}: no PE32 image at {host}, left out', file=sys.stderr)
            continue
        size, is_dll, movable = info
        if not is_dll or not movable:
            print(f'dll-bases: {p}: {"a program" if not is_dll else "no relocations"}, left at its own base', file=sys.stderr)
            continue
        slot = (size + GRANULE - 1) // GRANULE * GRANULE
        if at + slot > HI:
            print(f'dll-bases: {p}: the band is full, left at its own base', file=sys.stderr)
            continue
        rows.append((at, p))
        at += slot + GRANULE
    with open(out, 'w') as f:
        for a, p in rows:
            f.write(f'{a:08x} {p}\n')
    print(f'dll-bases: {len(rows)} images in [{LO:#x}, {at:#x}) of the band')


def check(path):
    bad = []
    prev = None
    for n, line in enumerate(open(path), 1):
        # the path is the rest of the line (a unix path may hold spaces)
        f = line.rstrip().split(None, 1)
        if len(f) != 2:
            continue
        a = int(f[0], 16)
        if a % GRANULE or not LO <= a < HI:
            bad.append(f'line {n}: {f[1]} at {a:#x}, outside [{LO:#x}, {HI:#x}) or unaligned')
        if prev is not None and a <= prev:
            bad.append(f'line {n}: {f[1]} at {a:#x} is not above the line before')
        prev = a
    for b in bad:
        print(f'dll-bases: {b}', file=sys.stderr)
    return 1 if bad else 0


if __name__ == '__main__':
    if len(sys.argv) >= 3 and sys.argv[1] == 'assign':
        maps = []
        a = sys.argv[3:]
        while a:
            if a[0] != '--map' or len(a) < 2 or '=' not in a[1]:
                sys.exit(__doc__)
            maps.append(tuple(a[1].split('=', 1)))
            a = a[2:]
        assign(sys.argv[2], maps, sys.stdin)
    elif len(sys.argv) == 3 and sys.argv[1] == 'check':
        sys.exit(check(sys.argv[2]))
    else:
        sys.exit(__doc__)
