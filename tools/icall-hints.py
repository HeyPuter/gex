#!/usr/bin/env python3
"""An image's indirect-call profile for gex's units, from a gex profile or
from pewasm's.

    tools/icall-hints.py PROFILE IMAGE --base HEX [--pewasm-base HEX] [--out FILE]

PROFILE is either
  - a gex profile (`gexc run --record DIR` writes DIR/profile.json; it
    works for Windows programs too): the indirect call sites in IMAGE's
    object (by the file's sha256), a site's return address being the end of
    the tier-1 block that ends in the call; or
  - pewasm's `PEWASM_PROFILE_INDIRECT` dump ("PEWASM: icall <return va>
    <target> <count>"), with --pewasm-base where pewasm placed IMAGE
    (noita.exe: 0x10000).

Addresses are written for IMAGE at --base, where gex places it (noita.exe:
0x400000). Targets in other images, and a gex site's uncounted rest, are
written as target 0, so they still count against the dominant target. The
output is the lines gex reads (shims/native/src/nt_cgen.rs `icall_hints`,
which applies pewasm's 90%-of-64 rule); it goes where
`gexc cgen-c IMAGE --pe-base BASE --icall-path` says, under the unit cache
that builds the image, and the unit key includes it.
"""
import hashlib, json, struct, sys

args = sys.argv[1:]
if len(args) < 2:
    sys.exit(__doc__)
prof, image = args.pop(0), args.pop(0)
pw_base = base = out = None
while args:
    a = args.pop(0)
    if a == '--pewasm-base': pw_base = int(args.pop(0), 16)
    elif a == '--base': base = int(args.pop(0), 16)
    elif a == '--out': out = args.pop(0)
if base is None:
    sys.exit(__doc__)

data = open(image, 'rb').read()
pe = struct.unpack_from('<I', data, 0x3c)[0]
magic = struct.unpack_from('<H', data, pe + 24)[0]
preferred = struct.unpack_from('<Q', data, pe + 24 + 24)[0] if magic == 0x20b else struct.unpack_from('<I', data, pe + 24 + 28)[0]
size = struct.unpack_from('<I', data, pe + 24 + 56)[0]

lines, sites = [], set()
if open(prof, 'rb').read(1) == b'{':
    p = json.load(open(prof))
    me = 'sha256:' + hashlib.sha256(data).hexdigest()
    obj = next((o for o in p['objects'] if o['id'] == me), None)
    if obj is None:
        sys.exit(f'{image} ({me}) is not in {prof}')
    move = base - preferred
    ends = {b[0]: b[1] for b in obj['blocks']}
    for s in obj['indirect']:
        if not s['call'] or s['off'] not in ends:
            continue
        r = ends[s['off']] + move
        sites.add(r)
        for o, t, n in s['targets']:
            lines.append(f'icall {r:#x} {(t + move) if o == me else 0:#x} {n}\n')
        if s.get('other'):
            lines.append(f'icall {r:#x} 0x0 {s["other"]}\n')
else:
    if pw_base is None:
        sys.exit('a pewasm profile needs --pewasm-base')
    inside = lambda a: pw_base <= a < pw_base + size
    for l in open(prof):
        k = l.find('icall ')
        if k < 0:
            continue
        w = l[k + 6:].split()
        if len(w) < 3:
            continue
        r, t, n = int(w[0], 0), int(w[1], 0), int(w[2], 0)
        if inside(r):
            lines.append(f'icall {r - pw_base + base:#x} {t - pw_base + base if inside(t) else 0:#x} {n}\n')
            sites.add(r)
f = open(out, 'w') if out else sys.stdout
f.writelines(lines)
print(f'{len(lines)} (site, target) pairs at {len(sites)} sites', file=sys.stderr)
