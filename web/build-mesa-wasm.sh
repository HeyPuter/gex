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
# emdawnwebgpu: a Mesa checkout that pins its own (the wink branch:
# wink/lib/emdawn.sh, a release matched to its Tint and patched by
# wink/tools/patch-emdawnwebgpu.py) supplies it; else gex's own fetch.
if [ -z "${GEX_EMDAWN_DIR:-}" ] && [ -f "$MESA/wink/lib/emdawn.sh" ]; then
    WINK_CACHE=${GEX_MESA_WINK_CACHE:-${TMPDIR:-/tmp}/gex-mesa-wink-cache}
    EMSDK_ROOT=$(CDPATH='' cd -- "$(dirname "$(command -v emcc)")/../.." && pwd)
    emdawn_log=$(WINK_ALLOW_ANY_EMSDK=1 bash "$MESA/wink/build.sh" --variant=mt --until=emdawn \
        --cache="$WINK_CACHE" --build-dir="$WINK_CACHE/build" --emsdk="$EMSDK_ROOT" 2>&1) || {
        echo "$emdawn_log" | tail -20 >&2
        exit 1
    }
    EMDAWN=$(echo "$emdawn_log" | sed -n 's/^\[emdawn\] \(ready\|cached\) at //p' | tail -1)
    [ -f "$EMDAWN/webgpu/include/webgpu/webgpu.h" ] || {
        echo "$emdawn_log" | tail -20 >&2
        echo "no emdawnwebgpu from wink/build.sh" >&2
        exit 1
    }
else
    EMDAWN=${GEX_EMDAWN_DIR:-$($ROOT/web/fetch-emdawnwebgpu.sh)}
fi
TINT_OUT=${GEX_MESA_TINT:-${TMPDIR:-/tmp}/gex-mesa-wasm/mesa-tint-pthread}
TINT_TMP=${GEX_MESA_TINT_TMP:-${TMPDIR:-/tmp}/gex-mesa-tint-build}

command -v meson >/dev/null 2>&1 || { echo "need meson" >&2; exit 1; }
command -v ninja >/dev/null 2>&1 || { echo "need ninja" >&2; exit 1; }
command -v emcc >/dev/null 2>&1 || { echo "need Emscripten" >&2; exit 1; }
[ -f "$MESA/meson.build" ] || {
    echo "initialize third_party/mesa" >&2
    exit 1
}
# Tint. A Mesa checkout that pins its own (the wink branch: wink/lib/tint.sh,
# Dawn and patches chosen by the fork) builds it with its own script, so the
# pin stays in one place; GEX_MESA_TINT still names a prebuilt one. Wink's
# script expects emsdk 3.1.56, and the newer clang in the emsdk gex builds
# Mesa with turns three of Tint's warnings into errors -- the same three
# web/tint/build-tint.sh silences.
if [ -z "${GEX_MESA_TINT:-}" ] && [ -f "$MESA/wink/lib/tint.sh" ]; then
    WINK_CACHE=${GEX_MESA_WINK_CACHE:-${TMPDIR:-/tmp}/gex-mesa-wink-cache}
    EMSDK_ROOT=$(CDPATH='' cd -- "$(dirname "$(command -v emcc)")/../.." && pwd)
    tint_log=$(EMCC_CFLAGS="-Wno-c2y-extensions -Wno-deprecated-pragma -Wno-lifetime-safety-intra-tu-suggestions" \
        WINK_ALLOW_ANY_EMSDK=1 bash "$MESA/wink/build.sh" --variant=mt --until=tint \
        --cache="$WINK_CACHE" --build-dir="$WINK_CACHE/build" --jobs="${JOBS:-$(nproc)}" \
        --emsdk="$EMSDK_ROOT" 2>&1) || { echo "$tint_log" | tail -20 >&2; exit 1; }
    TINT_OUT=$(echo "$tint_log" | sed -n 's/^\[tint\/mt\] \(ready\|cached\) at //p' | tail -1)
    [ -f "$TINT_OUT/libtint.a" ] || { echo "$tint_log" | tail -20 >&2; echo "no Tint from wink/build.sh" >&2; exit 1; }
fi
[ -f "$TINT_OUT/libtint.a" ] || {
    OUTPUT_DIR="$TINT_OUT" TMP_DIR="$TINT_TMP" KEEP_TMP=true WASM_THREADS=true \
        "$ROOT/web/tint/build-tint.sh"
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
