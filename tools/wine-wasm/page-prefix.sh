#!/usr/bin/env bash
# The page's 32-bit Wine prefix template: an
# installed 32-bit Wine prefix (wineboot -u leaves system32's programs and drivers
# and the registry), with the 32-bit Wine session (wine32-session.reg, or the files
# named) applied offline and the install marked current for DIST's
# wine.inf, so wineboot --init does not update it again in the page. 64-bit Wine's
# win64 prefix (wine64-dist.sh's) makes its template the same way.
#
#   tools/wine-wasm/page-prefix.sh INSTALLED_PFX DIST OUT [FILE.reg...]
#
# GRAPHICS=null|wayland sets the user's graphics driver (default: keep).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
[ $# -ge 3 ] || { sed -n 2,11p "$0" >&2; exit 2; }
src=$1 dist=$2 out=$3; shift 3
[ -f "$src/system.reg" ] || { echo "$src has no registry: install it first (wineboot -u)" >&2; exit 2; }
[ -f "$dist/share/wine/wine.inf" ] || { echo "$dist has no share/wine/wine.inf" >&2; exit 2; }
rm -rf "$out"
# Real files, without following dosdevices (z: is /): the page bundles the
# tree, and a run writes its own copy. Links elsewhere (a staged prefix's
# DLLs into the master prefix) are copied as what they point at. The user.
# extended attributes go too: Wine keeps a file's DOS attributes in
# user.DOSATTRIB (a folder's desktop.ini is hidden and system).
mkdir -p "$out"; out=$(cd "$out" && pwd)
tar -C "$src" --exclude=./dosdevices -h --xattrs --xattrs-include='user.*' -cf - . |
    tar -C "$out" --xattrs --xattrs-include='user.*' -xpf -
mkdir -p "$out/dosdevices"
ln -s ../drive_c "$out/dosdevices/c:" && ln -s / "$out/dosdevices/z:"
regs=("$@"); [ ${#regs[@]} -gt 0 ] || regs=("$here/wine32-session.reg")
python3 "$here/reg-apply.py" "$out/system.reg" "${regs[@]}"
stat -c %Y "$dist/share/wine/wine.inf" > "$out/.update-timestamp"
if [ -n "${GRAPHICS:-}" ]; then
    # "Graphics" in [Software\\Wine\\Drivers] and in no other key: a user.reg
    # whose value sits under another key (DllOverrides) keeps Wine's default
    # driver list. A user.reg without the key gets it at its end, and none
    # at all a whole file.
    arch=$(sed -n 's/^#arch=//p' "$out/system.reg" | head -1)
    python3 -I - "$out/user.reg" "$GRAPHICS" "${arch:-win32}" <<'PY'
import os, sys
path, graphics, arch = sys.argv[1:]
key, value = '[Software\\\\Wine\\\\Drivers]', f'"Graphics"="{graphics}"'
if not os.path.exists(path):
    open(path, 'w').write('WINE REGISTRY Version 2\n;; All keys relative to \\\\User\\\\S-1-5-21-0-0-0-1000\n\n'
                          f'#arch={arch}\n\n{key} 1791289000\n{value}\n')
    sys.exit()
lines = open(path, encoding='utf-8', errors='surrogateescape').read().split('\n')
# The value after the key's line and its options (#time=...).
out, here, placed = [], None, False
for line in lines:
    if here == key and not placed and not line.startswith('#'):
        out.append(value); placed = True
    if line.startswith('['):
        here = line.split('] ', 1)[0] + ']' if '] ' in line else line
    if not line.startswith('"Graphics"='):
        out.append(line)
if here == key and not placed:
    out.append(value); placed = True
if not placed:
    while out and out[-1] == '':
        out.pop()
    out += ['', f'{key} 1791289000', value, '']
text = '\n'.join(out)
open(path, 'w', encoding='utf-8', errors='surrogateescape').write(text)
PY
fi
# The install ran on DIST at its host path, and the registry names files
# there (the fonts in share/wine/fonts): in the page the dist is at its mount
# (DIST_MOUNT: /wine32, or /wine64 for a win64 prefix), and no host path ships.
arch=$(sed -n 's/^#arch=//p' "$out/system.reg" | head -1)
mount=${DIST_MOUNT:-$([ "$arch" = win64 ] && echo /wine64 || echo /wine32)}
python3 -I - "$(cd "$dist" && pwd -P)" "$(cd "$dist" && pwd)" "$mount" "$out"/*.reg <<'PY'
import sys
dists, mount, regs = {sys.argv[1], sys.argv[2]}, sys.argv[3], sys.argv[4:]
win = lambda p: 'Z:' + p.replace('/', '\\\\')
for r in regs:
    s = open(r, encoding='utf-8', errors='surrogateescape').read(); o = s
    for d in sorted(dists, key=len, reverse=True):
        s = s.replace(win(d), win(mount))
    if s != o: open(r, 'w', encoding='utf-8', errors='surrogateescape').write(s)
PY
echo "$out: $(du -sh "$out" | cut -f1)"
