#!/usr/bin/env python3
"""Pinned DLL addresses for a 64-bit Wine prefix: its dll-bases file, which 64-bit Wine
Wine's server reads (server/mapping.c, pinned_map_address: "ADDRESS
UNIXPATH" lines) to give each named image that address in every process.

64-bit Wine runs x86-64 code lowered to wasm32, in a 4 GiB
space whose code half ends at 0x7fff0000. The x86_64 builtins' own bases
(0x170000000, 0x180000000) are beyond it, so without pins each process put
each builtin top-down in the order it loaded them: an image's address
differed between the processes of one run and between runs, and a
function-tier unit, keyed by the image and its base, missed. The DLLs go
in [0x11000000, 0x20000000), the island gex's browser shim keeps clear of
its own heap in every Wine process (nt.rs, keep_exe_slot: 32-bit Wine's band too;
at 0x60000000 the shim's heap had reached them in a game's process),
bottom-up, each at its size rounded to 64 KiB plus a 64 KiB gap, in the
order given. Keep the list's order stable and only append to it
(dll-pins.txt): moving a DLL moves every one after it, and their units.

    dll-bases.py assign OUT [--lo ADDR] [--map GUEST=HOST]... < PATHS
    dll-bases.py check FILE

assign: PATHS is one image per line, as the server names it (the unix path
it opened: /w64/lib/wine/x86_64-windows/x.dll in the page, the dist's own
path natively). --map says where a path prefix is on this machine, to read
the image's header. Programs, images without relocations (they cannot
move) and paths not found are left out, each said once on stderr; a line
already given is skipped. An image the band has no room left for is left
out too (said on stderr) and placed as before. --lo starts the assignment
higher in the band: a game's own DLLs go after the builtins (dll-pins.txt)
in a part of their own, 0x19000000 up, so a game's list never moves a
builtin, and OUT is appended to rather than written then.
check: every line of FILE is in the band, granule-aligned and above the line
before it (or at the same address: another name for that image), as
assign writes them (exit 1 otherwise).
"""
import os
import struct
import sys

LO = 0x11000000
HI = 0x20000000
GAME_LO = 0x19000000  # a game's own DLLs (--lo); the builtins stay below
GRANULE = 0x10000


def image(path):
    """(SizeOfImage, is_dll, relocatable) of a PE32+ image, or None."""
    try:
        with open(path, 'rb') as f:
            d = f.read(4096)
    except OSError:
        return None
    if d[:2] != b'MZ':
        return None
    pe = struct.unpack_from('<I', d, 0x3c)[0]
    if d[pe:pe + 4] != b'PE\0\0' or struct.unpack_from('<H', d, pe + 24)[0] != 0x20b:
        return None
    charact = struct.unpack_from('<H', d, pe + 22)[0]
    size = struct.unpack_from('<I', d, pe + 24 + 56)[0]
    nrva = struct.unpack_from('<I', d, pe + 24 + 108)[0]
    reloc_size = struct.unpack_from('<I', d, pe + 24 + 112 + 5 * 8 + 4)[0] if nrva > 5 else 0
    return size, bool(charact & 0x2000), not (charact & 0x0001) and reloc_size > 0


def assign(out, maps, lines, lo=LO):
    at = lo
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
            print(f'dll-bases: {p}: no PE32+ image at {host}, left out', file=sys.stderr)
            continue
        size, is_dll, movable = info
        if not is_dll or not movable:
            print(f'dll-bases: {p}: {"a program" if not is_dll else "no relocations"}, left at its own base', file=sys.stderr)
            continue
        slot = (size + GRANULE - 1) // GRANULE * GRANULE
        if at + slot > (GAME_LO if lo < GAME_LO else HI):
            print(f'dll-bases: {p}: the band is full, left out', file=sys.stderr)
            continue
        rows.append((at, p))
        at += slot + GRANULE
    with open(out, 'a' if lo != LO else 'w') as f:
        for a, p in rows:
            f.write(f'{a:08x} {p}\n')
    print(f'dll-bases: {len(rows)} images in [{lo:#x}, {at:#x}) of the band')


def check(path):
    bad = []
    prev = None
    for n, line in enumerate(open(path), 1):
        f = line.split()
        if len(f) != 2:
            continue
        a = int(f[0], 16)
        if a % GRANULE or not LO <= a < HI:
            bad.append(f'line {n}: {f[1]} at {a:#x}, outside [{LO:#x}, {HI:#x}) or unaligned')
        # (equal: another name for the same image, as a game's DLLs have)
        if prev is not None and a < prev:
            bad.append(f'line {n}: {f[1]} at {a:#x} is below the line before')
        prev = a
    for b in bad:
        print(f'dll-bases: {b}', file=sys.stderr)
    return 1 if bad else 0


if __name__ == '__main__':
    if len(sys.argv) >= 3 and sys.argv[1] == 'assign':
        maps = []
        lo = LO
        a = sys.argv[3:]
        if a[:1] == ['--lo'] and len(a) >= 2:
            lo = int(a[1], 16)
            a = a[2:]
            if lo % GRANULE or not LO <= lo < HI:
                sys.exit(f'dll-bases: --lo {lo:#x} is not a granule in the band')
        while a:
            if a[0] != '--map' or len(a) < 2 or '=' not in a[1]:
                sys.exit(__doc__)
            maps.append(tuple(a[1].split('=', 1)))
            a = a[2:]
        assign(sys.argv[2], maps, sys.stdin, lo)
    elif len(sys.argv) == 3 and sys.argv[1] == 'check':
        sys.exit(check(sys.argv[2]))
    else:
        sys.exit(__doc__)
