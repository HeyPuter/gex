#!/usr/bin/env python3
"""Symbol surgery on wasm relocatable objects, for linking Wine's whole PE
side as one module (docs/wine-native.md §3.14).

  pesym.py dll IN.o OUT.o DLLNAME EXPORTS [ALIASES]
      One DLL's relocatable object (`wasm-ld -r` of its objects): every
      defined global symbol becomes local, except the DLL's exports (the
      link names listed in EXPORTS, one per line) and its spec glue
      (gexp_*, __gexw_adapters_*), which are renamed DLLNAME!<name>.
      DLLs' private symbols can then no longer collide, and two DLLs may
      export the same name. ALIASES lines are `new name`: another symbol
      for the function `name`, the export another DLL forwards to it
      (kernel32.dll!HeapAlloc for ntdll's RtlAllocateHeap).

  pesym.py imports IN.o OUT.o DLLNAME MAP
      Bind the DLL's undefined symbols to other DLLs' exports: MAP lines
      are `name owner [target]` (the first DLL, in this DLL's import order,
      that exports `name`, and the name it has there when a forward led to
      it); each undefined `name` with an owner becomes owner!target.

  pesym.py sigs IN.o
      Print `name defined|undefined SIG` for each global function symbol,
      SIG as `ret:params` in wasm value types (i, I, f, d, v for none).

The linking section's symbol table is rewritten in place (names, the
local binding, the explicit-name flag for renamed undefined symbols);
symbol indices and relocations are unchanged."""
import sys

WASM_SYM_BINDING_LOCAL = 0x2
WASM_SYM_UNDEFINED = 0x10
WASM_SYM_EXPLICIT_NAME = 0x40
SYMTAB = 8


SEGMENT_INFO = 5
WASM_SEG_FLAG_RETAIN = 4


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


def name(b, p):
    n, p = leb(b, p)
    return b[p:p + n].decode(), p + n


def enc_name(s):
    s = s.encode()
    return enc(len(s)) + s


def import_func_names(b):
    """Field names of the function imports, in order (an undefined function
    symbol without an explicit name is named by its import)."""
    p, names = 8, []
    while p < len(b):
        sid = b[p]; p += 1
        size, p = leb(b, p)
        end = p + size
        if sid == 2:
            n, q = leb(b, p)
            for _ in range(n):
                _, q = name(b, q)
                field, q = name(b, q)
                kind = b[q]; q += 1
                if kind == 0:
                    _, q = leb(b, q); names.append(field)
                elif kind == 1:
                    q += 1; fl, q = leb(b, q); _, q = leb(b, q)
                    if fl & 1: _, q = leb(b, q)
                elif kind == 2:
                    fl, q = leb(b, q); _, q = leb(b, q)
                    if fl & 1: _, q = leb(b, q)
                elif kind == 3:
                    q += 2
                elif kind == 4:
                    q += 1; _, q = leb(b, q)
        p = end
    return names


