#!/usr/bin/env python3
"""A prototype of winebuild's wasm32 output: from a DLL's .spec file and its
compiled wasm code, generate

  * the entry adapters: one gex F_body per export, `gexp_<name>`, which
    reads the i386 caller's arguments from the guest stack, calls the
    native function, puts the result where i386 code expects it (EAX,
    EDX:EAX, ST0) and returns as the spec's convention says (stdcall pops
    its arguments). gex binds each at its export's address in the function
    table (a stateful provider, shims/native/src/libprov.rs);
  * `gexp__entry` in place of DllMain, and `gexp__missing`;
  * the exit stubs the ICALL check calls for a guest target, one per wasm
    signature (`--exit RET:ARGS`, e.g. `i:pp`): they build an i386 frame on
    the guest stack and call the target through the function table;
  * the PE skeleton's source: an i386 DLL whose exports are a few bytes of
    ud2 each (never run: gex binds the adapters there) and its .def file.

The spec gives the convention and the argument words; the C signature
(argument and return types) comes from the compiled code itself, so the
generator reads the wasm type of each export from a linked probe module.

    specgen.py demo.spec probe.wasm OUT_PREFIX [--exit i:pp ...]

writes OUT_PREFIX.adapters.c, OUT_PREFIX.skel.c, OUT_PREFIX.def."""
import sys

WORDS = {'long': 1, 'ptr': 1, 'str': 1, 'wstr': 1, 'segptr': 1, 'float': 1,
         'int64': 2, 'double': 2, 'int128': 4}
VT = {0x7f: 'i32', 0x7e: 'i64', 0x7d: 'f32', 0x7c: 'f64'}
CT = {'i32': 'int32_t', 'i64': 'int64_t', 'f32': 'float', 'f64': 'double'}


def leb(b, p, signed=False):
    r = s = 0
    while True:
        x = b[p]; p += 1
        r |= (x & 0x7f) << s; s += 7
        if not x & 0x80:
            if signed and x & 0x40:
                r -= 1 << s
            return r, p


def name(b, p):
    n, p = leb(b, p)
    return b[p:p + n].decode(), p + n


def wasm_export_types(path):
    """{export name: (params, results)} of a module's function exports."""
    b = open(path, 'rb').read()
    p = 8
    types, funcs, nimp, exports = [], [], 0, {}
    while p < len(b):
        sid = b[p]; p += 1
        size, p = leb(b, p)
        end = p + size
        if sid == 1:
            n, q = leb(b, p)
            for _ in range(n):
                assert b[q] == 0x60; q += 1
                np_, q = leb(b, q); ps = [VT[b[q + i]] for i in range(np_)]; q += np_
                nr, q = leb(b, q); rs = [VT[b[q + i]] for i in range(nr)]; q += nr
                types.append((ps, rs))
        elif sid == 2:
            n, q = leb(b, p)
            for _ in range(n):
                _, q = name(b, q); _, q = name(b, q)
                kind = b[q]; q += 1
                if kind == 0:
                    t, q = leb(b, q); funcs.append(t); nimp += 1
                elif kind == 1:
                    q += 1; fl = b[q]; q += 1; _, q = leb(b, q)
                    if fl & 1: _, q = leb(b, q)
                elif kind == 2:
                    fl, q = leb(b, q); _, q = leb(b, q)
                    if fl & 1: _, q = leb(b, q)
                elif kind == 3:
                    q += 2
        elif sid == 3:
            n, q = leb(b, p)
            for _ in range(n):
                t, q = leb(b, q); funcs.append(t)
        elif sid == 7:
            n, q = leb(b, p)
            for _ in range(n):
                nm, q = name(b, q)
                kind = b[q]; q += 1
                idx, q = leb(b, q)
                if kind == 0:
                    exports[nm] = types[funcs[idx]]
        p = end
    return exports


