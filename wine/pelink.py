#!/usr/bin/env python3
"""Link Wine's PE side as one wasm module.

    pelink.py [--split FILE DLL,...] [--exports-of CORE.wasm] [--extra NAME R.O EXPORTS IMPORTS ...] WINE_BUILD WINE_SRC OUT.wasm [DLL ...]
    pelink.py --gexrt LIBGEXRT.a -o OUT.wasm --dll NAME R.o EXPORTS [IMPORTS,...] ...

For each DLL (all of them, or those named), the build left the DLL as one
relocatable object (dlls/X/X.dll.so.r.o) and its export list (.exports),
written by winegcc and winebuild for wasm32. This:
  1. renames each DLL's exports and spec glue DLL!name and makes the rest of
     its symbols local (pesym.py dll), so DLLs' private symbols and shared
     export names cannot collide;
  2. binds each DLL's undefined symbols to the DLL that exports them: the
     DLLs it imports (its Makefile.in's IMPORTS and DELAYIMPORTS) first, then
     any DLL (pesym.py imports);
  3. links them all with the shared runtime (libs/gexrt) into OUT.wasm, a
     position-independent module whose data exports are exported (globals
     holding their offsets) for gex to point the skeletons at.
Whatever stays undefined is listed (OUT.wasm.undefined) and left an import.

A wasm64 build tree (HOST_ARCH = wasm64, or --wasm64 before the arguments:
WINE64's LLP64 PE side) links with wasm-ld -mwasm64,
compiles its generated C with wcc64 -D_WIN32, takes compiler-rt from
GEX_WASM64_SYSROOT and lowers the module to wasm32 (--memory64-lowering), as
wcc64 does a DLL's own side module. A tree configured with CC="wcc64
-mgex-memory64" (wine64-dist.sh WINE64_MEM64=1), or --memory64 after --wasm64:
the memory64 flavour, for a process with a 64-bit memory, compiled the
same way, linked for a 16 GiB memory and not lowered."""
import atexit
import os
import re
import shutil
import subprocess
import sys
import tempfile

# The C functions a module of the process's other mode calls directly
# (shims' wasmdll.rs CROSS_MODE): WoW64's native wow64cpu, for native i386
# ntdll's system calls and Unix calls.
CROSS_MODE = ('wow64cpu.dll!__wine_gex_syscall32', 'wow64cpu.dll!__wine_gex_unix_call32')

here = os.path.dirname(os.path.realpath(__file__))
EAGER = {}  # module -> the DLLs to load before it (pelink's main)
# DLLs a program's activation context may redirect (to comctl32_v6.dll)
ACTCTX_DLLS = ('comctl32.dll',)
# A DLL the loader finds by another name: comctl32's version 6 is
# comctl32.dll in its side-by-side directory, which a program's manifest
# (its activation context) names; LoadLibraryA("comctl32_v6.dll") finds
# nothing.
LOAD_NAME = {'comctl32_v6.dll': 'comctl32.dll'}
DELAY = {}  # module -> its delay imports (pelink's main)
SRC = None  # the Wine source tree (pelink's main)
BUILD = None  # the Wine build tree (pelink's main)
WINE64 = False  # a wasm64 (LLP64) build: wcc64, lowered (pelink's main)
M64 = False  # of those, the memory64 flavour: not lowered (pelink's main)
SPLIT = {}  # DLL -> the module of its own it is linked into, not the core (--split)
STATIC = {}  # DLL -> the static libraries its IMPORTS name (libuuid.a), as paths (pelink's main)
KEEP_EXPORTS = set()  # names the core exports if it defines them (--exports-of)


def cc(*args):
    """The compiler for pelink's generated C: wcc, or for wasm64 wcc64 with
    Windows x64's data model (all of it is PE-side code there)."""
    if WINE64:
        return [os.path.join(here, 'wcc64'), '-D_WIN32'] + (['-mgex-memory64'] if M64 else []) + \
            [a for a in args if a != '-D_WIN32']
    return [os.path.join(here, 'wcc')] + list(args)


def enc_leb(v):
    out = bytearray()
    while True:
        x = v & 0x7f
        v >>= 7
        out.append(x | (0x80 if v else 0))
        if not v:
            return bytes(out)


def enc_name(s):
    b = s.encode()
    return enc_leb(len(b)) + b


def makefile_var(path, var):
    try:
        text = open(path).read().replace('\\\n', ' ')
    except OSError:
        return []
    m = re.search(rf'^{var}\s*=(.*)$', text, re.M)
    return m.group(1).split() if m else []


def spec_path(src, top, d, mod):
    """A module's .spec: X.spec for X.dll, else the full name (ntoskrnl.exe.spec)."""
    for name in (mod.rsplit('.', 1)[0] + '.spec', mod + '.spec'):
        p = os.path.join(src, top, d, name)
        if os.path.exists(p):
            return p
    return os.path.join(src, top, d, mod + '.spec')


def owner_spec(src, owner):
    """The .spec of a DLL by its file name: dlls/X/ for X.dll, dlls/X.drv/
    for one whose directory keeps its extension."""
    stem = owner.rsplit('.', 1)[0]
    p = spec_path(src, 'dlls', stem, owner)
    return p if os.path.exists(p) else spec_path(src, 'dlls', owner, owner)


def spec_varargs(spec):
    """The names a .spec exports as varargs (cdecl, the variadic arguments
    after the fixed ones on the stack)."""
    out = set()
    try:
        lines = open(spec).read().splitlines()
    except OSError:
        return out
    for line in lines:
        parts = line.split('#', 1)[0].split()
        if len(parts) >= 3 and parts[1] == 'varargs':
            rest = [t for t in parts[2:] if not t.startswith('-')]
            if rest:
                out.add(rest[0].split('(', 1)[0])
    return out


def spec_i386_args(spec):
    """The argument types of a .spec's i386 functions, {name: [type, ...]}
    (int64 and int128 are 8 and 16 bytes on the stack: a struct by value is
    one of them when its size is)."""
    out = {}
    try:
        lines = open(spec).read().splitlines()
    except OSError:
        return out
    for line in lines:
        parts = line.split('#', 1)[0].split()
        if len(parts) < 3 or parts[1] not in ('stdcall', 'cdecl', 'thiscall', 'fastcall', 'varargs'):
            continue
        arch = [t.split('=', 1)[1] for t in parts[2:] if t.startswith('-arch=')]
        if arch and not any(a in ('i386', 'win32') or (a.startswith('!') and a[1:] not in ('i386', 'win32'))
                            for a in arch[0].split(',')):
            continue
        rest = ' '.join(t for t in parts[2:] if not t.startswith('-'))
        m = re.match(r'([^\s(]+)\(([^)]*)\)', rest)
        if m:
            out[m.group(1)] = m.group(2).split()
    return out


# The words of a variadic call's buffer passed on to an i386 varargs export:
# its callee pops none, so the ones past its arguments are only read.
VARARGS_WORDS = 16


def spec_stdcalls(spec):
    """The names a .spec exports as stdcall."""
    out = set()
    try:
        lines = open(spec).read().splitlines()
    except OSError:
        return out
    for line in lines:
        parts = line.split('#', 1)[0].split()
        if len(parts) >= 3 and parts[1] == 'stdcall':
            rest = [t for t in parts[2:] if not t.startswith('-')]
            if rest:
                out.add(rest[0].split('(', 1)[0])
    return out


def spec_own(spec):
    """The function names a .spec exports from the DLL's own code: not
    `-import` entries, not forwards (a target after the arguments)."""
    try:
        lines = open(spec).read().splitlines()
    except OSError:
        return set()
    out = set()
    for line in lines:
        line = line.split('#', 1)[0].strip()
        parts = line.split()
        if len(parts) < 3 or parts[1] not in ('stdcall', 'cdecl', 'thiscall', 'varargs', 'fastcall'):
            continue
        if '-import' in parts:
            continue
        rest = [t for t in parts[2:] if not t.startswith('-')]
        if not rest or ')' not in ' '.join(rest):
            continue
        tail = ' '.join(rest).split(')', 1)[1].split()
        if tail:
            continue  # forwarded (or renamed to another symbol)
        out.add(rest[0].split('(', 1)[0])
    return out


def spec_functions(spec):
    """The function names a .spec exports (its stdcall/cdecl/thiscall/
    varargs entries; not data, stubs or extern)."""
    try:
        lines = open(spec).read().splitlines()
    except OSError:
        return []
    out = []
    for line in lines:
        line = line.split('#', 1)[0].strip()
        parts = line.split()
        if len(parts) < 3 or parts[1] not in ('stdcall', 'cdecl', 'thiscall', 'varargs', 'fastcall'):
            continue
        rest = [t for t in parts[2:] if not t.startswith('-')]
        if rest:
            out.append(rest[0].split('(', 1)[0])
    return out


