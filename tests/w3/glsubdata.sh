#!/bin/bash
# glBufferSubData of the bytes a buffer already holds (glsubdata.c): the page's
# Mesa skips such a repeat, so a CPU map write, a GPU copy or another subdata in
# between must make the next identical write land again. Prints "glsubdata ok".
#
#   tests/w3/glsubdata.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wayland Wine prefix made for it (copied,
# never written). Needs i686-w64-mingw32-gcc and an x86 gexc. Natively this
# runs on the host's GL, so it checks the test, not the page's Mesa: the page
# leg is a site whose program is glsubdata.exe with the argument "hold"; its
# output must say "glsubdata ok" and its console "a buffer_subdata of the
# bytes a buffer already holds is skipped".
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "skip glsubdata: no $gexc"; exit 0; }

w=${WORK:-$root/target/w3-glsubdata}
rm -rf "$w"; mkdir -p "$w/wl"
i686-w64-mingw32-gcc -O1 -Wall -o "$w/glsubdata.exe" "$here/glsubdata.c" -lgdi32 -lopengl32 || exit 1
cp -a "$prefix" "$w/pfx"; cp "$w/glsubdata.exe" "$w/pfx/drive_c/"
(cd "$w/pfx/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run --no-cgen --gl \
	--wayland "$w/wl" --wayland-size 640x480 --wayland-out null \
	--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all \
	"$dist/lib/wine/i386-unix/wine" ./glsubdata.exe) > "$w/native.log" 2>&1
grep -aE '^(ok  |FAIL|  GL_)' "$w/native.log"
if grep -aq '^glsubdata ok' "$w/native.log"; then echo "ok   glsubdata native"; exit 0; fi
echo "FAIL glsubdata native"; grep -av '^\(ok  \|FAIL\|  GL_\)' "$w/native.log" | tail -8 | sed 's/^/  /'
exit 1
