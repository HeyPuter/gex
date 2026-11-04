#!/usr/bin/env python3
"""icudat.py IN.dat OUT.dat LOCALE...: ICU's common data (Wine's nls/icudtl.dat)
with the locale data of LOCALE... and their parents only.

What is locale data: in the locale trees (the package's top level, coll, curr,
lang, region, unit, zone, rbnf, brkitr) a bundle named for a locale
(de.res, zh_Hant_TW.res). Each is kept when its locale is one of LOCALE... or
an ancestor of one (en for en_US; root always). Everything else stays: the
converters (.cnv), properties and names (.icu), normalization (.nrm), the
StringPrep profiles (.spp), the confusables (.cfu), break rules (.brk) and
dictionaries (.dict), the non-locale bundles (supplementalData, zoneinfo64,
metaZones, plurals, likelySubtags, ...), the transliterators and each tree's
root and string pool. Each tree's res_index (its InstalledLocales) is
rewritten with genrb (ICU's own compiler, the same ICU version), so the
available locales are the kept ones; a request for another falls back to
root, as for any locale ICU has no data for.

LOCALE "all" copies the file unchanged."""
import os
import re
import struct
import subprocess
import sys
import tempfile

TREES = {'', 'coll', 'curr', 'lang', 'region', 'unit', 'zone', 'rbnf', 'brkitr'}
# a locale bundle's name: language, then script, region, variants
LOCALE = re.compile(r'^[a-z]{2,3}(_[A-Z][a-z]{3})?(_([A-Z]{2}|[0-9]{3})?)?(_[A-Z0-9]{2,})*$')


def read(path):
    b = open(path, 'rb').read()
    hs = struct.unpack_from('<H', b, 0)[0]
    if b[2:4] != b'\xda\x27' or b[12:16] != b'CmnD' or b[8] != 0:
        sys.exit(f'icudat: {path} is no little-endian ICU common data file')
    n = struct.unpack_from('<I', b, hs)[0]
    ents = [struct.unpack_from('<II', b, hs + 4 + 8 * i) for i in range(n)]
    items = []
    for i, (no, do) in enumerate(ents):
        name = b[hs + no:b.index(b'\0', hs + no)].decode()
        end = hs + ents[i + 1][1] if i + 1 < n else len(b)
        items.append([name, b[hs + do:end]])
    return b[:hs], items


def write(path, header, items):
    hs = len(header)
    names = b''.join(n.encode() + b'\0' for n, _ in items)
    toc = 4 + 8 * len(items)
    pos = (toc + len(names) + 15) & ~15
    offs = []
    for _, d in items:
        offs.append(pos)
        pos += (len(d) + 15) & ~15
    out = bytearray(header)
    out += struct.pack('<I', len(items))
    no = toc
    for (n, _), do in zip(items, offs):
        out += struct.pack('<II', no, do)
        no += len(n) + 1
    out += names
    for (_, d), do in zip(items, offs):
        out += b'\0' * (hs + do - len(out)) + d
    out += b'\0' * ((-len(out)) % 16)
    open(path, 'wb').write(out)


def ancestors(loc):
    out = set()
    while loc:
        out.add(loc)
        loc = loc.rsplit('_', 1)[0] if '_' in loc else ''
    return out


def genrb_check(items):
    """genrb must be of the data's ICU (major version: the package name,
    icudt72l), or the bundles it writes may not read back."""
    want = re.match(r'icudt(\d+)', items[0][0]).group(1)
    try:
        r = subprocess.run(['genrb', '--version'], capture_output=True, text=True)
        out = r.stdout + r.stderr
    except FileNotFoundError:
        sys.exit('icudat: no genrb (ICU\'s tools: Debian icu-devtools); WINE_ICU_LOCALES=all needs none')
    m = re.search(r'ICU version (\d+)\.', out)
    if not m or m.group(1) != want:
        sys.exit(f'icudat: genrb is of ICU {m.group(1) if m else "(unknown)"}, the data of ICU {want}: '
                 f'put an ICU {want} genrb first in PATH, or WINE_ICU_LOCALES=all')


def res_index(locales):
    """A tree's res_index.res listing LOCALES, compiled by genrb."""
    with tempfile.TemporaryDirectory() as t:
        with open(os.path.join(t, 'res_index.txt'), 'w') as f:
            f.write('res_index:table(nofallback) {\n    InstalledLocales {\n')
            f.writelines(f'        {loc} {{""}}\n' for loc in sorted(locales))
            f.write('    }\n}\n')
        subprocess.run(['genrb', '-q', '-d', t, os.path.join(t, 'res_index.txt')], check=True)
        return open(os.path.join(t, 'res_index.res'), 'rb').read()


def main():
    if len(sys.argv) < 4:
        sys.exit(__doc__)
    src, dst, want = sys.argv[1], sys.argv[2], sys.argv[3:]
    if want == ['all']:
        open(dst, 'wb').write(open(src, 'rb').read())
        return
    keep = {'root'}
    for loc in want:
        keep |= ancestors(loc)
    header, items = read(src)
    genrb_check(items)
    out, kept = [], {}
    for name, data in items:
        pkg, rest = name.split('/', 1)
        tree, base = rest.rsplit('/', 1) if '/' in rest else ('', rest)
        stem, ext = os.path.splitext(base)
        if tree in TREES and ext == '.res' and stem != 'root' and LOCALE.match(stem):
            if stem not in keep:
                continue
            kept.setdefault(tree, set()).add(stem)
        out.append([name, data])
    for item in out:
        pkg, rest = item[0].split('/', 1)
        tree = rest.rsplit('/', 1)[0] if '/' in rest else ''
        if rest.rsplit('/', 1)[-1] == 'res_index.res' and tree in TREES:
            new = res_index(kept.get(tree, set()))
            # the same resource bundle format as the data's own (the
            # header's formatVersion)
            if new[16:20] != item[1][16:20]:
                sys.exit(f'icudat: genrb writes resource format {".".join(map(str, new[16:20]))}, '
                         f'the data has {".".join(map(str, item[1][16:20]))} ({item[0]})')
            item[1] = new
    write(dst, header, out)
    print(f'icudat: {len(out)} of {len(items)} items, {os.path.getsize(dst)} of {os.path.getsize(src)} bytes '
          f'(locales: root {" ".join(sorted(keep - {"root"}))})', file=sys.stderr)


main()
