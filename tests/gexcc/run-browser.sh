#!/bin/sh
# tests/gexcc programs in the browser runtime (shims/browser/js/run-node.mjs,
# the page's shim and glue under Node), against their native runs, as
# tests/wasm32/run-browser.sh does for stream W's. Programs with a NAME.sh
# driver (host FIFOs, host stdin) are native-only. BUSYBOX: a gexcc busybox
# for the guest's /bin/sh (system() in pathcorners), else that one is skipped.
#
#   tests/gexcc/run-browser.sh   (gexcc on PATH; web/public/gex has the x86
#                                 shared shim and seed.x86.shared.wasm)
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
SEED=$ROOT/web/public/gex/seed.x86.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED: GEX_ARCH=x86 GEX_SHARED=1 shims/browser/build.sh, gexc seed -o $SEED" >&2; exit 2; }
WORK=${WORK:-$(mktemp -d)}
fail=0
run() {
    GEX_ARCH=x86 GEX_SHARED=1 timeout "${TMO:-300}" node "$ROOT/shims/browser/js/run-node.mjs" "$@" 2>/dev/null | grep -v '^\[host\]\|^\[gex\] \|^\[worker\] \|^\[gex-console'
}
for src in "$HERE"/*.c "$HERE"/*.cpp; do
    case $src in *.cpp) name=$(basename "$src" .cpp); hcc=c++; gcc_=gexc++ ;; *) name=$(basename "$src" .c); hcc=cc; gcc_=gexcc ;; esac
    [ -f "$HERE/$name.sh" ] && { echo "skip $name (native only)"; continue; }
    bb=
    if grep -q 'system(' "$src"; then
        [ -n "${BUSYBOX:-}" ] || { echo "skip $name (set BUSYBOX)"; continue; }
        bb="--busybox $BUSYBOX"
    fi
    $hcc -O1 -w -o "$WORK/$name.native" "$src" -lpthread || { fail=1; continue; }
    $gcc_ -O2 -w -o "$WORK/$name" "$src" || { fail=1; continue; }
    native=$(cd "$WORK" && "./$name.native" 2>&1); ns=$?
    web=$(run $bb --file "/t/$name=$WORK/$name" --cwd /t --argv0 "./$name" "$SEED" "$WORK/$name"); 
    ws=$(run $bb --file "/t/$name=$WORK/$name" --cwd /t --argv0 "./$name" "$SEED" "$WORK/$name" >/dev/null; echo $?)
    if [ "$native" = "$web" ] && [ "$ns" = "$ws" ]; then
        echo "ok   $name (browser)"
    else
        echo "FAIL $name (browser): native exit $ns, browser exit $ws"
        printf '%s\n' "$native" > "$WORK/$name.out-native"; printf '%s\n' "$web" > "$WORK/$name.out-web"
        diff "$WORK/$name.out-native" "$WORK/$name.out-web" | head -20
        fail=1
    fi
done
exit $fail