def rewrite(b, fix, aliases=()):
    """Rewrite the symbol table: fix(kind, flags, name, defined) returns
    (flags, name) for each symbol. `aliases` are (new name, name) pairs:
    a symbol of its own for each, at the end of the table, defined as the
    function that had `name`."""
    func_imports = import_func_names(b)
    out = bytearray(b[:8])
    p = 8
    while p < len(b):
        sid = b[p]
        size, q = leb(b, p + 1)
        end = q + size
        if sid != 0:
            out += b[p:end]; p = end; continue
        secname, r = name(b, q)
        if secname == 'name':
            # `wasm-ld -r` writes the old names here, and a later link
            # prefers them to the symbols' names: drop it, so the final
            # module's names are the (renamed) symbols'.
            p = end; continue
        if secname != 'linking':
            out += b[p:end]; p = end; continue
        body = bytearray(enc_name('linking'))
        version, r = leb(b, r)
        body += enc(version)
        while r < end:
            sub = b[r]; r += 1
            sublen, r = leb(b, r)
            subend = r + sublen
            if sub == SEGMENT_INFO:
                # `wasm-ld -r` drops the RETAIN flag: put it back on the
                # ICALL pass's entry-thunk pairs (`gexw_entries`), which
                # nothing references but gex.
                count, s = leb(b, r)
                segs = enc(count)
                for _ in range(count):
                    nm, s = name(b, s)
                    align, s = leb(b, s)
                    flags, s = leb(b, s)
                    if nm == 'gexw_entries':
                        flags |= WASM_SEG_FLAG_RETAIN
                    segs += enc_name(nm) + enc(align) + enc(flags)
                body += bytes([sub]) + enc(len(segs)) + segs; r = subend; continue
            if sub != SYMTAB:
                body += bytes([sub]) + enc(sublen) + b[r:subend]; r = subend; continue
            count, s = leb(b, r)
            syms = bytearray()
            funcs = {}  # defined function name (before fix) -> (flags, index)
            for _ in range(count):
                kind = b[s]; s += 1
                flags, s = leb(b, s)
                undefined = bool(flags & WASM_SYM_UNDEFINED)
                if kind in (0, 2, 4, 5):  # function, global, tag, table
                    idx, s = leb(b, s)
                    has_name = not undefined or (flags & WASM_SYM_EXPLICIT_NAME)
                    if has_name:
                        nm, s = name(b, s)
                    else:
                        nm = func_imports[idx] if kind == 0 and idx < len(func_imports) else None
                    nflags, nnm = fix(kind, flags, nm, not undefined)
                    if kind == 0 and not undefined and nm is not None:
                        funcs[nm] = (nflags, idx)
                    if undefined and nnm != nm:
                        nflags |= WASM_SYM_EXPLICIT_NAME
                    syms += bytes([kind]) + enc(nflags) + enc(idx)
                    if not undefined or (nflags & WASM_SYM_EXPLICIT_NAME):
                        syms += enc_name(nnm)
                elif kind == 1:  # data
                    nm, s = name(b, s)
                    rest = b''
                    if not undefined:
                        t = s
                        for _ in range(3):
                            _, t = leb(b, t)
                        rest = b[s:t]; s = t
                    nflags, nnm = fix(kind, flags, nm, not undefined)
                    syms += bytes([kind]) + enc(nflags) + enc_name(nnm) + rest
                elif kind == 3:  # section
                    idx, s = leb(b, s)
                    syms += bytes([kind]) + enc(flags) + enc(idx)
                else:
                    sys.exit(f'unknown symbol kind {kind}')
            added = 0
            for new, old in aliases:
                if old in funcs:
                    fl, idx = funcs[old]
                    syms += bytes([0]) + enc(fl & ~WASM_SYM_BINDING_LOCAL) + enc(idx) + enc_name(new)
                    added += 1
            syms = enc(count + added) + syms
            body += bytes([SYMTAB]) + enc(len(syms)) + syms
            r = subend
        out += bytes([0]) + enc(len(body)) + body
        p = end
    return bytes(out)


VALTYPES = {0x7f: 'i', 0x7e: 'I', 0x7d: 'f', 0x7c: 'd', 0x7b: 'V'}


def func_sigs(b):
    """(signatures of the function imports, signatures of the defined
    functions), in index order."""
    p, types, imports, defined = 8, [], [], []
    while p < len(b):
        sid = b[p]; p += 1
        size, p = leb(b, p)
        end = p + size
        q = p
        if sid == 1:
            n, q = leb(b, q)
            for _ in range(n):
                q += 1  # 0x60
                np_, q = leb(b, q)
                params = ''.join(VALTYPES.get(b[q + i], '?') for i in range(np_)); q += np_
                nr, q = leb(b, q)
                rets = ''.join(VALTYPES.get(b[q + i], '?') for i in range(nr)); q += nr
                types.append(f'{rets or "v"}:{params}')
        elif sid == 2:
            n, q = leb(b, q)
            for _ in range(n):
                _, q = name(b, q)
                _, q = name(b, q)
                kind = b[q]; q += 1
                if kind == 0:
                    t, q = leb(b, q); imports.append(types[t])
                elif kind == 1:
                    q += 1; fl, q = leb(b, q); _, q = leb(b, q)
                    if fl & 1: _, q = leb(b, q)
                elif kind == 2:
                    fl, q = leb(b, q); _, q = leb(b, q)
                    if fl & 1: _, q = leb(b, q)
                elif kind == 3:
                    q += 2
                elif kind == 4:
                    q += 1; _, q = leb(b, q)
        elif sid == 3:
            n, q = leb(b, q)
            for _ in range(n):
                t, q = leb(b, q); defined.append(types[t])
        p = end
    return imports, defined


