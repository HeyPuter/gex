#!/usr/bin/env python3
"""The functions of a wasm module that can reach an import, by a call graph
in which an indirect call of type T can reach every table function of type T
(a call_indirect traps on any other): what asyncify must instrument for fork
(wcc), instead of everything that makes an indirect call.

    forkreach.py WASM-DIS MODULE.wasm IMPORT-MODULE.NAME [ROOT ...]

Prints the reaching functions but the ROOTs (fork's driver and what is
above it), comma-separated, for asyncify-onlylist. Exits 1 when the module
cannot be analysed this way (a name asyncify's list cannot carry)."""
import re, subprocess, sys

dis, path, target = sys.argv[1:4]
roots = set(sys.argv[4:])
mod, imp = target.split('.', 1)
text = subprocess.run([dis, path], capture_output=True, text=True, check=True).stdout

NAME = r'\$([^\s()]+)'
types = {}           # type name -> signature
import_name = None   # the import's function name
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

if import_name is None:
    sys.exit(1)
by_sig = {}
for f in table:
    if f in funcs:
        by_sig.setdefault(funcs[f], set()).add(f)

reach = {import_name}
changed = True
while changed:
    changed = False
    for f in funcs:
        if f in reach:
            continue
        if calls[f] & reach or any(by_sig.get(s, set()) & reach for s in icalls[f]):
            reach.add(f)
            changed = True
out = sorted(f for f in reach if f in funcs and f not in roots)
if any(re.search(r'[,"\\\s]', f) for f in out):
    sys.exit(1)
print(','.join(out))