def main():
    global WINE64, M64
    if sys.argv[1] == '--wasm64':
        WINE64 = True
        del sys.argv[1]
        if sys.argv[1] == '--memory64':
            M64 = True
            del sys.argv[1]
    if sys.argv[1] == '--gexrt':
        # explicit inputs (tests)
        args = sys.argv[1:]
        gexrt, out, dlls = None, None, []
        i = 0
        while i < len(args):
            if args[i] == '--gexrt':
                gexrt = args[i + 1]; i += 2
            elif args[i] == '-o':
                out = args[i + 1]; i += 2
            elif args[i] == '--dll':
                imports = args[i + 4].split(',') if i + 4 < len(args) and not args[i + 4].startswith('-') else []
                dlls.append((args[i + 1], args[i + 2], args[i + 3], imports))
                i += 5 if imports else 4
            else:
                sys.exit(f'unknown argument {args[i]}')
        return link(dlls, out, gexrt)
    # --split FILE DLL,...: those DLLs linked as a module of their own, FILE
    # beside OUT.wasm, not into it; its imports of the core's DLLs and of
    # gexrt are the core's exports (pelink exports them).
    while sys.argv[1] == '--split':
        for d in sys.argv[3].split(','):
            SPLIT[d] = sys.argv[2]
        del sys.argv[1:4]
    # --exports-of CORE.wasm: OUT.wasm exports, where it defines them, the
    # names that core of an earlier link exports, so that the --split modules
    # of that link load beside it (their imports bind to the core's exports
    # by name) without linking them again
    while sys.argv[1] == '--exports-of':
        KEEP_EXPORTS.update(wasm_exports(sys.argv[2]))
        del sys.argv[1:3]
    # --extra NAME R.O EXPORTS IMPORTS (a comma list): a DLL built outside
    # the tree (dirk's d3d11.dll and dxgi.dll), linked in, in place of the
    # tree's own of that name.
    extra = []
    while sys.argv[1] == '--extra':
        extra.append((sys.argv[2], sys.argv[3], sys.argv[4], [i for i in sys.argv[5].split(',') if i]))
        del sys.argv[1:6]
    build, src, out = sys.argv[1:4]
    global SRC, BUILD
    SRC = src
    BUILD = build
    WINE64 = WINE64 or makefile_var(os.path.join(build, 'Makefile'), 'HOST_ARCH') == ['wasm64']
    M64 = M64 or (WINE64 and '-mgex-memory64' in (makefile_var(os.path.join(build, 'Makefile'), 'CC') or []))
    only = set(sys.argv[4:]) | (set(SPLIT) if sys.argv[4:] else set())
    # An IMPORTS entry names an import library, which is the module's base
    # name unless its Makefile says otherwise (IMPORTLIB = ntoskrnl for
    # ntoskrnl.exe, winspool for winspool.drv): without this, winedevice's
    # ntoskrnl import was "ntoskrnl.dll", matched no owner, and its
    # ZwLoadDriver bound to ntdll's private one (NtLoadDriver, not
    # implemented), so every driver service failed to start.
    implib = {}
    for d in sorted(os.listdir(os.path.join(src, 'dlls'))):
        mk = os.path.join(src, 'dlls', d, 'Makefile.in')
        m, il = makefile_var(mk, 'MODULE'), makefile_var(mk, 'IMPORTLIB')
        if m and il:
            implib.setdefault(il[0], m[0])
    modname = lambda i: implib.get(i, i if '.' in i else i + '.dll')
    dlls = []  # (dll file name, r.o, exports, imports)
    for top in ('dlls', 'programs'):
        for d in sorted(os.listdir(os.path.join(build, top))):
            mk = os.path.join(src, top, d, 'Makefile.in')
            mod = makefile_var(mk, 'MODULE')
            if not mod:
                continue
            mod = mod[0]
            ro = os.path.join(build, top, d, mod + '.so.r.o')
            ex = os.path.join(build, top, d, mod + '.so.exports')
            if not os.path.exists(ro) or not os.path.exists(ex):
                continue
            # (--exports-of: every DLL's load-time imports, the earlier
            # core's split modules' among them, as that core had them)
            if only and mod not in only and not KEEP_EXPORTS:
                continue
            imports = [i.lstrip('-') for i in makefile_var(mk, 'IMPORTS') + makefile_var(mk, 'DELAYIMPORTS')]
            # (an IMPORTS entry may be a static library instead, uuid: the
            # tree's libs/uuid/libuuid.a, whose objects a spec's `extern`
            # can export)
            STATIC[mod] = [a for i in makefile_var(mk, 'IMPORTS')
                           for a in [os.path.join(build, t, i.lstrip('-'), f'lib{i.lstrip("-")}.a') for t in ('libs', 'dlls')]
                           if os.path.exists(a)]
            imports = [modname(i) for i in imports]
            if not only or mod in only:
                dlls.append((mod, ro, ex, imports))
            # what the loader is to load before the module (makedep's
            # direct imports and default ones: the C runtime, kernel32,
            # ntdll), which a skeleton's empty import table cannot say
            eager = [i.lstrip('-') for i in makefile_var(mk, 'IMPORTS')]
            if '-nodefaultlibs' not in makefile_var(mk, 'EXTRADLLFLAGS'):
                is_crt = lambda n: n.startswith(('msvcr', 'ucrt')) or n == 'crtdll'
                crt = next((i for i in eager if is_crt(i)), None if is_crt(mod.split('.')[0]) else 'ucrtbase')
                eager += [c for c in (crt, 'kernel32', 'ntdll') if c and c not in eager]
            # and its native delay imports, after them (moduleimports.c,
            # below): a native module's calls are bound in the one module,
            # so nothing would load a delay-loaded DLL on first call, and a
            # DLL never loaded never ran its DllMain (wineboot's user32 and
            # gdi32: win32u's syscall table missing, every NtUser call
            # STATUS_INVALID_SYSTEM_SERVICE)
            EAGER[mod] = [modname(i) for i in eager]
            DELAY[mod] = [modname(i.lstrip('-')) for i in makefile_var(mk, 'DELAYIMPORTS')]
    extra_names = {e[0] for e in extra}
    dlls = [d for d in dlls if d[0] not in extra_names] + extra
    for mod, ro, ex, imports in extra:
        EAGER[mod] = list(imports)
        DELAY[mod] = []
    if not dlls:
        sys.exit('no DLL objects found (build with winegcc for wasm32)')
    if WINE64 and only - {d[0] for d in dlls}:
        # (WINE64 names its set, stage-native64.sh: each one is wanted)
        sys.exit('no objects for ' + ' '.join(sorted(only - {d[0] for d in dlls})) + ' (make their wasm64-windows DLLs first)')
    # every DLL the build has an export list for, native or not: what an
    # import the linked DLLs leave undefined is found in (guest imports)
    others = {}
    spec_dir = tempfile.mkdtemp(prefix='pelink-specs-')
    for top in ('dlls',):
        for d in sorted(os.listdir(os.path.join(build, top))):
            mod = makefile_var(os.path.join(src, top, d, 'Makefile.in'), 'MODULE')
            ex = os.path.join(build, top, d, mod[0] + '.so.exports') if mod else ''
            if mod and os.path.exists(ex):
                others[mod[0]] = ex
            elif mod:
                # a DLL whose wasm32 build failed is still an i386 one in
                # the prefix: its .spec names its functions
                names = spec_functions(spec_path(src, top, d, mod[0]))
                if names:
                    ex = os.path.join(spec_dir, mod[0] + '.exports')
                    with open(ex, 'w') as f:
                        f.writelines(f'{n} func\n' for n in names)
                    others[mod[0]] = ex
    for mod, ro, ex, imports in extra:
        others[mod] = ex
    link(dlls, out, os.path.join(build, 'libs', 'gexrt', 'libgexrt.a'), others)


def leb_at(b, i):
    """An unsigned LEB128 at b[i]: (value, the index after it)."""
    r = sh = 0
    while True:
        x = b[i]
        i += 1
        r |= (x & 0x7f) << sh
        sh += 7
        if not x & 0x80:
            return r, i


def wasm_exports(path):
    """The names a wasm module exports."""
    b = open(path, 'rb').read()
    out, i = [], 8
    while i < len(b):
        sid = b[i]
        size, j = leb_at(b, i + 1)
        if sid == 7:
            cnt, k = leb_at(b, j)
            for _ in range(cnt):
                n, k = leb_at(b, k)
                out.append(b[k:k + n].decode())
                k += n + 1
                _, k = leb_at(b, k)
        i = j + size
    return out


def wasm_sig64(path):
    """A wasm module's gexw.sig64 lines (GexIcall's signatures of what it
    defines, bN a struct of N bytes by value) as {name: signature}."""
    try:
        b = open(path, 'rb').read()
    except OSError:
        return {}
    i = 8
    while i < len(b):
        sid = b[i]
        size, j = leb_at(b, i + 1)
        if sid == 0:
            n, k = leb_at(b, j)
            if b[k:k + n] == b'gexw.sig64':
                return dict(l.split(' ', 1) for l in b[k + n:j + size].decode().splitlines() if ' ' in l)
        i = j + size
    return {}


def sig_params(sig):
    """A gexw.sig64 signature's parameters as (kind, size) pairs."""
    out, ps = [], sig.split(':', 1)[1] if ':' in sig else ''
    i = 0
    while i < len(ps) and ps[i] != '.':
        k = ps[i]
        i += 1
        n = ''
        while k in 'bs' and i < len(ps) and ps[i].isdigit():
            n += ps[i]
            i += 1
        out.append((k, int(n or 0)))
    return out


def check_adapters(dlls):
    """winebuild's adapters made without their callee's signature (an
    export a DLL takes from another, its .adapters file: each parameter `v`
    a value, `r` a struct by value through its slots' address) against the
    defining DLL's gexw.sig64. A struct by value taken as a value passes it
    as an address: unicows' COORDs to kernelbase's console functions were
    read from addresses made of them. Such an export is to be the defining
    DLL's, a forward in its .spec; a mismatch fails the link."""
    sig64 = {}
    def defs(mod, ro):
        if mod not in sig64:
            sig64[mod] = wasm_sig64(ro)
        return sig64[mod]
    bad = []
    for mod, ro, ex, imports in dlls:
        ad = ex[:-len('.exports')] + '.adapters'
        if not ex.endswith('.exports') or not os.path.exists(ad):
            continue
        for line in open(ad):
            parts = line.split()
            if len(parts) != 3:
                continue
            name, link_name, mask = parts
            mask = '' if mask == '-' else mask
            owners = [d for d in dlls if d[0] != mod and link_name in defs(d[0], d[1])]
            owners.sort(key=lambda d: d[0] not in imports)
            if not owners:
                continue
            owner = owners[0][0]
            for j, (k, n) in enumerate(sig_params(sig64[owner][link_name])):
                if j >= len(mask) or mask[j] not in 'vr':
                    continue
                # (x86-64: one of 1, 2, 4 or 8 bytes in its slot, any other
                # by reference, a pointer as the adapter passes it; i386:
                # every one in its words)
                byval = k == 'b' and (n in (1, 2, 4, 8) or not WINE64)
                if byval != (mask[j] == 'r'):
                    bad.append(f'{mod}!{name}: parameter {j} is {owner}\'s {k}{n or ""} ({sig64[owner][link_name]}), '
                               f'its adapter takes it as {"a struct" if mask[j] == "r" else "a value"}')
    if bad:
        sys.exit('pelink: adapters that disagree with their callee (make each export a forward to the '
                 'defining DLL in its .spec):\n  ' + '\n  '.join(bad))


def dll_slots(path, mapfile, obj_dll):
    """Which DLL each of a linked module's table slots and data bytes is,
    for gex to find the module of an address (GetModuleHandleExW's
    FROM_ADDRESS): a function pointer is its table slot, a static
    variable's address is in the module's data, neither in its skeleton.
    From wasm-ld's map of the link (each function and data chunk's input
    object; OBJ_DLL names the DLL of an object) and the module's element
    segment (slot -> function). Lines `dll f LO HI` (slots past
    __table_base) and `dll d LO HI` (bytes past __memory_base), HI
    exclusive, ranges merged."""
    b = open(path, 'rb').read()
    i, nimp, elems = 8, 0, []
    while i < len(b):
        sid, i = b[i], i + 1
        size, i = leb_at(b, i)
        end = i + size
        if sid == 2:
            n, j = leb_at(b, i)
            for _ in range(n):
                for _ in range(2):
                    ln, j = leb_at(b, j)
                    j += ln
                kind, j = b[j], j + 1
                if kind == 0:
                    nimp += 1
                    _, j = leb_at(b, j)
                elif kind == 1:
                    j += 1
                    fl, j = leb_at(b, j)
                    _, j = leb_at(b, j)
                    if fl & 1:
                        _, j = leb_at(b, j)
                elif kind == 2:
                    fl, j = leb_at(b, j)
                    _, j = leb_at(b, j)
                    if fl & 1:
                        _, j = leb_at(b, j)
                elif kind == 3:
                    j += 2
                else:
                    # a tag (C++ exceptions): its attribute, its type
                    _, j = leb_at(b, j + 1)
        elif sid == 9:
            n, j = leb_at(b, i)
            for _ in range(n):
                fl, j = leb_at(b, j)
                if fl != 0:
                    sys.exit(f'pelink: {path}: element segment flags {fl}: only the active function list is read')
                while b[j] != 0x0b:
                    j += 1
                c, j = leb_at(b, j + 1)
                for _ in range(c):
                    f, j = leb_at(b, j)
                    elems.append(f)
        i = end
    funcs, data, out_sec = [], [], ''
    for line in open(mapfile):
        # the Out, In and Symbol columns: one name, indented by the column
        m = re.match(r'\s*(\S+)\s+(\S+)\s+(\S+) (\s*)(\S+)$', line)
        if not m:
            continue
        t, col = m.groups(), len(m.group(4))
        if col < 4:
            out_sec = t[4]
        elif col < 12 and ':(' in t[4]:
            obj = t[4].rsplit(':(', 1)[0]
            if out_sec == 'CODE':
                funcs.append(obj_dll.get(obj))
            elif out_sec.startswith('.') and not out_sec.startswith(('.tdata', '.tbss')) and t[0] != '-':
                data.append((int(t[0], 16), int(t[2], 16), obj_dll.get(obj)))
    ranges = {}

    def add(dll, kind, lo, hi):
        if not dll or hi <= lo:
            return
        rs = ranges.setdefault((dll, kind), [])
        if rs and rs[-1][1] == lo:
            rs[-1][1] = hi
        else:
            rs.append([lo, hi])
    for k, f in enumerate(elems):
        add(funcs[f - nimp] if 0 <= f - nimp < len(funcs) else None, 'f', k, k + 1)
    for a, n, dll in sorted(data):
        add(dll, 'd', a, a + n)
    return ''.join(f'{dll} {kind} {lo:x} {hi:x}\n' for (dll, kind), rs in sorted(ranges.items()) for lo, hi in rs)


