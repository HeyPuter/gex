#!/usr/bin/env python3
"""Turn the page's icall dumps into per-image profile files.

split.py LIST IDLOG DUMPLOG OUTDIR
  LIST    units list (page path, base) as build-units.sh reads it
  IDLOG   a units build log with one "indirect-call hints" line per LIST entry, in order
  DUMPLOG console log holding "icall-dump begin pid P" ... "icall-dump end pid P" blocks
W3DIST and NOITA_DIR: where the page paths /w3 and /g are on disk (default the gate's).
The output, for a units dir, is that dir's icall/; checked in, it is this directory (place.py).
For each pid the last complete dump is taken; dumps of all pids are summed.
Each (ret, target) pair goes to the image whose [base, base+SizeOfImage) holds ret."""
import os, re, struct, sys
lst, idlog, dumplog, out = sys.argv[1:5]
def disk(p):
    if p.startswith('/w3/'): return os.environ.get('W3DIST', os.path.expanduser('~/gex-sandbox/scratch/W/page/w3dist')) + '/' + p[4:]
    if p.startswith('/pfx/dosdevices/z:/g/'): return os.environ.get('NOITA_DIR', os.path.expanduser('~/gex-sandbox/Noita')) + '/' + p[len('/pfx/dosdevices/z:/g/'):]
    return None
def size_of_image(f):
    b = open(f, 'rb').read(4096)
    pe = struct.unpack_from('<I', b, 0x3c)[0]
    return struct.unpack_from('<I', b, pe + 24 + 56)[0]
ids = [re.search(r'icall/([0-9a-f]{64})\.txt', l).group(1) for l in open(idlog, errors='replace') if 'indirect-call hints' in l]
imgs = []
for (l, ident) in zip([l.split() for l in open(lst) if l.strip()], ids):
    f = disk(l[0]); base = int(l[1], 16)
    if f and os.path.exists(f): imgs.append((base, base + size_of_image(f), ident, l[0]))
assert len(ids) == len(imgs), (len(ids), len(imgs))
last, cur = {}, None
for l in open(dumplog, errors='replace'):
    m = re.search(r'icall-dump begin pid (\d+)', l)
    if m: cur = (m.group(1), []); continue
    m = re.search(r'icall-dump end pid (\d+)', l)
    if m and cur and cur[0] == m.group(1): last[cur[0]] = cur[1]; cur = None; continue
    m = re.search(r'icall (0x[0-9a-f]+) (0x[0-9a-f]+) (\d+)', l)
    if m and cur: cur[1].append((int(m.group(1), 16), int(m.group(2), 16), int(m.group(3))))
tot = {}
for pairs in last.values():
    for r, t, n in pairs: tot[(r, t)] = tot.get((r, t), 0) + n
per, stray = {}, 0
for (r, t), n in tot.items():
    hit = [i for i in imgs if i[0] <= r < i[1]]
    if hit: per.setdefault(hit[0], []).append((r, t, n))
    else: stray += n
os.makedirs(out, exist_ok=True)
for img, pairs in sorted(per.items()):
    with open(os.path.join(out, img[2] + '.txt'), 'w') as fo:
        fo.write(f'# {img[3]} at {img[0]:#x}: (return address, target, calls) at table calls, counted by gex (icallcount units)\n')
        for r, t, n in sorted(pairs): fo.write(f'icall {r:#x} {t:#x} {n}\n')
    print(f'{img[3]}: {len(pairs)} pairs, {sum(p[2] for p in pairs)} calls')
print(f'{len(last)} pids, {len(tot)} pairs, {stray} calls outside listed images')
