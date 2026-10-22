#!/usr/bin/env python3
"""Split Mesa's replay module into what the page runs and a cold part.

    web/mesa-split.py ORIG.wasm HOT.txt OUTDIR [--max-cold BYTES] [--cold REGEX]

ORIG.wasm is the link's plain module (gex-wgpu-replay.wasm.orig: the target
links with -sSPLIT_MODULE=1, so the .wasm beside it is wasm-split's
instrumented one and never shipped). HOT.txt lists the functions to keep in
the primary (web/mesa-hot.txt, from a GEX_MESA_PROFILE=1 build's run).
Writes OUTDIR/gex-wgpu-replay.wasm (names kept) and, when anything is left
over, OUTDIR/gex-wgpu-replay.deferred.wasm (names stripped). --max-cold
fails the split when the cold part is that big or bigger (default: no
limit; a Worker compiles it synchronously at any size, and a page's main
thread loads it with the module, web/gex-wgpu-split.js).

HOT.txt `-` keeps every function. --cold moves the functions whose names
match REGEX to the cold part whatever HOT.txt says: a forced miss, for
tests/mesa-cold.

Names are matched as Binaryen prints them, without ThinLTO's `.llvm.<hash>`
suffixes, which change whenever their translation unit does. A name in the
list that the module lacks is ignored; a function the list lacks is cold and
costs one load of the cold part the first time it runs.
"""
import os
import re
import shutil
import subprocess
import sys
import tempfile

LLVM_SUFFIX = re.compile(r'\.llvm\.\d+')
# Binaryen tells functions of the same name apart with a `_N` suffix whose N
# moves whenever the module gains or loses one, so a list is matched with
# those suffixes off too (a duplicate kept with its twin costs little).
DUP_SUFFIX = re.compile(r'_\d+$')


def stable(name):
    return DUP_SUFFIX.sub('', LLVM_SUFFIX.sub('', name))
# The features the link used. The module carries no target_features section
# for Binaryen to detect them from, and `-all` is not a substitute: with it,
# wasm-split writes the cold part's imports in newer encodings (exact
# function imports, kind 0x20) that Chromium refuses to compile. Nor are
# reference types here, though the module would validate with them: they let
# wasm-split put the placeholders in a second table, and Emscripten's glue
# calls the function it loaded through the first (`wasmTable.get(index)`),
# which then holds something else at that index.
FEATURES = ['--mvp-features', '--enable-threads', '--enable-simd', '--enable-bulk-memory',
            '--enable-bulk-memory-opt', '--enable-nontrapping-float-to-int', '--enable-sign-ext',
            '--enable-mutable-globals', '--enable-multivalue']


def binaryen(tool):
    """Binaryen from the Emscripten that linked the module."""
    try:
        root = subprocess.run(['em-config', 'BINARYEN_ROOT'], capture_output=True, text=True,
                              check=True).stdout.strip()
        path = os.path.join(root, 'bin', tool)
        if os.path.exists(path):
            return path
    except (OSError, subprocess.CalledProcessError):
        pass
    return shutil.which(tool) or sys.exit(f'mesa-split: no {tool}')


# -- Trampolines ---------------------------------------------------------------
#
# wasm-split leaves each table slot that reaches a cold function holding an
# imported placeholder, and Emscripten's glue makes every placeholder a JS
# function that loads the cold part and then calls the function from JS. A
# JS frame in the middle of the stack is what JSPI cannot suspend across:
# the first call into a cold function that then waits on WebGPU (a glReadPixels,
# a buffer map) failed with "SuspendError: trying to suspend JS frames". So
# the placeholders are wasm instead: a trampoline per slot that calls the one
# import `gexcold.load` (web/gex-wgpu-split.js: loads the cold part, which
# overwrites the slots) and then calls through its own slot again, now the
# function itself, with no JS frame left on the stack.