def name_imports(path):
    """A --split module's function imports named by their symbols: wasm-ld
    keeps an undefined symbol's original import name (env.strlen) though
    pesym renamed the symbol (ucrtbase.dll!strlen); the name section has the
    symbol's. gex binds each import to the core export of that name."""
    b = open(path, 'rb').read()
    secs, i = [], 8
    while i < len(b):
        sid = b[i]
        size, j = leb_at(b, i + 1)
        secs.append((sid, b[j:j + size]))
        i = j + size
    names = {}
    for sid, body in secs:
        if sid == 0:
            n, j = leb_at(body, 0)
            if body[j:j + n] != b'name':
                continue
            j += n
            while j < len(body):
                kind = body[j]
                size, k = leb_at(body, j + 1)
                if kind == 1:
                    cnt, m = leb_at(body, k)
                    for _ in range(cnt):
                        idx, m = leb_at(body, m)
                        ln, m = leb_at(body, m)
                        names[idx] = body[m:m + ln].decode()
                        m += ln
                j = k + size
    out = bytearray(b[:8])
    for sid, body in secs:
        if sid == 2:
            cnt, j = leb_at(body, 0)
            new = bytearray(enc_leb(cnt))
            func = 0
            for _ in range(cnt):
                ml, k = leb_at(body, j)
                mod = body[k:k + ml]
                k += ml
                fl, k2 = leb_at(body, k)
                field = body[k2:k2 + fl]
                k = k2 + fl
                kind = body[k]
                start = k
                k += 1
                if kind == 0:
                    _, k = leb_at(body, k)
                elif kind == 1:
                    k += 1
                    flags, k = leb_at(body, k)
                    _, k = leb_at(body, k)
                    if flags & 1:
                        _, k = leb_at(body, k)
                elif kind == 2:
                    flags, k = leb_at(body, k)
                    _, k = leb_at(body, k)
                    if flags & 1:
                        _, k = leb_at(body, k)
                elif kind == 3:
                    k += 2
                elif kind == 4:
                    k += 1
                    _, k = leb_at(body, k)
                if kind == 0:
                    sym = names.get(func, '')
                    if mod == b'env' and '!' in sym:
                        field = sym.encode()
                    func += 1
                new += enc_leb(len(mod)) + mod + enc_leb(len(field)) + field + body[start:k]
                j = k
            body = bytes(new)
        out += bytes([sid]) + enc_leb(len(body)) + body
    open(path, 'wb').write(out)


def with_static_exports(dlls):
    """`dlls` with each DLL's object holding the exports its own code does
    not define but a static library it imports does: a spec's `extern
    IID_IRichEditOle`, the data that libuuid.a's object defines (msftedit).
    A DLL's object is its own code alone, so the static library's members
    came in only at the module's link, under their C names, never
    DLL!name, and the export was missing. The members defining them are
    linked into the DLL's object (`wasm-ld -r`), as the C runtime's are
    into the core's, so pesym.py makes them the DLL's exports and the rest
    of each member local to it."""
    out = []
    for mod, ro, ex, imports in dlls:
        libs = STATIC.get(mod, [])
        names = [l.split()[0] for l in open(ex) if l.split() and 'forward' not in l.split()[1:2]] if libs else []
        defined = set(subprocess.run(['llvm-nm', '--defined-only', '--format=just-symbols', ro],
                                     capture_output=True, text=True).stdout.split()) if names else set()
        want = [n for n in names if n not in defined and not n.startswith('__gexw_')]
        members = []  # (library, member)
        for lib in libs if want else ():
            listing = subprocess.run(['llvm-nm', '--defined-only', '--print-file-name', lib],
                                     capture_output=True, text=True).stdout
            # (`libuuid.a:uuid.o: 00005050 D IID_IRichEditOle`)
            at = {l.split()[-1]: l.split()[0].split(':')[-2] for l in listing.splitlines()
                  if len(l.split()) >= 3 and l.split()[-2] in 'TDRB'}
            for n in [n for n in want if n in at]:
                if (lib, at[n]) not in members:
                    members.append((lib, at[n]))
                want.remove(n)
        if not members:
            out.append((mod, ro, ex, imports))
            continue
        d = tempfile.mkdtemp(prefix='pelink-static-')
        atexit.register(shutil.rmtree, d, True)
        objs = []
        for k, (lib, m) in enumerate(members):
            sub = os.path.join(d, str(k))
            os.mkdir(sub)
            subprocess.check_call(['llvm-ar', 'x', '--output', sub, lib, m])
            objs.append(os.path.join(sub, m))
        merged = os.path.join(d, mod + '.r.o')
        subprocess.check_call(['wasm-ld'] + (['-mwasm64'] if WINE64 else []) + ['-r', '-o', merged, ro] + objs)
        out.append((mod, merged, ex, imports))
    return out


