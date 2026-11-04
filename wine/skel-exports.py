#!/usr/bin/env python3
"""skel-exports.py LIST: each line of LIST is `SKELETON SRCDIR` (stage-native.sh, stage-native64.sh); fails
when a skeleton has no export table but a spec in its source directory exports something: GetProcAddress
would find nothing (the skeletons of the modules whose spec keeps the extension, wshom.ocx.spec, were made
from an empty spec, which a lookup of the one spec it took would not have seen).

skel-exports.py --count DIR [ALLOWED...]: the gate's check over a staged directory: the skeletons other
than programs with no export table, which must be none but the ALLOWED names.

skel-exports.py --against PEDIR NATIVEDIR SRC ARCH: each skeleton in NATIVEDIR (wine64-dist.sh's staged
native side) against the PE image of the same name in PEDIR (Wine's own PE build of that tree, ARCH
i386 or x86_64): fails when the skeleton lacks a name the PE image exports, but for the names SRC's
specs leave out of the wasm arch (-arch=...,!wasm32): a program importing it, or GetProcAddress, would
find nothing."""
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


def named_exports(path):
    """The names the PE at path exports (forwards included), or None: not a PE."""
    b = open(path, 'rb').read()
    if b[:2] != b'MZ' or len(b) < 0x40:
        return None
    pe = struct.unpack_from('<I', b, 0x3c)[0]
    if b[pe:pe + 4] != b'PE\0\0':
        return None
    nsec, optsz = struct.unpack_from('<H', b, pe + 6)[0], struct.unpack_from('<H', b, pe + 20)[0]
    opt = pe + 24
    rva = struct.unpack_from('<I', b, opt + (112 if struct.unpack_from('<H', b, opt)[0] == 0x20b else 96))[0]
    secs = [struct.unpack_from('<IIII', b, opt + optsz + i * 40 + 8) for i in range(nsec)]

    def off(r):
        for vsz, va, rsz, rp in secs:
            if va <= r < va + max(vsz, rsz):
                return r - va + rp
        raise ValueError(path)
    if not rva:
        return set()
    q = off(rva)
    nnames, anames = struct.unpack_from('<I', b, q + 24)[0], struct.unpack_from('<I', b, q + 32)[0]
    names = set()
    for i in range(nnames):
        s = off(struct.unpack_from('<I', b, off(anames) + 4 * i)[0])
        names.add(b[s:b.index(b'\0', s)].decode('latin1'))
    return names


def wasm_excluded(src, wasm):
    """The export names SRC's specs keep from the wasm arch (an -arch list with !wasm32 or !wasm64)."""
    out = set()
    for top in ('dlls', 'programs'):
        for d in os.listdir(os.path.join(src, top)):
            dd = os.path.join(src, top, d)
            for n in os.listdir(dd) if os.path.isdir(dd) else ():
                if not n.endswith('.spec'):
                    continue
                for line in open(os.path.join(dd, n), errors='replace'):
                    t = line.split('#')[0].split()
                    a = [x for x in t if x.startswith('-arch=')]
                    if len(t) < 3 or not a or '!' + wasm not in a[0].split('=', 1)[1].split(','):
                        continue
                    rest = [x for x in t[2:] if not x.startswith('-')]
                    if rest:
                        out.add(rest[0].split('(')[0])
    return out


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

if sys.argv[1] == '--against':
    pedir, native, src, arch = sys.argv[2:6]
    skip = wasm_excluded(src, 'wasm32' if arch == 'i386' else 'wasm64')
    bad = 0
    for n in sorted(os.listdir(native)):
        p, ref = os.path.join(native, n), os.path.join(pedir, n)
        if n.endswith(('.wasm', '.cwasm')) or not os.path.isfile(p) or not os.path.isfile(ref):
            continue
        have, want = named_exports(p), named_exports(ref)
        if have is None or want is None:
            continue
        miss = sorted(want - have - skip)
        if miss:
            bad += 1
            print(f'skel-exports: {arch} {n} lacks {len(miss)} of the {len(want)} exports of its PE build: '
                  f'{" ".join(miss[:8])}{" ..." if len(miss) > 8 else ""}', file=sys.stderr)
    print(f'skel-exports: {arch}: {bad} skeletons short of their PE build\'s exports')
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
