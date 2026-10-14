#!/usr/bin/env python3
"""Make the Start menu shortcuts Windows setup makes from component manifests.

    manifest-lnk.py MANIFESTS_DIR DRIVE_C [--only NAME.lnk ...]

An extracted install image lacks the shortcuts that setup creates from the
<shortCut> elements of its components' manifests (Windows\\winsxs\\Manifests):
Sticky Notes, Snipping Tool, Media Center and others. For each one into the
all users' Start menu ($(runtime.startMenu)) or the default user's profile
that DRIVE_C does not already have, this writes the shell link (MS-SHLLINK:
target and icon as environment strings, description, arguments) and its
LocalizedFileNames line in the folder's desktop.ini, as setup does. It does
not copy the programs: stage.sh's lnk-target.py --prune then keeps only the
links whose programs the prefix has.

--link DRIVE_C FOLDER NAME TARGET [DESCRIPTION [DISPLAY]] writes one link
the same way (FOLDER relative to DRIVE_C; TARGET also its icon), unless
the folder has it already.

Default users' links go to every profile under DRIVE_C/users that has a
Start menu (a profile made from the default one has its links). --only
limits the work to the named links.
"""
import glob
import os
import re
import struct
import sys
import uuid

LINK_CLSID = bytes.fromhex('0114020000000000c000000000000046')
HAS_ID_LIST = 0x1
HAS_NAME, HAS_REL_PATH, HAS_ARGS, HAS_ICON, IS_UNICODE, FORCE_NO_LINK_INFO, HAS_EXP_STRING, HAS_EXP_ICON = \
    0x4, 0x8, 0x20, 0x40, 0x80, 0x100, 0x200, 0x4000
PREFER_ENVIRONMENT_PATH = 0x2000000
SM = 'ProgramData/Microsoft/Windows/Start Menu'
USER_SM = 'AppData/Roaming/Microsoft/Windows/Start Menu'
VARS = {
    'system32': '%SystemRoot%\\system32',
    'windows': '%SystemRoot%',
    'programfiles': '%ProgramFiles%',
}


def expand(s):
    """A manifest's $(runtime.x) variables as the environment strings a link holds."""
    return re.sub(r'\$\(runtime\.(\w+)\)', lambda m: VARS.get(m.group(1).lower(), m.group(0)), s)


def env_block(sig, path):
    ansi = path.encode('mbcs' if os.name == 'nt' else 'latin-1', 'replace')[:259].ljust(260, b'\0')
    wide = path.encode('utf-16le')[:518].ljust(520, b'\0')
    return struct.pack('<II', 0x314, sig) + ansi + wide


ROOTS = (('%systemroot%', 'Windows'), ('%windir%', 'Windows'), ('%programfiles%', 'Program Files'))


def rel_path(target, folder_rel):
    """The target relative to the link's folder (FOLDER_REL: from C:), as setup's links have it."""
    t = target
    for var, real in ROOTS:
        if t.lower().startswith(var): t = real + t[len(var):]; break
    else:
        if not re.match(r'[A-Za-z]:\\', t): return ''
        t = t[3:]
    depth = len([x for x in re.split(r'[\\/]', folder_rel) if x])
    return '..\\' * depth + t


def game_id_list(target):
    """The ID list of a game in the Games folder, 'shell:::{FOLDER}\\{GAME}', as
    setup's links to games have it: the folder's root item, then the game's
    (signature GFSI)."""
    m = re.match(r'shell:::(\{[-0-9A-Fa-f]+\})\\(\{[-0-9A-Fa-f]+\})$', target)
    if not m: return None
    folder, game = (uuid.UUID(g).bytes_le for g in m.groups())
    items = struct.pack('<HBB', 0x14, 0x1f, 0x80) + folder + \
        struct.pack('<HH', 0x20, 0) + b'GFSI' + game + b'\0' * 8
    return struct.pack('<H', len(items) + 2) + items + b'\0\0'


def lnk(target, args='', icon='', index=0, desc='', show=1, rel=''):
    """A link as Windows setup makes one: no ID list or link info (ForceNoLinkInfo), the
    target as an environment string (PreferEnvironmentPath) and relative to the link;
    a game's, through the Games folder, as an ID list."""
    ids = game_id_list(target)
    flags = IS_UNICODE | FORCE_NO_LINK_INFO | (HAS_ID_LIST if ids else HAS_EXP_STRING | PREFER_ENVIRONMENT_PATH)
    strings = b''
    for flag, s in ((HAS_NAME, desc), (HAS_REL_PATH, '' if ids else rel), (HAS_ARGS, args), (HAS_ICON, icon)):
        if s:
            flags |= flag
            strings += struct.pack('<H', len(s)) + s.encode('utf-16le')
    if icon: flags |= HAS_EXP_ICON
    head = struct.pack('<I16sII', 0x4c, LINK_CLSID, flags, 0x20) + b'\0' * 24 + \
        struct.pack('<IiIHHII', 0, index, show, 0, 0, 0, 0)
    assert len(head) == 0x4c
    extra = b'' if ids else env_block(0xA0000001, target)
    if icon: extra += env_block(0xA0000007, icon)
    return head + (ids or b'') + strings + extra + b'\0\0\0\0'


