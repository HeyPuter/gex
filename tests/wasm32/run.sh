#!/bin/sh
# wasm32 programs (stream W) checked against native execution: each is
# built for the host with cc and for gex's wasm32 with tools/wine-wasm/wcc,
# and its output and exit status must match.
#
#   tests/wasm32/run.sh [gexc]   (an x86 gexc; GEX_WASM_SYSROOT = the
#                                 sysroot from tools/wine-wasm/build-sysroot.sh)
set -u
# Output is compared with the native run: not even gex's default lines (log.mjs).
export GEX_LOG=${GEX_LOG:-off}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
GEXC=${1:-target/x86/release/gexc}
: "${GEX_WASM_SYSROOT:?set GEX_WASM_SYSROOT (tools/wine-wasm/build-sysroot.sh)}"
WORK=${WORK:-$(mktemp -d)}
fail=0
for src in "$HERE"/*.c; do
    name=$(basename "$src" .c)
    cc -O1 -o "$WORK/$name.native" "$src" -lpthread || { fail=1; continue; }
    "$ROOT/tools/wine-wasm/wcc" -O2 -o "$WORK/$name" "$src" || { fail=1; continue; }
    # argv[0] differs; programs print nothing that depends on it
    native=$(cd "$WORK" && "./$name.native" 2>/dev/null); ns=$?
    gex=$(cd "$WORK" && "$GEXC" run --cwd "$WORK" "./$name" 2>/dev/null); gs=$?
    if [ "$native" = "$gex" ] && [ $ns = $gs ]; then
        echo "ok   $name (exit $ns)"
    else
        echo "FAIL $name: native exit $ns, gex exit $gs"
        printf '%s\n' "$native" > "$WORK/$name.out-native"
        printf '%s\n' "$gex" > "$WORK/$name.out-gex"
        diff "$WORK/$name.out-native" "$WORK/$name.out-gex" | head -20
        fail=1
    fi
done
exit $fail