def link(dlls, out, gexrt, others=None):

    exporters = {}  # link name -> [dll, ...]
    forwards = {}  # export name -> {dll: 'TARGET.name'}
    data = []
    for mod, ro, ex, _ in dlls:
        for line in open(ex):
            parts = line.split()
            if not parts:
                continue
            if len(parts) > 2 and parts[1] == 'forward':
                forwards.setdefault(parts[0], {})[mod] = parts[2]
                continue
            exporters.setdefault(parts[0], []).append(mod)
            if len(parts) > 1 and parts[1] == 'data':
                data.append(f'{mod}!{parts[0]}')

    # (--exports-of: the earlier core's exports that are forwards of DLLs
    # not linked here, user32's DefWindowProcW to ntdll's
    # NtdllDefWindowProc_W, are aliases this core defines too)
    linked = {m for m, _, _, _ in dlls}
    for via, ex in sorted((others or {}).items()) if KEEP_EXPORTS else ():
        if via in linked:
            continue
        for line in open(ex):
            parts = line.split()
            if len(parts) > 2 and parts[1] == 'forward' and f'{via}!{parts[0]}' in KEEP_EXPORTS:
                forwards.setdefault(parts[0], {})[via] = parts[2]

    dlls = with_static_exports(dlls)
    check_adapters(dlls)
    sigs = {}  # dll -> {(name, 'defined'|'undefined'): sig}
    for mod, ro, ex, imports in dlls:
        listing = subprocess.run([sys.executable, os.path.join(here, 'pesym.py'), 'sigs', ro],
                                 capture_output=True, text=True, check=True).stdout
        sigs[mod] = {(l.split()[0], l.split()[1]): l.split()[2] for l in listing.splitlines() if len(l.split()) == 3}
    # C runtime functions a DLL's code calls by their C names that no .spec
    # exports (Wine's i386 msvcrt and ucrtbase export sinf, atan2f and the
    # other float functions on 64-bit only, and the compiler's own calls are
    # among them): the core DLL that defines one exports it, so a --split
    # module's call is bound to it rather than left an import nothing provides.
    rt_exports = {}  # core dll -> [C names it exports beside its .spec's]
    core_defs = {}
    # (a program in the core provides none: services.exe's __main_argc_argv
    # bound a split notepad.exe's weak main to it, and notepad ran services)
    for mod, ro, ex, _ in sorted(dlls, key=lambda d: ('ucrtbase.dll', 'msvcrt.dll').index(d[0])
                                 if d[0] in ('ucrtbase.dll', 'msvcrt.dll') else 2):
        if not SPLIT.get(mod) and not mod.endswith('.exe'):
            for (n, k) in sigs[mod]:
                if k == 'defined' and n not in WEAK_IMPORTS:
                    core_defs.setdefault(n, mod)
    # (one the core's DLLs do not define either, ceilf or trunc: the C
    # runtime's own library, libs/musl, which Wine links into msvcrt and
    # ucrtbase, has it: its objects, with the members they need in turn, are
    # linked into the core's C runtime DLL, whose own math_error and _fdclass
    # they call, and that DLL exports its C name)
    # (and with --exports-of, the C names the earlier core's DLLs exported:
    # its split modules call them, though no DLL linked here does)
    core_mods = {m for m, _, _, _ in dlls if not SPLIT.get(m)}
    kept = {tuple(reversed(n.split('!', 1))) for n in KEEP_EXPORTS
            if '!' in n and n.split('!', 1)[0] in core_mods}
    musl = os.path.join(BUILD or '', 'libs', 'musl', 'libmusl.a')
    ro_own = {}
    crt = next((d for d in ('ucrtbase.dll', 'msvcrt.dll') if any(m == d and not SPLIT.get(m) for m, _, _, _ in dlls)), None)
    if crt and os.path.exists(musl):
        lib = subprocess.run(['llvm-nm', '--defined-only', '--print-file-name', musl], capture_output=True, text=True).stdout
        # (`libmusl.a:ceilf.o: 00000001 T ceilf`)
        in_lib = {l.split()[-1]: l.split()[0].split(':')[-2] for l in lib.splitlines()
                  if len(l.split()) >= 3 and l.split()[-2] == 'T'}
        want = sorted({n for mod, _, _, _ in dlls for (n, k) in sigs[mod]
                       if k == 'undefined' and n in in_lib and n not in core_defs
                       and (n, 'defined') not in sigs[mod]}
                      | {n for n, mod in kept if mod == crt and n in in_lib and n not in core_defs})
        if want:
            crt_tmp = tempfile.mkdtemp(prefix='pelink-crt-')
            members = sorted({in_lib[n] for n in want})
            subprocess.check_call(['llvm-ar', 'x', '--output', crt_tmp, musl] + members)
            ro_crt = next(ro for m, ro, _, _ in dlls if m == crt)
            merged = os.path.join(crt_tmp, crt + '.r.o')
            # (the archive after them: the members they need, and only those)
            subprocess.check_call(['wasm-ld'] + (['-mwasm64'] if WINE64 else []) + ['-r', '-o', merged, ro_crt]
                                  + [os.path.join(crt_tmp, m) for m in members] + [musl])
            dlls = [(m, merged if m == crt else ro, ex, imp) for m, ro, ex, imp in dlls]
            # (the link drops what only an export names, rewind_preserve_stack:
            # what nothing defines is found in the DLL's own object)
            ro_own[crt] = ro_crt
            listing = subprocess.run([sys.executable, os.path.join(here, 'pesym.py'), 'sigs', merged],
                                     capture_output=True, text=True, check=True).stdout
            sigs[crt] = {(l.split()[0], l.split()[1]): l.split()[2] for l in listing.splitlines() if len(l.split()) == 3}
            for n in want:
                if (n, 'defined') in sigs[crt]:
                    core_defs[n] = crt
    for mod, ro, ex, _ in dlls:
        for (n, k) in sigs[mod]:
            owner = core_defs.get(n)
            if k == 'undefined' and owner and owner != mod and n not in exporters and n not in forwards \
                    and (n, 'defined') not in sigs[mod]:
                exporters[n] = [owner]
                rt_exports.setdefault(owner, []).append(n)
    for n, mod in sorted(kept):
        if core_defs.get(n) == mod and n not in exporters and n not in forwards:
            exporters[n] = [mod]
            rt_exports.setdefault(mod, []).append(n)
    def resolve_forward(fwd, depth=0):
        """(dll, name) a forward like NTDLL.RtlEnterCriticalSection reaches."""
        dll, _, name = fwd.partition('.')
        dll = dll.lower() + ('' if '.' in dll else '.dll')
        if name.startswith('#') or depth > 8:
            return None
        if dll in exporters.get(name, []) and (name, 'defined') in sigs.get(dll, {}):
            return dll, name
        nxt = forwards.get(name, {}).get(dll)
        if nxt:
            return resolve_forward(nxt, depth + 1)
        # a name the DLL exports but takes from another (kernel32's
        # RtlSetLastWin32Error is ntdll's): whoever defines it
        owner = next((d for d in exporters.get(name, []) if (name, 'defined') in sigs.get(d, {})), None)
        return (owner, name) if owner else None

    # Forwards some DLL's code calls by name: the forwarding DLL's name for
    # the function, an alias the defining DLL gets (pesym.py dll ALIASES).
    aliases = {}  # defining dll -> [(alias, name there)]
    forward_owner = {}  # export name -> forwarding dll (the name is bound to dll!name)
    unresolved = []  # (export name, forwarding dll, forward) no native DLL defines
    # A name some DLL defines and another forwards elsewhere (unicows' own
    # DefWindowProcW, user32's forward to ntdll's NtdllDefWindowProc_W) is
    # bound by the forward where the module imports the forwarding DLL
    # first (fwd_first, below; gdi32's GetCharWidthW, its own GetCharWidth32W,
    # and unicows' GetCharWidthW too); forwards to one of the defining DLLs
    # are left to the direct binding.
    shadowed = {}  # export name -> {forwarding dll}
    elsewhere = {}  # export name -> {forwarding dll} whose forward reaches none of its definitions
    for name, by in forwards.items():
        for via, fwd in by.items():
            hit = resolve_forward(fwd)
            if name in exporters:
                if hit and hit[0] in exporters[name]:
                    continue
                elsewhere.setdefault(name, set()).add(via)
                if hit:
                    shadowed.setdefault(name, set()).add(via)
            if hit:
                aliases.setdefault(hit[0], []).append((f'{via}!{name}', hit[1]))
                forward_owner.setdefault(name, [])
                forward_owner[name].append(via)
            else:
                unresolved.append((name, via, fwd))

    tmp = tempfile.mkdtemp(prefix='pelink-')
    # Each module's objects: the core's (''), and a --split module's by its file name.
    objs = {'': []}
    obj_dll = {}  # a DLL's object -> the DLL (dll_slots)
    grp = lambda m: SPLIT.get(m, '')
    for g in set(SPLIT.values()):
        objs[g] = []

    # Signatures: a caller of a forward declaring the function otherwise
    # than its definition (kernel32's void EnterCriticalSection is ntdll's
    # NTSTATUS RtlEnterCriticalSection) goes through a thunk of its own
    # (below): wasm calls must match exactly, and two callers may disagree
    # (psapi's adapter from the .spec takes words where iphlpapi's
    # K32GetModuleFileNameExW takes pointers).
    thunks = []
    fwd_def = {}  # alias -> (its definition dll!name, that signature)
    for d in aliases:
        for alias, target in aliases[d]:
            fwd_def[alias] = (f'{d}!{target}', sigs[d].get((target, 'defined')))
    # Exports of the DLLs that stay i386 code, for imports a native module
    # takes from one of them (below, and the stubs further down).
    native = {mod for mod, _, _, _ in dlls}
    owner_of = {}
    for omod, oex in (others or {}).items():
        if omod in native:
            continue
        for line in open(oex):
            parts = line.split()
            if len(parts) >= 2 and parts[1] in ('func', 'forward'):
                owner_of.setdefault(parts[0], []).append(omod)
    # A forward to what the native DLL itself takes from an i386 one
    # (user32's BeginPaint is win32u's NtUserBeginPaint): that DLL's export,
    # through a guest stub.
    guest_forward = {}  # export name -> [(forwarding dll, i386 dll, its export)]
    for name, via, fwd in unresolved:
        tdll, _, tname = fwd.partition('.')
        tdll = tdll.lower() + ('' if '.' in tdll else '.dll')
        gdll = next((o for o in owner_of.get(tname, ()) if o != tdll), None)
        if tdll in native and gdll:
            guest_forward.setdefault(name, []).append((via, gdll, tname))
        # a forward into a DLL that stays i386 (a native kernel32's HeapAlloc
        # is ntdll's RtlAllocateHeap, ntdll translated): its stub calls that
        elif tdll not in native and tdll in owner_of.get(tname, ()):
            guest_forward.setdefault(name, []).append((via, tdll, tname))
    guest_binds = []  # (symbol, sig, i386 owner, export name, group, importing DLL)
    guest_sig = {}  # (group, dll.name) -> the signature its stub has
    def guest_sym(mod, sym, have):
        """The stub's symbol for `sym` as `mod` declares it: two DLLs in one
        group declaring an import differently (user32's and gdi32's
        NtGdiCreateHalftonePalette) each get a stub of their own."""
        first = guest_sig.setdefault((grp(mod), sym), have)
        return sym if first == have else f'{sym}~{have.replace(":", "_")}'
    own_cache = {}
    def own_defs(o):
        if o not in own_cache:
            own_cache[o] = spec_own(owner_spec(SRC, o)) if SRC else set()
        return own_cache[o]
    def bind_guest(f, mod, imports, name, pick):
        """An i386 DLL the module imports explicitly, ahead of the native
        owner `pick`, is the one `name` comes from, as an import library's
        link order has it (winedevice's ZwLoadDriver is ntoskrnl.exe's, not
        ntdll's private alias of NtLoadDriver, which is not implemented:
        every driver service failed to start). Binds it to a guest stub."""
        guest = next((o for o in imports if o in owner_of.get(name, ()) and name in own_defs(o)), None)
        have = sigs[mod].get((name, 'undefined'))
        if not guest or not have or (pick in imports and imports.index(pick) < imports.index(guest)):
            return False
        sym = guest_sym(mod, f'{guest}.{name}', have)
        guest_binds.append((f'gexw-guest!{sym}', have, guest, name, grp(mod), mod))
        f.write(f'{name} gexw-guest {sym}\n')
        return True

    # A native DLL that a module only delay-imports (its Makefile.in's
    # DELAYIMPORTS, none of its static imports needing it either) is loaded on
    # the module's first call into it, as Windows' delay-load helper loads it:
    # the call goes through a thunk that loads the DLL once (`gexw-thunk`,
    # below). Loaded with the module instead, every delay import's DllMain ran
    # at its start (explorer's shell32, shdocvw and ieframe: ieframe's window
    # class needed the display driver before explorer had loaded it) and a
    # process loaded its delay imports' whole closure.
    static_below = {}
    def static_closure(m):
        if m not in static_below:
            seen, todo = set(), [d for d in EAGER.get(m, []) if d != m]
            while todo:
                d = todo.pop()
                if d not in seen:
                    seen.add(d)
                    todo += EAGER.get(d, [])
            static_below[m] = seen
        return static_below[m]
    native_set = {m for m, _, _, _ in dlls}
    # (Not one an activation context redirects: a DLL's LoadLibrary of
    # comctl32 may load comctl32_v6, and the calls bound to comctl32's own
    # module then reached one never installed.)
    def lazy_delay(mod, pick):
        return (pick != mod and pick in DELAY.get(mod, []) and pick in native_set
                and pick not in ACTCTX_DLLS and pick not in static_closure(mod))
    delay_eager = {}  # module -> delay imports it still loads first (a data import, an unknown signature)
    direct_refs = {}  # group -> the DLLs its code is bound to directly
    # group -> the DLLs its delay thunks call into that the delay import
    # forwards to (rpcrt4's CoTaskMemAlloc: a delay import of ole32's, which
    # combase defines): loaded with the delay import, never ahead of the
    # group, so marked as delayed too unless its code binds them directly.
    # Bound directly, they installed ahead of it with their own static
    # imports: rpcrt4 brought combase, ole32, user32, gdi32, win32u and
    # coml2 into every process that loads it (sc, services, winedevice).
    delay_targets = {}
    delay_marks = {}  # group -> (the DLLs marked delayed, the DLLs its delay thunks load)

    def fwd_first(mod, imports, name):
        """`name` is the module's through a forwarding DLL it imports ahead
        of every DLL defining it, or through a forward when it imports
        neither."""
        vias = shadowed.get(name)
        # (an export of the module's that it takes from another DLL is not
        # its own: unicows' lstrlenW, kernelbase's)
        if not vias or mod in vias or (mod in exporters[name] and (name, 'defined') in sigs[mod]):
            return False
        first = next((o for o in EAGER.get(mod, []) + imports
                      if o in vias or o in exporters[name]), None)
        # (none of them: the forward's definition rather than whichever DLL
        # defines the name first, xaudio2's lstrcpynW kernelbase's, not
        # unicows')
        return first is None or first in vias

    for mod, ro, ex, imports in dlls:
        local = os.path.join(tmp, mod + '.l.o')
        alias_file = os.path.join(tmp, mod + '.aliases')
        with open(alias_file, 'w') as f:
            for a, n in aliases.get(mod, []):
                f.write(f'{a} {n}\n')
        if rt_exports.get(mod):
            ex_rt = os.path.join(tmp, mod + '.exports')
            with open(ex_rt, 'w') as f:
                f.write(open(ex).read())
                f.writelines(f'{n} func\n' for n in sorted(set(rt_exports[mod])))
            ex = ex_rt
        subprocess.check_call([sys.executable, os.path.join(here, 'pesym.py'), 'dll', ro, local, mod, ex, alias_file])
        mapfile = os.path.join(tmp, mod + '.map')
        with open(mapfile, 'w') as f:
            # its entry point's binding of its static guest imports (winebuild)
            f.write(f'wine_gexw_bind_imports gexw-bind {mod}\n')
            for name, owners in exporters.items():
                if fwd_first(mod, imports, name):
                    continue  # bound by its forward, below
                # (a DLL may list an export its code takes from another DLL:
                # kernel32's DebugBreak is kernelbase's)
                defs = [o for o in owners if (name, 'defined') in sigs[o]]
                # listed but defined by none of them: an export that is the
                # DLL's own import from an i386 DLL (user32's
                # NtUserRegisterTouchPadCapable, win32u's), bound there
                gown = owner_of.get(name, ())
                have = sigs[mod].get((name, 'undefined'))
                if not defs and have and gown:
                    gdll = next((o for o in imports if o in gown), gown[0])
                    sym = guest_sym(mod, f'{gdll}.{name}', have)
                    guest_binds.append((f'gexw-guest!{sym}', have, gdll, name, grp(mod), mod))
                    f.write(f'{name} gexw-guest {sym}\n')
                    continue
                owners = defs or owners
                if mod in owners and (name, 'defined') not in sigs[mod]:
                    # an export it takes from another DLL (unicows' lstrlenW):
                    # bound to that DLL's, or by its forward below
                    owners = [o for o in owners if o != mod]
                    if not owners:
                        continue
                if mod in owners:
                    continue  # its own: already resolved inside it
                # (its own C runtime first, which its import list does not
                # name: ucrtbase's _wopen and msvcrt's _fstat32 are two fd
                # tables, and wineboot's fstat of a file it had just opened
                # failed)
                pick = next((o for o in EAGER.get(mod, []) + imports if o in owners), owners[0])
                # (none of the DLLs it imports defines it, and one forwards it
                # to an i386 DLL: bound through that, below, as when no native
                # DLL defined the name; combase's PostThreadMessageW is
                # user32's forward to win32u, not unicows' own)
                if pick not in EAGER.get(mod, []) + imports and any(
                        v in EAGER.get(mod, []) + imports for v in elsewhere.get(name, ())):
                    continue
                # An i386 DLL the module imports explicitly, ahead of the
                # native owner, is the one the name comes from, as an import
                # library's link order has it (winedevice's ZwLoadDriver is
                # ntoskrnl.exe's, not ntdll's private NtLoadDriver alias).
                if bind_guest(f, mod, imports, name, pick):
                    continue
                # A program's comctl32 is the one its manifest's activation
                # context names (comctl32_v6 for notepad), which its loader
                # binds: a call bound to comctl32 itself reached a copy that
                # was never attached in that process (its subclass atom 0).
                # So is a DLL's (shell32's PropertySheetW, in a process whose
                # context names comctl32_v6, found its resources in the
                # program's image: the copy's module handle 0).
                if pick in ACTCTX_DLLS and mod not in ACTCTX_DLLS and mod not in LOAD_NAME and have:
                    sym = guest_sym(mod, f'{pick}.{name}', have)
                    guest_binds.append((f'gexw-guest!{sym}', have, pick, name, grp(mod), mod))
                    f.write(f'{name} gexw-guest {sym}\n')
                    continue
                # a caller declaring the function otherwise than its
                # definition (an adapter from the .spec: user32's
                # NotifyWinEvent is void, win32u's stub returns a word) goes
                # through a thunk: wasm calls must match exactly
                have, want = sigs[mod].get((name, 'undefined')), sigs[pick].get((name, 'defined'))
                used = have or f'{pick}!{name}' in data  # (not an export the module does not call)
                # the delay import the name is the module's through: pick, or
                # one that forwards it to pick (imm32's CoRegisterInitializeSpy
                # is ole32's, defined in combase: ole32 is still the DLL loaded,
                # and imm32 then finds it by GetModuleHandle)
                via = pick if lazy_delay(mod, pick) else None
                if used and not via and pick not in EAGER.get(mod, []):
                    via = next((d for d in DELAY.get(mod, []) if lazy_delay(mod, d)
                                and (d in exporters.get(name, ()) or d in forwards.get(name, {}))), None)
                if used and via:
                    if have and want:
                        thunk = f'{mod}.{name}'
                        thunks.append((f'gexw-thunk!{thunk}', have, f'{pick}!{name}', want, grp(mod), via))
                        f.write(f'{name} gexw-thunk {thunk}\n')
                        if via != pick:
                            delay_targets.setdefault(grp(mod), set()).add(pick)
                        continue
                    if os.environ.get('PELINK_DELAY_DEBUG'):
                        print(f'pelink: {mod} loads {via} first: {name} ({have} -> {want})', file=sys.stderr)
                    delay_eager.setdefault(mod, set()).add(via)
                if used:
                    direct_refs.setdefault(grp(mod), set()).add(pick)
                if have and want and have != want:
                    thunk = f'{mod}.{name}'
                    thunks.append((f'gexw-thunk!{thunk}', have, f'{pick}!{name}', want, grp(mod), None))
                    f.write(f'{name} gexw-thunk {thunk}\n')
                    continue
                f.write(f'{name} {pick}\n')
            # names only forwarded (kernel32's EnterCriticalSection is
            # ntdll's RtlEnterCriticalSection, under kernel32.dll!EnterCriticalSection)
            for name, vias in forward_owner.items():
                if name in exporters and not fwd_first(mod, imports, name):
                    continue  # bound to its definition, above
                # (none it imports: its default C runtime, else the core's,
                # before a module of its own; user32's sqrt, which msvcrt,
                # ucrtbase and crtdll all forward, was crtdll's, and every
                # process loading user32 installed crtdll)
                core_first = sorted(vias, key=lambda o: (bool(SPLIT.get(o)), o not in ('ucrtbase.dll', 'msvcrt.dll')))
                pick = next((o for o in [mod] + imports + EAGER.get(mod, []) if o in vias), core_first[0])
                if bind_guest(f, mod, imports, name, pick):
                    continue
                target, want = fwd_def.get(f'{pick}!{name}', (None, None))
                have = sigs[mod].get((name, 'undefined'))
                used = have or f'{pick}!{name}' in data  # (not an export the module does not call)
                if used and lazy_delay(mod, pick):
                    if have and want and target:
                        thunk = f'{mod}.{name}'
                        thunks.append((f'gexw-thunk!{thunk}', have, target, want, grp(mod), pick))
                        f.write(f'{name} gexw-thunk {thunk}\n')
                        if target.split('!', 1)[0] != pick:
                            delay_targets.setdefault(grp(mod), set()).add(target.split('!', 1)[0])
                        continue
                    if os.environ.get('PELINK_DELAY_DEBUG'):
                        print(f'pelink: {mod} loads {pick} first: {name} ({have} -> {target} {want})', file=sys.stderr)
                    delay_eager.setdefault(mod, set()).add(pick)
                if used:
                    direct_refs.setdefault(grp(mod), set()).update({pick} | ({target.split('!', 1)[0]} if target else set()))
                if have and want and have != want:
                    thunk = f'{mod}.{name}'
                    thunks.append((f'gexw-thunk!{thunk}', have, target, want, grp(mod), None))
                    f.write(f'{name} gexw-thunk {thunk}\n')
                    continue
                f.write(f'{name} {pick}\n')
            for name, cands in guest_forward.items():
                have = sigs[mod].get((name, 'undefined'))
                if not have or name in forward_owner:
                    continue
                via, gdll, gname = next((c for c in cands if c[0] in imports), cands[0])
                # (named as the importer names it: wasm-ld refuses one
                # symbol imported under two names, comctl32's
                # CreateCompatibleDC and gdi32's NtGdiCreateCompatibleDC)
                sym = guest_sym(mod, f'{gdll}.{gname}' + ('' if name == gname else f'={name}'), have)
                guest_binds.append((f'gexw-guest!{sym}', have, gdll, gname, grp(mod), mod))
                f.write(f'{name} gexw-guest {sym}\n')
        bound = os.path.join(tmp, mod + '.o')
        subprocess.check_call([sys.executable, os.path.join(here, 'pesym.py'), 'imports', local, bound, mod, mapfile])
        # The ICALL pass's stdcall annotations are for the compiler only;
        # wasm-ld 22 crashes relocating them when a function they name is
        # not linked (services.exe, plugplay.exe).
        subprocess.check_call(['llvm-objcopy', '--remove-section=llvm.func_attr.annotate.gex.stdcall',
                               '--remove-section=reloc.llvm.func_attr.annotate.gex.stdcall', bound])
        objs[grp(mod)].append(bound)
        obj_dll[bound] = mod

    for g in objs:
      gthunks = [t[:4] + (t[5],) for t in thunks if t[4] == g]
      if gthunks:
          ctype = {'i': 'int', 'I': 'long long', 'f': 'float', 'd': 'double', 'v': 'void'}
          src = os.path.join(tmp, f'forwards{g}.c')
          loads = {t[0]: delay_loads(t[4], t[2], static_closure) for t in gthunks if t[4]}
          delayed = sorted({d for ds in loads.values() for d in ds})
          with open(src, 'w') as f:
              if delayed:
                  # A delay import's first call: the DLL loaded (and attached)
                  # once, the flag set after it with a release store, read with
                  # an acquire load (two threads' first calls load it twice:
                  # a reference more). A DLL that fails to load raises the
                  # delay-load helper's exception, VcppException(ERROR_SEVERITY_ERROR,
                  # ERROR_MOD_NOT_FOUND), not a call through nothing. (Its
                  # functions are bound when linked: no export can be missing.)
                  own = lambda n: next((d for d in ('kernelbase.dll', 'kernel32.dll') if d in exporters.get(n, [])), 'kernel32.dll')
                  f.write(f'void *gexw_dl_LoadLibraryA(const char *) __asm__("{own("LoadLibraryA")}!LoadLibraryA");\n')
                  f.write(f'void gexw_dl_RaiseException(unsigned int, unsigned int, unsigned int, const void *) '
                          f'__asm__("{own("RaiseException")}!RaiseException");\n')
                  f.write('static int dl[%d];\n' % len(delayed))
                  # each DLL only these thunks reach, marked for gex (wasmdll):
                  # its module is not installed ahead of this one
                  marked = sorted((set(delayed) | {d for d in delay_targets.get(g, ()) if grp(d) != g}) - direct_refs.get(g, set()))
                  delay_marks[g] = (set(marked), set(delayed) | {c for d in delayed for c in static_closure(d)})
                  for k, d in enumerate(marked):
                      f.write(f'__attribute__((export_name("gexw-delay!{d}"))) void dm{k}(void) {{}}\n')
                  f.write('static __attribute__((noinline)) void dl_load(int i, const char *name)\n{\n'
                          '    if (!gexw_dl_LoadLibraryA(name)) gexw_dl_RaiseException(0xc06d007e, 0, 0, 0);\n'
                          '    __atomic_store_n(&dl[i], 1, __ATOMIC_RELEASE);\n}\n')
              for k, (alias, have, target, want, delay) in enumerate(gthunks):
                  hr, hp = have.split(':')
                  wr, wp = want.split(':')
                  params = ', '.join(f'{ctype[c]} a{i}' for i, c in enumerate(hp)) or 'void'
                  tparams = ', '.join(ctype[c] for c in wp) or 'void'
                  args = ', '.join(f'a{i}' for i in range(len(wp)))
                  f.write(f'{ctype[wr]} t{k}({tparams}) __asm__("{target}");\n')
                  f.write(f'{ctype[hr]} f{k}({params}) __asm__("{alias}");\n')
                  call = f't{k}({args})'
                  if hr == 'v':
                      body = f'{call};'
                  elif wr == 'v':
                      body = f'{call}; return 0;'
                  else:
                      body = f'return ({ctype[hr]}){call};'
                  if delay:
                      body = ''.join(f'if (!__atomic_load_n(&dl[{delayed.index(d)}], __ATOMIC_ACQUIRE)) dl_load({delayed.index(d)}, "{d}"); '
                                     for d in loads[alias]) + body
                  f.write(f'{ctype[hr]} f{k}({params}) {{ {body} }}\n')
          obj = os.path.join(tmp, f'forwards{g}.o')
          subprocess.check_call(cc('-O2', '-c', '-o', obj, src))
          objs[g].append(obj)

    # Imports from DLLs that stay i386 code (msvcrt's user32 and advapi32
    # calls): a stub per name, which finds the export through kernel32
    # (LoadLibraryA, GetProcAddress) once and calls it -- a guest address,
    # so through the ICALL pass's exit stub, as a guest call. The stub's
    # signature is its callers'.
    bound_groups = set()
    for g in objs if others and 'kernel32.dll' in native else ():
        undefined = {}  # name -> (sig, importing dll)
        users = {}  # stub symbol -> the DLLs calling it
        for mod, ro, ex, imports in dlls:
            if grp(mod) != g:
                continue
            for (n, k), v in sigs[mod].items():
                if k == 'undefined' and n not in exporters and n not in forward_owner and '!' not in n:
                    undefined.setdefault(n, (v, mod, imports))
                    users.setdefault(n, set()).add(mod)
        stubs = []  # (symbol, sig, owner, export name)
        for n, (sig, mod, imports) in sorted(undefined.items()):
            owners = owner_of.get(n)
            if not owners:
                continue
            stubs.append((n, sig, next((o for o in imports if o in owners), owners[0]), n))
        stubs += sorted({b[:4] for b in guest_binds if b[4] == g})
        for b in guest_binds:
            if b[4] == g:
                users.setdefault(b[0], set()).add(b[5])
        if any(b[4] == g for b in guest_binds):
            print(f'{out}: bound to i386 DLLs ahead of native ones: ' +
                  ', '.join(sorted({b[0].split('!', 1)[1] for b in guest_binds if b[4] == g})))
        if stubs:
            ctype = {'i': 'int', 'I': 'long long', 'f': 'float', 'd': 'double', 'v': 'void'}
            src = os.path.join(tmp, f'guestimports{g}.c')
            with open(src, 'w') as f:
                own = lambda n: next((d for d in ('kernelbase.dll', 'kernel32.dll') if d in exporters.get(n, [])), 'kernel32.dll')
                f.write(f'void *gexw_LoadLibraryA(const char *) __asm__("{own("LoadLibraryA")}!LoadLibraryA");\n')
                f.write(f'void *gexw_GetProcAddress(void *, const char *) __asm__("{own("GetProcAddress")}!GetProcAddress");\n')
                f.write(f'void *gexw_GetModuleHandleA(const char *) __asm__("{own("GetModuleHandleA")}!GetModuleHandleA");\n')
                f.write(f'static void *gp[{len(stubs)}];\n')
                # An i386 ntdll (a WoW64 process's, its native kernelbase
                # beside it): its exports found by walking the loader's list
                # and ntdll's export directory, which calls nothing. Through
                # kernelbase, GetModuleHandleA and GetProcAddress call ntdll's
                # RtlInitAnsiString and the like: stubs not bound yet, which
                # bound themselves through GetModuleHandleA again.
                guest_ntdll = not WINE64 and 'ntdll.dll' not in native
                if guest_ntdll:
                    f.write('''void *__wine_wasm_current_teb(void);
static void *gexw_ntdll_proc(const char *name)
{
    static unsigned char *base;
    unsigned char *nt, *exp;
    unsigned int *names, *funcs, lo, hi;
    unsigned short *ords;
    if (!base) {
        unsigned char *peb = *(unsigned char **)((unsigned char *)__wine_wasm_current_teb() + 0x30);
        unsigned char *head = *(unsigned char **)(peb + 0x0c) + 0x0c, *e;
        for (e = *(unsigned char **)head; e && e != head; e = *(unsigned char **)e) {
            const unsigned short *n = *(const unsigned short **)(e + 0x30);
            const char *want = "ntdll.dll";
            int i;
            if (*(unsigned short *)(e + 0x2c) != 18) continue;
            for (i = 0; i < 9 && (n[i] | 0x20) == (want[i] | 0x20); i++);
            if (i == 9) { base = *(unsigned char **)(e + 0x18); break; }
        }
        if (!base) return 0;
    }
    nt = base + *(unsigned int *)(base + 0x3c);
    exp = base + *(unsigned int *)(nt + 0x78);
    names = (unsigned int *)(base + *(unsigned int *)(exp + 0x20));
    funcs = (unsigned int *)(base + *(unsigned int *)(exp + 0x1c));
    ords = (unsigned short *)(base + *(unsigned int *)(exp + 0x24));
    lo = 0; hi = *(unsigned int *)(exp + 0x18);
    while (lo < hi) {
        unsigned int mid = (lo + hi) / 2;
        const unsigned char *a = base + names[mid], *b = (const unsigned char *)name;
        while (*a && *a == *b) a++, b++;
        if (*a == *b) return base + funcs[ords[mid]];
        if (*a < *b) lo = mid + 1; else hi = mid;
    }
    return 0;
}
''')
                resolve = lambda owner, n: (f'gexw_ntdll_proc("{n}")' if guest_ntdll and owner == 'ntdll.dll'
                                            else f'gexw_GetProcAddress(gexw_LoadLibraryA("{LOAD_NAME.get(owner, owner)}"), "{n}")')
                stdcalls, varargs, owner_sigs, written_bv = {}, {}, {}, set()
                spec_args, n_byval = {}, 0
                for k, (sym, sig, owner, n) in enumerate(stubs):
                    if owner not in stdcalls:
                        stdcalls[owner] = spec_stdcalls(owner_spec(SRC, owner)) if SRC else set()
                        varargs[owner] = spec_varargs(owner_spec(SRC, owner)) if SRC and not WINE64 else set()
                        spec_args[owner] = spec_i386_args(owner_spec(SRC, owner)) if SRC and not WINE64 else {}
                    # a stdcall entry thunk (callee pops) for i386 callers
                    # of the stub's table index (below)
                    # (x64 has one convention)
                    attr = '__attribute__((annotate("gex.stdcall"))) ' if n in stdcalls[owner] and not WINE64 else ''
                    r, ps = sig.split(':')
                    params = ', '.join(f'{ctype[c]} a{i}' for i, c in enumerate(ps)) or 'void'
                    types = ', '.join(ctype[c] for c in ps) or 'void'
                    args = ', '.join(f'a{i}' for i in range(len(ps)))
                    # A variadic call: wasm32 hands over the variadic arguments
                    # as a pointer to a buffer, the last parameter; the i386
                    # callee takes them on the stack after the fixed ones.
                    if n in varargs[owner] and ps and ps[-1] == 'i':
                        types = ', '.join([ctype[c] for c in ps[:-1]] + ['int'] * VARARGS_WORDS)
                        args = ', '.join([f'a{i}' for i in range(len(ps) - 1)] +
                                         [f'((const int *)a{len(ps) - 1})[{j}]' for j in range(VARARGS_WORDS)])
                    # A struct by value: the caller hands over a pointer to
                    # it, as wasm passes one; the stub passes the struct on by
                    # value, so the ICALL pass's exit stub puts the struct in
                    # its slot (x64: 1, 2, 4 or 8 bytes, in its argument's
                    # register; a larger one goes by reference, as the pointer
                    # it already is) or on the i386 stack, not the pointer.
                    # x64: the owner's gexw.sig64 says which (its .spec says
                    # int64). i386: the .spec's int64 or int128 where the
                    # caller passes a pointer.
                    byval = {}
                    if not WINE64:
                        sa = spec_args[owner].get(n)
                        if sa is not None and len(sa) == len(ps) and n not in varargs[owner]:
                            byval = {i: {'int64': 8, 'int128': 16}[t] for i, t in enumerate(sa)
                                     if t in ('int64', 'int128') and ps[i] == 'i'}
                    else:
                        if owner not in owner_sigs:
                            owner_sigs[owner] = wasm_sig64(others[owner][:-len('.exports')]) if others[owner].endswith('.so.exports') else {}
                        osig = owner_sigs[owner].get(n, '')
                        ors, _, ops = osig.partition(':')
                        ops = re.findall(r'b\d+|[a-zA-Z]', ops)
                        lead = 1 if ors.startswith('s') else 0  # (a struct return's buffer)
                        if len(ops) + lead == len(ps):
                            byval = {lead + j: int(t[1:]) for j, t in enumerate(ops)
                                     if t[0] == 'b' and int(t[1:]) in (2, 4, 8)}
                    if byval:
                        n_byval += 1
                        for sz in sorted(set(byval.values()) - written_bv):
                            f.write(f'struct gexw_bv{sz} {{ char c0; char c[{sz - 1}]; }};\n')
                            written_bv.add(sz)
                        types = ', '.join(f'struct gexw_bv{byval[i]}' if i in byval else ctype[c]
                                          for i, c in enumerate(ps))
                        args = ', '.join(f'*(const struct gexw_bv{byval[i]} *)(__SIZE_TYPE__)a{i}' if i in byval
                                         else f'a{i}' for i in range(len(ps)))
                    f.write(f'{attr}{ctype[r]} g{k}({params}) __asm__("{sym}");\n')
                    # (bound by its DLL's entry point when the owner is a
                    # static import, else on first call, as a delay load)
                    f.write(f'{attr}{ctype[r]} g{k}({params}) {{\n'
                            f'    void *p = gp[{k}];\n'
                            f'    if (!p) gp[{k}] = p = {resolve(owner, n)};\n'
                            f'    {"" if r == "v" else "return "}(({ctype[r]} (*)({types}))p)({args});\n}}\n')
                # Their addresses taken, so the ICALL pass gives each an entry
                # thunk: a native module may hand one to i386 code as a
                # function pointer (services.exe's RPC dispatch table holds
                # rpcrt4's NdrServerCall2), which then calls its table index.
                f.write('void *const __gexw_guestimport_refs[] __attribute__((used)) = {\n')
                for k in range(len(stubs)):
                    f.write(f'    (void *)g{k},\n')
                f.write('};\n')
                # Each DLL's binding of the stubs it calls whose owner it
                # imports statically (loaded and attached before it): at its
                # process attach, with the loader lock held, as an import
                # table is bound (winebuild's entry point calls it).
                for i, (mod, _, _, _) in enumerate(d for d in dlls if grp(d[0]) == g):
                    eager = set(EAGER.get(mod, []))
                    mine = [(k, o, n) for k, (sym, _, o, n) in enumerate(stubs) if mod in users.get(sym, ()) and o in eager]
                    f.write(f'void gb{i}(void) __asm__("gexw-bind!{mod}");\n')
                    f.write(f'void gb{i}(void)\n{{\n')
                    for o in sorted({o for _, o, _ in mine}):
                        if guest_ntdll and o == 'ntdll.dll':
                            for k, _, n in (m for m in mine if m[1] == o):
                                f.write(f'    if (!gp[{k}]) gp[{k}] = gexw_ntdll_proc("{n}");\n')
                            continue
                        # (a program's, at its entry point: the DLL loaded
                        # first, as its import table would have had the
                        # loader do; GetModuleHandle finds it before its
                        # first call, as a PE program's static import)
                        get = 'gexw_LoadLibraryA' if mod.lower().endswith('.exe') else 'gexw_GetModuleHandleA'
                        f.write(f'    {{ void *h = {get}("{LOAD_NAME.get(o, o)}");\n      if (h) {{\n')
                        for k, _, n in (m for m in mine if m[1] == o):
                            f.write(f'        if (!gp[{k}]) gp[{k}] = gexw_GetProcAddress(h, "{n}");\n')
                        f.write('      }\n    }\n')
                    f.write('}\n')
                bound_groups.add(g)
            obj = os.path.join(tmp, f'guestimports{g}.o')
            subprocess.check_call(cc('-O2', '-D_WIN32', '-c', '-o', obj, src))
            objs[g].append(obj)
            print(f'{g or out}: {len(stubs)} imports from i386 DLLs ({", ".join(sorted({o for _, _, o, _ in stubs}))})'
                  + (f', {n_byval} with a struct by value' if n_byval else ''))

    # (a DLL in a group without guest-import stubs binds nothing)
    for g in objs:
        if g in bound_groups:
            continue
        src = os.path.join(tmp, f'guestbinds{g}.c')
        with open(src, 'w') as f:
            for i, (mod, _, _, _) in enumerate(d for d in dlls if grp(d[0]) == g):
                f.write(f'void gb{i}(void) __asm__("gexw-bind!{mod}");\nvoid gb{i}(void) {{}}\n')
        obj = os.path.join(tmp, f'guestbinds{g}.o')
        subprocess.check_call(cc('-O2', '-D_WIN32', '-c', '-o', obj, src))
        objs[g].append(obj)

    # Each module's load-time imports for Wine's PE loader on wasm32
    # (ntdll's fixup_imports asks __gexw_module_imports): only DLLs.
    if EAGER:
        known = {m for m, _, _, _ in dlls} | set(others or {})
        src = os.path.join(tmp, 'moduleimports.c')
        with open(src, 'w') as f:
            f.write('static const char *const gexw_imports[] = {\n')
            # what each module needs loaded and initialised first, through
            # its static imports
            below = {}
            def needs_first(m):
                if m not in below:
                    below[m] = set()
                    todo = [d for d in EAGER.get(m, []) if d != m]
                    while todo:
                        d = todo.pop()
                        if d not in below[m]:
                            below[m].add(d)
                            todo += EAGER.get(d, [])
                return below[m]
            for mod in sorted(EAGER):
                # Delay imports too, after them, but only native ones: an
                # i386 one is reached through a guest-import stub, which
                # loads it on first call as a delay-load thunk would
                # (loading gdi32's winspool.drv up front moved the DLLs
                # after it off the bases their units were built for).
                native_mods = {m for m, _, _, _ in dlls}
                deps = [d for d in EAGER[mod] if d in known and d != mod]
                # (not one that itself needs the module first: ucrtbase's
                # user32 put user32 and the DLLs it imports ahead of the C
                # runtime they call, whose lock table was not made yet, and
                # its _lock recursed until the stack ran out)
                deps += [d for d in DELAY.get(mod, []) if d in native_mods and d != mod and d not in deps
                         and mod not in needs_first(d) and d in delay_eager.get(mod, ())]
                f.write('    "%s", "%s",\n' % (mod, ''.join(d + '\\0' for d in deps)))
            f.write('    0\n};\n')
            f.write('const char *__gexw_module_imports(const unsigned short *name)\n{\n'
                    '    for (int i = 0; gexw_imports[i]; i += 2) {\n'
                    '        const char *a = gexw_imports[i]; const unsigned short *b = name;\n'
                    '        while (*a && *b && (*a | 0x20) == ((*b < 128 ? *b : 0) | 0x20)) a++, b++;\n'
                    '        if (!*a && !*b) return gexw_imports[i + 1];\n'
                    '    }\n    return 0;\n}\n')
        obj = os.path.join(tmp, 'moduleimports.o')
        subprocess.check_call(cc('-O2', '-D_WIN32', '-c', '-o', obj, src))
        objs[''].append(obj)

    # The modules: the --split ones' imports of the core known first (their
    # objects' undefined symbols less what they define), so the core exports
    # them; then the core; then each --split module, its imports bound to
    # the core's exports by gex when it is loaded.
    def syms(paths, flag):
        out = set()
        for o in paths:
            nm = subprocess.run(['llvm-nm', flag, o], capture_output=True, text=True).stdout
            out |= {l.split()[-1] for l in nm.splitlines() if l.split()}
        return out
    shim = {'__indirect_function_table', '__memory_base', '__table_base', '__stack_pointer', '__table_base32', 'memory'}
    needs = set()
    # (and the core's of them, which they export)
    if SPLIT:
        needs |= {n for n in syms(objs[''], '--undefined-only') - syms(objs[''], '--defined-only')
                  if '!' in n and grp(n.split('!', 1)[0])}
    for g in objs:
        if g:
            # its own bounds of the entry-thunk pairs (gexrt's are the core's)
            src = os.path.join(tmp, f'entries{g}.c')
            # (the data segments' names, in each object's linking section)
            blobs = [open(o, 'rb').read() for o in objs[g]]
            have = {sec for sec in ('gexw_entries', 'gexw_entries_decl')
                    if any(enc_name(sec) in x for x in blobs)}
            undef = syms(objs[g], '--undefined-only')
            with open(src, 'w') as f:
                # the gex ABI its objects were built for (gex_abi.h's gex.abi
                # section, which only gexrt's objects include, and gexrt is the
                # core's): the Wine tree's, as the core's
                if not any(b'gex.abi' in x for x in blobs):
                    hdr = open(os.path.join(SRC, 'include', 'wine', 'gex', 'gex_abi.h')).read()
                    abi = re.search(r'#define GEX_NATIVE_ABI (\d+)u', hdr)
                    if abi:
                        f.write(f'__asm__(".section .custom_section.gex.abi,\\"\\",@\\n.int32 {abi.group(1)}\\n'
                                f'.section .text,\\"\\",@\\n");\n')
                # its own empty CRT initializer tables, as gexrt's are the
                # core's (a program's mainCRTStartup walks them)
                for t in ('xi', 'xc', 'xp', 'xt'):
                    if f'__{t}_a' in undef or f'__{t}_z' in undef:
                        f.write(f'void *__{t}_a[1] __attribute__((used));\n'
                                f'extern void *__{t}_z[1] __attribute__((alias("__{t}_a")));\n')
                # a C destructor's registration (wldap32's), which nothing in
                # the core defines: none runs, as none did linked into it
                if '__cxa_atexit' in undef:
                    f.write('int __cxa_atexit(void (*f)(void *), void *a, void *d) { return 0; }\n')
                for sec in ('gexw_entries', 'gexw_entries_decl'):
                    # (none at all: a module none of whose functions is handed out)
                    if sec not in have:
                        f.write(f'const void *const __{sec}[2] __attribute__((used)) = {{ 0, 0 }};\n')
                        continue
                    f.write(f'extern const char __start_{sec}[], __stop_{sec}[];\n'
                            f'const void *const __{sec}[2] __attribute__((used)) = {{ __start_{sec}, __stop_{sec} }};\n')
            obj = os.path.join(tmp, f'entries{g}.o')
            subprocess.check_call(cc('-O2', '-D_WIN32', '-c', '-o', obj, src))
            objs[g].append(obj)
            needs |= syms(objs[g], '--undefined-only') - syms(objs[g], '--defined-only') - shim
    provided = set()
    # what each module defines, for the others' imports
    elsewhere = {n for g in objs for n in syms(objs[g], '--defined-only')} if SPLIT else set()
    main_out = out
    def emit(g, out, gexrt):
        rsp = os.path.join(tmp, f'exports{g}.rsp')
        with open(rsp, 'w') as f:
            for d in data:
                if grp(d.split('!', 1)[0]) == g:
                    f.write(f'--export-if-defined={d}\n')
            # what the --split modules import from the core
            for n in sorted(needs):
                f.write(f'--export-if-defined={n}\n')
            # an earlier core's (--exports-of)
            if not g:
                for n in sorted(KEEP_EXPORTS):
                    f.write(f'--export-if-defined={n}\n')
            # PELINK_EXPORTS=FILE: the DLL!name entries it lists (one a line,
            # those of this module's DLLs), for modules linked later that call
            # them (a site's gexcc programs: their imports)
            if os.environ.get('PELINK_EXPORTS'):
                for line in open(os.environ['PELINK_EXPORTS']):
                    n = line.strip()
                    if '!' in n and grp(n.split('!', 1)[0]) == g:
                        f.write(f'--export-if-defined={n}\n')
            # PELINK_EXPORT_ALL=1: every export of its DLLs, for modules
            # linked later against this one (gexcc's Windows programs: their
            # imports are whatever their code calls)
            if os.environ.get('PELINK_EXPORT_ALL'):
                for mod, _, ex, _ in dlls:
                    if grp(mod) != g:
                        continue
                    for line in open(ex):
                        w = line.split()
                        if w and not w[0].startswith('__wine'):
                            f.write(f'--export-if-defined={mod}!{w[0]}\n')
            # what the other mode's modules call (wasmdll's CROSS_MODE)
            for n in CROSS_MODE:
                f.write(f'--export-if-defined={n}\n')
            # gexrt's bounds of the entry-thunk pairs (gexw_entries)
            f.write('--export-if-defined=__gexw_entries\n')
            f.write('--export-if-defined=__gexw_entries_decl\n')
            # a module with thread-local data (a gexcc-win program's C++
            # runtime): the host gives each thread's instance a block of
            # its own (wasmdll), as wasm-ld's start function does only for
            # the first
            for n in ('__wasm_init_tls', '__tls_size', '__tls_align'):
                f.write(f'--export-if-defined={n}\n')
        cmd = ['wasm-ld'] + (['-mwasm64'] if WINE64 else []) + ['--experimental-pic', '-shared', '--shared-memory',
               '--import-memory', '--max-memory=' + ('17179869184' if M64 else '4294967296'),
               '--unresolved-symbols=import-dynamic', '-o', out, '-Map=' + out + '.map',
               f'@{rsp}'] + objs[g] + ([gexrt] if gexrt else [])
        # compiler-rt's builtins (__multi3 and the like), from the toolchain's
        # sysroot (toolchain/sysroot/build32.sh, build64.sh)
        sysroot = os.environ.get('GEX_WASM64_SYSROOT' if WINE64 else 'GEX_WASM_SYSROOT')
        if sysroot:
            cmd.append(os.path.join(sysroot, 'lib', 'libclang_rt.builtins-%s.a' % ('wasm64' if WINE64 else 'wasm32')))
        elif WINE64:
            sys.exit('pelink: set GEX_WASM64_SYSROOT')
        subprocess.check_call(cmd)
        # What nothing defines (an export whose code is i386 assembly, say): a
        # stub that traps, so one gap does not keep the module from
        # instantiating. Listed.
        imported = subprocess.run(['wasm-objdump', '-x', '-j', 'Import', out], capture_output=True, text=True).stdout
        missing = set(re.findall(r'func\[\d+\] sig=\d+ <[^>]*> <- env\.(\S+)', imported))
        callers = {}
        # (a --split module's imports the core exports are no gaps, nor what
        # another module defines: bound when both are loaded)
        missing -= provided | elsewhere
        for mod in sigs:
            if grp(mod) != g:
                continue
            for (n, k), v in sigs[mod].items():
                if k == 'undefined' and n in missing:
                    callers.setdefault(n, v)
        # Only weakly referenced (msvcrt's crt_main asks whether the program
        # defined __main_argc_argv or __original_main): left undefined, so the
        # reference stays null. A trapping stub would make it non-null, and a
        # program with the other form of main would call the trap.
        strong = set()
        for mod, ro, ex, imports in dlls:
            if grp(mod) != g:
                continue
            nm = subprocess.run(['llvm-nm', '--undefined-only', ro_own.get(mod, ro)], capture_output=True, text=True).stdout
            strong |= {l.split()[-1] for l in nm.splitlines() if l.split()[:1] == ['U']}
        callers = {n: v for n, v in callers.items() if n in strong}
        gaps = sorted(callers.items())
        if gaps:
            ctype = {'i': 'int', 'I': 'long long', 'f': 'float', 'd': 'double', 'v': 'void'}
            src = os.path.join(tmp, f'gaps{g}.c')
            with open(src, 'w') as f:
                for k, (n, sig) in enumerate(gaps):
                    r, ps = sig.split(':')
                    params = ', '.join(f'{ctype[c]} a{i}' for i, c in enumerate(ps)) or 'void'
                    f.write(f'{ctype[r]} x{k}({params}) __asm__("{n}");\n')
                    f.write(f'{ctype[r]} x{k}({params}) {{ __builtin_trap(); }}\n')
            obj = os.path.join(tmp, f'gaps{g}.o')
            subprocess.check_call(cc('-O2', '-c', '-o', obj, src))
            cmd.insert(cmd.index(f'@{rsp}') + 1, obj)
            subprocess.check_call(cmd)
            print(f'{out}: {len(gaps)} functions nothing defines, trapping: {" ".join(n for n, _ in gaps)}')
        # Each DLL's table slots and data (`gexw.dllslots`), read from the
        # link as it is: wasm-opt keeps slots and data offsets, not function
        # numbers.
        slots = dll_slots(out, out + '.map', obj_dll)
        os.unlink(out + '.map')
        # wcc compiles code without -D_WIN32 as Unix-side code, whose setjmp
        # lowers onto the legacy exception instructions (for asyncify); that
        # includes winebuild's glue, where an adapter of msvcrt's _setjmp calls
        # a function clang lowers as setjmp. gex runs the standard instructions
        # only, so the module is translated, as wcc's own links are.
        opt = os.path.join(sysroot or '', 'binaryen', 'bin', 'wasm-opt')
        opt = os.environ.get('GEX_WASM_OPT') or opt
        if WINE64:
            # lowered to wasm32 (wcc64's side modules: the one place an address
            # becomes a memory index), optimised as they are; the memory64
            # flavour left as it is, as wcc64 -mgex-memory64 leaves them
            snap = not M64 and os.path.exists(os.path.join(sysroot or '', 'lib', 'gex-snapshot'))
            # (the snapshot flavour's level root stays apart from the entry's C
            # below it, which asyncify instruments)
            apart = ['--no-inline=__gexw_entry_c'] if snap else []
            subprocess.check_call([opt, out, '-g', '--strip-dwarf', '--enable-threads', '--enable-bulk-memory',
                                   '--enable-mutable-globals', '--enable-sign-ext', '--enable-nontrapping-float-to-int',
                                   '--enable-exception-handling', '--enable-reference-types', '--enable-memory64'] +
                                  ([] if M64 else ['--memory64-lowering']) + apart + ['-O2', '-o', out])
            if snap:
                # The snapshot flavour's (wcc64's sysroot): asyncify over the
                # lowered module, what can reach a call-out by a type-correct
                # call graph (forkreach.py), below gexrt's entry from the guest
                # (each level's root, where an unwind ends: neither it nor its
                # callers); its state and data the host's (relocatable), then
                # the standard exception instructions.
                dis = os.path.join(os.path.dirname(opt), 'wasm-dis')
                roots = ['__gexw_entry']
                reach = subprocess.run([sys.executable, os.path.join(here, '..', 'toolchain', 'gexcc', 'forkreach.py'), '--quoted', '--ignore', ','.join(roots), dis, out, 'gex.call_out'] + roots,
                                       capture_output=True, text=True)
                passes = ['--asyncify', '--pass-arg=asyncify-imports@gex.call_out', '--pass-arg=asyncify-relocatable']
                if reach.returncode == 0:
                    lst = out + '.onlylist'
                    open(lst, 'w').write(reach.stdout.strip() + '\n')
                    passes += ['--pass-arg=asyncify-onlylist@@' + lst]
                else:
                    passes += ['--pass-arg=asyncify-removelist@' + ','.join(roots)]
                subprocess.check_call([opt, out, '-g', '--enable-threads', '--enable-bulk-memory', '--enable-mutable-globals',
                                       '--enable-sign-ext', '--enable-nontrapping-float-to-int', '--enable-exception-handling',
                                       '--enable-reference-types'] + apart + passes + ['-O2', '--translate-to-exnref', '-o', out])
                if reach.returncode == 0:
                    os.unlink(lst)
        elif os.path.exists(opt):
            subprocess.check_call([opt, out, '-g', '--strip-dwarf', '--enable-threads', '--enable-bulk-memory',
                                   '--enable-mutable-globals', '--enable-sign-ext', '--enable-nontrapping-float-to-int',
                                   '--enable-exception-handling', '--enable-reference-types',
                                   '--translate-to-exnref', '-o', out])
        # The forwards, for gex to bind a -import re-export of a forwarded name
        # (kernel32's HeapFree is kernelbase's, which forwards to ntdll's
        # RtlFreeHeap): a custom section `gexw.forwards`, lines `name dll!target`.
        lines = []
        for name, by in sorted(forwards.items()):
            if name in exporters:
                continue
            # (the first forwarder that reaches a definition: kernel32's
            # SetLastError names its own RtlSetLastWin32Error, which it imports)
            hit = next((h for h in map(resolve_forward, by.values()) if h), None)
            if hit and grp(hit[0]) == g:
                lines.append(f'{name} {hit[0]}!{hit[1]}')
        if lines:
            payload = enc_name('gexw.forwards') + '\n'.join(lines).encode() + b'\n'
            with open(out, 'ab') as f:
                f.write(bytes([0]) + enc_leb(len(payload)) + payload)
        if slots:
            payload = enc_name('gexw.dllslots') + slots.encode()
            with open(out, 'ab') as f:
                f.write(bytes([0]) + enc_leb(len(payload)) + payload)
        # With modules of their own: which module each DLL is in
        # (`gexw.modules`, lines `dll file`), for gex to install the ones
        # a module calls into before it
        if SPLIT:
            core_file = os.path.basename(out) if not g else os.path.basename(main_out)
            mods = '\n'.join(f'{d} {SPLIT.get(d) or core_file}' for d in sorted({m for m, _, _, _ in dlls}))
            payload = enc_name('gexw.modules') + mods.encode() + b'\n'
            with open(out, 'ab') as f:
                f.write(bytes([0]) + enc_leb(len(payload)) + payload)
        if g or SPLIT:
            name_imports(out)
        undefined = subprocess.run(['wasm-objdump', '-x', '-j', 'Import', out], capture_output=True, text=True).stdout
        names = sorted(set(re.findall(r'<- env\.(\S+)', undefined)) - {'memory', '__indirect_function_table',
                       '__stack_pointer', '__memory_base', '__table_base'})
        open(out + '.undefined', 'w').write('\n'.join(names) + '\n')
        print(f'{out}: {sum(grp(d[0]) == g for d in dlls)} DLLs, {os.path.getsize(out) >> 20} MiB, {len(names)} undefined '
              f'(listed in {out}.undefined)')

    emit('', out, gexrt)
    exp = subprocess.run(['wasm-objdump', '-x', '-j', 'Export', out], capture_output=True, text=True).stdout
    provided = set(re.findall(r'-> "([^"]+)"', exp))
    if needs - provided:
        print(f'{out}: --split modules import what the core does not export (another --split module\'s, or nobody\'s): '
              + ' '.join(sorted(needs - provided)[:40]))
    # each module exports what the others import of it
    for g in objs:
        if g:
            emit(g, os.path.join(os.path.dirname(out), g), None)
    if SPLIT:
        check_imports(out, [os.path.join(os.path.dirname(out), g) for g in objs if g])
    check_delay_imports({g: os.path.join(os.path.dirname(out), g) if g else out for g in objs}, delay_marks)


