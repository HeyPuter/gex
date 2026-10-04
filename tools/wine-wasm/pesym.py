#!/usr/bin/env python3
"""Symbol surgery on wasm relocatable objects, for linking Wine's whole PE
side as one module (docs/wine-native.md §3.14).

  pesym.py dll IN.o OUT.o DLLNAME EXPORTS
      One DLL's relocatable object (`wasm-ld -r` of its objects): every
      defined global symbol becomes local, except the DLL's exports (the
      link names listed in EXPORTS, one per line) and its spec glue
      (gexp_*, __gexw_adapters_*), which are renamed DLLNAME!<name>.
      DLLs' private symbols can then no longer collide, and two DLLs may
      export the same name.

  pesym.py imports IN.o OUT.o DLLNAME MAP
      Bind the DLL's undefined symbols to other DLLs' exports: MAP lines
      are `name owner` (the first DLL, in this DLL's import order, that
      exports `name`); each undefined `name` with an owner becomes
      owner!name.

The linking section's symbol table is rewritten in place (names, the
local binding, the explicit-name flag for renamed undefined symbols);
symbol indices and relocations are unchanged."""
import sys

WASM_SYM_BINDING_LOCAL = 0x2
WASM_SYM_UNDEFINED = 0x10
WASM_SYM_EXPLICIT_NAME = 0x40
SYMTAB = 8


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


def rewrite(b, fix):
    """Rewrite the symbol table: fix(kind, flags, name, defined) returns
    (flags, name) for each symbol."""
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
            if sub != SYMTAB:
                body += bytes([sub]) + enc(sublen) + b[r:subend]; r = subend; continue
            count, s = leb(b, r)
            syms = bytearray(enc(count))
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
            body += bytes([SYMTAB]) + enc(len(syms)) + syms
            r = subend
        out += bytes([0]) + enc(len(body)) + body
        p = end
    return bytes(out)


KEEP_PREFIXES = ('gexp_', '__gexw_adapters_')
# The shared runtime (winecrt0's wasm32.o and wasm32_sjlj.o, linked once):
# never made local, never renamed.
RUNTIME = ('__gexw_', 'gexw_', '__wasm_', '__c_longjmp', '__stack_pointer',
           '__memory_base', '__table_base', '__indirect_function_table',
           '__tls_base', 'setjmp', 'longjmp')


def is_runtime(nm):
    return nm is not None and (nm.startswith(RUNTIME) and not nm.startswith('__gexw_adapters_'))


def main():
    mode, src, dst, dll = sys.argv[1:5]
    b = open(src, 'rb').read()
    if mode == 'dll':
        exports = {l.split()[0] for l in open(sys.argv[5]) if l.strip()}

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
            if len(parts) == 2:
                owner.setdefault(parts[0], parts[1])

        def fix(kind, flags, nm, defined):
            if defined or nm is None or is_runtime(nm) or '!' in nm:
                return flags, nm
            o = owner.get(nm)
            return (flags, f'{o}!{nm}') if o else (flags, nm)
    else:
        sys.exit(__doc__)
    open(dst, 'wb').write(rewrite(b, fix))


main()
