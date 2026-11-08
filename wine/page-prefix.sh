#!/usr/bin/env bash
# The page's 32-bit Wine prefix template: an
# installed 32-bit Wine prefix (wineboot -u leaves system32's programs and drivers
# and the registry), with the 32-bit Wine session (wine32-session.reg, or the files
# named) applied offline and the install marked current for DIST's
# wine.inf, so wineboot --init does not update it again in the page. 64-bit Wine's
# win64 prefix (wine64-dist.sh's) makes its template the same way.
#
#   wine/page-prefix.sh INSTALLED_PFX DIST OUT [FILE.reg...]
#
# GRAPHICS=null|wayland sets the user's graphics driver (default: keep).
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
[ $# -ge 3 ] || { sed -n 2,11p "$0" >&2; exit 2; }
src=$1 dist=$2 out=$3; shift 3
[ -f "$src/system.reg" ] || { echo "$src has no registry: install it first (wineboot -u)" >&2; exit 2; }
[ -f "$dist/share/wine/wine.inf" ] || { echo "$dist has no share/wine/wine.inf" >&2; exit 2; }
# A prefix that names the builtins it was made for (dist.sh's .gex-builtins)
# must have each of DIST's: one made for another install lacks them, and is
# never updated in the page (stamped current below), so a DLL it never
# installed or registered (winegstreamer's classes) fails only in a program.
# The display driver aside: a 64-bit prefix has none (made without a
# display), and Wine loads it from the install.
if [ -f "$src/.gex-builtins" ]; then
	lacks=$(for a in x86_64 i386; do
		[ -d "$dist/lib/wine/$a-windows" ] || continue
		ls "$dist/lib/wine/$a-windows" | grep -Ev '\.(a|so|wasm|wasmref|m64)$' | grep -vx winewayland.drv | sed "s/^/$a /"
	done | sort -u | comm -23 - <(sort -u "$src/.gex-builtins"))
	[ -z "$lacks" ] || { printf '%s was made for another Wine install than %s: it has no %s\n' "$src" "$dist" "$(echo "$lacks" | sed 's/^\([^ ]*\) \(.*\)/\2 (\1)/' | head -8 | paste -sd, - | sed 's/,/, /g')" >&2; exit 1; }
fi
rm -rf "$out"
# Real files, without following dosdevices (z: is /): the page bundles the
# tree, and a run writes its own copy. Links elsewhere (a staged prefix's
# DLLs into the master prefix) are copied as what they point at. The user.
# extended attributes go too: Wine keeps a file's DOS attributes in
# user.DOSATTRIB (a folder's desktop.ini is hidden and system).
#
# The users' folders are copied without following any link: the install ran
# as the build machine's user, whose profile Wine links to that user's own
# folders (Downloads -> ~/Downloads, ...). Each link there becomes an empty
# folder. Only Public and the page's user (PAGE_USER, default gex: the
# install's user, renamed) are kept; anything else under users (another
# user's profile, a profile with no name, files a run left) is not.
mkdir -p "$out"; out=$(cd "$out" && pwd)
user=${PAGE_USER:-gex}
tar -C "$src" --exclude=./dosdevices --exclude=./drive_c/users --exclude=./.gex-builtins --exclude=./.gex-dists -h --xattrs --xattrs-include='user.*' -cf - . |
    tar -C "$out" --xattrs --xattrs-include='user.*' -xpf -
# The install's user: the profile the registry names (ProfileList).
built=$(sed -n 's/^"ProfileImagePath"="C:\\\\users\\\\\([^"\\]*\)"$/\1/p' "$src/system.reg" | head -1)
mkdir -p "$out/drive_c/users"
for d in Public "$built" "$user"; do
    [ -n "$d" ] && [ -d "$src/drive_c/users/$d" ] && [ ! -L "$src/drive_c/users/$d" ] || continue
    to=$d; [ "$d" = "$built" ] && to=$user
    mkdir -p "$out/drive_c/users/$to"
    tar -C "$src/drive_c/users/$d" --xattrs --xattrs-include='user.*' -cf - . |
        tar -C "$out/drive_c/users/$to" --xattrs --xattrs-include='user.*' -xpf -
done
find "$out/drive_c/users" -type l | while IFS= read -r l; do rm "$l"; mkdir "$l"; done
# Its paths and name in the registry are the page user's (Shell Folders,
# ProfileList, the saved environment).
[ -z "$built" ] || [ "$built" = "$user" ] || python3 -I - "$built" "$user" "$out"/*.reg <<'PY'
import re, sys
built, user, regs = sys.argv[1], sys.argv[2], sys.argv[3:]
# C:\users\NAME and \users\NAME (HOMEPATH) in a value, and USERNAME.
pat = re.compile(r'(?<![\w\\:])((?:C:)?\\\\users\\\\)' + re.escape(built) + r'(?=\\\\|"|;)', re.I)
name = re.compile(r'^("USERNAME"=")' + re.escape(built) + '"$', re.I | re.M)
for r in regs:
    s = open(r, encoding='utf-8', errors='surrogateescape').read()
    t = name.sub(lambda m: m.group(1) + user + '"', pat.sub(lambda m: m.group(1) + user, s))
    if t != s: open(r, 'w', encoding='utf-8', errors='surrogateescape').write(t)
PY
mkdir -p "$out/dosdevices"
ln -s ../drive_c "$out/dosdevices/c:" && ln -s / "$out/dosdevices/z:"
regs=("$@")
if [ ${#regs[@]} -eq 0 ]; then
	regs=("$here/session.reg")
	# A prefix with game controllers (wine64-dist.sh's WINE64_PADS: winebus's
	# service has "DemandStart"=1) keeps winebus's root device, which the
	# session would remove.
	if python3 -I - "$out/system.reg" <<'PY'
import re, sys
s = open(sys.argv[1], encoding='utf-8', errors='surrogateescape').read()
sys.exit(0 if re.search(r'^\[System\\\\ControlSet001\\\\Services\\\\winebus\][^\[]*^"DemandStart"=dword:0*1$', s, re.M | re.I) else 1)
PY
	then
		grep -v -i '^\[-HKEY_LOCAL_MACHINE\\System\\CurrentControlSet\\Enum\\ROOT\\WINE\\WINEBUS\]' "$here/session.reg" > "$out/.session.reg"
		regs=("$out/.session.reg")
	fi
fi
python3 "$here/reg-apply.py" "$out/system.reg" "${regs[@]}"
rm -f "$out/.session.reg"
# the file's own time, a dist of links included: wineboot compares the
# stamp with the wine.inf it reads, and reruns Wine's whole setup when they differ
stat -L -c %Y "$dist/share/wine/wine.inf" > "$out/.update-timestamp"
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
# Nothing in the template leads out of it but dosdevices' c: and z:.
links=$(find "$out" -type l ! -path "$out/dosdevices/*")
[ -z "$links" ] || { printf 'page-prefix: links left in %s:\n%s\n' "$out" "$links" >&2; exit 1; }
echo "$out: $(du -sh "$out" | cut -f1)"