PLACEHOLDER = 'placeholder.deferred'
TRAMP = 'gexcoldtramp'
VALTYPES = {0x7f: 'i32', 0x7e: 'i64', 0x7d: 'f32', 0x7c: 'f64', 0x7b: 'v128', 0x70: 'funcref',
            0x6f: 'externref'}


def leb(b, i):
    r = s = 0
    while True:
        x = b[i]
        i += 1
        r |= (x & 0x7f) << s
        s += 7
        if x < 0x80:
            return r, i


def uleb(n):
    out = bytearray()
    while True:
        x = n & 0x7f
        n >>= 7
        out.append(x | (0x80 if n else 0))
        if not n:
            return bytes(out)


def sections(b):
    i, out = 8, []
    while i < len(b):
        sid = b[i]
        size, j = leb(b, i + 1)
        out.append((sid, i, j, j + size))
        i = j + size
    return out


def name(b, i):
    n, i = leb(b, i)
    return b[i:i + n].decode(), i + n


def limits(b, i):
    flags = b[i]
    lo, i = leb(b, i + 1)
    hi = None
    if flags & 1:
        hi, i = leb(b, i)
    return (lo, hi), i


def trampolines(primary, out_path, tmp):
    """Replace PRIMARY's placeholder imports with wasm trampolines, in place."""
    b = open(primary, 'rb').read()
    types, imports, table_export, imp_at = [], [], None, None
    for sid, start, body, end in sections(b):
        if sid == 1:
            n, i = leb(b, body)
            for _ in range(n):
                assert b[i] == 0x60, 'mesa-split: a type other than a function type'
                np, i = leb(b, i + 1)
                params = [VALTYPES[x] for x in b[i:i + np]]
                i += np
                nr, i = leb(b, i)
                results = [VALTYPES[x] for x in b[i:i + nr]]
                i += nr
                types.append((params, results))
        elif sid == 2:
            imp_at = (start, end)
            n, i = leb(b, body)
            for _ in range(n):
                at = i
                mod, i = name(b, i)
                base, i = name(b, i)
                kind = b[i]
                i += 1
                if kind == 0:
                    t, i = leb(b, i)
                elif kind == 1:
                    i += 1
                    _, i = limits(b, i)
                    t = None
                elif kind == 2:
                    _, i = limits(b, i)
                    t = None
                elif kind == 3:
                    i += 2
                    t = None
                else:
                    sys.exit(f'mesa-split: import kind {kind}')
                imports.append((mod, base, kind, t, b[at:i]))
        elif sid == 7:
            n, i = leb(b, body)
            for _ in range(n):
                ex, i = name(b, i)
                kind = b[i]
                idx, i = leb(b, i + 1)
                if kind == 1 and idx == 0:
                    table_export = ex
    holders = [(int(base), t) for mod, base, kind, t, _ in imports if mod == PLACEHOLDER]
    if not holders:
        return 0
    if table_export is None:
        sys.exit('mesa-split: the primary exports no table')
    # The primary, its placeholders imported from the trampolines' module.
    entries = bytearray()
    for mod, base, kind, t, raw in imports:
        if mod == PLACEHOLDER:
            m, nm = TRAMP.encode(), f'{TRAMP}_{base}'.encode()
            raw = uleb(len(m)) + m + uleb(len(nm)) + nm + bytes([0]) + uleb(t)
        entries += raw
    body = uleb(len(imports)) + entries
    start, end = imp_at
    renamed = os.path.join(tmp, 'renamed.wasm')
    with open(renamed, 'wb') as f:
        f.write(b[:start] + bytes([2]) + uleb(len(body)) + body + b[end:])
    # The trampolines.
    wat = [f'(module (import "gexcold" "load" (func $load))',
           f' (import "primary" "{table_export}" (table $t 1 funcref))']
    for k, t in holders:
        params, results = types[t]
        sig = ''.join(f' (param {p})' for p in params) + ''.join(f' (result {r})' for r in results)
        gets = ''.join(f' (local.get {i})' for i in range(len(params)))
        wat.append(f' (func (export "{TRAMP}_{k}"){sig} (call $load)'
                   f' (call_indirect $t{sig}{gets} (i32.const {k})))')
    wat.append(')')
    wat_path = os.path.join(tmp, 'tramp.wat')
    tramp = os.path.join(tmp, 'tramp.wasm')
    with open(wat_path, 'w') as f:
        f.write('\n'.join(wat))
    subprocess.run([binaryen('wasm-as'), *FEATURES, wat_path, '-o', tramp], check=True)
    merged = os.path.join(tmp, 'merged.wasm')
    subprocess.run([binaryen('wasm-merge'), *FEATURES, renamed, 'primary', tramp, TRAMP, '-g',
                    '-o', merged], check=True)
    subprocess.run([binaryen('wasm-opt'), *FEATURES, merged, '-g', '--remove-exports',
                    '--pass-arg', f'remove-exports@{TRAMP}_*', '-o', out_path], check=True)
    return len(holders)


