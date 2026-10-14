#!/usr/bin/env python3
"""Copy registry keys from a Windows 7 hive into a Wine prefix, offline.

Windows 7 keeps its service definitions, COM classes and shell settings in
the hives under Windows/System32/config (SYSTEM, SOFTWARE) and in the default
user's NTUSER.DAT. This reads a subtree of such a hive and writes it into the
prefix's system.reg or user.reg in Wine's text format. A key that exists is
replaced as a whole, values and all; its subkeys are replaced only if the
hive has them.

    hive-reg.py PFX SPEC...
    hive-reg.py --com SOFTWARE_HIVE PFX DLL...
    hive-reg.py --ns SOFTWARE_HIVE PFX KEY...

The second form copies the COM classes the images serve (Classes\\CLSID keys
whose InProcServer32 or LocalServer32 is one of them), their AppIDs, the
type libraries they hold and the interfaces their proxies or type libraries
marshal, into the prefix's system.reg: native images find each other's objects
through these registrations, as on Windows.

The third copies the classes registered under namespace keys (KEY like
Microsoft\\Windows\\CurrentVersion\\Explorer\\Desktop\\NameSpace, whose subkeys
are class IDs): the shell reads a namespace item's attributes (hidden, not
enumerated) from its class, so a class whose DLL is not staged still needs
its registration.

Each SPEC is HIVE:SRCKEY[=DSTKEY][@FILE][!], where HIVE is a hive file, SRCKEY
a key path inside it (without the hive's root name), DSTKEY where it goes in
Wine's registry (default: SRCKEY, with ControlSet001 written as
CurrentControlSet), and FILE system (default) or user. A trailing ! replaces
the destination's whole subtree: subkeys the hive does not have go (Wine's
registration of a class Windows registers otherwise). For example:

    hive-reg.py pfx 'SYSTEM:ControlSet001\\Services\\Themes=System\\CurrentControlSet\\Services\\Themes'

Needs python-registry (PYTHONPATH, or REGISTRY_LIB=dir).
"""
import os, re, sys, time

lib = os.environ.get('REGISTRY_LIB')
if lib: sys.path.insert(0, lib)
from Registry import Registry  # noqa: E402

def esc(s):
    out = []
    for c in s:
        o = ord(c)
        if c == '\\': out.append('\\\\')
        elif c == '"': out.append('\\"')
        elif c == '\n': out.append('\\n')
        elif c == '\r': out.append('\\r')
        elif c == '\t': out.append('\\t')
        elif c == '\0': out.append('\\0')
        elif o < 0x20 or o > 0x7e: out.append('\\x%04x' % o)
        else: out.append(c)
    return ''.join(out)

def hexbytes(b):
    return ','.join('%02x' % x for x in b)

def value_line(v):
    name = v.name()
    # python-registry names a key's default (unnamed) value "(default)"
    n = '@' if name in ('', '(default)') else '"%s"' % esc(name)
    t = v.value_type()
    raw = v.raw_data()
    if t == Registry.RegSZ:
        s = v.value().rstrip('\0')
        # the image was built on drive D: (D:\Windows, D:\Program Files);
        # installed, and in the prefix, it is C:
        if re.match(r'^[Dd]:\\', s): s = 'C' + s[1:]
        return '%s="%s"' % (n, esc(s))
    if t == Registry.RegExpandSZ:
        return '%s=str(2):"%s"' % (n, esc(v.value().rstrip('\0')))
    if t == Registry.RegMultiSZ:
        items = [s for s in v.value()]
        while items and items[-1] == '': items.pop()
        return '%s=str(7):"%s"' % (n, esc(''.join(s + '\0' for s in items)))
    if t == Registry.RegDWord and len(raw) == 4:
        return '%s=dword:%08x' % (n, v.value())
    if t == Registry.RegBin:
        return '%s=hex:%s' % (n, hexbytes(raw))
    return '%s=hex(%x):%s' % (n, t, hexbytes(raw))

def placeholder(v):
    # setup leaves IniFileMapping placeholders ("#USR:key") in the default
    # user's hive where the installed system has no value
    return v.value_type() == Registry.RegSZ and str(v.value()).startswith(('#USR:', '#SYS:'))

def collect(key, dst, out):
    out.append((dst, [value_line(v) for v in key.values() if not placeholder(v)]))
    for sub in key.subkeys():
        collect(sub, dst + '\\' + sub.name(), out)

def default_dst(src):
    return re.sub(r'^ControlSet00\d\\', r'CurrentControlSet\\', src, flags=re.I) if src.lower().startswith('controlset') \
        else src

def merge(path, keys, trees=()):
    text = open(path, encoding='utf-8').read()
    stamp = int(time.time())
    for t in trees:
        pre = re.escape('[' + t.replace('\\', '\\\\'))
        text = re.sub(r'\n' + pre + r'(?:\\\\[^\]]*)?\] \d+\n(?:(?!\n\[).)*', '\n', text, flags=re.S | re.I)
    for k, vals in keys:
        hdr = '[' + k.replace('\\', '\\\\') + ']'
        text = re.sub(r'\n' + re.escape(hdr) + r' \d+\n(?:(?!\n\[).)*', '\n', text, flags=re.S | re.I)
        text = text.rstrip('\n') + '\n\n%s %d\n' % (hdr, stamp) + ''.join(v + '\n' for v in vals)
    open(path, 'w', encoding='utf-8').write(text + '\n')

