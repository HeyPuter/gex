#!/usr/bin/env python3
"""An image's indirect-call profile for gex's units, from pewasm's
(`PEWASM_PROFILE_INDIRECT` dump, "PEWASM: icall <return va> <target> <count>").

    tools/icall-hints.py PROFILE IMAGE --pewasm-base HEX --base HEX [--out FILE]

Keeps the sites whose return address and target are both in IMAGE as pewasm
placed it (at --pewasm-base; noita.exe: 0x10000), moved to where gex places
it (--base; noita.exe: 0x400000). Writes the lines gex reads
(shims/native/src/nt_cgen.rs `icall_hints`, which applies pewasm's 90%-of-64
rule). The file goes where `gexc cgen-c IMAGE --pe-base BASE --icall-path`
says, under the unit cache that builds the image; the unit key includes it.
"""
import struct, sys

args = sys.argv[1:]
prof, image = args.pop(0), args.pop(0)
pw_base = base = out = None
while args:
    a = args.pop(0)
    if a == '--pewasm-base': pw_base = int(args.pop(0), 16)
    elif a == '--base': base = int(args.pop(0), 16)
    elif a == '--out': out = args.pop(0)
if pw_base is None or base is None:
    sys.exit(__doc__)

d = open(image, 'rb').read(4096)
pe = struct.unpack_from('<I', d, 0x3c)[0]
size = struct.unpack_from('<I', d, pe + 24 + 56)[0]
inside = lambda a: pw_base <= a < pw_base + size

lines, kept, sites = [], 0, set()
for l in open(prof):
    k = l.find('icall ')
    if k < 0:
        continue
    w = l[k + 6:].split()
    if len(w) < 3:
        continue
    r, t, n = int(w[0], 0), int(w[1], 0), int(w[2], 0)
    if inside(r) and inside(t):
        lines.append(f'icall {r - pw_base + base:#x} {t - pw_base + base:#x} {n}\n')
        sites.add(r)
f = open(out, 'w') if out else sys.stdout
f.writelines(lines)
print(f'{len(lines)} (site, target) pairs at {len(sites)} sites', file=sys.stderr)
