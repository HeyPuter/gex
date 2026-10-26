#!/usr/bin/env python3
"""Apply a REGEDIT4 file to a Wine prefix's system.reg offline.

    tools/wine-wasm/reg-apply.py PFX/system.reg FILE.reg...

Handles what the 32-bit Wine session files use: `[-HKEY_LOCAL_MACHINE\\KEY]` deletes
KEY and its subkeys, `[HKEY_LOCAL_MACHINE\\KEY]` followed by `"Name"=dword:N`
or `"Name"="str"` sets values in an existing key. CurrentControlSet is
ControlSet001, as Wine's system.reg stores it. A key to set that is missing
is an error (these files only change keys wineboot made).
"""
import re
import sys

ROOT = 'HKEY_LOCAL_MACHINE\\'


def norm(key):
    if not key.startswith(ROOT):
        sys.exit(f'only {ROOT} keys: {key}')
    key = key[len(ROOT):]
    parts = key.split('\\')
    if len(parts) > 1 and parts[0].lower() == 'system' and parts[1].lower() == 'currentcontrolset':
        parts[1] = 'ControlSet001'
    return '\\\\'.join(parts)  # system.reg doubles its backslashes


def main():
    path, regs = sys.argv[1], sys.argv[2:]
    text = open(path, encoding='utf-8').read()
    blocks = re.split(r'(?=\n\[)', text)
    head = lambda b: b.lstrip('\n').split('\n', 1)[0]
    name_of = lambda b: re.match(r'\[(.*?)\](?: \d+)?$', head(b))
    for reg in regs:
        key = None
        for line in open(reg, encoding='utf-8'):
            line = line.strip()
            if not line or line.startswith(';') or line == 'REGEDIT4':
                continue
            m = re.match(r'\[(-?)(.*)\]$', line)
            if m:
                key = norm(m.group(2))
                if m.group(1):
                    lk = key.lower()
                    blocks = [b for b in blocks if not (name_of(b) and (name_of(b).group(1).lower() == lk or name_of(b).group(1).lower().startswith(lk + '\\\\')))]
                    key = None
                continue
            m = re.match(r'"([^"]+)"=(dword:[0-9a-fA-F]{8}|"[^"]*")$', line)
            if not m or key is None:
                sys.exit(f'{reg}: cannot apply: {line}')
            idx = [i for i, b in enumerate(blocks) if name_of(b) and name_of(b).group(1).lower() == key.lower()]
            if not idx:
                sys.exit(f'{reg}: no key {key} in {path}')
            b = blocks[idx[0]]
            val = f'"{m.group(1)}"={m.group(2)}'
            pat = re.compile(rf'^"{re.escape(m.group(1))}"=.*$', re.M | re.I)
            blocks[idx[0]] = pat.sub(val, b) if pat.search(b) else b.rstrip('\n') + '\n' + val + '\n'
    open(path, 'w', encoding='utf-8').write(''.join(blocks))


main()
