#!/bin/bash
# The wink smoke suite (third_party/mesa/wink/test/wink_smoke.c) in a browser on a
# real GPU: GL ES, core and compatibility through the gex_wgpu driver packaged as
# libwink, on the page's main thread, with GL on the main thread from a worker, and
# on a worker that owns the canvas (index.html, main-thread.html, offscreen.html).
#
#   tests/wink-smoke-page.sh --build   on the build host: libwink (mt) from
#       third_party/mesa (wink/build.sh, its cache in GEX_MESA_WINK_CACHE as for
#       web/build-mesa-wasm.sh) and the suite linked against it, into
#       target/wink-smoke (the runner is copied beside it, so the bench host needs
#       only that directory; the build tree is target/wink-smoke-build)
#   tests/wink-smoke-page.sh           on the bench host (through harness/bench/bench.sh,
#       BENCH_HOST=worldmachine: edger's headless Chromium shows no WebGPU canvas):
#       the three pages; passes when each reports PASS.
# Headless Chromium without a GPU is not enough: SwiftShader's limits stop the
# driver at GL 3.2 and the device is lost part way through.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=$ROOT/target/wink-smoke
# The build tree beside it, not in it: the bench host is sent OUT alone.
BUILD=$ROOT/target/wink-smoke-build
MESA=$ROOT/third_party/mesa
if [ "${1:-}" = --build ]; then
    [ -f "$MESA/wink/build.sh" ] || { echo "FAIL $MESA has no wink/build.sh"; exit 1; }
    EMSDK_ROOT=${EMSDK:-$HOME/emsdk}
    CACHE=${GEX_MESA_WINK_CACHE:-${TMPDIR:-/tmp}/gex-mesa-wink-cache}
    mkdir -p "$OUT"
    PATH=$EMSDK_ROOT/upstream/emscripten:$EMSDK_ROOT/upstream/bin:$PATH WINK_ALLOW_ANY_EMSDK=1 \
        bash "$MESA/wink/build.sh" --variant=mt --until=package --cache="$CACHE" \
        --build-dir="$BUILD" --emsdk="$EMSDK_ROOT" --jobs="${JOBS:-16}" > "$BUILD.log" 2>&1 \
        || { tail -20 "$BUILD.log"; echo "FAIL wink build (log in $BUILD.log)"; exit 1; }
    PATH=$EMSDK_ROOT/upstream/emscripten:$EMSDK_ROOT/upstream/bin:$PATH WINK_ALLOW_ANY_EMSDK=1 \
        bash "$MESA/wink/test/build-smoke.sh" mt "$BUILD/out/mt" "$BUILD/bin/emcc" "$OUT/mt" >> "$BUILD.log" 2>&1 \
        || { tail -20 "$BUILD.log"; echo "FAIL wink smoke link (log in $BUILD.log)"; exit 1; }
    cp "$MESA/wink/test/run-browser.mjs" "$OUT/run-browser.mjs"
    echo "built $OUT"
    exit 0
fi
[ -f "$OUT/mt/index.html" ] && [ -f "$OUT/run-browser.mjs" ] || { echo "FAIL no suite: run $0 --build on the build host first"; exit 1; }
rc=0
for p in index.html main-thread.html offscreen.html; do
    log=$(node "$OUT/run-browser.mjs" "$OUT/mt" --page=$p --timeout=240 2>&1)
    if printf '%s\n' "$log" | tail -1 | grep -q '^PASS'; then
        echo "ok wink smoke $p"
    else
        echo "FAIL wink smoke $p"; printf '%s\n' "$log" | grep -a -E 'FAIL|WebGPU (error|device)|timed out' | head -12
        rc=1
    fi
done
exit $rc
