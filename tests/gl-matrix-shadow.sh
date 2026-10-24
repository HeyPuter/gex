#!/bin/sh
# The matrices and enables the guest GL library answers itself
# (guest/gl/gexgl-shadow.inc), against native Mesa's answers to the same
# commands (tests/integration/programs/gl-matrix-shadow-probe.c): under gexc
# --gl with the shadow on, checked against the host, and off.
#
#   tests/gl-matrix-shadow.sh [gexc]      (default target/x86/release/gexc)
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
GEXC=${1:-$ROOT/target/x86/release/gexc}
SOURCE="$ROOT/tests/integration/programs/gl-matrix-shadow-probe.c"
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
cc -O2 -Wall -Wextra -o "$WORK/probe" "$SOURCE" -lEGL -lGL -lm
EGL_PLATFORM=surfaceless LIBGL_ALWAYS_SOFTWARE=1 "$WORK/probe" > "$WORK/native.log"
grep -q '^DONE' "$WORK/native.log"
GEX_CC=${GEX_CC:-cc} "$ROOT/guest/gl/build.sh" "$WORK/guest-gl" >/dev/null
for mode in on check off; do
    case $mode in
        on) env=GEXGL_SHADOW=on ;;
        check) env=GEXGL_SHADOW_CHECK=1 ;;
        off) env=GEXGL_NO_QUERY_CACHE=1 ;;
    esac
    "$GEXC" run --gl --env LD_LIBRARY_PATH="$WORK/guest-gl" --env "$env" "$WORK/probe" \
        > "$WORK/gex-$mode.log" 2> "$WORK/gex-$mode.err"
    if ! cmp -s "$WORK/native.log" "$WORK/gex-$mode.log"; then
        echo "FAIL gl-matrix-shadow ($mode): answers differ from native Mesa's"
        diff "$WORK/native.log" "$WORK/gex-$mode.log" | head -20
        exit 1
    fi
    if grep -q 'shadow .* differs' "$WORK/gex-$mode.err"; then
        echo "FAIL gl-matrix-shadow ($mode)"; grep 'shadow' "$WORK/gex-$mode.err" | head; exit 1
    fi
    if [ $mode = check ] && ! grep -Eq 'shadow: [1-9][0-9]* answers compared, 0 differed' "$WORK/gex-$mode.err"; then
        echo "FAIL gl-matrix-shadow (check): nothing was answered from the shadow"; grep 'shadow' "$WORK/gex-$mode.err" | head; exit 1
    fi
done
echo "ok   gl-matrix-shadow"
