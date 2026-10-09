#!/usr/bin/env python3
"""Adapters for a C library's exports, called by aarch64 guest code
(docs/gexcc.md §7.8): for each export with a prototype in the headers, a gex
F_body `gexp_<name>` that reads AAPCS64's integer arguments (x0-x7, then
the stack), calls the function and returns to x30 with the result in x0.
Integer and pointer arguments and results only (zlib's).

  gen.py EXPORTS|- HEADER... > adapters.c
"""
import re, sys

protos = {}
for h in sys.argv[2:]:
    text = re.sub(r'/\*.*?\*/', ' ', open(h).read(), flags=re.S)
    for m in re.finditer(r'ZEXTERN\s+([^;{]*?)\s*\bZEXPORT[VA]?\s+(\w+)\s*\(([^;{]*?)\)\s*;', text, re.S):
        ret, name, params = (re.sub(r'\s+', ' ', g).strip() for g in m.groups())
        protos.setdefault(name, (ret, params))

def ptype(p):
    p = p.strip()
    m = re.match(r'^(.*?)\b([A-Za-z_]\w*)\s*$', p)
    return m.group(1).strip() if m and m.group(1).strip() else p

# EXPORTS: a file of names, or '-' for every prototype in the headers
exports = sorted(protos) if sys.argv[1] == '-' else [l.strip() for l in open(sys.argv[1]) if l.strip()]
print('#include "item3.h"')
print('#include "zlib.h"')
done, table = [], []
for e in exports:
    if e not in protos:
        continue
    ret, params = protos[e]
    if '...' in params or 'gzFile' in params + ret or 'va_list' in params:
        continue
    ps = [] if params in ('', 'void') else [ptype(p) for p in params.split(',')]
    args = ', '.join(f'({t})A64_ARG(ctx, {i})' for i, t in enumerate(ps))
    call = f'{e}({args})'
    print(f'GEXP(gexp_{e}) {{')
    print('\ta64_enter(ctx);')
    if ret == 'void':
        print(f'\t{call};')
        print('\treturn a64_ret(ctx, ret, 0);')
    elif '*' in ret or ret.endswith('p') or ret in ('voidpf', 'gzFile'):
        print(f'\treturn a64_ret(ctx, ret, (uint64_t)(uintptr_t){call});')
    else:
        print(f'\treturn a64_ret(ctx, ret, (uint64_t){call});')
    print('}')
    table.append(e)
print('void *gexp_table[] = {')
for e in table:
    print(f'\t(void *)gexp_{e},')
print('\t(void *)gexp__missing, (void *)gexp__noop, (void *)__gex_thread_start, 0 };')
print(f'/* {len(table)} of {len(exports)} exports */', file=sys.stderr)
