#!/usr/bin/env python3
"""Symbolise a GEX_PROF profile (crates/gexc/src/prof.rs).

    scripts/prof-report.py PROFILE [--modules] [--top N] [--cats FILE.json] [--perfmap PATH]

--modules sums compiled wasm by module (big modules are whole-image
units, small ones tier-1's runtime translations).

PROFILE is what GEX_PROF wrote (`pid`, `samples`, then `pc count` in hex),
with PROFILE.maps beside it. Compiled wasm is named by wasmtime's perf map
(/tmp/perf-<pid>.map, from GEX_PERFMAP=1); native code by the binaries'
symbol tables (`nm`). Prints the top symbols by self samples, then the
share of each category: the first regex in --cats (a JSON list of
[name, regex] matched against "object:symbol") that matches, else "other".
"""
import bisect, collections, json, re, subprocess, sys

args = sys.argv[1:]
bymod = '--modules' in args
args = [a for a in args if a != '--modules']
top = 40
cats = None
perfmap = None
while len(args) > 1:
    if args[-2] == '--top':
        top = int(args[-1]); args = args[:-2]
    elif args[-2] == '--cats':
        cats = json.load(open(args[-1])); args = args[:-2]
    elif args[-2] == '--perfmap':
        perfmap = args[-1]; args = args[:-2]
    else:
        break
prof = args[0]
lines = open(prof).read().split('\n')
pid = int(lines[0].split()[1])
total = int(lines[1].split()[1])
hist = {}
for l in lines[2:]:
    if l.strip():
        pc, c = l.split()
        hist[int(pc, 16)] = int(c)

maps = []
for l in open(prof.rsplit('.', 1)[0] + '.maps' if not prof.endswith('.prof') else prof[:-5] + '.maps'):
    p = l.split()
    lo, hi = (int(x, 16) for x in p[0].split('-'))
    maps.append((lo, hi, p[1], int(p[2], 16), p[5] if len(p) > 5 else ''))
maps.sort()
starts = [m[0] for m in maps]

jit = []
# wasmtime names every module's functions `wasm[0]::function[k]`: modules
# are told apart by the perf map's order (each module's entries are written
# together, its function indices rising), and named by their size, so the
# big whole-image units stand out from tier-1's small runtime modules.
mods = []  # per jit entry: module number
sizes = collections.Counter()
try:
    mod, last = -1, None
    for l in open(perfmap or f'/tmp/perf-{pid}.map'):
        a, n, name = l.rstrip('\n').split(' ', 2)
        m = re.match(r'wasm\[\d+\]::function\[(\d+)\]', name)
        if m:
            k = int(m.group(1))
            if last is None or k <= last:
                mod += 1
            last = k
            sizes[mod] += 1
        jit.append((int(a, 16), int(n, 16), name, mod))
except FileNotFoundError:
    pass
jit.sort()
jstarts = [j[0] for j in jit]
def modname(m):
    n = sizes[m]
    return f'module#{m}({n} fns)' if n >= 1000 else 'small-module'

symcache = {}
def syms(path):
    if path not in symcache:
        out = []
        try:
            # file offset -> vaddr, from the LOAD headers
            ph = subprocess.run(['readelf', '-lW', path], capture_output=True, text=True).stdout
            loads = [(int(m.group(1), 16), int(m.group(2), 16), int(m.group(3), 16)) for m in
                     re.finditer(r'LOAD\s+0x([0-9a-f]+)\s+0x([0-9a-f]+)\s+0x[0-9a-f]+\s+0x([0-9a-f]+)', ph)]
            nm = subprocess.run(['nm', '-n', '-C', '--defined-only', path], capture_output=True, text=True).stdout
            if not nm.strip():
                nm = subprocess.run(['nm', '-n', '-C', '-D', '--defined-only', path], capture_output=True, text=True).stdout
            for l in nm.split('\n'):
                p = l.split(' ', 2)
                if len(p) == 3 and p[1] in 'tTwWiI':
                    out.append((int(p[0], 16), p[2]))
        except Exception:
            loads = []
        out.sort()
        symcache[path] = (loads, out, [o[0] for o in out])
    return symcache[path]

def name(pc):
    k = bisect.bisect_right(jstarts, pc) - 1
    if k >= 0 and jit[k][0] <= pc < jit[k][0] + jit[k][1]:
        if bymod and 'trampoline' not in jit[k][2]:
            return 'jit:' + modname(jit[k][3])
        return 'jit:' + jit[k][2]
    k = bisect.bisect_right(starts, pc) - 1
    if k < 0 or not (maps[k][0] <= pc < maps[k][1]):
        return '?:?'
    lo, hi, perm, off, path = maps[k]
    if not path.startswith('/'):
        return f'anon[{perm}]:?'
    loads, out, ost = syms(path)
    foff = pc - lo + off
    va = foff
    for o, v, fsz in loads:
        if o <= foff < o + fsz:
            va = foff - o + v
    j = bisect.bisect_right(ost, va) - 1
    s = out[j][1] if j >= 0 else '?'
    return path.rsplit('/', 1)[-1] + ':' + s

by = collections.Counter()
for pc, c in hist.items():
    by[name(pc)] += c
print(f'{total} samples')
for s, c in by.most_common(top):
    print(f'{100 * c / total:6.2f}%  {c:8}  {s[:160]}')
if cats:
    cc = collections.Counter()
    for s, c in by.items():
        for cat, rx in cats:
            if re.search(rx, s):
                cc[cat] += c
                break
        else:
            cc['other'] += c
    print()
    for cat, c in cc.most_common():
        print(f'{100 * c / total:6.2f}%  {cat}')
