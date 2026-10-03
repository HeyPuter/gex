#!/usr/bin/env python3
"""AOT coverage from a GEX_PROF profile (docs/noita-perf.md): how much guest
CPU ran in units, and, per image, what ran on tier-1 and why.

    tools/aot-coverage.py PROFILE CGEN_CACHE [--layout PREFIX/gex-layout]
                          [--image PATH@BASE ...] [--top N]

Needs the run's perf map (GEX_PERFMAP=1): unit code is named f_<addr> /
l_<addr>, tier-1 code b_<addr> (the block a translation starts at). Each
tier-1 sample is classified by its block address against the image's cached
recovery (CGEN_CACHE/recovery/*/functions.gir, matched by address range):

  entry     the entry of a function the unit holds: tier-1 reached it
            without going through the function table (a binding gap);
  interior  inside a recovered block of a function the unit holds: tier-1
            ran it after a leave or a resume (a runtime fallback);
  excluded  recovered, but cgen left the function out of the unit;
  missed    in the image but in no recovered block (a coverage gap:
            recovery did not find it, or cgen left the function out);
  no unit   in an image with no recovery at all;
  anon      in no image (runtime-generated code: LuaJIT's mcode).
"""
import bisect, collections, glob, os, re, struct, sys

args = sys.argv[1:]
prof, cache = args.pop(0), args.pop(0)
top, layout, images = 15, None, []
while args:
    a = args.pop(0)
    if a == '--top': top = int(args.pop(0))
    elif a == '--layout': layout = args.pop(0)
    elif a == '--image': images.append(args.pop(0))

def size_of_image(path):
    d = open(path, 'rb').read(4096)
    pe = struct.unpack_from('<I', d, 0x3c)[0]
    return struct.unpack_from('<I', d, pe + 24 + 56)[0]

imgs = []  # (base, end, name)
for spec in images:
    p, b = spec.rsplit('@', 1)
    b = int(b, 16)
    imgs.append((b, b + size_of_image(p), os.path.basename(p)))
if layout:
    for l in open(layout):
        w = l.split()
        if len(w) == 4 and w[0].startswith('0x'):
            b, n = int(w[0], 16), int(w[1], 16)
            # An i386 process maps syswow64's; system32's share bases.
            if 'syswow64' in w[3]:
                imgs.append((b, b + n, os.path.basename(w[3])))
imgs.sort()
starts = [i[0] for i in imgs]

def image_of(a):
    k = bisect.bisect_right(starts, a) - 1
    if k >= 0 and a < imgs[k][1]:
        return imgs[k][2]
    return None

# Recovered blocks, per recovery directory, as sorted interval lists.
blocks = []  # (lo, hi, function entry)
rec_ranges = []  # (min, max) per recovery
for r in glob.glob(os.path.join(cache, 'recovery', '*', 'functions.gir')):
    iv = []
    fn = 0
    with open(r, 'rb') as f:
        for line in f:
            if line[:5] == b'func ':
                fn = int(line.split()[1], 16)
            elif line[:1] == b'b':
                m = re.match(rb'b\d+ 0x([0-9a-f]+)\.\.0x([0-9a-f]+)', line)
                if m:
                    iv.append((int(m.group(1), 16), int(m.group(2), 16), fn))
    if iv:
        blocks.extend(iv)
        rec_ranges.append((min(x[0] for x in iv), max(x[1] for x in iv)))
blocks.sort()
bstarts = [b[0] for b in blocks]
def recovered(a):
    """The entries of the recovered functions with a block holding `a`."""
    out = set()
    k = bisect.bisect_right(bstarts, a) - 1
    while k >= 0 and k > bisect.bisect_right(bstarts, a) - 64:
        if blocks[k][0] <= a < blocks[k][1]:
            out.add(blocks[k][2])
        k -= 1
    return out
def has_recovery(img):
    b, e = next((x[0], x[1]) for x in imgs if x[2] == img)
    return any(lo < e and hi > b for lo, hi in rec_ranges)

pid = open(prof).readline().split()[1]
jit = []
for l in open(f'/tmp/perf-{pid}.map'):
    a, n, name = l.rstrip('\n').split(' ', 2)
    jit.append((int(a, 16), int(n, 16), name))
jit.sort()
jst = [j[0] for j in jit]
in_unit = set()
for _, _, n in jit:
    m = re.search(r'\bf_([0-9a-f]+)$', n)
    if m:
        in_unit.add(int(m.group(1), 16))
def jname(pc):
    k = bisect.bisect_right(jst, pc) - 1
    if k >= 0 and pc < jit[k][0] + jit[k][1]:
        return jit[k][2]
    return None

lines = open(prof).read().split('\n')
total = int(lines[1].split()[1])
unit = collections.Counter()
t1 = collections.Counter()
t1addr = collections.defaultdict(collections.Counter)
guest = 0
cls_cache = {}
for l in lines[2:]:
    if not l.strip():
        continue
    pc, c = l.split()
    c = int(c)
    n = jname(int(pc, 16))
    if not n:
        continue
    m = re.search(r'\b([fl])_([0-9a-f]+)', n) or re.search(r'\b(b)_([0-9a-f]+)', n)
    if not m:
        continue
    a = int(m.group(2), 16)
    img = image_of(a) or 'anon'
    guest += c
    if m.group(1) in 'fl':
        unit[img] += c
    else:
        if a not in cls_cache:
            if img == 'anon':
                cls_cache[a] = 'anon'
            elif not has_recovery(img):
                cls_cache[a] = 'no unit'
            else:
                fs = recovered(a)
                if not fs:
                    cls_cache[a] = 'missed'
                elif a in (fs & in_unit):
                    cls_cache[a] = 'entry'
                elif fs & in_unit:
                    cls_cache[a] = 'interior'
                else:
                    cls_cache[a] = 'excluded'
        cl = cls_cache[a]
        t1[(img, cl)] += c
        t1addr[(img, cl)][a] += c

print(f'{total} samples; guest code {guest} ({100*guest/total:.1f}%)')
tu = sum(unit.values())
print(f'AOT share of guest code: {100*tu/guest:.1f}% (units {tu}, tier-1 {guest-tu})')
anon = sum(v for (i, c), v in t1.items() if c == 'anon')
print(f'AOT share excluding anon (LuaJIT mcode): {100*tu/(guest-anon):.1f}%')
print()
print(f'{"image":24} {"units%":>7} {"entry%":>7} {"interior%":>9} {"excluded%":>9} {"missed%":>8} {"no-unit%":>8} {"anon%":>6}  (% of guest)')
names = sorted(set(list(unit) + [i for i, _ in t1]), key=lambda i: -(unit[i] + sum(v for (j, _), v in t1.items() if j == i)))
for i in names:
    g = lambda cl: 100 * t1[(i, cl)] / guest
    print(f'{i:24} {100*unit[i]/guest:7.2f} {g("entry"):7.2f} {g("interior"):9.2f} {g("excluded"):9.2f} {g("missed"):8.2f} {g("no unit"):8.2f} {g("anon"):6.2f}')
print()
for key in sorted(t1, key=lambda k: -t1[k]):
    if key[1] in ('missed', 'no unit', 'excluded', 'interior', 'entry') and t1[key] * 1000 >= guest:
        tops = ', '.join(f'{a:#x} {100*c/guest:.2f}%' for a, c in t1addr[key].most_common(top))
        print(f'{key[0]} {key[1]}: {tops}')
