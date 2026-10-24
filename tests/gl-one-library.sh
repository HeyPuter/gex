#!/bin/sh
# The guest GL library's names are one library (guest/gl/build.sh), so a
# program that loads more than one of them has one GL state
# (tests/integration/programs/gl-one-library-probe.c).
#
#   tests/gl-one-library.sh [gexc]      (default target/x86/release/gexc)
set -eu
ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
GEXC=${1:-$ROOT/target/x86/release/gexc}
WORK=${WORK:-$(mktemp -d)}
mkdir -p "$WORK"
cc -O2 -Wall -Wextra -o "$WORK/probe" "$ROOT/tests/integration/programs/gl-one-library-probe.c" -ldl
GEX_CC=${GEX_CC:-cc} "$ROOT/guest/gl/build.sh" "$WORK/guest-gl" >/dev/null
"$GEXC" run --env LD_LIBRARY_PATH="$WORK/guest-gl" "$WORK/probe" 2>"$WORK/gex.err" | tee "$WORK/gex.log"
grep -q 'SUMMARY failed=0' "$WORK/gex.log"
echo "ok   gl-one-library"
