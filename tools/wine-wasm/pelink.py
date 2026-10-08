#!/usr/bin/env python3
"""Link Wine's PE side as one wasm module (docs/wine-native.md §3.14).

    pelink.py WINE_BUILD WINE_SRC OUT.wasm [DLL ...]
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
Whatever stays undefined is listed (OUT.wasm.undefined) and left an import."""
import os
import re
import subprocess
import sys
import tempfile

here = os.path.dirname(os.path.realpath(__file__))
EAGER = {}  # module -> the DLLs to load before it (pelink's main)
SRC = None  # the Wine source tree (pelink's main)


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
    build, src, out = sys.argv[1:4]
    global SRC
    SRC = src
    only = set(sys.argv[4:])
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
            if only and mod not in only:
                continue
            imports = [i.lstrip('-') for i in makefile_var(mk, 'IMPORTS') + makefile_var(mk, 'DELAYIMPORTS')]
            imports = [i if '.' in i else i + '.dll' for i in imports]
            dlls.append((mod, ro, ex, imports))
            # what the loader is to load before the module (makedep's
            # direct imports and default ones: the C runtime, kernel32,
            # ntdll), which a skeleton's empty import table cannot say
            eager = [i.lstrip('-') for i in makefile_var(mk, 'IMPORTS')]
            if '-nodefaultlibs' not in makefile_var(mk, 'EXTRADLLFLAGS'):
                is_crt = lambda n: n.startswith(('msvcr', 'ucrt')) or n == 'crtdll'
                crt = next((i for i in eager if is_crt(i)), None if is_crt(mod.split('.')[0]) else 'ucrtbase')
                eager += [c for c in (crt, 'kernel32', 'ntdll') if c and c not in eager]
            # and its delay imports, after them: a native module's calls are
            # bound in the one module, so nothing would load a delay-loaded
            # DLL on first call, and a DLL never loaded never ran its DllMain
            # (wineboot's user32 and gdi32: win32u's syscall table missing,
            # every NtUser call STATUS_INVALID_SYSTEM_SERVICE)
            eager += [i.lstrip('-') for i in makefile_var(mk, 'DELAYIMPORTS') if i.lstrip('-') not in eager]
            EAGER[mod] = [i if '.' in i else i + '.dll' for i in eager]
    if not dlls:
        sys.exit('no DLL objects found (build with winegcc for wasm32)')
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
    link(dlls, out, os.path.join(build, 'libs', 'gexrt', 'libgexrt.a'), others)


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

    sigs = {}  # dll -> {(name, 'defined'|'undefined'): sig}
    for mod, ro, ex, imports in dlls:
        listing = subprocess.run([sys.executable, os.path.join(here, 'pesym.py'), 'sigs', ro],
                                 capture_output=True, text=True, check=True).stdout
        sigs[mod] = {(l.split()[0], l.split()[1]): l.split()[2] for l in listing.splitlines() if len(l.split()) == 3}
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
    for name, by in forwards.items():
        if name in exporters:
            continue
        for via, fwd in by.items():
            hit = resolve_forward(fwd)
            if hit:
                aliases.setdefault(hit[0], []).append((f'{via}!{name}', hit[1]))
                forward_owner.setdefault(name, [])
                forward_owner[name].append(via)

    tmp = tempfile.mkdtemp(prefix='pelink-')
    objs = []

    # Signatures: a forward whose callers declare the function otherwise
    # than its definition (kernel32's void EnterCriticalSection is ntdll's
    # NTSTATUS RtlEnterCriticalSection) gets a thunk, not an alias: wasm
    # calls must match exactly.
    thunks = []
    for d in list(aliases):
        keep = []
        for alias, target in aliases[d]:
            want = sigs[d].get((target, 'defined'))
            name = alias.split('!', 1)[1]
            seen = {v for m in sigs for (n, k), v in sigs[m].items() if n == name and k == 'undefined'}
            other = [v for v in seen if v != want]
            if want and other:
                thunks.append((alias, other[0], f'{d}!{target}', want))
            else:
                keep.append((alias, target))
        aliases[d] = keep
    for mod, ro, ex, imports in dlls:
        local = os.path.join(tmp, mod + '.l.o')
        alias_file = os.path.join(tmp, mod + '.aliases')
        with open(alias_file, 'w') as f:
            for a, n in aliases.get(mod, []):
                f.write(f'{a} {n}\n')
        subprocess.check_call([sys.executable, os.path.join(here, 'pesym.py'), 'dll', ro, local, mod, ex, alias_file])
        mapfile = os.path.join(tmp, mod + '.map')
        with open(mapfile, 'w') as f:
            for name, owners in exporters.items():
                # (a DLL may list an export its code takes from another DLL:
                # kernel32's DebugBreak is kernelbase's)
                owners = [o for o in owners if (name, 'defined') in sigs[o]] or owners
                if mod in owners:
                    continue  # its own: already resolved inside it
                # (its own C runtime first, which its import list does not
                # name: ucrtbase's _wopen and msvcrt's _fstat32 are two fd
                # tables, and wineboot's fstat of a file it had just opened
                # failed)
                pick = next((o for o in EAGER.get(mod, []) + imports if o in owners), owners[0])
                # a caller declaring the function otherwise than its
                # definition (an adapter from the .spec: user32's
                # NotifyWinEvent is void, win32u's stub returns a word) goes
                # through a thunk: wasm calls must match exactly
                have, want = sigs[mod].get((name, 'undefined')), sigs[pick].get((name, 'defined'))
                if have and want and have != want:
                    thunk = f'{mod}.{name}'
                    thunks.append((f'gexw-thunk!{thunk}', have, f'{pick}!{name}', want))
                    f.write(f'{name} gexw-thunk {thunk}\n')
                    continue
                f.write(f'{name} {pick}\n')
            # names only forwarded (kernel32's EnterCriticalSection is
            # ntdll's RtlEnterCriticalSection, under kernel32.dll!EnterCriticalSection)
            for name, vias in forward_owner.items():
                pick = next((o for o in [mod] + imports if o in vias), vias[0])
                f.write(f'{name} {pick}\n')
        bound = os.path.join(tmp, mod + '.o')
        subprocess.check_call([sys.executable, os.path.join(here, 'pesym.py'), 'imports', local, bound, mod, mapfile])
        # The ICALL pass's stdcall annotations are for the compiler only;
        # wasm-ld 22 crashes relocating them when a function they name is
        # not linked (services.exe, plugplay.exe).
        subprocess.check_call(['llvm-objcopy', '--remove-section=llvm.func_attr.annotate.gex.stdcall',
                               '--remove-section=reloc.llvm.func_attr.annotate.gex.stdcall', bound])
        objs.append(bound)

    if thunks:
        ctype = {'i': 'int', 'I': 'long long', 'f': 'float', 'd': 'double', 'v': 'void'}
        src = os.path.join(tmp, 'forwards.c')
        with open(src, 'w') as f:
            for k, (alias, have, target, want) in enumerate(thunks):
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
                f.write(f'{ctype[hr]} f{k}({params}) {{ {body} }}\n')
        obj = os.path.join(tmp, 'forwards.o')
        subprocess.check_call([os.path.join(here, 'wcc'), '-O2', '-c', '-o', obj, src])
        objs.append(obj)

    # Imports from DLLs that stay i386 code (msvcrt's user32 and advapi32
    # calls): a stub per name, which finds the export through kernel32
    # (LoadLibraryA, GetProcAddress) once and calls it -- a guest address,
    # so through the ICALL pass's exit stub, as a guest call. The stub's
    # signature is its callers'.
    native = {mod for mod, _, _, _ in dlls}
    if others and 'kernel32.dll' in native:
        undefined = {}  # name -> (sig, importing dll)
        for mod, ro, ex, imports in dlls:
            for (n, k), v in sigs[mod].items():
                if k == 'undefined' and n not in exporters and n not in forward_owner and '!' not in n:
                    undefined.setdefault(n, (v, mod, imports))
        owner_of = {}
        for omod, oex in others.items():
            if omod in native:
                continue
            for line in open(oex):
                parts = line.split()
                if len(parts) >= 2 and parts[1] in ('func', 'forward'):
                    owner_of.setdefault(parts[0], []).append(omod)
        stubs = []
        for n, (sig, mod, imports) in sorted(undefined.items()):
            owners = owner_of.get(n)
            if not owners:
                continue
            stubs.append((n, sig, next((o for o in imports if o in owners), owners[0])))
        if stubs:
            ctype = {'i': 'int', 'I': 'long long', 'f': 'float', 'd': 'double', 'v': 'void'}
            src = os.path.join(tmp, 'guestimports.c')
            with open(src, 'w') as f:
                own = lambda n: next((d for d in ('kernelbase.dll', 'kernel32.dll') if d in exporters.get(n, [])), 'kernel32.dll')
                f.write(f'void *gexw_LoadLibraryA(const char *) __asm__("{own("LoadLibraryA")}!LoadLibraryA");\n')
                f.write(f'void *gexw_GetProcAddress(void *, const char *) __asm__("{own("GetProcAddress")}!GetProcAddress");\n')
                stdcalls = {}
                for k, (n, sig, owner) in enumerate(stubs):
                    if owner not in stdcalls:
                        stdcalls[owner] = spec_stdcalls(spec_path(SRC, 'dlls', owner, owner)) if SRC else set()
                    # a stdcall entry thunk (callee pops) for i386 callers
                    # of the stub's table index (below)
                    attr = '__attribute__((annotate("gex.stdcall"))) ' if n in stdcalls[owner] else ''
                    r, ps = sig.split(':')
                    params = ', '.join(f'{ctype[c]} a{i}' for i, c in enumerate(ps)) or 'void'
                    types = ', '.join(ctype[c] for c in ps) or 'void'
                    args = ', '.join(f'a{i}' for i in range(len(ps)))
                    f.write(f'{attr}{ctype[r]} g{k}({params}) __asm__("{n}");\n')
                    f.write(f'{attr}{ctype[r]} g{k}({params}) {{\n'
                            f'    static void *p;\n'
                            f'    if (!p) p = gexw_GetProcAddress(gexw_LoadLibraryA("{owner}"), "{n}");\n'
                            f'    {"" if r == "v" else "return "}(({ctype[r]} (*)({types}))p)({args});\n}}\n')
                # Their addresses taken, so the ICALL pass gives each an entry
                # thunk: a native module may hand one to i386 code as a
                # function pointer (services.exe's RPC dispatch table holds
                # rpcrt4's NdrServerCall2), which then calls its table index.
                f.write('void *const __gexw_guestimport_refs[] __attribute__((used)) = {\n')
                for k in range(len(stubs)):
                    f.write(f'    (void *)g{k},\n')
                f.write('};\n')
            obj = os.path.join(tmp, 'guestimports.o')
            subprocess.check_call([os.path.join(here, 'wcc'), '-O2', '-D_WIN32', '-c', '-o', obj, src])
            objs.append(obj)
            print(f'{out}: {len(stubs)} imports from i386 DLLs ({", ".join(sorted({o for _, _, o in stubs}))})')

    # Each module's load-time imports for Wine's PE loader on wasm32
    # (ntdll's fixup_imports asks __gexw_module_imports): only DLLs.
    if EAGER:
        known = {m for m, _, _, _ in dlls} | set(others or {})
        src = os.path.join(tmp, 'moduleimports.c')
        with open(src, 'w') as f:
            f.write('static const char *const gexw_imports[] = {\n')
            for mod in sorted(EAGER):
                deps = [d for d in EAGER[mod] if d in known and d != mod]
                f.write('    "%s", "%s",\n' % (mod, ''.join(d + '\\0' for d in deps)))
            f.write('    0\n};\n')
            f.write('const char *__gexw_module_imports(const unsigned short *name)\n{\n'
                    '    for (int i = 0; gexw_imports[i]; i += 2) {\n'
                    '        const char *a = gexw_imports[i]; const unsigned short *b = name;\n'
                    '        while (*a && *b && (*a | 0x20) == ((*b < 128 ? *b : 0) | 0x20)) a++, b++;\n'
                    '        if (!*a && !*b) return gexw_imports[i + 1];\n'
                    '    }\n    return 0;\n}\n')
        obj = os.path.join(tmp, 'moduleimports.o')
        subprocess.check_call([os.path.join(here, 'wcc'), '-O2', '-D_WIN32', '-c', '-o', obj, src])
        objs.append(obj)

    rsp = os.path.join(tmp, 'exports.rsp')
    with open(rsp, 'w') as f:
        for d in data:
            f.write(f'--export-if-defined={d}\n')
        # gexrt's bounds of the entry-thunk pairs (gexw_entries)
        f.write('--export-if-defined=__gexw_entries\n')
        f.write('--export-if-defined=__gexw_entries_decl\n')
    cmd = ['wasm-ld', '--experimental-pic', '-shared', '--shared-memory', '--import-memory',
           '--max-memory=4294967296', '--unresolved-symbols=import-dynamic', '-o', out,
           f'@{rsp}'] + objs + [gexrt]
    # compiler-rt's builtins (__multi3 and the like), from the toolchain's
    # sysroot (tools/wine-wasm/build-sysroot.sh)
    if os.environ.get('GEX_WASM_SYSROOT'):
        cmd.append(os.path.join(os.environ['GEX_WASM_SYSROOT'], 'lib', 'libclang_rt.builtins-wasm32.a'))
    subprocess.check_call(cmd)
    # What nothing defines (an export whose code is i386 assembly, say): a
    # stub that traps, so one gap does not keep the module from
    # instantiating. Listed.
    imported = subprocess.run(['wasm-objdump', '-x', '-j', 'Import', out], capture_output=True, text=True).stdout
    missing = set(re.findall(r'func\[\d+\] sig=\d+ <[^>]*> <- env\.(\S+)', imported))
    callers = {}
    for mod in sigs:
        for (n, k), v in sigs[mod].items():
            if k == 'undefined' and n in missing:
                callers.setdefault(n, v)
    # Only weakly referenced (msvcrt's crt_main asks whether the program
    # defined __main_argc_argv or __original_main): left undefined, so the
    # reference stays null. A trapping stub would make it non-null, and a
    # program with the other form of main would call the trap.
    strong = set()
    for mod, ro, ex, imports in dlls:
        nm = subprocess.run(['llvm-nm', '--undefined-only', ro], capture_output=True, text=True).stdout
        strong |= {l.split()[-1] for l in nm.splitlines() if l.split()[:1] == ['U']}
    callers = {n: v for n, v in callers.items() if n in strong}
    gaps = sorted(callers.items())
    if gaps:
        ctype = {'i': 'int', 'I': 'long long', 'f': 'float', 'd': 'double', 'v': 'void'}
        src = os.path.join(tmp, 'gaps.c')
        with open(src, 'w') as f:
            for k, (n, sig) in enumerate(gaps):
                r, ps = sig.split(':')
                params = ', '.join(f'{ctype[c]} a{i}' for i, c in enumerate(ps)) or 'void'
                f.write(f'{ctype[r]} x{k}({params}) __asm__("{n}");\n')
                f.write(f'{ctype[r]} x{k}({params}) {{ __builtin_trap(); }}\n')
        obj = os.path.join(tmp, 'gaps.o')
        subprocess.check_call([os.path.join(here, 'wcc'), '-O2', '-c', '-o', obj, src])
        cmd.insert(cmd.index(gexrt), obj)
        subprocess.check_call(cmd)
        print(f'{out}: {len(gaps)} functions nothing defines, trapping: {" ".join(n for n, _ in gaps)}')
    # wcc compiles code without -D_WIN32 as Unix-side code, whose setjmp
    # lowers onto the legacy exception instructions (for asyncify); that
    # includes winebuild's glue, where an adapter of msvcrt's _setjmp calls
    # a function clang lowers as setjmp. gex runs the standard instructions
    # only, so the module is translated, as wcc's own links are.
    opt = os.path.join(os.environ.get('GEX_WASM_SYSROOT', ''), 'binaryen', 'bin', 'wasm-opt')
    opt = os.environ.get('GEX_WASM_OPT') or opt
    if os.path.exists(opt):
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
        if hit:
            lines.append(f'{name} {hit[0]}!{hit[1]}')
    if lines:
        payload = enc_name('gexw.forwards') + '\n'.join(lines).encode() + b'\n'
        with open(out, 'ab') as f:
            f.write(bytes([0]) + enc_leb(len(payload)) + payload)
    undefined = subprocess.run(['wasm-objdump', '-x', '-j', 'Import', out], capture_output=True, text=True).stdout
    names = sorted(set(re.findall(r'<- env\.(\S+)', undefined)) - {'memory', '__indirect_function_table',
                   '__stack_pointer', '__memory_base', '__table_base'})
    open(out + '.undefined', 'w').write('\n'.join(names) + '\n')
    print(f'{out}: {len(dlls)} DLLs, {os.path.getsize(out) >> 20} MiB, {len(names)} undefined '
          f'(listed in {out}.undefined)')


main()
