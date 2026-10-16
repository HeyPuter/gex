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
# Output is compared with the native run: not even gex's default lines (log.mjs).
export GEX_LOG=${GEX_LOG:-off}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
# GEX_ARCH=aarch64: the AArch64 build's shim and seed (native wasm32
# programs run on both guests' builds); default x86
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/web/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED: GEX_ARCH=x86 GEX_SHARED=1 shims/browser/build.sh, gexc seed -o $SEED" >&2; exit 2; }
WORK=${WORK:-$(mktemp -d)}
fail=0
# GEXCC_SHARED=1: the programs link against libc.so, which the guest gets
# where gex's loader looks
LIBC=
if [ "${GEXCC_SHARED:-0}" = 1 ]; then
    LIBC="--file /usr/lib/wasm32-linux-musl/libc.so=$(dirname "$(command -v gexcc)")/../sysroot/lib/libc.so"
fi
run() {
    GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" node "$ROOT/shims/browser/js/run-node.mjs" $LIBC "$@" 2>/dev/null | grep -v '^\[host\]\|^\[gex\] \|^\[worker\] \|^\[gex-console'
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
