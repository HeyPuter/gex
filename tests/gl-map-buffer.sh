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
grep -q 'SUMMARY cases=14 passed=14 failed=0' "$WORK/native.log"
GEX_CC=${GEX_CC:-cc} "$ROOT/guest/gl/build.sh" "$WORK/guest-gl" >/dev/null
"$GEXC" run --gl --env LD_LIBRARY_PATH="$WORK/guest-gl" "$WORK/probe" 2>"$WORK/gex.err" | tee "$WORK/gex.log"
grep -q 'SUMMARY cases=14 passed=14 failed=0' "$WORK/gex.log"
# The 64 MiB ASTC upload, tried twice, is reported once.
[ "$(grep -c 'gexgl: dropped opcode' "$WORK/gex.err")" = 1 ] || { echo "FAIL gl-map-buffer: want one 'dropped opcode' line"; grep 'gexgl:' "$WORK/gex.err"; exit 1; }
echo "ok   gl-map-buffer"
