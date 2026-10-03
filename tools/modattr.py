#!/usr/bin/env python3
"""CPU share by guest module from a GEX_PROF profile (docs/noita-perf.md).

    tools/modattr.py PROFILE [--layout PREFIX/gex-layout] [--image PATH@BASE ...]
                     [--exports PATH@BASE] [--top N]

Needs the run's GEX_PERFMAP=1 perf map (/tmp/perf-<pid>.map). Compiled wasm
named b_<addr> (tier-1 blocks, gex-emit's name section under GEX_PERFMAP) or
f_<addr> / l_<addr> (unit bodies) is attributed to the guest image holding
<addr>: each --image (a PE file mapped at BASE; SizeOfImage from its
header) and every i386 DLL in --layout (gexc cgen-prefix's fixed bases). A
guest address in no image is "anon code" (runtime-generated code such as
LuaJIT's mcode). Everything else is runtime wasm, wasmtime, or host code by
object. --exports PATH@BASE also groups that image's samples by the export
at or below each address (a function's name, give or take).
"""
import bisect, collections, os, re, struct, sys

args = sys.argv[1:]
prof = args.pop(0)
top, layout, images, exports = 25, None, [], None
while args:
    a = args.pop(0)
    if a == '--top': top = int(args.pop(0))
    elif a == '--layout': layout = args.pop(0)
    elif a == '--image': images.append(args.pop(0))
    elif a == '--exports': exports = args.pop(0)

def pe_sections(d):
    pe = struct.unpack_from('<I', d, 0x3c)[0]
    nsec, optsz = struct.unpack_from('<H', d, pe + 6)[0], struct.unpack_from('<H', d, pe + 20)[0]
    out = []
    for i in range(nsec):
        vsz, va, rsz, rof = struct.unpack_from('<IIII', d, pe + 24 + optsz + 40 * i + 8)
        out.append((va, max(vsz, rsz), rof))
    return pe + 24, out

def size_of_image(path):
    d = open(path, 'rb').read(4096)
    opt, _ = pe_sections(d)
    return struct.unpack_from('<I', d, opt + 56)[0]

def export_table(path, base):
    d = open(path, 'rb').read()
    opt, secs = pe_sections(d)
    def r2o(r):
        for va, sz, ro in secs:
            if va <= r < va + sz: return r - va + ro
    e = r2o(struct.unpack_from('<I', d, opt + 96)[0])
    nf, nn, af, an, ao = struct.unpack_from('<IIIII', d, e + 20)
    funcs = [struct.unpack_from('<I', d, r2o(af) + 4 * i)[0] for i in range(nf)]
    names = {}
    for i in range(nn):
        o = r2o(struct.unpack_from('<I', d, r2o(an) + 4 * i)[0])
        names[funcs[struct.unpack_from('<H', d, r2o(ao) + 2 * i)[0]]] = d[o:d.index(b'\0', o)].decode()
    return sorted((base + r, n) for r, n in names.items())

imgs = []
for spec in images:
    path, base = spec.rsplit('@', 1)
    base = int(base, 16)
    imgs.append((base, base + size_of_image(path), os.path.basename(path)))
if layout:
    for l in open(layout):
        p = l.split()
        if len(p) == 4 and p[0].startswith('0x') and 'syswow64' in p[3]:
            b = int(p[0], 16)
            imgs.append((b, b + int(p[1], 16), os.path.basename(p[3])))
imgs.sort()
istarts = [i[0] for i in imgs]
def image(a):
    i = bisect.bisect_right(istarts, a) - 1
    return imgs[i][2] if i >= 0 and a < imgs[i][1] else 'anon code (runtime-generated)'

ex, ex_lo, ex_hi = [], 0, 0
if exports:
    path, base = exports.rsplit('@', 1)
    base = int(base, 16)
    ex, ex_lo, ex_hi = export_table(path, base), base, base + size_of_image(path)
exs = [e[0] for e in ex]

lines = open(prof).read().split('\n')
pid = int(lines[0].split()[1])
hist = collections.Counter()
for l in lines[2:]:
    if l.strip():
        a, c = l.split()
        hist[int(a, 16)] += int(c)
N = sum(hist.values()) or 1

jit = sorted((int(a, 16), int(n, 16), name) for a, n, name in
             (l.rstrip('\n').split(' ', 2) for l in open(f'/tmp/perf-{pid}.map')))
js = [j[0] for j in jit]
maps = []
for l in open(prof[:-5] + '.maps' if prof.endswith('.prof') else prof + '.maps'):
    p = l.split()
    lo, hi = (int(x, 16) for x in p[0].split('-'))
    maps.append((lo, hi, p[5] if len(p) > 5 else '[anon]'))
maps.sort()
ms = [m[0] for m in maps]

by, tier, hot, byex = (collections.Counter() for _ in range(4))
for pc, c in hist.items():
    i = bisect.bisect_right(js, pc) - 1
    if i >= 0 and pc < jit[i][0] + jit[i][1]:
        name = jit[i][2]
        m = re.search(r'(?:^|::)(b|f|l)_([0-9a-f]+)$', name)
        if m:
            g = int(m.group(2), 16)
            mod = image(g)
            by[mod] += c
            tier['tier-1' if m.group(1) == 'b' else 'units'] += c
            hot[(mod, m.group(1) + '_' + m.group(2))] += c
            if ex and ex_lo <= g < ex_hi:
                k = bisect.bisect_right(exs, g) - 1
                byex[ex[k][1] if k >= 0 else '?'] += c
            continue
        if 'trampoline' in name or 'wasmtime' in name:
            by['host: wasmtime trampolines'] += c
            tier['host'] += c
        else:
            by['wasm: runtime / unnamed'] += c
            tier['wasm other'] += c
        continue
    j = bisect.bisect_right(ms, pc) - 1
    obj = os.path.basename(maps[j][2]) if j >= 0 and pc < maps[j][1] else '?'
    by['host: ' + ('gexc' if obj.startswith('gexc') else obj)] += c
    tier['host'] += c

print(f'{N} samples ({N / 1000:.0f} s of CPU)')
print('by tier:', ', '.join(f'{k} {v * 100 / N:.1f}%' for k, v in tier.most_common()))
print('by module:')
for k, v in by.most_common(top):
    print(f'  {v * 100 / N:5.1f}%  {k}')
print('hottest guest blocks / functions:')
for (m, f), v in hot.most_common(15):
    print(f'  {v * 100 / N:5.2f}%  {m} {f}')
if ex:
    print(f'{os.path.basename(exports.rsplit("@", 1)[0])} by export (nearest at or below):')
    for k, v in byex.most_common(top):
        print(f'  {v * 100 / N:5.2f}%  {k}')
