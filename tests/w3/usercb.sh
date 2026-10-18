#!/bin/bash
# A user-mode callback made from a Unix call after the PE side has pushed
# frames since its last system call (usercb.c): opengl32's GL debug
# messages, delivered inside glDebugMessageInsert and at the guest GL
# library's next waiting submit, while 6 KiB of canary frames sit between the
# last system call's stack pointer and the call. Prints "usercb ok".
#
#   tests/w3/usercb.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wayland Wine prefix made for it (copied,
# never written). Needs i686-w64-mingw32-gcc and an x86 gexc; the run uses
# gexc's own Wayland compositor (--wayland, frames dropped) and its host GL.
# The same program in the page: a site whose program is usercb.exe.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "skip usercb: no $gexc"; exit 0; }

w=${WORK:-$root/target/w3-usercb}
rm -rf "$w"; mkdir -p "$w/wl"
i686-w64-mingw32-gcc -O1 -Wall -o "$w/usercb.exe" "$here/usercb.c" -lgdi32 -lopengl32 || exit 1
cp -a "$prefix" "$w/pfx"; cp "$w/usercb.exe" "$w/pfx/drive_c/"
(cd "$w/pfx/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run --no-cgen --gl \
	--wayland "$w/wl" --wayland-size 640x480 --wayland-out null \
	--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all \
	"$dist/lib/wine/i386-unix/wine" ./usercb.exe) > "$w/native.log" 2>&1
grep -aE '^(ok  |FAIL|  GL_)' "$w/native.log"
if grep -aq '^usercb ok' "$w/native.log"; then echo "ok   usercb native"; exit 0; fi
echo "FAIL usercb native"; grep -av '^\(ok  \|FAIL\|  GL_\)' "$w/native.log" | tail -8 | sed 's/^/  /'
exit 1
