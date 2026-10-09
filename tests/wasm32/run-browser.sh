#!/bin/sh
# wasm32 programs in the browser runtime (shims/browser/js/run-node.mjs: the
# shim and glue a page loads, under Node): each tests/wasm32/*.c against its
# native run, as tests/wasm32/run.sh does for gexc; with BUSYBOX, the
# busybox.sh script under the browser's /bin/sh (run-node --busybox).
#
#   tests/wasm32/run-browser.sh      (GEX_WASM_SYSROOT set; the x86 shared
#                                     shim and seed in web/public/gex:
#                                     GEX_ARCH=x86 GEX_SHARED=1 shims/browser/build.sh)
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
: "${GEX_WASM_SYSROOT:?set GEX_WASM_SYSROOT (tools/wine-wasm/build-sysroot.sh)}"
# GEX_ARCH=aarch64: the AArch64 build's shim and seed (native wasm32
# programs run on both guests' builds); default x86
ARCH=${GEX_ARCH:-x86}
case $ARCH in x86) INFIX=.x86 ;; *) INFIX= ;; esac
SEED=$ROOT/web/public/gex/seed$INFIX.shared.wasm
[ -f "$SEED" ] || { echo "no $SEED; build with GEX_ARCH=x86 GEX_SHARED=1 shims/browser/build.sh" >&2; exit 2; }
WORK=${WORK:-$(mktemp -d)}
fail=0
run() {
    GEX_ARCH=$ARCH GEX_SHARED=1 timeout "${TMO:-300}" node "$ROOT/shims/browser/js/run-node.mjs" "$@" 2>/dev/null | grep -v '^\[host\]\|^\[gex\] \|^\[worker\] '
}
for src in "$HERE"/*.c; do
    name=$(basename "$src" .c)
    cc -O1 -o "$WORK/$name.native" "$src" -lpthread || { fail=1; continue; }
    "$ROOT/tools/wine-wasm/wcc" -O2 -o "$WORK/$name" "$src" || { fail=1; continue; }
    native=$(cd "$WORK" && "./$name.native" 2>/dev/null); ns=$?
    web=$(run --file "/t/$name=$WORK/$name" --cwd /t --argv0 "./$name" "$SEED" "$WORK/$name")
    ws=$(run --file "/t/$name=$WORK/$name" --cwd /t --argv0 "./$name" "$SEED" "$WORK/$name" >/dev/null; echo $?)
    if [ "$native" = "$web" ] && [ "$ns" = "$ws" ]; then
        echo "ok   $name (browser, exit $ns)"
    else
        echo "FAIL $name (browser): native exit $ns, browser exit $ws"
        printf '%s\n' "$native" > "$WORK/$name.out-native"
        printf '%s\n' "$web" > "$WORK/$name.out-web"
        diff "$WORK/$name.out-native" "$WORK/$name.out-web" | head -20
        fail=1
    fi
done
if [ -n "${BUSYBOX:-}" ] && [ -f "$BUSYBOX" ]; then
    # the script busybox.sh runs, from its own text
    script=$(sed -n "/^script='/,/^echo done'/p" "$HERE/busybox.sh" | sed "1s/^script='//; \$s/'\$//")
    printf 'b\na\nc\nb\n' > "$WORK/in.txt"
    native=$(cd "$WORK" && sh -c "$script" 2>&1)
    web=$(run --busybox "$BUSYBOX" --file "/t/in.txt=$WORK/in.txt" --cwd /t --argv0 sh "$SEED" "$BUSYBOX" -c "$script")
    if [ "$native" = "$web" ]; then
        echo "ok   busybox (browser)"
    else
        echo "FAIL busybox (browser)"
        printf '%s\n' "$native" > "$WORK/bb.out-native"; printf '%s\n' "$web" > "$WORK/bb.out-web"
        diff "$WORK/bb.out-native" "$WORK/bb.out-web" | head -20
        fail=1
    fi
else
    echo "skip busybox (browser; set BUSYBOX)"
fi
exit $fail
