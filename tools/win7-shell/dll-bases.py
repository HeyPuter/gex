#!/usr/bin/env python3
"""Pinned DLL addresses for the W3 wineserver (server/mapping.c reads the
prefix's dll-bases file), and the function-tier units the site ships.

    dll-bases.py TREE SESSION.json OUT_BASES OUT_UNITS

TREE is the bundle's tree (pfx/, w3/, usr/), SESSION a W3 recording
(tools/eject/record-w3.mjs). Every DLL image the recording names (its units
and unit misses) is laid out from 0x60000000 up in path order, at 64 KiB
granularity with a 64 KiB gap: OUT_BASES, "ADDRESS GUEST_PATH" lines with
the path as the guest resolves it (through dosdevices). OUT_UNITS lists
"GUEST_PATH at base ADDRESS" for each of those DLLs and for each program
(.exe) among the recording's units, at its preferred base: what
33-units.sh builds with `gexc cgen-image`.
"""
import json
import os
import struct
import sys

tree, session, out_bases, out_units = sys.argv[1:]
v = json.load(open(session))
dlls = {u['path'] for u in v['units'] + v['unit_misses'] if u['path'].lower().endswith('.dll')}
exes = sorted({u['path'] for u in v['units'] if u['path'].lower().endswith('.exe')})
t = os.path.realpath(tree)


def real(p):  # the guest's realpath, through the tree's links
    r = os.path.realpath(tree + p)
    assert r.startswith(t + '/'), (p, r)
    return r[len(t):]


def header(h):  # (ImageBase, SizeOfImage) of a PE32 image
    d = open(h, 'rb').read(4096)
    pe = struct.unpack_from('<I', d, 0x3c)[0]
    assert d[pe:pe + 4] == b'PE\0\0', h
    opt = pe + 24
    return struct.unpack_from('<I', d, opt + 28)[0], struct.unpack_from('<I', d, opt + 56)[0]


G = 0x10000
addr = 0x60000000
bases, units, missing = [], [], 0
for p in sorted(dlls, key=real):
    h = tree + p
    if not os.path.isfile(h):
        print('dll-bases: not in the tree:', p, file=sys.stderr)
        missing += 1
        continue
    bases.append(f'{addr:#x} {real(p)}')
    units.append(f'{p} at base {addr:#x}')
    addr += (header(h)[1] + G - 1) // G * G + G
assert addr <= 0x7c000000, hex(addr)
for p in exes:
    if os.path.isfile(tree + p):
        units.append(f'{p} at base {header(tree + p)[0]:#x}')
open(out_bases, 'w').write('\n'.join(bases) + '\n')
open(out_units, 'w').write('\n'.join(units) + '\n')
print(f'{len(bases)} DLLs pinned, 0x60000000-{addr:#x} ({missing} not in the tree); {len(units)} units')
