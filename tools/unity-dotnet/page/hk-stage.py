#!/usr/bin/env python3
# hk-stage.py STRACE_RUN OUT: from a native `unity-bench.sh hk RUN ... --strace` run, the page's
# staging for Hollow Knight: OUT/game (the run's launcher and steam_appid.txt, links to only the game
# files the run opened, so the page's tar holds those and not the whole 4.9 GB install) and
# OUT/files.txt (the host files it opened under /etc, /lib, /usr).
import os, re, sys
run, out = sys.argv[1], sys.argv[2]
game = os.path.join(run, 'game')
pat = re.compile(r'(?:openat|newfstatat|statx|faccessat2?|access|readlinkat|stat|lstat|open)\(.*?"([^"]+)"[^=]*= (-?\d+|0x[0-9a-f]+)')
host, inside = set(), set()
for line in open(os.path.join(run, 'out.txt'), errors='replace'):
    m = pat.search(line)
    if not m or m.group(2).startswith('-'):
        continue
    p = m.group(1)
    if p.startswith(game + '/'):
        inside.add(os.path.relpath(p, game))
    elif re.match(r'/(etc|lib|lib64|usr)/', p):
        if os.path.isfile(p): host.add(p)
g = os.path.join(out, 'game')
os.makedirs(g, exist_ok=True)
n = 0
for rel in sorted(inside):
    src = os.path.join(game, rel)
    real = os.path.realpath(src)
    dst = os.path.join(g, rel)
    if os.path.isdir(real):
        os.makedirs(dst, exist_ok=True)
        continue
    if not os.path.isfile(real):
        continue
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    if os.path.lexists(dst):
        os.remove(dst)
    if os.path.dirname(rel) == '' and not os.path.islink(src):
        # The launcher and steam_appid.txt are the run's own files: copied.
        with open(real, 'rb') as a, open(dst, 'wb') as b:
            b.write(a.read())
        os.chmod(dst, os.stat(real).st_mode)
    else:
        os.symlink(real, dst)
    n += 1
with open(os.path.join(out, 'files.txt'), 'w') as f:
    f.write('\n'.join(sorted(host)) + '\n')
size = sum(os.path.getsize(os.path.realpath(os.path.join(dp, x))) for dp, _, fs in os.walk(g) for x in fs)
print(f'{n} game files ({size / 2**20:.0f} MiB), {len(host)} host files')
