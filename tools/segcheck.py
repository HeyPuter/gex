"""Does gexfillall write every passive segment the module has?

If a closure library holds N passive segments and its gexfillall body has
fewer than N table.init instructions, the missing slots are never written and
the guest branches into them. That is exactly what an illegal instruction in
the guest looks like, and it can be settled without a browser.
"""
import sys

def leb(b, i):
    n = 0
    shift = 0
    while True:
        c = b[i]
        i += 1
        n |= (c & 0x7F) << shift
        shift += 7
        if not c & 0x80:
            return n, i

def sections(b):
    i = 8
    while i < len(b):
        sid = b[i]
        i += 1
        size, i = leb(b, i)
        yield sid, i, i + size
        i += size

def analyse(path):
    b = open(path, 'rb').read()
    elems = []          # (flag, ) per segment
    exports = {}
    code = None
    func_imports = 0
    for sid, s, e in sections(b):
        if sid == 2:  # imports
            n, i = leb(b, s)
            for _ in range(n):
                ln, i = leb(b, i); i += ln
                ln, i = leb(b, i); i += ln
                kind = b[i]; i += 1
                if kind == 0:
                    _, i = leb(b, i)
                    func_imports += 1
                elif kind == 1:
                    i += 1
                    lim = b[i]; i += 1
                    _, i = leb(b, i)
                    if lim: _, i = leb(b, i)
                elif kind == 2:
                    lim = b[i]; i += 1
                    _, i = leb(b, i)
                    if lim: _, i = leb(b, i)
                elif kind == 3:
                    i += 2
        elif sid == 7:  # exports
            n, i = leb(b, s)
            for _ in range(n):
                ln, i = leb(b, i)
                name = b[i:i+ln].decode('utf8', 'replace'); i += ln
                kind = b[i]; i += 1
                idx, i = leb(b, i)
                exports[name] = (kind, idx)
        elif sid == 9:  # elements
            n, i = leb(b, s)
            for _ in range(n):
                flag, i = leb(b, i)
                elems.append(flag)
                if flag == 0x00:
                    while b[i] != 0x0B: i += 1
                    i += 1
                    cnt, i = leb(b, i)
                    for _ in range(cnt): _, i = leb(b, i)
                elif flag == 0x01:
                    i += 1  # elemkind
                    cnt, i = leb(b, i)
                    for _ in range(cnt): _, i = leb(b, i)
                elif flag == 0x02:
                    _, i = leb(b, i)  # table
                    while b[i] != 0x0B: i += 1
                    i += 1
                    i += 1  # elemkind
                    cnt, i = leb(b, i)
                    for _ in range(cnt): _, i = leb(b, i)
                else:
                    return path, f'unhandled element flag {flag}'
        elif sid == 10:
            code = (s, e)

    passive = sum(1 for f in elems if f == 0x01)
    if 'gexfillall' not in exports:
        return path, f'{len(elems)} segments ({passive} passive), no gexfillall'

    idx = exports['gexfillall'][1] - func_imports
    s, e = code
    n, i = leb(b, s)
    body = None
    for k in range(n):
        size, i = leb(b, i)
        if k == idx:
            body = b[i:i+size]
            break
        i += size
    inits = sum(1 for j in range(len(body) - 1)
                if body[j] == 0xFC and body[j + 1] == 12)
    ok = 'OK' if inits == passive else 'MISMATCH'
    return path, (f'{len(elems)} segments, {passive} passive, '
                  f'{inits} table.init in gexfillall -- {ok}')

for p in sys.argv[1:]:
    name, msg = analyse(p)
    print(f'{name.split("/")[-1]:<44} {msg}')
