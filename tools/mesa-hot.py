#!/usr/bin/env python3
"""Make web/mesa-hot.txt, the functions Mesa's replay keeps out of its cold part.

    tools/mesa-hot.py PLAIN.wasm LOG... [-o web/mesa-hot.txt] [--callee-max BYTES]
                      [--fanout-max N] [--also NAMES.txt]... [--drop REGEX]...

PLAIN.wasm is the profiling build's module before instrumentation; each LOG
holds the `[gex-wgpu] mesa-profile N HITS BASE64` lines the instrumented one
printed while a workload ran (docs/mesa-wasm-size.md, "Measuring"). A
function is hot if any LOG saw it run. The margin adds what a hot function
calls directly when it is at most --callee-max bytes (default 1024): the
error and edge paths a hot function reaches on another workload's input.
A caller of more than --fanout-max functions (default 64) adds none: that is
a dispatcher, like the wire decoder's `one()` over its 2,520 per-command
decoders, and its callees are as cold as the profiles say.

--also adds the names in a list this script wrote before, for another build
of the same sources (a page run's profile outlives a change elsewhere in
Mesa: names are matched, not indices). --callee-max 0 leaves the margin out.
--drop leaves out every function whose name matches REGEX (Python, searched),
from the profiles and the margin both: code a workload ran that a later
change keeps from running (`spvtools::val::`, the SPIR-V validator, which
runs only with TINT_SPIRV_VALIDATE=1).

Prints the hot set per log, the union and the margin, in functions and code
bytes. The list holds Binaryen's names without ThinLTO's `.llvm.<hash>`
suffixes (web/mesa-split.py matches them the same way).
"""
import base64
import os
import re
import shutil
import subprocess
import sys

LLVM_SUFFIX = re.compile(r'\.llvm\.\d+')
# Binaryen tells functions of the same name apart with a `_N` suffix whose N
# moves whenever the module gains or loses one, so a list is matched with
# those suffixes off too (a duplicate kept with its twin costs little).
DUP_SUFFIX = re.compile(r'_\d+$')


def stable(name):
    return DUP_SUFFIX.sub('', LLVM_SUFFIX.sub('', name))
PROFILE = re.compile(r'mesa-profile (\d+) (\d+) (?:@(\d+) )?([A-Za-z0-9+/=]+)')


def tool(name):
    try:
        root = subprocess.run(['em-config', 'BINARYEN_ROOT'], capture_output=True, text=True,
                              check=True).stdout.strip()
        if os.path.exists(os.path.join(root, 'bin', name)):
            return os.path.join(root, 'bin', name)
    except (OSError, subprocess.CalledProcessError):
        pass
    return shutil.which(name) or sys.exit(f'no {name}')


def leb(b, i):
    r = s = 0
    while True:
        x = b[i]
        i += 1
        r |= (x & 0x7f) << s
        s += 7
        if x < 0x80:
            return r, i


def body_sizes(path):
    """Code bytes of each defined function, in order."""
    b = open(path, 'rb').read()
    i = 8
    while i < len(b):
        sid = b[i]
        size, i = leb(b, i + 1)
        if sid == 10:
            n, j = leb(b, i)
            out = []
            for _ in range(n):
                s, k = leb(b, j)
                out.append(s + k - j)
                j = k + s
            return out
        i += size
    return []


