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
    data = []
    for mod, ro, ex, _ in dlls:
        for line in open(ex):
            parts = line.split()
            if not parts:
                continue
            exporters.setdefault(parts[0], []).append(mod)
            if len(parts) > 1 and parts[1] == 'data':
                data.append(f'{mod}!{parts[0]}')

    tmp = tempfile.mkdtemp(prefix='pelink-')
    objs = []
    for mod, ro, ex, imports in dlls:
        local = os.path.join(tmp, mod + '.l.o')
        subprocess.check_call([sys.executable, os.path.join(here, 'pesym.py'), 'dll', ro, local, mod, ex])
        mapfile = os.path.join(tmp, mod + '.map')
        with open(mapfile, 'w') as f:
            for name, owners in exporters.items():
                if mod in owners:
                    continue  # its own: already resolved inside it
                pick = next((o for o in imports if o in owners), owners[0])
                f.write(f'{name} {pick}\n')
        bound = os.path.join(tmp, mod + '.o')
        subprocess.check_call([sys.executable, os.path.join(here, 'pesym.py'), 'imports', local, bound, mod, mapfile])
        objs.append(bound)

    rsp = os.path.join(tmp, 'exports.rsp')
    with open(rsp, 'w') as f:
        for d in data:
            f.write(f'--export-if-defined={d}\n')
    cmd = ['wasm-ld', '--experimental-pic', '-shared', '--shared-memory', '--import-memory',
           '--max-memory=4294967296', '--unresolved-symbols=import-dynamic', '-o', out,
           f'@{rsp}'] + objs + [gexrt]
    subprocess.check_call(cmd)
    undefined = subprocess.run(['wasm-objdump', '-x', '-j', 'Import', out], capture_output=True, text=True).stdout
    names = sorted(set(re.findall(r'<- env\.(\S+)', undefined)) - {'memory', '__indirect_function_table',
                   '__stack_pointer', '__memory_base', '__table_base'})
    open(out + '.undefined', 'w').write('\n'.join(names) + '\n')
    print(f'{out}: {len(dlls)} DLLs, {os.path.getsize(out) >> 20} MiB, {len(names)} undefined '
          f'(listed in {out}.undefined)')


main()
