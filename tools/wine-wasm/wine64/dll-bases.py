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
    dll-bases.py assign OUT --i386 [--lo ADDR] [--map GUEST=HOST]... < PATHS
    dll-bases.py check FILE [--map GUEST=HOST]...

assign: PATHS is one image per line, as the server names it (the unix path
it opened: /wine64/lib/wine/x86_64-windows/x.dll in the page, the dist's own
path natively). --map says where a path prefix is on this machine, to read
the image's header. A game's program can be given too: one whose base is
beyond the process's space is otherwise placed by what was mapped before
it, which moves with the environment. Images without relocations (they
cannot move) and paths not found are left out, each said once on stderr; a line
already given is skipped. An image the band has no room left for is left
out too (said on stderr) and placed as before. --lo starts the assignment
higher in the band: a game's own DLLs go after the builtins (dll-pins.txt)
in a part of their own, 0x19000000 up, so a game's list never moves a
builtin, and OUT is appended to rather than written then.
--i386: the 32-bit DLLs of a WoW64 prefix (PE32, lib/wine/i386-windows),
appended to OUT, which holds the 64-bit pins already: a WoW64 process maps
32-bit images in [0x11000000, 0x19000000) (below the 64-bit compact island),
and the 64-bit images every WoW64 process maps (WOW64_SET: wow64.dll,
wow64cpu.dll, wow64win.dll, wow64con.dll, win32u.dll, mapped before any
32-bit DLL) keep their pins there, so the 32-bit ones are packed around
them, bottom-up from 0x11000000 (or --lo, for a 32-bit program's own DLLs
after the builtins). A 64-bit process never maps a 32-bit DLL, so the rest
of the 64-bit pins may share the addresses.
check: every line of FILE is in the band, granule-aligned and above the line
before it of its kind (64-bit, or 32-bit: a PE32 image, read through --map,
or a path in i386-windows or syswow64), or at the same address (another name
for that image), as assign writes them; a 32-bit pin ends below
0x19000000 and overlaps none of WOW64_SET's (exit 1 otherwise).
"""
import os
import struct
import sys

LO = 0x11000000
HI = 0x20000000
GAME_LO = 0x19000000  # a game's own DLLs (--lo); the builtins stay below
GRANULE = 0x10000
WOW64_HI = GAME_LO  # a WoW64 process's 32-bit pins end below it
WOW64_SET = {'wow64.dll', 'wow64cpu.dll', 'wow64win.dll', 'wow64con.dll', 'win32u.dll'}


def image(path, magic=0x20b):
    """(SizeOfImage, is_dll, relocatable) of a PE32+ image (magic 0x20b) or
    a PE32 one (0x10b), or None."""
    try:
        with open(path, 'rb') as f:
            d = f.read(4096)
    except OSError:
        return None
    if d[:2] != b'MZ':
        return None
    pe = struct.unpack_from('<I', d, 0x3c)[0]
    if d[pe:pe + 4] != b'PE\0\0' or struct.unpack_from('<H', d, pe + 24)[0] != magic:
        return None
    dirs = 112 if magic == 0x20b else 96  # the data directories' offset in the optional header
    charact = struct.unpack_from('<H', d, pe + 22)[0]
    size = struct.unpack_from('<I', d, pe + 24 + 56)[0]
    nrva = struct.unpack_from('<I', d, pe + 24 + dirs - 4)[0]
    reloc_size = struct.unpack_from('<I', d, pe + 24 + dirs + 5 * 8 + 4)[0] if nrva > 5 else 0
    return size, bool(charact & 0x2000), not (charact & 0x0001) and reloc_size > 0


def host_of(p, maps):
    for g, h in maps:
        if p.startswith(g):
            return h + p[len(g):]
    return p


def lines_of(path):
    """(address, path) of each line of a dll-bases file."""
    rows = []
    for line in open(path):
        # the path is the rest of the line (a unix path may hold spaces)
        f = line.rstrip().split(None, 1)
        if len(f) == 2:
            rows.append((int(f[0], 16), f[1]))
    return rows


def wow64_taken(path, maps):
    """[start, end) of WOW64_SET's 64-bit pins in a dll-bases file."""
    taken = []
    for a, p in lines_of(path) if os.path.exists(path) else []:
        if os.path.basename(p).lower() in WOW64_SET:
            info = image(host_of(p, maps))
            if info:
                taken.append((a, a + (info[0] + GRANULE - 1) // GRANULE * GRANULE + GRANULE))
    return sorted(taken)


def assign(out, maps, lines, lo=LO, i386=False):
    at = lo
    seen = set()
    rows = []
    taken = wow64_taken(out, maps) if i386 else []
    end = WOW64_HI if i386 else (GAME_LO if lo < GAME_LO else HI)
    for p in lines:
        p = p.strip()
        if not p or p.startswith('#') or p in seen:
            continue
        seen.add(p)
        host = host_of(p, maps)
        info = image(host, 0x10b if i386 else 0x20b)
        if info is None:
            print(f'dll-bases: {p}: no {"PE32" if i386 else "PE32+"} image at {host}, left out', file=sys.stderr)
            continue
        size, is_dll, movable = info
        if not movable:
            print(f'dll-bases: {p}: no relocations, left at its own base', file=sys.stderr)
            continue
        slot = (size + GRANULE - 1) // GRANULE * GRANULE
        # (past the 64-bit images a WoW64 process maps first)
        moved = True
        while moved:
            moved = False
            for a, e in taken:
                if at < e and a < at + slot:
                    at, moved = e, True
        if at + slot > end:
            print(f'dll-bases: {p}: the band is full, left out', file=sys.stderr)
            continue
        rows.append((at, p))
        at += slot + GRANULE
    with open(out, 'a' if lo != LO or i386 else 'w') as f:
        for a, p in rows:
            f.write(f'{a:08x} {p}\n')
    print(f'dll-bases: {len(rows)} images in [{lo:#x}, {at:#x}) of the band')


def is_i386(p, maps):
    host = host_of(p, maps)
    if image(host, 0x10b):
        return True
    if image(host, 0x20b):
        return False
    q = p.lower()
    return '/i386-windows/' in q or '/syswow64/' in q


def check(path, maps=()):
    bad = []
    prev = {False: None, True: None}
    taken = wow64_taken(path, maps)
    for n, (a, p) in enumerate(lines_of(path), 1):
        i386 = is_i386(p, maps)
        if a % GRANULE or not LO <= a < HI:
            bad.append(f'line {n}: {p} at {a:#x}, outside [{LO:#x}, {HI:#x}) or unaligned')
        # (equal: another name for the same image, as a game's DLLs have)
        if prev[i386] is not None and a < prev[i386]:
            bad.append(f'line {n}: {p} at {a:#x} is below the {"32" if i386 else "64"}-bit line before')
        prev[i386] = a
        if i386:
            info = image(host_of(p, maps), 0x10b)
            e = a + ((info[0] + GRANULE - 1) // GRANULE * GRANULE if info else GRANULE)
            if e > WOW64_HI:
                bad.append(f'line {n}: {p} at {a:#x} ends at {e:#x}, past a WoW64 process\'s 32-bit band ({WOW64_HI:#x})')
            for ta, te in taken:
                if a < te and ta < e:
                    bad.append(f'line {n}: {p} at {a:#x} overlaps a 64-bit image every WoW64 process maps ({ta:#x})')
    for b in bad:
        print(f'dll-bases: {b}', file=sys.stderr)
    return 1 if bad else 0


if __name__ == '__main__':
    if len(sys.argv) >= 3 and sys.argv[1] == 'assign':
        maps = []
        lo = LO
        a = sys.argv[3:]
        i386 = a[:1] == ['--i386']
        if i386:
            a = a[1:]
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
        assign(sys.argv[2], maps, sys.stdin, lo, i386)
    elif len(sys.argv) >= 3 and sys.argv[1] == 'check':
        maps = []
        a = sys.argv[3:]
        while a:
            if a[0] != '--map' or len(a) < 2 or '=' not in a[1]:
                sys.exit(__doc__)
            maps.append(tuple(a[1].split('=', 1)))
            a = a[2:]
        sys.exit(check(sys.argv[2], maps))
    else:
        sys.exit(__doc__)
