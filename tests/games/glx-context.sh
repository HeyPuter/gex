#!/bin/sh
# GLX contexts under gex (tests/games/src/glx-context.c): a program on
# Xwayland inside the guest, with gex's guest GL for x86-64.
#
#   tests/games/glx-context.sh [GEXC]
#
# Env: GEX_GL_X64 (guest/gl/build.sh with GEX_CC=gcc; built here if unset).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
gexc=${1:-$root/target/x86/release/gexc}
work=${TMPDIR:-/tmp}/gex-glx-context-$(id -u)
rm -rf "$work"; mkdir -p "$work"
gl=${GEX_GL_X64:-}
if [ -z "$gl" ]; then gl=$work/gl; GEX_CC=gcc "$root/guest/gl/build.sh" "$gl" > /dev/null; fi
gcc -O1 -o "$work/glx-context" "$here/src/glx-context.c" -lX11 -lGL
n=$(( 50 + $(id -u) % 40 ))
rm -f "/tmp/.X$n-lock" "/tmp/.X11-unix/X$n"
cd "$work"
set +e
timeout 1800 "$gexc" run --no-cgen --gl --wayland "$work/wl" --wayland-size 320x240 --wayland-out null \
    --env LD_LIBRARY_PATH="$gl" --env XDG_RUNTIME_DIR="$work" --env HOME="$work" \
    /bin/sh "$here/xwayland-inguest.sh" "$n" "$work/xwayland.log" ./glx-context > "$work/out" 2>&1
rc=$?
rm -f "/tmp/.X$n-lock" "/tmp/.X11-unix/X$n"
grep -E '^(ok|FAIL|  |[0-9]+ failures)' "$work/out"
[ $rc = 0 ] && grep -q '^0 failures' "$work/out"