def main():
    args = sys.argv[1:]
    max_cold = None
    cold_re = None
    if '--cold' in args:
        i = args.index('--cold')
        cold_re = re.compile(args[i + 1])
        del args[i:i + 2]
    if '--max-cold' in args:
        i = args.index('--max-cold')
        max_cold = int(args[i + 1])
        del args[i:i + 2]
    if len(args) != 3:
        sys.exit(__doc__)
    orig, hot_path, out = args
    os.makedirs(out, exist_ok=True)
    primary = os.path.join(out, 'gex-wgpu-replay.wasm')
    cold = os.path.join(out, 'gex-wgpu-replay.deferred.wasm')
    if os.path.exists(cold):
        os.remove(cold)
    if hot_path != '-' and not os.path.exists(hot_path):
        shutil.copyfile(orig, primary)
        print(f'mesa-split: no {hot_path}; not split')
        return

    hot = None if hot_path == '-' else {l.strip() for l in open(hot_path) if l.strip() and not l.startswith('#')}
    fmap = subprocess.run([binaryen('wasm-opt'), *FEATURES, orig, '--print-function-map', '-o', os.devnull],
                          capture_output=True, text=True, check=True).stdout
    # `index:name`, imports included (wasm-split never moves those).
    names = [l.split(':', 1)[1] for l in fmap.splitlines() if ':' in l]
    if hot is not None:
        hot = {stable(n) for n in hot}
    keep = [n for n in names if (hot is None or stable(n) in hot)
            and not (cold_re and cold_re.search(n))]
    with tempfile.NamedTemporaryFile('w', suffix='.txt', delete=False) as f:
        f.write(''.join(n + '\n' for n in keep))
        keep_file = f.name
    with tempfile.TemporaryDirectory() as tmp:
        raw_cold = os.path.join(tmp, 'cold.wasm')
        raw_primary = os.path.join(tmp, 'primary.wasm')
        subprocess.run([binaryen('wasm-split'), orig, *FEATURES, '--split', '-g', '-q',
                        '--keep-funcs', '@' + keep_file, '-o1', raw_primary, '-o2', raw_cold],
                       check=True)
        slots = trampolines(raw_primary, primary, tmp)
        if not slots:
            shutil.copyfile(raw_primary, primary)
        subprocess.run([binaryen('wasm-opt'), *FEATURES, raw_cold, '--strip-debug', '-o', cold], check=True)
    os.remove(keep_file)
    size = os.path.getsize(cold)
    print(f'mesa-split: {len(keep)} of {len(names)} functions kept; '
          f'{os.path.getsize(primary)} bytes + {size} cold; {slots} trampolines')
    if hot is not None:
        gone = hot - {stable(n) for n in names}
        if gone:
            # A listed function this build no longer has under that name (a
            # rename leaves the new one cold): re-run tools/mesa-hot.py.
            print(f'mesa-split: {len(gone)} listed names not in this build, e.g. {sorted(gone)[0][:120]}')
    if max_cold is not None and size >= max_cold:
        sys.exit(f'mesa-split: the cold part is {size} bytes, not under {max_cold}')


if __name__ == '__main__':
    main()
