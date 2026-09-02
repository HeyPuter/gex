#!/bin/sh
# Build Mesa's desktop OpenGL state tracker and a software Gallium reference
# driver for wasm32.  Softpipe is a bootstrap/reference path; the production
# renderer is the WebGPU Gallium driver built in the same Mesa tree.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
MESA=$ROOT/third_party/mesa
SANITIZE=${GEX_MESA_SANITIZE:-0}
if [ "$SANITIZE" = 1 ]; then
    DEFAULT_OUT=${TMPDIR:-/tmp}/gex-mesa-wasm-sanitize
    CROSS=$ROOT/web/mesa-wasm-sanitize-cross.ini
    BUILD_TYPE=debugoptimized
    GEX_SANITIZE=true
else
    DEFAULT_OUT=${TMPDIR:-/tmp}/gex-mesa-wasm
    CROSS=$ROOT/web/mesa-wasm-cross.ini
    BUILD_TYPE=release
    GEX_SANITIZE=false
fi
OUT=${1:-$DEFAULT_OUT}
BUILD=$OUT/build
EMDAWN=${GEX_EMDAWN_DIR:-$($ROOT/web/fetch-emdawnwebgpu.sh)}
TINT_OUT=${GEX_MESA_TINT:-${TMPDIR:-/tmp}/gex-virgl-wasm/mesa-tint-pthread}
TINT_TMP=${GEX_MESA_TINT_TMP:-${TMPDIR:-/tmp}/gex-mesa-tint-build}

command -v meson >/dev/null 2>&1 || { echo "need meson" >&2; exit 1; }
command -v ninja >/dev/null 2>&1 || { echo "need ninja" >&2; exit 1; }
command -v emcc >/dev/null 2>&1 || { echo "need Emscripten" >&2; exit 1; }
[ -f "$MESA/meson.build" ] || {
    echo "initialize third_party/mesa" >&2
    exit 1
}
[ -f "$TINT_OUT/libtint.a" ] || {
    OUTPUT_DIR="$TINT_OUT" TMP_DIR="$TINT_TMP" KEEP_TMP=true WASM_THREADS=true \
        "$ROOT/third_party/mobilegl-webgpu/tools/build-tint.sh"
}

if [ ! -f "$BUILD/build.ninja" ]; then
    mkdir -p "$OUT"
    meson setup "$BUILD" "$MESA" \
        --cross-file "$CROSS" \
        -Dbuildtype="$BUILD_TYPE" \
        -Dplatforms=[] \
        -Dgallium-drivers=softpipe \
        -Dvulkan-drivers=[] \
        -Dglx=disabled \
        -Degl=disabled \
        -Dgbm=disabled \
        -Dllvm=disabled \
        -Dshared-glapi=disabled \
        -Dopengl=true \
        -Dgles1=disabled \
        -Dgles2=enabled \
        -Dvideo-codecs=[] \
        -Dvalgrind=disabled \
        -Dlibunwind=disabled \
        -Dzstd=disabled \
        -Dzlib=disabled \
        -Ddisplay-info=disabled \
        -Dexpat=disabled \
        -Dspirv-tools=disabled \
        -Dshader-cache=disabled \
        -Dxmlconfig=disabled \
        -Dbuild-tests=false \
        -Dgex-emdawn-dir="$EMDAWN" \
        -Dgex-tint-dir="$TINT_OUT" \
        -Dgex-sanitize="$GEX_SANITIZE"
else
    configured_type=$(meson configure "$BUILD" | sed -n 's/^  buildtype[[:space:]]*\([^[:space:]]*\).*/\1/p')
    if [ "$configured_type" != "$BUILD_TYPE" ]; then
        echo "$BUILD was configured as $configured_type, expected $BUILD_TYPE; use a separate output directory" >&2
        exit 1
    fi
    meson configure "$BUILD" -Dgex-emdawn-dir="$EMDAWN" \
        -Dgex-tint-dir="$TINT_OUT" -Dgex-sanitize="$GEX_SANITIZE"
fi

if [ "$SANITIZE" = 1 ]; then
    ninja -C "$BUILD" \
        src/gallium/targets/gex-wasm/gex-wgpu-smoke.js \
        src/gallium/targets/gex-wasm/gex-gallium-softpipe-conformance.js \
        -j"${JOBS:-$(nproc)}"
else
    ninja -C "$BUILD" \
        src/gallium/targets/gex-wasm/gex-mesa-smoke.js \
        src/gallium/targets/gex-wasm/gex-mesa-replay.js \
        src/gallium/targets/gex-wasm/gex-wgpu-smoke.js \
        src/gallium/targets/gex-wasm/gex-wgpu-replay.js \
        src/mesa/libmesa.a \
        src/mesa/glapi/shared-glapi/libglapi.a \
        src/gallium/auxiliary/libgallium.a \
        src/gallium/drivers/softpipe/libsoftpipe.a \
        -j"${JOBS:-$(nproc)}"
fi

# Where the page loads them from. Left out before, so a rebuilt module sat in
# the build directory while the site served whatever had last been copied by
# hand -- which looks exactly like a change that did not work.
PUB="$ROOT/web/public/gex"
if [ -d "$PUB" ]; then
    for f in gex-wgpu-replay.js gex-wgpu-replay.wasm; do
        [ -f "$BUILD/src/gallium/targets/gex-wasm/$f" ] || continue
        cp "$BUILD/src/gallium/targets/gex-wasm/$f" "$PUB/$f"
        echo "  published $PUB/$f"
    done
fi

printf '%s\n' \
    "$BUILD/src/gallium/targets/gex-wasm/gex-mesa-smoke.js" \
    "$BUILD/src/gallium/targets/gex-wasm/gex-mesa-replay.js" \
    "$BUILD/src/gallium/targets/gex-wasm/gex-wgpu-smoke.js" \
    "$BUILD/src/mesa/libmesa.a" \
    "$BUILD/src/mesa/glapi/shared-glapi/libglapi.a" \
    "$BUILD/src/gallium/auxiliary/libgallium.a" \
    "$BUILD/src/gallium/drivers/softpipe/libsoftpipe.a"
