#!/usr/bin/env python3
"""The functions of a wasm module that can reach an import, by a call graph
in which an indirect call of type T can reach every table function of type T
(a call_indirect traps on any other): what asyncify must instrument for fork
(wcc), instead of everything that makes an indirect call.

    forkreach.py [--also M.N,...] [--indirect] [--exports FILE]
                 WASM-DIS MODULE.wasm IMPORT-MODULE.NAME [ROOT ...]

Prints the reaching functions but the ROOTs (fork's driver and what is
above it), comma-separated, for asyncify-onlylist. Exits 1 when the module
cannot be analysed this way (a name asyncify's list cannot carry).

For shared libraries and programs linked against them:
--also names more imports that may fork (other modules' functions their
gex.forkreach summaries say may, or whose provider is unknown); --quoted takes
functions whose names the text format quotes (C++'s, with spaces and
commas) and prints one name per line, as a list file carries them
(asyncify-onlylist@@FILE); --indirect
makes every indirect call reach (the callee may be another module's);
--exports FILE writes the exports that reach, one per line (the module's
own gex.forkreach summary); --ignore F,... names functions taken not to
reach whatever they call (libgex's signal-handler call: a handler that
forks while interrupting a system call is not supported in a shared
program)."""
import re, subprocess, sys

argv = sys.argv[1:]
also, indirect, exports_out, ignore, quoted = set(), False, None, set(), False
while argv and argv[0].startswith('--'):
    o = argv.pop(0)
    if o == '--also':
        also |= {tuple(x.split('.', 1)) for x in argv.pop(0).split(',') if x}
    elif o == '--indirect':
        indirect = True
    elif o == '--exports':
        exports_out = argv.pop(0)
    elif o == '--ignore':
        ignore |= set(argv.pop(0).split(','))
    elif o == '--quoted':
        quoted = True
dis, path, target = argv[:3]
roots = set(argv[3:])
mod, imp = target.split('.', 1)
text = subprocess.run([dis, path], capture_output=True, text=True, check=True).stdout
# (--quoted: each quoted name stands in the text as a token of its own)
qnames, qtok = [], {}
if quoted:
    def _q(m):
        t = qtok.get(m.group(1))
        if t is None:
            t = qtok[m.group(1)] = '$\x00q%d' % len(qnames)
            qnames.append(m.group(1))
        return t
    text = re.sub(r'\$"((?:[^"\\]|\\.)*)"', _q, text)

NAME = r'\$([^\s()]+)'
types = {}           # type name -> signature
import_name = None   # the import's function name
targets = set()      # every import that reaches (import_name and --also)
exports = {}         # export name -> function name
funcs = {}           # name -> signature
calls = {}           # name -> set of direct callees
icalls = {}          # name -> set of signatures called indirectly
table = set()        # functions in element segments


def sig_of(s):
    params = re.findall(r'\(param(?: \$[^\s()]+)?((?: [a-z0-9]+)+)\)', s)
    results = re.findall(r'\(result((?: [a-z0-9]+)+)\)', s)
    return ' '.join(p.strip() for p in params) + ' -> ' + ' '.join(r.strip() for r in results)


cur = None
for line in text.split('\n'):
    m = re.match(r' \(type ' + NAME + r' \(func(.*)\)\)\s*$', line)
    if m:
        types[m.group(1)] = sig_of(m.group(2))
        continue
    m = re.match(r' \(import "([^"]*)" "([^"]*)" \(func ' + NAME + r'(.*)\)\)\s*$', line)
    if m:
        if (m.group(1), m.group(2)) == (mod, imp):
            import_name = m.group(3)
            targets.add(m.group(3))
        if (m.group(1), m.group(2)) in also:
            targets.add(m.group(3))
        continue
    m = re.match(r' \(export "([^"]*)" \(func ' + NAME + r'\)\)\s*$', line)
    if m:
        exports[m.group(1)] = m.group(2)
        continue
    m = re.match(r' \(elem .*', line)
    if m:
        table.update(re.findall(NAME, line.split(')', 1)[1] if 'global.get' in line else line))
        continue
    m = re.match(r' \(func ' + NAME + r'(.*)$', line)
    if m:
        cur = m.group(1)
        funcs[cur] = sig_of(m.group(2))
        calls[cur] = set()
        icalls[cur] = set()
        continue
    if cur is None:
        continue
    for c in re.findall(r'\((?:return_)?call ' + NAME, line):
        calls[cur].add(c)
    for t in re.findall(r'\((?:return_)?call_indirect (?:' + NAME + r' )?\(type ' + NAME + r'\)', line):
        icalls[cur].add(types.get(t[1], '?'))

if import_name is None and not targets and not indirect:
    sys.exit(1)
by_sig = {}
for f in table:
    if f in funcs:
        by_sig.setdefault(funcs[f], set()).add(f)

reach = set(targets)
changed = True
while changed:
    changed = False
    for f in funcs:
        if f in reach or f in ignore:
            continue
        if calls[f] & reach or (indirect and icalls[f]) or any(by_sig.get(s, set()) & reach for s in icalls[f]):
            reach.add(f)
            changed = True
if exports_out:
    with open(exports_out, 'w') as fh:
        for e in sorted(e for e, f in exports.items() if f in reach):
            fh.write(e + '\n')
out = sorted(f for f in reach if f in funcs and f not in roots)
if quoted:
    def _name(f):
        return qnames[int(f[2:])] if f.startswith('\x00q') else f
    out = [_name(f) for f in out]
    if any('\n' in f or '\\' in f for f in out):
        sys.exit(1)
    print('\n'.join(out))
    sys.exit(0)
if any(re.search(r'[,"\\\s]', f) for f in out):
    sys.exit(1)
print(','.join(out))