def parse_spec(path):
    out = []
    for line in open(path):
        line = line.split('#', 1)[0].strip()
        if not line.startswith('@'):
            continue
        toks = line.split()
        conv = toks[1]
        i = 2
        while toks[i].startswith('-'):
            i += 1
        rest = ' '.join(toks[i:])
        nm, args = rest.split('(', 1)
        args = args.split(')', 1)[0].split()
        out.append((conv, nm.strip(), args))
    return out


def adapter(conv, nm, args, sig):
    ps, rs = sig
    words = sum(WORDS[a] for a in args)
    wasm_words = sum(2 if t in ('i64', 'f64') else 1 for t in ps)
    if words != wasm_words:
        sys.exit(f'{nm}: the spec says {words} argument words, the code {wasm_words}')
    if conv not in ('stdcall', 'cdecl', 'varargs'):
        sys.exit(f'{nm}: convention {conv} is not in the prototype')
    L = [f'GEX_PROVIDER(gexp_{nm})', '{']
    off, vals = 0, []
    for k, t in enumerate(ps):
        if t == 'i32':
            vals.append(f'(int32_t)GW(sp + {4 + off})'); off += 4
        elif t == 'f32':
            vals.append(f'gex_f32(GW(sp + {4 + off}))'); off += 4
        elif t == 'i64':
            vals.append(f'(int64_t)(*(gex_u64 *)GEX_P(sp + {4 + off}))'); off += 8
        else:
            vals.append(f'gex_f64(*(gex_u64 *)GEX_P(sp + {4 + off}))'); off += 8
    pop = 4 * words if conv == 'stdcall' else 0
    L.append('    uint32_t sp = gp_reg(ctx, R_ESP);')
    L.append('    int32_t save_ctx = gexw_get_ctx(), save_depth = gexw_get_depth();')
    L.append('    gexw_set_ctx(ctx); gexw_set_depth(depth);')
    # Called through its wasm signature: the C prototype is the DLL's, the
    # generator knows only what the code compiled to (pointers are i32).
    fty = f'{CT[rs[0]] if rs else "void"} (*)({", ".join(CT[t] for t in ps) or "void"})'
    call = f'(({fty}){nm})({", ".join(vals)})'
    r = rs[0] if rs else None
    if r is None:
        L.append(f'    {call};')
    else:
        L.append(f'    {CT[r]} r = {call};')
    L.append('    gexw_set_ctx(save_ctx); gexw_set_depth(save_depth);')
    if r == 'i32':
        L.append(f'    return gp_return_eax(ctx, ret, {pop}, (uint32_t)r);')
    elif r == 'i64':
        L.append('    gp_set(ctx, R_EDX, (uint32_t)((uint64_t)r >> 32));')
        L.append(f'    return gp_return_eax(ctx, ret, {pop}, (uint32_t)r);')
    elif r in ('f32', 'f64'):
        L.append('    gp_push_st0(ctx, (double)r);')
        L.append(f'    return gp_return(ctx, ret, {pop});')
    else:
        L.append(f'    return gp_return(ctx, ret, {pop});')
    L.append('}')
    return '\n'.join(L)


EXIT_CT = {'i': 'int32_t', 'p': 'const void *', 'I': 'int64_t', 'd': 'double', 'v': 'void'}