def shortcuts(mdir):
    """{(folder kind, folder, name): attributes} from every manifest, the last version's winning."""
    out = {}
    for f in sorted(glob.glob(os.path.join(mdir, '*.manifest'))):
        try: s = open(f, encoding='utf-8', errors='replace').read()
        except OSError: continue
        for m in re.finditer(r'<shortCut\b[^>]*>', s, re.I):
            a = dict(re.findall(r'(\w+)="([^"]*)"', m.group()))
            dest, name = a.get('destinationPath', ''), a.get('destinationName', '')
            if not name.lower().endswith('.lnk') or 'targetPath' not in a: continue
            d = dest.replace('/', '\\').rstrip('\\')
            mm = re.match(r'\$\(runtime\.startmenu\)(.*)$', d, re.I)
            if mm: kind, sub = 'common', mm.group(1)
            else:
                mm = re.match(r'\$\(runtime\.documentsSettings\)\\Default\\AppData\\Roaming\\Microsoft\\Windows\\Start Menu(.*)$', d, re.I)
                if not mm: continue
                kind, sub = 'user', mm.group(1)
            out[(kind, sub.strip('\\'), name)] = a
    return out


def find_ci(base, rel):
    """base/rel with each component matched case-insensitively (or as given)."""
    p = base
    for part in [x for x in rel.split('\\') if x]:
        hit = None
        if os.path.isdir(p):
            for e in os.listdir(p):
                if e.lower() == part.lower(): hit = e; break
        p = os.path.join(p, hit or part)
    return p


def add_localized(folder, name, resource):
    ini = os.path.join(folder, 'desktop.ini')
    for e in os.listdir(folder):
        if e.lower() == 'desktop.ini': ini = os.path.join(folder, e)
    text = ''
    if os.path.exists(ini):
        raw = open(ini, 'rb').read()
        text = raw.decode('utf-16') if raw[:2] in (b'\xff\xfe', b'\xfe\xff') else raw.decode('latin-1')
    if re.search(r'^' + re.escape(name) + r'=', text, re.M | re.I): return
    line = '%s=%s' % (name, resource)
    if re.search(r'^\[LocalizedFileNames\]', text, re.M | re.I):
        text = re.sub(r'(^\[LocalizedFileNames\][^\n]*\n)', lambda m: m.group(1) + line + '\r\n', text, count=1, flags=re.M | re.I)
    else:
        text = text.rstrip('\r\n') + ('\r\n' if text else '') + '[LocalizedFileNames]\r\n' + line + '\r\n'
    open(ini, 'wb').write(b'\xff\xfe' + text.encode('utf-16le'))


def main():
    args = sys.argv[1:]
    if args[:1] == ['--link']:
        drive_c, folder, name, target = args[1:5]
        desc = args[5] if len(args) > 5 else ''
        folder = find_ci(drive_c, folder.replace('/', '\\'))
        path = find_ci(folder, name)
        if not os.path.exists(path):
            os.makedirs(folder, exist_ok=True)
            open(path, 'wb').write(lnk(target, '', target, 0, desc, 1, rel_path(target, os.path.relpath(folder, drive_c))))
            if len(args) > 6: add_localized(folder, os.path.basename(path), args[6])
            print('%s\\%s' % (os.path.relpath(folder, drive_c), os.path.basename(path)))
        return
    only = None
    if '--only' in args:
        i = args.index('--only'); only = {n.lower() for n in args[i + 1:]}; args = args[:i]
    mdir, drive_c = args
    roots = {'common': [os.path.join(drive_c, SM)],
             'user': [p for p in glob.glob(os.path.join(drive_c, 'users', '*', USER_SM)) if not os.path.islink(p)]}
    for (kind, sub, name), a in sorted(shortcuts(mdir).items()):
        if only is not None and name.lower() not in only: continue
        for root in roots[kind]:
            if not os.path.isdir(root): continue
            folder = find_ci(root, sub)
            path = find_ci(folder, name)
            if os.path.exists(path): continue
            icon, index = '', 0
            if a.get('iconPath'):
                ip = expand(a['iconPath'])
                mm = re.match(r'(.*),(-?\d+)$', ip)
                icon, index = (mm.group(1), int(mm.group(2))) if mm else (ip, 0)
            show = {'normal': 1, 'maximized': 3, 'minimized': 7}.get(a.get('windowStyle', 'normal').lower(), 1)
            os.makedirs(folder, exist_ok=True)
            target = expand(a['targetPath'])
            open(path, 'wb').write(lnk(target, expand(a.get('arguments', '')), icon, index,
                                       expand(a.get('description', '')), show,
                                       rel_path(target, os.path.relpath(folder, drive_c))))
            res = a.get('displayResource')
            if res:
                mm = re.match(r'(.*),(-?\d+)$', expand(res))
                if mm: add_localized(folder, os.path.basename(path), '@%s,-%d' % (mm.group(1), abs(int(mm.group(2)))))
            print('%s\\%s' % (os.path.relpath(folder, drive_c), os.path.basename(path)))


if __name__ == '__main__':
    main()
