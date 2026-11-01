#!/usr/bin/env python3
"""stripnames.py IN OUT: a release copy of a gexcc-win module. Out: the debug
sections, producers, and every name gex never reads. Kept: the names the loader
looks a table slot up by (provider::slot_names, a slot by its function's
name): the PE adapters' (gexp_*, DLL!gexp_*), and an exported function's
(a GOT.func import of it, here or in another module, takes its slot)."""
import sys

DROP = ('producers', 'external_debug_info', 'sourceMappingURL')


def leb(b, p):
    v = s = 0
    while True:
        c = b[p]; p += 1
        v |= (c & 0x7f) << s; s += 7
        if c < 0x80:
            return v, p


def enc(v):
    o = bytearray()
    while True:
        c = v & 0x7f; v >>= 7
        if v:
            o.append(c | 0x80)
        else:
            o.append(c)
            return bytes(o)


def name_of(b, p):
    n, p = leb(b, p)
    return b[p:p + n].decode('utf-8', 'replace'), p + n


def skip_expr(b, p, funcs):
    # a constant expression: ref.func IDX / ref.null T / global.get / i32.const .. end
    while True:
        op = b[p]; p += 1
        if op == 0x0b:
            return p
        if op == 0xd2:
            f, p = leb(b, p); funcs.add(f)
        elif op in (0xd0, 0x23, 0x41, 0x42):
            _, p = leb(b, p)
        else:
            raise SystemExit('stripnames: unknown opcode 0x%02x in an element expression' % op)


def slot_funcs(sec):
    funcs = set()
    n, p = leb(sec, 0)
    for _ in range(n):
        flags, p = leb(sec, p)
        if flags & 1 == 0:          # active
            if flags & 2:
                _, p = leb(sec, p)  # table index
            p = skip_expr(sec, p, set())
        if flags & 3:               # elemkind or reftype
            p += 1
        cnt, p = leb(sec, p)
        for _ in range(cnt):
            if flags & 4:
                p = skip_expr(sec, p, funcs)
            else:
                f, p = leb(sec, p); funcs.add(f)
    return funcs


def main():
    b = open(sys.argv[1], 'rb').read()
    assert b[:4] == b'\0asm'
    secs, p = [], 8
    while p < len(b):
        sid = b[p]
        size, q = leb(b, p + 1)
        secs.append((sid, b[q:q + size]))
        p = q + size
    slots, exported = set(), set()
    for sid, body in secs:
        if sid == 9:
            slots |= slot_funcs(body)
        elif sid == 7:
            n, q = leb(body, 0)
            for _ in range(n):
                name, q = name_of(body, q)
                kind = body[q]
                _, q = leb(body, q + 1)
                if kind == 0:
                    exported.add(name)
    out = bytearray(b[:8])
    for sid, body in secs:
        if sid == 0:
            name, q = name_of(body, 0)
            if name.startswith('.debug_') or name in DROP:
                continue
            if name == 'name':
                sub, r = b'', q
                while r < len(body):
                    kind = body[r]
                    size, r2 = leb(body, r + 1)
                    if kind == 1:   # function names: the slots' only
                        cnt, s = leb(body, r2)
                        kept = bytearray()
                        k = 0
                        for _ in range(cnt):
                            idx, s = leb(body, s)
                            ln, s2 = leb(body, s)
                            nm = body[s2:s2 + ln].decode('utf-8', 'replace')
                            if idx in slots and (nm.startswith('gexp_') or '!gexp_' in nm or nm in exported):
                                kept += enc(idx) + enc(ln) + body[s2:s2 + ln]
                                k += 1
                            s = s2 + ln
                        part = enc(k) + bytes(kept)
                        sub += bytes([1]) + enc(len(part)) + part
                    r = r2 + size
                body = body[:q] + sub
        out += bytes([sid]) + enc(len(body)) + body
    open(sys.argv[2], 'wb').write(out)


main()
