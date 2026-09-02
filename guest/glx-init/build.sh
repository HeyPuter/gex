#!/bin/sh
# Build the SDL-shaped GLX probe for the guest.
#
#   guest/glx-init/build.sh [out-dir]
#
# Everything it needs it reaches through dlopen, so this wants nothing but a
# cross compiler -- no X11 or GL headers, and no libraries to link against.
set -eu

HERE=$(CDPATH='' cd -- "$(dirname "$0")" && pwd)
OUT=${1:-$HERE/out}
CC=${GEX_CC:-aarch64-linux-gnu-gcc}

command -v "$CC" >/dev/null 2>&1 || { echo "need $CC" >&2; exit 1; }
mkdir -p "$OUT"
"$CC" -O2 -Wall -Wextra -o "$OUT/glx-init" "$HERE/glx-init.c" -ldl
"$CC" -O2 -Wall -Wextra -o "$OUT/egl-init" "$HERE/egl-init.c" -ldl
echo "$OUT/glx-init $OUT/egl-init"
