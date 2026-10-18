#!/bin/bash
# The DLLs a W3 bundle pins load in the band (dllband.c): the prefix's
# dll-bases from tools/wine-wasm/w3/dll-bases.py, which must pass its own
# check, and four of the dist's DLLs loaded at the addresses it gives them,
# in [0x11000000, 0x20000000). Prints "ok   dllband native".
#
#   tests/w3/dllband.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wayland Wine prefix made for it (copied,
# never written). Needs i686-w64-mingw32-gcc and an x86 gexc; the run uses
# gexc's own Wayland compositor (--wayland, frames dropped).
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "skip dllband: no $gexc"; exit 0; }

w=${WORK:-$root/target/w3-dllband}
rm -rf "$w"; mkdir -p "$w/wl"
i686-w64-mingw32-gcc -O1 -Wall -o "$w/dllband.exe" "$here/dllband.c" || exit 1
cp -a "$prefix" "$w/pfx"; cp "$w/dllband.exe" "$w/pfx/drive_c/"
# The server names an image by the unix path it opened: natively the dist's.
for d in version winmm dbghelp opengl32; do echo "$dist/lib/wine/i386-windows/$d.dll"; done |
	python3 "$root/tools/wine-wasm/w3/dll-bases.py" assign "$w/pfx/dll-bases" > "$w/assign.log" 2>&1 ||
	{ echo "FAIL dllband: dll-bases.py assign"; sed 's/^/  /' "$w/assign.log"; exit 1; }
python3 "$root/tools/wine-wasm/w3/dll-bases.py" check "$w/pfx/dll-bases" ||
	{ echo "FAIL dllband: dll-bases.py check"; exit 1; }
(cd "$w/pfx/drive_c" && timeout -k 10 "${TMO:-200}" "$gexc" run --no-cgen \
	--wayland "$w/wl" --wayland-size 640x480 --wayland-out null \
	--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all \
	"$dist/lib/wine/i386-unix/wine" ./dllband.exe 'Z:'"$w/pfx/dll-bases") > "$w/native.log" 2>&1
grep -aE '^(ok  |FAIL)' "$w/native.log"
if grep -aq '^dllband ok' "$w/native.log"; then echo "ok   dllband native"; exit 0; fi
echo "FAIL dllband native"; grep -av '^\(ok  \|FAIL\)' "$w/native.log" | tail -8 | sed 's/^/  /'
exit 1