def delay_loads(delay, target, static_closure):
    """The DLLs a delay thunk loads before its call: the delay import, and
    the DLL the call reaches when the delay import only forwards it there
    and does not bring it in itself (msvcrt's RtlGenRandom: advapi32's
    SystemFunction036 is cryptbase's, which advapi32 does not import;
    Windows loads it when it resolves the forward)."""
    dll = target.split('!', 1)[0]
    return [delay] + ([dll] if dll != delay and dll not in static_closure(delay) else [])


def check_delay_imports(modules, marks):
    """A DLL marked delayed is not installed ahead of the module importing
    it, so each of its functions the module imports must be one a delay
    thunk calls after loading it. Any other reaches an empty slot at its
    first call: the link fails here instead."""
    bad = []
    for g, path in modules.items():
        marked, loaded = marks.get(g, (set(), set()))
        imported = subprocess.run(['wasm-objdump', '-x', '-j', 'Import', path], capture_output=True, text=True).stdout
        gaps = sorted({n for n in re.findall(r'<- env\.(\S+)', imported)
                       if '!' in n and n.split('!', 1)[0] in marked - loaded})
        if gaps:
            # (not "NAME.wasm: ...", which wine/dist.sh reads as a --split
            # module to leave out)
            bad.append(f'in {os.path.basename(path)}, {" ".join(gaps[:20])}' + (f' (and {len(gaps) - 20} more)' if len(gaps) > 20 else ''))
    if bad:
        sys.exit('pelink: functions of delayed DLLs that no delay thunk loads first:\n  ' + '\n  '.join(bad))