def main():
    args = sys.argv[1:]
    out, callee_max, fanout_max, also, drop = None, 1024, 64, [], []
    while '--drop' in args:
        k = args.index('--drop')
        drop.append(re.compile(args[k + 1]))
        del args[k:k + 2]
    while '--also' in args:
        k = args.index('--also')
        also.append(args[k + 1])
        del args[k:k + 2]
    if '-o' in args:
        k = args.index('-o')
        out = args[k + 1]
        del args[k:k + 2]
    if '--fanout-max' in args:
        k = args.index('--fanout-max')
        fanout_max = int(args[k + 1])
        del args[k:k + 2]
    if '--callee-max' in args:
        k = args.index('--callee-max')
        callee_max = int(args[k + 1])
        del args[k:k + 2]
    if len(args) < 1 or (len(args) < 2 and not also):
        sys.exit(__doc__)
    plain, logs = args[0], args[1:]
    fmap = subprocess.run([tool('wasm-opt'), '-all', plain, '--print-function-map', '-o', os.devnull],
                          capture_output=True, text=True, check=True).stdout
    names = [l.split(':', 1)[1] for l in fmap.splitlines() if ':' in l]
    sizes = body_sizes(plain)
    nimp = len(names) - len(sizes)
    defined = names[nimp:]
    total = sum(sizes)
    print(f'{plain}: {len(defined)} functions, {total} code bytes')

    def report(what, idx):
        b = sum(sizes[i] for i in idx)
        print(f'  {what}: {len(idx)} functions ({100 * len(idx) / len(defined):.1f}%), '
              f'{b} bytes ({100 * b / total:.1f}%)')

    union = set()
    for log in logs:
        seen = set()
        for line in open(log, errors='replace'):
            m = PROFILE.search(line)
            if not m:
                continue
            n = int(m.group(1))
            if n != len(defined):
                sys.exit(f'{log}: a profile of {n} functions, not this module\'s {len(defined)}')
            # `@OFFSET`: a piece of the set from that byte on (web/gex-wgpu-jspi.js);
            # without one, the whole set. A line a harness cut short still
            # gives the functions it got as far as.
            text = m.group(4)
            bits = base64.b64decode(text[:len(text) // 4 * 4])
            base = int(m.group(3) or 0) * 8
            seen |= {base + i for i in range(min(len(bits) * 8, n - base)) if bits[i >> 3] >> (i & 7) & 1}
        report(os.path.basename(log), seen)
        union |= seen
    by_name = {}
    for i, n in enumerate(defined):
        by_name.setdefault(stable(n), []).append(i)
    for path in also:
        names = {l.strip() for l in open(path) if l.strip() and not l.startswith('#')}
        seen = {i for n in names for i in by_name.get(stable(n), ())}
        report(f'{os.path.basename(path)} ({len(names)} names, {len({n for n in names if stable(n) not in by_name})} not in this build)', seen)
        union |= seen
    unescape = lambda n: re.sub(r'\\([0-9a-f]{2})', lambda m: chr(int(m.group(1), 16)), n)
    dropped = {i for i in range(len(defined))
               if any(r.search(defined[i]) or r.search(unescape(defined[i])) for r in drop)}
    if drop:
        report('dropped (--drop)', union & dropped)
        union -= dropped
    report('union', union)

    # Direct calls, from Binaryen's text: `(func $name` opens a body.
    index = {n: i for i, n in enumerate(defined)}
    calls, cur = {}, None
    head, call = re.compile(r'^ \(func \$(\S+)'), re.compile(r'\(call \$([^\s)]+)')
    dis = subprocess.Popen([tool('wasm-dis'), plain], stdout=subprocess.PIPE, text=True,
                           errors='replace')
    for line in dis.stdout:
        m = head.match(line)
        if m:
            cur = index.get(m.group(1))
            continue
        if cur in union and '(call $' in line:
            for c in call.findall(line):
                j = index.get(c)
                if j is not None:
                    calls.setdefault(cur, set()).add(j)
    dis.wait()
    margin = {j for i in union if len(calls.get(i, ())) <= fanout_max
              for j in calls.get(i, ()) if j not in union and j not in dropped and sizes[j] <= callee_max}
    report(f'margin (callees up to {callee_max} bytes of callers of up to {fanout_max})', margin)
    keep = union | margin
    report('kept', keep)
    if out:
        with open(out, 'w') as f:
            f.write('# Mesa replay functions kept out of the cold part: tools/mesa-hot.py,\n'
                    '# docs/mesa-wasm-size.md. Binaryen names, without .llvm.<hash>.\n')
            f.writelines(sorted({LLVM_SUFFIX.sub('', defined[i]) + '\n' for i in keep}))
        print(f'wrote {out}')


if __name__ == '__main__':
    main()