def print_sigs(b):
    imports, defined = func_sigs(b)
    out = []
    p = 8
    func_imports = import_func_names(b)
    while p < len(b):
        sid = b[p]
        size, q = leb(b, p + 1)
        end = q + size
        if sid == 0:
            secname, r = name(b, q)
            if secname == 'linking':
                _, r = leb(b, r)
                while r < end:
                    sub = b[r]; r += 1
                    sublen, r = leb(b, r)
                    subend = r + sublen
                    if sub == SYMTAB:
                        count, t = leb(b, r)
                        for _ in range(count):
                            kind = b[t]; t += 1
                            flags, t = leb(b, t)
                            undefined = bool(flags & WASM_SYM_UNDEFINED)
                            if kind in (0, 2, 4, 5):
                                idx, t = leb(b, t)
                                if not undefined or (flags & WASM_SYM_EXPLICIT_NAME):
                                    nm, t = name(b, t)
                                else:
                                    nm = func_imports[idx] if kind == 0 and idx < len(func_imports) else None
                                if kind == 0 and nm and not flags & WASM_SYM_BINDING_LOCAL:
                                    sig = imports[idx] if undefined else defined[idx - len(imports)]
                                    out.append(f'{nm} {"undefined" if undefined else "defined"} {sig}')
                            elif kind == 1:
                                _, t = name(b, t)
                                if not undefined:
                                    for _ in range(3):
                                        _, t = leb(b, t)
                            elif kind == 3:
                                _, t = leb(b, t)
                    r = subend
        p = end
    print('\n'.join(out))


KEEP_PREFIXES = ('gexp_', '__gexw_adapters_')
# The shared runtime (winecrt0's wasm32.o and wasm32_sjlj.o, linked once):
# never made local, never renamed.
RUNTIME = ('__gexw_', 'gexw_', '__wasm_', '__c_longjmp', '__stack_pointer',
           '__memory_base', '__table_base', '__indirect_function_table',
           '__tls_base', 'setjmp', 'longjmp')


def is_runtime(nm):
    return nm is not None and (nm.startswith(RUNTIME) and not nm.startswith('__gexw_adapters_'))


def main():
    if sys.argv[1] == 'sigs':
        return print_sigs(open(sys.argv[2], 'rb').read())
    mode, src, dst, dll = sys.argv[1:5]
    b = open(src, 'rb').read()
    if mode == 'dll':
        exports = {l.split()[0] for l in open(sys.argv[5]) if l.strip() and 'forward' not in l.split()[1:2]}
        aliases = [tuple(l.split()) for l in open(sys.argv[6]) if len(l.split()) == 2] if len(sys.argv) > 6 else []

        def fix(kind, flags, nm, defined):
            if not defined or nm is None or flags & WASM_SYM_BINDING_LOCAL or is_runtime(nm):
                return flags, nm
            if nm in exports or nm.startswith(KEEP_PREFIXES):
                return flags, f'{dll}!{nm}'
            return flags | WASM_SYM_BINDING_LOCAL, nm
    elif mode == 'imports':
        owner = {}
        for l in open(sys.argv[5]):
            parts = l.split()
            if len(parts) in (2, 3):
                owner.setdefault(parts[0], (parts[1], parts[-1] if len(parts) == 3 else parts[0]))

        def fix(kind, flags, nm, defined):
            if defined or nm is None or is_runtime(nm) or '!' in nm:
                return flags, nm
            o = owner.get(nm)
            return (flags, f'{o[0]}!{o[1]}') if o else (flags, nm)
    else:
        sys.exit(__doc__)
    open(dst, 'wb').write(rewrite(b, fix, aliases if mode == 'dll' else ()))


main()
