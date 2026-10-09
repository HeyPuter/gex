#!/bin/sh
# The guest GL library's cached framebuffer state queries against native Mesa's
# answers (tests/integration/programs/gl-state-query-probe.c), then under gexc
# --gl with the cache on and off.
#
#   tests/gl-state-query-cache.sh [gexc]      (default target/x86/release/gexc)
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
GEXC=${1:-$ROOT/target/x86/release/gexc}
SOURCE="$ROOT/tests/integration/programs/gl-state-query-probe.c"
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
cc -O2 -Wall -Wextra -o "$WORK/probe" "$SOURCE" -lEGL -lGL
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 "$WORK/probe" | tee "$WORK/native.log"
grep -q 'failed=0' "$WORK/native.log"
GEX_CC=${GEX_CC:-cc} "$ROOT/guest/gl/build.sh" "$WORK/guest-gl" >/dev/null
"$GEXC" run --gl --env LD_LIBRARY_PATH="$WORK/guest-gl" "$WORK/probe" | tee "$WORK/gex.log"
grep -q 'failed=0' "$WORK/gex.log"
"$GEXC" run --gl --env LD_LIBRARY_PATH="$WORK/guest-gl" --env GEXGL_NO_QUERY_CACHE=1 "$WORK/probe" | tee "$WORK/gex-nocache.log"
grep -q 'failed=0' "$WORK/gex-nocache.log"
echo "ok   gl-state-query-cache"
