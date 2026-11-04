#!/usr/bin/env python3
"""side-imports.py SIDE.so: the symbols a side module imports from the
program that dlopens it, one per line: its env functions and globals and
its GOT.mem and GOT.func entries, less what gex gives every module (its
memory, table, stack pointer, bases and asyncify globals). A static program
linked with -Wl,--export= each of them serves the module (gex-dylink's
static-program binding)."""
import sys

GEX = {'memory', '__indirect_function_table', '__stack_pointer', '__memory_base', '__table_base',
       '__table_base32', '__asyncify_state', '__asyncify_data'}


def leb(w, p):
    r = sh = 0
    while True:
        x = w[p]; p += 1
        r |= (x & 0x7f) << sh; sh += 7
        if not x & 0x80:
            return r, p


def name(w, p):
    n, p = leb(w, p)
    return w[p:p + n].decode(), p + n


def imports(w):
    if w[:8] != b'\0asm\x01\0\0\0':
        sys.exit(f'side-imports: {sys.argv[1]}: not a wasm module')
    p = 8
    while p < len(w):
        sid = w[p]
        n, p = leb(w, p + 1)
        end = p + n
        if sid == 2:
            k, p = leb(w, p)
            for _ in range(k):
                m, p = name(w, p)
                f, p = name(w, p)
                kind = w[p]; p += 1
                if kind == 0:      # function: type index
                    _, p = leb(w, p)
                elif kind == 1:    # table: reftype, limits
                    p += 1
                    fl = w[p]; p += 1
                    _, p = leb(w, p)
                    if fl & 1:
                        _, p = leb(w, p)
                elif kind == 2:    # memory: limits
                    fl = w[p]; p += 1
                    _, p = leb(w, p)
                    if fl & 1:
                        _, p = leb(w, p)
                elif kind == 3:    # global: valtype, mutability
                    p += 2
                else:              # tag
                    p += 1
                    _, p = leb(w, p)
                yield m, f
            return
        p = end


out = set()
for m, f in imports(open(sys.argv[1], 'rb').read()):
    if m in ('GOT.mem', 'GOT.func') or (m == 'env' and f not in GEX):
        out.add(f)
print('\n'.join(sorted(out)))