def server_name(cls):
    """The file a class's InprocServer32 or LocalServer32 names (lower case)."""
    for sub in ('InprocServer32', 'LocalServer32'):
        try: srv = cls.subkey(sub).value('(default)').value()
        except (Registry.RegistryKeyNotFoundException, Registry.RegistryValueNotFoundException):
            try: srv = cls.subkey(sub).value('').value()
            except (Registry.RegistryKeyNotFoundException, Registry.RegistryValueNotFoundException): continue
        if not isinstance(srv, str) or not srv.strip(): continue
        srv = srv.strip()
        if srv.startswith('"'): path = srv[1:].split('"', 1)[0]
        # a local server's is a command line: its program is up to the first
        # blank after the file name (".exe /factory,..."); an in-process
        # server's is a path, blanks and all
        elif sub == 'LocalServer32': path = re.split(r'(?<=\.exe) ', srv, maxsplit=1, flags=re.I)[0]
        else: path = srv
        return re.split(r'[\\/]', path)[-1].lower()
    return None

def com_classes(hive, pfx, dlls):
    names = {d.lower() for d in dlls}
    reg = Registry.Registry(hive)
    out, appids, served, n = [], set(), set(), 0
    for cls in reg.open('Classes\\CLSID').subkeys():
        if server_name(cls) not in names: continue
        collect(cls, 'Software\\Classes\\CLSID\\' + cls.name(), out)
        served.add(cls.name())
        try: appids.add(cls.value('AppID').value())
        except Registry.RegistryValueNotFoundException: pass
        n += 1
    # the AppIDs the classes name (a local server's identity and launch settings)
    for a in sorted(x for x in appids if isinstance(x, str)):
        try: collect(reg.open('Classes\\AppID\\' + a), 'Software\\Classes\\AppID\\' + a, out)
        except Registry.RegistryKeyNotFoundException: pass
    # the type libraries the images hold, and the interfaces marshalled by
    # the images' proxies or by those type libraries
    libs = set()
    for lib in reg.open('Classes\\TypeLib').subkeys():
        hit = False
        for ver in lib.subkeys():
            for lcid in ver.subkeys():
                for plat in lcid.subkeys():
                    if plat.name().lower() not in ('win32', 'win64'): continue
                    try: path = plat.value('').value()
                    except Registry.RegistryValueNotFoundException: continue
                    if not isinstance(path, str): continue
                    path = re.sub(r'\\\d+$', '', path.strip())   # a resource index
                    if re.split(r'[\\/]', path)[-1].lower() in names: hit = True
        if hit:
            libs.add(lib.name().upper())
            collect(lib, 'Software\\Classes\\TypeLib\\' + lib.name(), out)
    classes = {c.upper() for c in served}
    m = 0
    for itf in reg.open('Classes\\Interface').subkeys():
        try: ps = itf.subkey('ProxyStubClsid32').value('').value().upper()
        except (Registry.RegistryKeyNotFoundException, Registry.RegistryValueNotFoundException): continue
        lib = None
        try: lib = itf.subkey('TypeLib').value('').value().upper()
        except (Registry.RegistryKeyNotFoundException, Registry.RegistryValueNotFoundException): pass
        if ps in classes or lib in libs:
            collect(itf, 'Software\\Classes\\Interface\\' + itf.name(), out)
            m += 1
    print('%s: %d classes, %d AppIDs, %d type libraries, %d interfaces of %s' %
          (os.path.basename(hive), n, len(appids), len(libs), m, ' '.join(sorted(names))))
    merge(os.path.join(pfx, 'system.reg'), out)

def ns_classes(hive, pfx, keys):
    reg = Registry.Registry(hive)
    out, n = [], 0
    for key in keys:
        for sub in reg.open(key).subkeys():
            try: cls = reg.open('Classes\\CLSID\\' + sub.name())
            except Registry.RegistryKeyNotFoundException: continue
            collect(cls, 'Software\\Classes\\CLSID\\' + cls.name(), out)
            n += 1
    print('%s: %d namespace classes' % (os.path.basename(hive), n))
    merge(os.path.join(pfx, 'system.reg'), out)

def main():
    if sys.argv[1] == '--ns':
        return ns_classes(sys.argv[2], sys.argv[3], sys.argv[4:])
    if sys.argv[1] == '--com':
        return com_classes(sys.argv[2], sys.argv[3], sys.argv[4:])
    pfx, specs = sys.argv[1], sys.argv[2:]
    files, trees = {}, {}
    for spec in specs:
        target = 'system'
        tree = spec.endswith('!')
        if tree: spec = spec[:-1]
        if '@' in spec: spec, target = spec.rsplit('@', 1)
        hive, rest = spec.split(':', 1)
        src, dst = (rest.split('=', 1) + [None])[:2]
        if dst is None:
            dst = default_dst(src)
            if target == 'system' and dst.lower().startswith('currentcontrolset'): dst = 'System\\' + dst
        reg = Registry.Registry(hive)
        out = []
        collect(reg.open(src), dst, out)
        files.setdefault(target, []).extend(out)
        if tree: trees.setdefault(target, []).append(dst)
        print('%s: %s -> %s (%d keys)' % (os.path.basename(hive), src, dst, len(out)))
    for target, keys in files.items():
        merge(os.path.join(pfx, target + '.reg'), keys, trees.get(target, ()))

if __name__ == '__main__':
    main()
