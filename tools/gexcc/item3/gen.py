#!/usr/bin/env python3
"""Adapters for a C library's exports, called by aarch64 guest code
(docs/gexcc.md §7.8): for each export with a prototype in the headers, a gex
F_body `gexp_<name>` that reads AAPCS64's arguments (x0-x7, then the stack),
calls the function and returns to x30 with the result in x0. Integer and
pointer arguments and results, and structs by value as AAPCS64 passes them
(up to 16 bytes in registers, larger ones by reference; a larger result
through x8). No floating point (none in zlib's or zstd's API).

  gen.py LIB EXPORTS|- HEADER... > adapters.c
  LIB: zlib or zstd (which declaration macros mark an export)
"""
import re, sys

lib, exp_arg, headers = sys.argv[1], sys.argv[2], sys.argv[3:]
DECL = {
    'zlib': r'ZEXTERN\s+([^;{]*?)\s*\bZEXPORT[VA]?\s+(\w+)\s*\(([^;{]*?)\)\s*;',
    'zstd': r'\b(?:ZSTDLIB_API|ZSTDLIB_STATIC_API|ZDICTLIB_API|ZDICTLIB_STATIC_API)\s+([^;{()]*?)\s*\b(\w+)\s*\(([^;{]*?)\)\s*;',
}[lib]
protos, structs = {}, set()
for h in headers:
    text = re.sub(r'/\*.*?\*/', ' ', open(h).read(), flags=re.S)
    text = re.sub(r'//[^\n]*', ' ', text)
    text = re.sub(r'^[ \t]*#(?:[^\n]*\\\n)*[^\n]*', ' ', text, flags=re.M)  # preprocessor lines
    for m in re.finditer(DECL, text, re.S):
        ret, name, params = (re.sub(r'\s+', ' ', g).strip() for g in m.groups())
        protos.setdefault(name, (ret, params))
    for m in re.finditer(r'typedef\s+struct\s*\w*\s*\{[^{}]*\}\s*(\w+)\s*;', text):
        structs.add(m.group(1))

def ptype(p):
    p = p.strip()
    if p.endswith(']'):                      # T name[N]: a pointer
        p = re.sub(r'\s*\w*\s*\[[^\]]*\]$', '', p) + ' *'
        return p
    m = re.match(r'^(.*?)\b([A-Za-z_]\w*)\s*$', p)
    t = m.group(1).strip() if m and m.group(1).strip() else p
    return t

def is_struct(t):
    t = re.sub(r'\bconst\b', ' ', t).strip()
    return '*' not in t and t in structs

exports = sorted(protos) if exp_arg == '-' else [l.strip() for l in open(exp_arg) if l.strip()]
print('#include "item3.h"')
for h in headers:
    print(f'#include "{h}"')
done, table = [], []
for e in exports:
    if e not in protos:
        continue
    ret, params = protos[e]
    if '...' in params or 'gzFile' in params + ret or 'va_list' in params or re.search(r'\b(float|double)\b', params + ret):
        continue
    ps = [] if params in ('', 'void') else [ptype(p) for p in params.split(',')]
    print(f'GEXP(gexp_{e}) {{')
    print('\ta64_enter(ctx);')
    print('\tunsigned k = 0;')
    names = []
    for i, t in enumerate(ps):
        v = f'a{i}'
        if is_struct(t):
            t = re.sub(r'\s+', ' ', re.sub(r'\bconst\b', ' ', t)).strip()
            # AAPCS64 B.4/C.10: a composite over 16 bytes goes by reference;
            # a smaller one in consecutive registers (all on the stack when
            # they run out: C.11, not handled here, never reached by these APIs)
            print(f'\t{t} {v};')
            print(f'\tif (sizeof {v} > 16) {v} = *({t} *)(uintptr_t)A64_ARG(ctx, k++);')
            print(f'\telse {{ uint64_t w[2] = {{ A64_ARG(ctx, k), sizeof {v} > 8 ? A64_ARG(ctx, k + 1) : 0 }}; __builtin_memcpy(&{v}, w, sizeof {v}); k += (sizeof {v} + 7) / 8; }}')
        else:
            print(f'\t{t} {v} = ({t})A64_ARG(ctx, k++);')
        names.append(v)
    call = f'{e}({", ".join(names)})'
    if ret == 'void':
        print(f'\t{call};')
        print('\treturn a64_ret(ctx, ret, 0);')
    elif is_struct(ret):
        print(f'\t{ret} r = {call};')
        print('\tif (sizeof r > 16) {')
        print('\t\tuint64_t x8 = A64_X(ctx, 8);')
        print('\t\t__builtin_memcpy((void *)(uintptr_t)x8, &r, sizeof r);')
        print('\t\treturn a64_ret(ctx, ret, x8);')
        print('\t}')
        print('\tuint64_t w[2] = { 0, 0 };')
        print('\t__builtin_memcpy(w, &r, sizeof r);')
        print('\tA64_X(ctx, 1) = w[1];')
        print('\treturn a64_ret(ctx, ret, w[0]);')
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
