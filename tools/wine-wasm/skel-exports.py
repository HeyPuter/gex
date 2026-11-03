#!/usr/bin/env python3
"""skel-exports.py LIST: each line of LIST is `SKELETON SRCDIR` (stage-native.sh, stage-native64.sh); fails
when a skeleton has no export table but a spec in its source directory exports something: GetProcAddress
would find nothing (the skeletons of the modules whose spec keeps the extension, wshom.ocx.spec, were made
from an empty spec, which a lookup of the one spec it took would not have seen).

skel-exports.py --count DIR [ALLOWED...]: the gate's check over a staged directory: the skeletons other
than programs with no export table, which must be none but the ALLOWED names."""
import os
import re
import struct
import sys


def exports(path):
    """Whether the PE at path has an export directory (None: not a PE)."""
    b = open(path, 'rb').read(4096)
    if b[:2] != b'MZ' or len(b) < 0x40:
        return None
    pe = struct.unpack_from('<I', b, 0x3c)[0]
    if b[pe:pe + 4] != b'PE\0\0':
        return None
    magic = struct.unpack_from('<H', b, pe + 24)[0]
    dd = pe + 24 + (112 if magic == 0x20b else 96)
    return struct.unpack_from('<II', b, dd) != (0, 0)


def spec_exports(spec):
    """Whether a .spec declares an export: a line `@ ...` or `ORDINAL ...` (comments are `#`)."""
    try:
        return any(re.match(r'\s*(@|\d+)\s', line) for line in open(spec, errors='replace'))
    except FileNotFoundError:
        return False


if sys.argv[1] == '--count':
    d, allowed = sys.argv[2], {a.lower() for a in sys.argv[3:]}
    bad = []
    for n in sorted(os.listdir(d)):
        p = os.path.join(d, n)
        if n.endswith(('.wasm', '.exe', '.com', '.cwasm')) or not os.path.isfile(p):
            continue
        if exports(p) is False and n.lower() not in allowed:
            bad.append(n)
    print(f'skel-exports: {len(bad)} skeletons with no exports{": " + " ".join(bad) if bad else ""}')
    sys.exit(1 if bad else 0)

bad = []
for line in open(sys.argv[1]):
    skel, src = line.split()
    specs = [os.path.join(src, n) for n in sorted(os.listdir(src)) if n.endswith('.spec')] if os.path.isdir(src) else [src]
    full = [sp for sp in specs if spec_exports(sp)]
    if exports(skel) is False and full:
        bad.append(f'{os.path.basename(skel)} (its spec {full[0]} has exports)')
for b in bad:
    print(f'skel-exports: a skeleton with no export table: {b}', file=sys.stderr)
sys.exit(1 if bad else 0)
