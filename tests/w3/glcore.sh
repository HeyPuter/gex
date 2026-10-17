#!/bin/bash
# A core-profile GL program on W3 (glcore.c), shaped like wined3d: a 3.2 core
# context, GLSL attached before it is compiled, an RGBA8 FBO, blits into the
# window (plain, flipped, scaled), a shared context presenting from a second
# thread and wined3d's hidden helper GL windows. Prints "glcore ok".
#
#   tests/w3/glcore.sh DIST PREFIX [gexc]
#
# DIST is a W3 Wine install, PREFIX a Wayland Wine prefix made for it (copied,
# never written). Needs i686-w64-mingw32-gcc and an x86 gexc; the run uses
# gexc's own Wayland compositor (--wayland, frames dropped) and its host GL.
#
# The page leg needs WebGPU, so it is not run here: build a site whose program
# is glcore.exe with the arguments "hold thread fake brush" and open it. The
# window must show red over green for a minute; before g/page-gl-validation a
# WebGPU validation error anywhere in the session left GL windows white.
set -u
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
dist=$(realpath "${1:?DIST}"); prefix=$(realpath "${2:?PREFIX}")
gexc=$(realpath -m "${3:-$root/target/x86/release/gexc}")
[ -x "$gexc" ] || { echo "skip glcore: no $gexc"; exit 0; }

w=${WORK:-$root/target/w3-glcore}
rm -rf "$w"; mkdir -p "$w/wl"
i686-w64-mingw32-gcc -O1 -Wall -o "$w/glcore.exe" "$here/glcore.c" -lgdi32 -lopengl32 || exit 1
cp -a "$prefix" "$w/pfx"; cp "$w/glcore.exe" "$w/pfx/drive_c/"
(cd "$w/pfx/drive_c" && timeout -k 10 "${TMO:-300}" "$gexc" run --no-cgen --gl \
	--wayland "$w/wl" --wayland-size 640x480 --wayland-out null \
	--env WINEPREFIX="$w/pfx" --env WINEDEBUG=err+all,fixme-all \
	"$dist/lib/wine/i386-unix/wine" ./glcore.exe) > "$w/native.log" 2>&1
grep -aE '^(ok  |FAIL|  GL_)' "$w/native.log"
if grep -aq '^glcore ok' "$w/native.log"; then echo "ok   glcore native"; exit 0; fi
echo "FAIL glcore native"; grep -av '^\(ok  \|FAIL\|  GL_\)' "$w/native.log" | tail -8 | sed 's/^/  /'
exit 1
