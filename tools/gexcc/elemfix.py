#!/usr/bin/env python3
"""Point table entries back at the real functions after binaryen's
--fpcast-emu (gexcc FPCAST): every element `byn$fpcast-emu$NAME` for NAME in
the arguments becomes NAME again. gex calls those entries from the host with
their own signatures. Binary rewrite of the element section, by the name
section's function names.

    elemfix.py MODULE.wasm NAME...
"""
import sys

path, names = sys.argv[1], sys.argv[2:]
w = bytearray(open(path, 'rb').read())


def leb(b, p):
    r = s = 0
    while True:
        x = b[p]; p += 1
        r |= (x & 0x7f) << s; s += 7
        if not x & 0x80:
            return r, p


def enc(v):
    out = bytearray()
    while True:
        x = v & 0x7f; v >>= 7
        if v:
            out.append(x | 0x80)
        else:
            out.append(x)
            return bytes(out)


# sections
secs = []
p = 8
while p < len(w):
    sid = w[p]
    n, q = leb(w, p + 1)
    secs.append((sid, p, q, q + n))
    p = q + n

# function names (custom "name" section, subsection 1)
fname = {}
for sid, s0, b0, e in secs:
    if sid != 0:
        continue
    ln, q = leb(w, b0)
    if bytes(w[q:q + ln]) != b'name':
        continue
    q += ln
    while q < e:
        kind = w[q]
        sz, q = leb(w, q + 1)
        end = q + sz
        if kind == 1:
            cnt, r = leb(w, q)
            for _ in range(cnt):
                idx, r = leb(w, r)
                nl, r = leb(w, r)
                fname[idx] = w[r:r + nl].decode('utf-8', 'replace')
                r += nl
        q = end
byname = {v: k for k, v in fname.items()}
remap = {}
for n in names:
    t = byname.get('byn$fpcast-emu$' + n)
    f = byname.get(n)
    if t is not None and f is not None:
        remap[t] = f

for sid, s0, b0, e in secs:
    if sid != 9:
        continue
    out = bytearray()
    cnt, q = leb(w, b0)
    out += enc(cnt)
    for _ in range(cnt):
        flags, q2 = leb(w, q)
        if flags != 0:
            sys.exit('elemfix: element segment flags %d not handled' % flags)
        # offset expression up to its end (0x0b)
        r = q2
        while w[r] != 0x0b:
            r += 1
        r += 1
        out += w[q:r]
        n, r = leb(w, r)
        out += enc(n)
        for _ in range(n):
            fi, r = leb(w, r)
            out += enc(remap.get(fi, fi))
        q = r
    new = w[:s0] + bytes([9]) + enc(len(out)) + out + w[e:]
    open(path, 'wb').write(new)
    print('elemfix: %d entries restored' % len(remap), file=sys.stderr)
    sys.exit(0)
sys.exit('elemfix: no element section')
