#!/usr/bin/env python3
"""renexp.py WASM OLD NEW: rename export OLD to NEW in place."""
import sys
p, old, new = sys.argv[1], sys.argv[2].encode(), sys.argv[3].encode()
b = open(p, 'rb').read()
def uleb(b, i):
    r = s = 0
    while True:
        x = b[i]; i += 1; r |= (x & 0x7f) << s; s += 7
        if x < 0x80: return r, i
def enc(n):
    o = bytearray()
    while True:
        x = n & 0x7f; n >>= 7
        if n: o.append(x | 0x80)
        else: o.append(x); return bytes(o)
i, out, done = 8, bytearray(b[:8]), False
while i < len(b):
    sid = b[i]; size, j = uleb(b, i + 1); body = b[j:j + size]; i = j + size
    if sid == 7:
        n, k = uleb(body, 0); nb = bytearray(enc(n))
        for _ in range(n):
            l, k = uleb(body, k); name = body[k:k + l]; k += l
            kind = body[k]; idx, k2 = uleb(body, k + 1); rest = body[k:k2]; k = k2
            if name == old: name = new; done = True
            nb += enc(len(name)) + name + rest
        body = bytes(nb)
    out += bytes([sid]) + enc(len(body)) + body
open(p, 'wb').write(out)
sys.exit(0 if done else 'renexp: no export ' + old.decode())
