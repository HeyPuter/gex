#!/usr/bin/env python3
"""Set string values in a Wine prefix's user.reg, offline.

    user-reg.py USER_REG KEY NAME=VALUE... [-- KEY NAME=VALUE...]...

Each KEY (e.g. 'Software\\Wine\\DllOverrides') keeps its other values; the
named ones are set (created with the key if need be). A VALUE of the form
dword:XXXXXXXX is a REG_DWORD, anything else a string.
"""
import re, sys, time

def set_values(text, key, vals):
    hdr = '[%s]' % key.replace('\\', '\\\\')
    m = re.search(r'\n' + re.escape(hdr) + r' \d+\n((?:(?!\n\[).)*)', text, flags=re.S | re.I)
    # the key's lines, by value name (other kinds of values kept as they are)
    old = dict(re.findall(r'^("[^"]+"|@)=(.*)$', m.group(1), flags=re.M)) if m else {}
    if m: text = text[:m.start()] + text[m.end():]
    for k, v in vals.items():
        old['"%s"' % k] = v if re.match(r'^dword:[0-9a-fA-F]{8}$', v) else '"%s"' % v
    return text.rstrip('\n') + '\n\n%s %d\n' % (hdr, int(time.time())) + \
        ''.join('%s=%s\n' % kv for kv in sorted(old.items()))

def main():
    path, args = sys.argv[1], sys.argv[2:]
    text = open(path, encoding='utf-8').read()
    groups, cur = [], []
    for a in args:
        if a == '--':
            groups.append(cur); cur = []
        else: cur.append(a)
    groups.append(cur)
    for g in groups:
        if not g: continue
        key, vals = g[0], dict(v.split('=', 1) for v in g[1:])
        text = set_values(text, key, vals)
    open(path, 'w', encoding='utf-8').write(text + '\n')

if __name__ == '__main__':
    main()
