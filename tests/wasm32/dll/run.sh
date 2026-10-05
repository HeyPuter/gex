#!/bin/sh
# The wasm32 entry-adapter test: build abi.dll and abi_test.exe (build.sh)
# and run the program under gexc on each tier.
#   tests/wasm32/dll/run.sh GEXC WINE_SRC WINE_TOOLS   (GEX_WASM_SYSROOT set)
set -u
here=$(cd "$(dirname "$0")" && pwd)
GEXC=$1 WINE=$2 TOOLS=$3
OUT=${WORK:-$(mktemp -d)}
"$here/build.sh" "$OUT" "$WINE" "$TOOLS" >/dev/null 2>&1 || { echo "FAIL build"; exit 1; }
fail=0
for tier in --no-cgen --fjit; do
    out=$(cd "$OUT" && "$GEXC" run $tier abi_test.exe 2>/dev/null)
    if printf '%s\n' "$out" | tail -1 | tr -d '\r' | grep -qx PASS; then echo "ok   abi_test ($tier)"
    else echo "FAIL abi_test ($tier)"; printf '%s\n' "$out" | grep -v '^ok'; fail=1; fi
done
exit $fail
