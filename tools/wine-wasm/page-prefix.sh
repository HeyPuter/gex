#!/usr/bin/env bash
# The page's W3 prefix template (docs/wine-native.md §3.17, W3.7): an
# installed W3 prefix (wineboot -u leaves system32's programs and drivers
# and the registry), with the W3 session (w3-session.reg, or the files
# named) applied offline and the install marked current for DIST's
# wine.inf, so wineboot --init does not update it again in the page. W64's
# win64 prefix (w64-dist.sh's) makes its template the same way
# (docs/wine-native-x64.md, W64.5).
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
regs=("$@"); [ ${#regs[@]} -gt 0 ] || regs=("$here/w3-session.reg")
python3 "$here/reg-apply.py" "$out/system.reg" "${regs[@]}"
stat -c %Y "$dist/share/wine/wine.inf" > "$out/.update-timestamp"
if [ -n "${GRAPHICS:-}" ]; then
    if [ -f "$out/user.reg" ] && grep -q '^"Graphics"=' "$out/user.reg"; then
        sed -i "s/^\"Graphics\"=.*/\"Graphics\"=\"$GRAPHICS\"/" "$out/user.reg"
    else
        # the prefix's own architecture (win32 for W3, win64 for W64)
        arch=$(sed -n 's/^#arch=//p' "$out/system.reg" | head -1)
        printf 'WINE REGISTRY Version 2\n;; All keys relative to \\\\User\\\\S-1-5-21-0-0-0-1000\n\n#arch=%s\n\n[Software\\\\Wine\\\\Drivers] 1791289000\n"Graphics"="%s"\n' "${arch:-win32}" "$GRAPHICS" >> "$out/user.reg"
    fi
fi
echo "$out: $(du -sh "$out" | cut -f1)"