# What a module may import beside the others' exports: the memory, table and
# stack the loader gives every module, and the weak main names a C runtime
# asks a program for (left null: pelink's emit).
LOADER_IMPORTS = {'memory', '__indirect_function_table', '__stack_pointer', '__memory_base', '__table_base'}
WEAK_IMPORTS = {'main', '__main_argc_argv', '__original_main', 'WinMain'}


def check_imports(core, splits):
    """With modules of their own, each function a module imports is
    another's export: the core's, another --split module's, or the other
    mode's (CROSS_MODE). A name none exports would fail at load, in a
    process that only then maps the DLL: the link fails here instead."""
    def section(path, sid_want):
        b = open(path, 'rb').read()
        i, out = 8, []
        while i < len(b):
            sid = b[i]
            size, j = leb_at(b, i + 1)
            if sid == sid_want:
                out.append(b[j:j + size])
            i = j + size
        return out
    def exports(path):
        names = set()
        for body in section(path, 7):
            cnt, k = leb_at(body, 0)
            for _ in range(cnt):
                ln, k = leb_at(body, k)
                names.add(body[k:k + ln].decode())
                k += ln + 1
                _, k = leb_at(body, k)
        return names
    def func_imports(path):
        names = []
        for body in section(path, 2):
            cnt, k = leb_at(body, 0)
            for _ in range(cnt):
                ml, k = leb_at(body, k)
                mod = body[k:k + ml].decode()
                k += ml
                fl, k = leb_at(body, k)
                field = body[k:k + fl].decode()
                k += fl
                kind = body[k]
                k += 1
                if kind == 0:
                    _, k = leb_at(body, k)
                    if mod == 'env':
                        names.append(field)
                elif kind in (1, 2):
                    if kind == 1:
                        k += 1
                    flags, k = leb_at(body, k)
                    _, k = leb_at(body, k)
                    if flags & 1:
                        _, k = leb_at(body, k)
                elif kind == 3:
                    k += 2
                elif kind == 4:
                    k += 1
                    _, k = leb_at(body, k)
        return names
    mods = [core] + splits
    have = {m: exports(m) for m in mods}
    bad = []
    for m in mods:
        others = set().union(*(have[o] for o in mods if o != m))
        gaps = sorted({n for n in func_imports(m) if n not in others and n not in LOADER_IMPORTS
                       and n not in WEAK_IMPORTS and n not in CROSS_MODE})
        if gaps:
            bad.append(f'{os.path.basename(m)}: {" ".join(gaps[:20])}' + (f' (and {len(gaps) - 20} more)' if len(gaps) > 20 else ''))
    if bad:
        sys.exit('pelink: functions modules import that no module exports:\n  ' + '\n  '.join(bad))


main()
