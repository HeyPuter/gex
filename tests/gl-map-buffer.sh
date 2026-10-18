#!/bin/sh
# glMapBufferRange's contents rules, natively (x86-64 guest, host GL): the
# probe checks itself on native Mesa first, then runs under gexc against
# guest/gl built for x86-64.
#
#   tests/gl-map-buffer.sh [gexc]      (default target/x86/release/gexc)
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
GEXC=${1:-$ROOT/target/x86/release/gexc}
SOURCE="$ROOT/tests/integration/programs/gl-map-buffer-probe.c"
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
cc -O2 -Wall -Wextra -o "$WORK/probe" "$SOURCE" -lEGL -lGL
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 "$WORK/probe" | tee "$WORK/native.log"
grep -q 'SUMMARY cases=11 passed=11 failed=0' "$WORK/native.log"
GEX_CC=${GEX_CC:-cc} "$ROOT/guest/gl/build.sh" "$WORK/guest-gl" >/dev/null
"$GEXC" run --gl --env LD_LIBRARY_PATH="$WORK/guest-gl" "$WORK/probe" | tee "$WORK/gex.log"
grep -q 'SUMMARY cases=11 passed=11 failed=0' "$WORK/gex.log"
echo "ok   gl-map-buffer"