def exit_stub(spec):
    ret, args = spec.split(':')
    fn = f'gexw_exit_{ret}_{args}'
    params = ', '.join(f'{EXIT_CT[c]} a{k}' for k, c in enumerate(args)) or 'void'
    words = sum(2 if c in 'Id' else 1 for c in args)
    L = [f'{EXIT_CT[ret]} {fn}({params})', '{']
    L.append('    uint32_t target = gexw_get_icall_target();')
    L.append('    int ctx = gexw_get_ctx();')
    L.append('    if (!ctx) ctx = gex_provider_ctx();')
    L.append('    uint32_t sp0 = gp_reg(ctx, R_ESP);')
    L.append(f'    uint32_t sp = sp0 - {4 * words + 4};')
    off = 4
    for k, c in enumerate(args):
        if c in 'Id':
            L.append(f'    *(gex_u64 *)GEX_P(sp + {off}) = {"gex_b64(a%d)" % k if c == "d" else "(uint64_t)a%d" % k};'); off += 8
        else:
            L.append(f'    GW(sp + {off}) = (uint32_t)(uintptr_t)a{k};'); off += 4
    L.append('    uint32_t ra = target ^ 1u; /* in the callee\'s page, so tier-1 can watch it (gp_call0\'s trick) */')
    L.append('    GW(sp) = ra;')
    L.append('    gp_set(ctx, R_ESP, sp);')
    L.append('    GEX_C64(GEX_CTX_LAST_TARGET) = target;')
    L.append('    if (!GEX_FTAB(gex_ftab_idx(target))(ctx, gexw_get_depth() + 1, 0xffffffffu, ra))')
    L.append('        __builtin_trap(); /* the guest left another way: W2 unwinds here */')
    L.append('    /* Whatever the callee popped (stdcall or cdecl), the frame is ours. */')
    L.append('    gp_set(ctx, R_ESP, sp0);')
    if ret == 'i':
        L.append('    return (int32_t)gp_reg(ctx, R_EAX);')
    elif ret == 'I':
        L.append('    return (int64_t)(((uint64_t)gp_reg(ctx, R_EDX) << 32) | gp_reg(ctx, R_EAX));')
    elif ret == 'd':
        L.append('    return gexw_pop_st0(ctx);')
    L.append('}')
    return '\n'.join(L)


def main():
    spec, probe, out = sys.argv[1:4]
    exits = [sys.argv[i + 1] for i in range(4, len(sys.argv)) if sys.argv[i] == '--exit']
    src = None
    for i in range(4, len(sys.argv)):
        if sys.argv[i] == '--source':
            src = sys.argv[i + 1]
    entries = parse_spec(spec)
    types = wasm_export_types(probe)
    dll = spec.rsplit('/', 1)[-1].rsplit('.', 1)[0] + '.dll'

    a = [f'/* Generated by tools/wine-wasm/specgen.py from {spec.rsplit("/", 1)[-1]}. Do not edit. */',
         '#include "gex_abi.h"', '#include "provider.h"',
         '__attribute__((import_module("gex"), import_name("provider_ctx"))) int gex_provider_ctx(void);',
         '__attribute__((import_module("gex"), import_name("provider_missing"))) void gex_provider_missing(int addr);',
         ]
    if src:
        a.append(f'#include "{src}"')
    for conv, nm, args in entries:
        if nm not in types:
            sys.exit(f'{nm}: not in {probe}')
        a.append(adapter(conv, nm, args, types[nm]))
    a.append('/* DllMain(hinst, reason, reserved): the skeleton has none to run. */')
    a.append('GEX_PROVIDER(gexp__entry) { return gp_return_eax(ctx, ret, 12, 1); }')
    a.append('GEX_PROVIDER(gexp__missing) { gex_provider_missing((int)GP_SELF_PC()); return 0; }')
    for e in exits:
        a.append(exit_stub(e))
    names = [f'gexp_{nm}' for _, nm, _ in entries] + ['gexp__entry', 'gexp__missing']
    a.append('/* In the element segment, where gex finds them by name. */')
    a.append('__attribute__((used)) gex_body_t const gexw_table[] = { ' + ', '.join(names) + ' };')
    open(out + '.adapters.c', 'w').write('\n\n'.join(a) + '\n')

    s = [f'/* Generated by tools/wine-wasm/specgen.py: the PE skeleton of {dll}. */',
         '/* Each export is a few bytes of ud2 that never run: gex binds the',
         ' * native adapter at the export\'s address. */']
    for _, nm, _ in entries:
        s.append(f'__attribute__((naked)) void skel_{nm}(void) {{ __asm__("ud2; ud2"); }}')
    s.append('__attribute__((naked)) void skel_entry(void) { __asm__("ud2; ud2"); }')
    open(out + '.skel.c', 'w').write('\n'.join(s) + '\n')
    d = [f'LIBRARY {dll}', 'EXPORTS'] + [f'    {nm}=skel_{nm}' for _, nm, _ in entries]
    open(out + '.def', 'w').write('\n'.join(d) + '\n')


main()
