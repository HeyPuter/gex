#!/usr/bin/env python3
"""A W3 program's eject site for a page leg (tests/eject/keyfocus.sh): Wine's
Unix side, share/ (without Wine Mono) and a copy of the prefix in the bundle,
its PE side and the prefix's big files lazy (read from the server as Wine
opens them), the program at /game. The page itself (app.mjs, gex/) comes
from web/build-eject-page.sh into SITE first.

    wine-site.py SITE W3_DIST W3_PREFIX EXE [ARG...]

Writes SITE/bundle.tar, SITE/lazy/, SITE/lazy.json and SITE/game.json (the
launch the leg's index.html reads)."""
import json, os, shutil, subprocess, sys, tarfile

site, dist, prefix, exe, *args = sys.argv[1:]
stage, lz = f'{site}/stage', f'{site}/lazy'
for d in (stage, lz):
    shutil.rmtree(d, ignore_errors=True)
os.makedirs(f'{stage}/w3/lib/wine')
os.makedirs(f'{lz}/w3/lib/wine')
shutil.copytree(f'{dist}/lib/wine/i386-unix', f'{stage}/w3/lib/wine/i386-unix', symlinks=True)
shutil.copytree(f'{dist}/share', f'{stage}/w3/share', symlinks=True,
                ignore=lambda d, names: ['mono'] if d.endswith('/share/wine') else [])
# lib/wine/share -> ../../share, as in the dist: wineserver finds its NLS
# files from its own directory through it.
os.symlink('../../share', f'{stage}/w3/lib/wine/share')
shutil.copytree(f'{dist}/lib/wine/i386-windows', f'{lz}/w3/lib/wine/i386-windows', symlinks=True)
subprocess.run(['cp', '-a', prefix, f'{stage}/pfx'], check=True)
with open(f'{stage}/pfx/user.reg', 'a') as f:
    f.write('\n[Software\\\\Wine\\\\Drivers] 1791289000\n"Graphics"="wayland"\n')
# The prefix's big files (system32's DLLs) lazy; the registry, which Wine
# rewrites, and the links stay in the bundle.
for dp, _, fn in os.walk(f'{stage}/pfx'):
    for n in fn:
        p = os.path.join(dp, n)
        if not os.path.islink(p) and os.path.isfile(p) and os.path.getsize(p) > 256 * 1024:
            d = f'{lz}/pfx/' + os.path.relpath(p, f'{stage}/pfx')
            os.makedirs(os.path.dirname(d), exist_ok=True)
            os.rename(p, d)
os.makedirs(f'{stage}/game')
shutil.copy2(exe, f'{stage}/game/')
for d in ('tmp', 'home/gex'):
    os.makedirs(f'{stage}/{d}')
# winewayland's keyboard (libxkbcommon) reads the XKB data, as the game sites
# have it (scratch/G/mksite.py).
if os.path.isdir('/usr/share/X11/xkb'):
    os.makedirs(f'{stage}/usr/share/X11')
    subprocess.run(['cp', '-a', '/usr/share/X11/xkb', f'{stage}/usr/share/X11/'], check=True)
with tarfile.open(f'{site}/bundle.tar', 'w', format=tarfile.GNU_FORMAT) as t:
    for top in [d for d in ('pfx', 'w3', 'game', 'tmp', 'home', 'usr') if os.path.isdir(f'{stage}/{d}')]:
        t.add(f'{stage}/{top}', arcname=top)
shutil.rmtree(stage)
files = []
for top in ('w3', 'pfx'):
    for dp, dn, fn in os.walk(f'{lz}/{top}'):
        dn.sort()
        for n in sorted(fn):
            p = os.path.join(dp, n)
            if os.path.isfile(p) and not os.path.islink(p):
                rel = os.path.relpath(p, lz)
                files.append(['/' + rel, os.path.getsize(p), rel])
json.dump({'base': '/lazy/', 'files': files}, open(f'{site}/lazy.json', 'w'))
json.dump({
    'tar': ['/bundle.tar'], 'lazy': '/lazy.json', 'exe': '/w3/lib/wine/i386-unix/wine',
    'argv': ['/game/' + os.path.basename(exe), *args],
    'env': ['WINEPREFIX=/pfx', 'WINEDEBUG=err+all,fixme-all', 'XDG_RUNTIME_DIR=/tmp',
            'WAYLAND_DISPLAY=wayland-0', 'USER=gex', 'HOME=/home/gex'],
    'cwd': '/game',
}, open(f'{site}/game.json', 'w'), indent=1)
print(f'wine-site: {site}: bundle {os.path.getsize(site + "/bundle.tar") >> 20} MiB, '
      f'lazy {len(files)} files {sum(f[1] for f in files) >> 20} MiB')
