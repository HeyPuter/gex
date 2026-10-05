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
    if not dlls:
        sys.exit('no DLL objects found (build with winegcc for wasm32)')
    link(dlls, out, os.path.join(build, 'libs', 'gexrt', 'libgexrt.a'))


def link(dlls, out, gexrt):

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

    def resolve_forward(fwd, depth=0):
        """(dll, name) a forward like NTDLL.RtlEnterCriticalSection reaches."""
        dll, _, name = fwd.partition('.')
        dll = dll.lower() + ('' if '.' in dll else '.dll')
        if name.startswith('#') or depth > 8:
            return None
        if dll in exporters.get(name, []):
            return dll, name
        nxt = forwards.get(name, {}).get(dll)
        return resolve_forward(nxt, depth + 1) if nxt else None

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
    sigs = {}  # dll -> {(name, 'defined'|'undefined'): sig}
    for mod, ro, ex, imports in dlls:
        listing = subprocess.run([sys.executable, os.path.join(here, 'pesym.py'), 'sigs', ro],
                                 capture_output=True, text=True, check=True).stdout
        sigs[mod] = {(l.split()[0], l.split()[1]): l.split()[2] for l in listing.splitlines() if len(l.split()) == 3}
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
                if mod in owners:
                    continue  # its own: already resolved inside it
                pick = next((o for o in imports if o in owners), owners[0])
                f.write(f'{name} {pick}\n')
            # names only forwarded (kernel32's EnterCriticalSection is
            # ntdll's RtlEnterCriticalSection, under kernel32.dll!EnterCriticalSection)
            for name, vias in forward_owner.items():
                pick = next((o for o in [mod] + imports if o in vias), vias[0])
                f.write(f'{name} {pick}\n')
        bound = os.path.join(tmp, mod + '.o')
        subprocess.check_call([sys.executable, os.path.join(here, 'pesym.py'), 'imports', local, bound, mod, mapfile])
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

    rsp = os.path.join(tmp, 'exports.rsp')
    with open(rsp, 'w') as f:
        for d in data:
            f.write(f'--export-if-defined={d}\n')
        # gexrt's bounds of the entry-thunk pairs (gexw_entries)
        f.write('--export-if-defined=__gexw_entries\n')
    cmd = ['wasm-ld', '--experimental-pic', '-shared', '--shared-memory', '--import-memory',
           '--max-memory=4294967296', '--unresolved-symbols=import-dynamic', '-o', out,
           f'@{rsp}'] + objs + [gexrt]
    # compiler-rt's builtins (__multi3 and the like), from the toolchain's
    # sysroot (tools/wine-wasm/build-sysroot.sh)
    if os.environ.get('GEX_WASM_SYSROOT'):
        cmd.append(os.path.join(os.environ['GEX_WASM_SYSROOT'], 'lib', 'libclang_rt.builtins-wasm32.a'))
    subprocess.check_call(cmd)
    # The forwards, for gex to bind a -import re-export of a forwarded name
    # (kernel32's HeapFree is kernelbase's, which forwards to ntdll's
    # RtlFreeHeap): a custom section `gexw.forwards`, lines `name dll!target`.
    lines = []
    for name, by in sorted(forwards.items()):
        if name in exporters:
            continue
        hit = resolve_forward(next(iter(by.values())))
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
