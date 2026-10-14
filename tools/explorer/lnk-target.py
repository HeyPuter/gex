#!/usr/bin/env python3
"""Print a shell link's target and icon paths (MS-SHLLINK), one per line:

    target PATH
    icon PATH

PATH as the link stores it: the expandable path of its environment block
(%windir%\\...) when it has one, else LinkInfo's local base path. A link
whose target is a shell item only (an ID list without a file system path)
prints no target line.

    lnk-target.py FILE.lnk...

--prune DRIVE_C DIR...: delete every link under DIR whose target is a file
that is not in the prefix whose C: is DRIVE_C (%windir%, %SystemRoot%,
%ProgramFiles% and %ProgramData% expanded, names matched case-insensitively,
as Windows does), printing each. Links to shell items, or to targets that
are there, stay; a link to a shell item is judged by its icon's file.
"""
import os
import struct
import sys

HAS_ID_LIST, HAS_LINK_INFO, HAS_NAME, HAS_REL_PATH, HAS_WORKING_DIR, HAS_ARGS, HAS_ICON = (1 << i for i in range(7))
IS_UNICODE = 0x80


def parse(data):
    flags = struct.unpack_from('<I', data, 0x14)[0]
    at = 0x4c
    target = None
    if flags & HAS_ID_LIST:
        at += 2 + struct.unpack_from('<H', data, at)[0]
    if flags & HAS_LINK_INFO:
        size, hdr, li_flags, _vol, base_off = struct.unpack_from('<5I', data, at)
        if li_flags & 1 and base_off:
            if hdr >= 0x24:
                ubase = struct.unpack_from('<I', data, at + 0x1c)[0]
                end = data.index(b'\0\0', at + ubase)
                end += (end - (at + ubase)) % 2
                target = data[at + ubase:end].decode('utf-16le')
            else:
                end = data.index(b'\0', at + base_off)
                target = data[at + base_off:end].decode('mbcs' if sys.platform == 'win32' else 'latin-1')
        at += size
    strings = {}
    for bit in (HAS_NAME, HAS_REL_PATH, HAS_WORKING_DIR, HAS_ARGS, HAS_ICON):
        if flags & bit:
            n = struct.unpack_from('<H', data, at)[0]
            at += 2
            if flags & IS_UNICODE:
                strings[bit] = data[at:at + 2 * n].decode('utf-16le')
                at += 2 * n
            else:
                strings[bit] = data[at:at + n].decode('latin-1')
                at += n
    icon = strings.get(HAS_ICON)
    # extra data blocks: environment (0xa0000001) and icon environment (0xa0000007)
    while at + 8 <= len(data):
        size, sig = struct.unpack_from('<II', data, at)
        if size < 8:
            break
        if sig in (0xa0000001, 0xa0000007):
            w = data[at + 268:at + 268 + 520].decode('utf-16le').split('\0', 1)[0]
            if sig == 0xa0000001 and w:
                target = w
            elif sig == 0xa0000007 and w:
                icon = w
        at += size
    return target, icon


ENV = {'windir': 'windows', 'systemroot': 'windows', 'programfiles': 'Program Files',
       'programdata': 'ProgramData', 'systemdrive': ''}


def resolve(drive_c, path):
    """The file a Windows path names in the prefix, or None."""
    parts = path.replace('/', '\\').split('\\')
    if parts and parts[0].startswith('%') and parts[0].endswith('%'):
        var = parts[0].strip('%').lower()
        if var not in ENV:
            return None
        parts = [p for p in ENV[var].split('/') if p] + parts[1:]
    elif len(parts[0]) == 2 and parts[0][1] == ':':
        if parts[0][0].lower() != 'c':
            return None
        parts = parts[1:]
    else:
        return None
    at = drive_c
    for p in [p for p in parts if p]:
        try:
            names = {n.lower(): n for n in os.listdir(at)}
        except OSError:
            return None
        if p.lower() not in names:
            return None
        at = os.path.join(at, names[p.lower()])
    return at


def prune(drive_c, dirs):
    for d in dirs:
        for root, _, files in os.walk(d):
            for f in files:
                if not f.lower().endswith('.lnk'):
                    continue
                full = os.path.join(root, f)
                try:
                    target, icon = parse(open(full, 'rb').read())
                except (struct.error, ValueError, UnicodeDecodeError):
                    continue
                # A shell item (a game, through the Games folder) is judged by
                # its icon's file, which is the program's own.
                path = target or icon
                if path and resolve(drive_c, path.split(',')[0]) is None:
                    print(f'pruned {os.path.relpath(full, d)} ({path})')
                    os.remove(full)


def main():
    if sys.argv[1:2] == ['--prune']:
        prune(sys.argv[2], sys.argv[3:])
        return
    for f in sys.argv[1:]:
        target, icon = parse(open(f, 'rb').read())
        if target:
            print('target', target)
        if icon:
            print('icon', icon)


if __name__ == '__main__':
    main()
