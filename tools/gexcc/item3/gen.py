#!/usr/bin/env python3
"""Adapters for a C library's exports, called by aarch64 guest code
(docs/gexcc.md §7.8): for each export with a prototype in the headers, a gex
F_body `gexp_<name>` that reads AAPCS64's arguments (x0-x7, then the stack),
calls the function and returns to x30 with the result in x0. Integer and
pointer arguments and results, and structs by value as AAPCS64 passes them
(up to 16 bytes in registers, larger ones by reference; a larger result
through x8). Floating-point arguments in v0-v7 and a result in v0.
Function-pointer arguments are passed as they are (guest addresses, which
the GexA64Icall pass's exit stubs call). For sqlite (built with gexva.h):
a variadic export's adapter announces the guest's call to the export's
va_start, and a va_list argument is the guest's (AAPCS64's).

  gen.py LIB [--gz] EXPORTS|- HEADER... > adapters.c
  LIB: zlib, zstd or sqlite (which declaration macros mark an export)
"""
import re, sys

args = sys.argv[1:]
gz = '--gz' in args            # zlib's gz* too (with the LP64 libc)
args = [a for a in args if a != '--gz']
lib, exp_arg, headers = args[0], args[1], args[2:]
# (gzopen_w is Windows's, under an #if gen.py does not read)
DECL = {
    'zlib': r'ZEXTERN\s+([^;{]*?)\s*\bZEXPORT[VA]?\s+(\w+)\s*\(([^;{]*?)\)\s*;',
    'zstd': r'\b(?:ZSTDLIB_API|ZSTDLIB_STATIC_API|ZDICTLIB_API|ZDICTLIB_STATIC_API)\s+([^;{()]*?)\s*\b(\w+)\s*\(([^;{]*?)\)\s*;',
    'sqlite': r'\bSQLITE_API\s+([^;{()]*?)\s*\b(\w+)\s*\(([^;{]*?)\)\s*;',
}[lib]
varargs = lib == 'sqlite'
protos, structs = {}, set()
for h in headers:
    text = re.sub(r'/\*.*?\*/', ' ', open(h).read(), flags=re.S)
    text = re.sub(r'//[^\n]*', ' ', text)
    text = re.sub(r'^[ \t]*#(?:[^\n]*\\\n)*[^\n]*', ' ', text, flags=re.M)  # preprocessor lines
    for m in re.finditer(DECL, text, re.S):
        ret, name, params = (re.sub(r'\s+', ' ', g).strip() for g in m.groups())
        ret = re.sub(r'\b(?:SQLITE_DEPRECATED|SQLITE_EXPERIMENTAL|SQLITE_EXTERN)\b', ' ', ret).strip()
        protos.setdefault(name, (ret, params))
    for m in re.finditer(r'typedef\s+struct\s*\w*\s*\{[^{}]*\}\s*(\w+)\s*;', text):
        structs.add(m.group(1))

def split_params(params):
    out, depth, cur = [], 0, ''
    for ch in params:
        if ch == ',' and depth == 0:
            out.append(cur); cur = ''
            continue
        depth += (ch == '(') - (ch == ')')
        cur += ch
    out.append(cur)
    return [x.strip() for x in out]

def ptype(p):
    p = p.strip()
    if '(' in p:                             # a function pointer: drop its name
        return re.sub(r'\(\s*\*\s*\w*\s*\)', '(*)', p, count=1)
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
    if e == 'gzopen_w' or (not gz and ('gzFile' in params + ret or e.startswith('gz'))):
        continue
    if not varargs and ('...' in params or 'va_list' in params):
        continue
    ps = [] if params in ('', 'void') else [ptype(p) for p in split_params(params)]
    variadic = bool(ps) and ps[-1] == '...'
    if variadic:
        ps = ps[:-1]
    print(f'GEXP(gexp_{e}) {{')
    print('\tunsigned k = 0, kv = 0;')
    names = []
    for i, t in enumerate(ps):
        v = f'a{i}'
        if t == 'va_list':
            # AAPCS64 passes the 32-byte struct by reference
            print(f'\tgexva_list {v} = gexva_guest(A64_ARG(ctx, k++));')
        elif re.fullmatch(r'(?:const\s+)?(?:double|float)', t):
            print(f'\t{t} {v} = a64_varg_{t.split()[-1]}(ctx, kv++, &k);')
        elif is_struct(t):
            t = re.sub(r'\s+', ' ', re.sub(r'\bconst\b', ' ', t)).strip()
            # AAPCS64 B.4/C.10: a composite over 16 bytes goes by reference;
            # a smaller one in consecutive registers (all on the stack when
            # they run out: C.11, not handled here, never reached by these APIs)
            print(f'\t{t} {v};')
            print(f'\tif (sizeof {v} > 16) {v} = *({t} *)(uintptr_t)A64_ARG(ctx, k++);')
            print(f'\telse {{ uint64_t w[2] = {{ A64_ARG(ctx, k), sizeof {v} > 8 ? A64_ARG(ctx, k + 1) : 0 }}; __builtin_memcpy(&{v}, w, sizeof {v}); k += (sizeof {v} + 7) / 8; }}')
        else:
            tt = f'__typeof__({t})' if '(' in t else t
            print(f'\t{tt} {v} = ({tt})A64_ARG(ctx, k++);')
        names.append(v)
    # after the arguments are read: a thread's first entry may make syscalls
    print('\ta64_enter(ctx);')
    # Which export this thread last entered (for a crash's report). It also
    # keeps adapters of identical code apart (ZSTD_isError, ZDICT_isError),
    # which wasm-opt would merge under one name, losing the other's slot.
    print(f'\ta64_last_export = {len(table) + 1};')
    call = f'{e}({", ".join(names)})'
    if variadic:
        # the named arguments' registers and stack words (k counts both:
        # integers past x7 are stack words; floating point here never are)
        print(f'\tgexva_pending("{e}", k < 8 ? k : 8, kv, k > 8 ? k - 8 : 0);')
        rv = 'void' if ret == 'void' else ret
        if ret != 'void':
            print(f'\t{ret} r = {call};')
            call = 'r'
        else:
            print(f'\t{call};')
        print('\tgexva_done();')
        if ret == 'void':
            print('\treturn a64_ret(ctx, ret, 0);')
            print('}')
            table.append(e)
            continue
    if ret == 'void':
        print(f'\t{call};')
        print('\treturn a64_ret(ctx, ret, 0);')
    elif re.fullmatch(r'(?:const\s+)?(?:double|float)', ret):
        print(f'\treturn a64_ret_v(ctx, ret, (double){call});')
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
