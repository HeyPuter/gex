#!/bin/sh
# Build Mesa's desktop OpenGL state tracker and a software Gallium reference
# driver for wasm32.  Softpipe is a bootstrap/reference path; the production
# renderer is the WebGPU Gallium driver built in the same Mesa tree.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
MESA=$ROOT/third_party/mesa
SANITIZE=${GEX_MESA_SANITIZE:-0}
# GEX_MESA_THREADS=0: the single-thread build's Mesa,
# without -pthread -- a plain memory, no glthread -- and Wink's `st` Tint.
# Built in its own directory and published under web/public/gex/st/, beside
# the threaded one.
# GEX_MESA_PAGE_ONLY=1: link only the page's gex-wgpu-replay (the gate's web build), not
# the softpipe replay and smokes the tests/mesa-* scripts use.
THREADS=${GEX_MESA_THREADS:-1}
VARIANT=mt
[ "$THREADS" = 0 ] && VARIANT=st
if [ "$SANITIZE" = 1 ]; then
    DEFAULT_OUT=${TMPDIR:-/tmp}/gex-mesa-wasm-sanitize
    CROSS=$ROOT/web/mesa-wasm-sanitize-cross.ini
    BUILD_TYPE=debugoptimized
    GEX_SANITIZE=true
else
    DEFAULT_OUT=${TMPDIR:-/tmp}/gex-mesa-wasm
    CROSS=$ROOT/web/mesa-wasm-cross.ini
    if [ "$THREADS" = 0 ]; then
        DEFAULT_OUT=${TMPDIR:-/tmp}/gex-mesa-wasm-st
        CROSS=$ROOT/web/mesa-wasm-st-cross.ini
    fi
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
if [ "$THREADS" = 0 ]; then
    TINT_OUT=${GEX_MESA_TINT:-${TMPDIR:-/tmp}/gex-mesa-wasm-st/mesa-tint}
else
    TINT_OUT=${GEX_MESA_TINT:-${TMPDIR:-/tmp}/gex-mesa-wasm/mesa-tint-pthread}
fi
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
        WINK_ALLOW_ANY_EMSDK=1 bash "$MESA/wink/build.sh" --variant=$VARIANT --until=tint \
        --cache="$WINK_CACHE" --build-dir="$WINK_CACHE/build" --jobs="${JOBS:-$(nproc)}" \
        --emsdk="$EMSDK_ROOT" 2>&1) || { echo "$tint_log" | tail -20 >&2; exit 1; }
    TINT_OUT=$(echo "$tint_log" | sed -n "s/^\[tint\/$VARIANT\] \(ready\|cached\) at //p" | tail -1)
    [ -f "$TINT_OUT/libtint.a" ] || { echo "$tint_log" | tail -20 >&2; echo "no Tint from wink/build.sh" >&2; exit 1; }
fi
[ -f "$TINT_OUT/libtint.a" ] || {
    WT=true; [ "$THREADS" = 0 ] && WT=false
    OUTPUT_DIR="$TINT_OUT" TMP_DIR="$TINT_TMP-$VARIANT" KEEP_TMP=true WASM_THREADS=$WT \
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
elif [ "${GEX_MESA_PAGE_ONLY:-0}" = 1 ]; then
    # Only what the page loads. The softpipe replay and the smokes are for the
    # tests/mesa-* scripts; each of their links (wasm-opt, SPLIT_MODULE) costs
    # minutes, which the gate's web build doesn't need to spend.
    ninja -C "$BUILD" src/gallium/targets/gex-wasm/gex-wgpu-replay.js -j"${JOBS:-$(nproc)}"
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
#
# The replay links with -sSPLIT_MODULE=1: its .wasm is wasm-split's
# instrumented module and never shipped, and .wasm.orig is the plain one,
# which web/mesa-split.py splits by web/mesa-hot.txt into what the page loads
# and a cold part (gex-wgpu-replay.deferred.wasm) it fetches the first time a
# function there runs. GEX_MESA_HOT=none ships it
# whole; GEX_MESA_HOT=FILE splits by another list. GEX_MESA_PROFILE=1 ships
# it whole and instrumented instead, to measure which functions a workload
# runs (wasm-split --instrument --in-memory: a byte per function at address
# 0 up, below the main stack, which --stack-first puts first; the glue prints
# the set, and tools/mesa-hot.py turns logs of it into web/mesa-hot.txt).
TGT=$BUILD/src/gallium/targets/gex-wasm
if [ "$SANITIZE" != 1 ] && [ -f "$TGT/gex-wgpu-replay.wasm.orig" ]; then
    HOT=${GEX_MESA_HOT:-$ROOT/web/mesa-hot.txt}
    if [ "${GEX_MESA_PROFILE:-0}" = 1 ]; then
        mkdir -p "$TGT/split"
        rm -f "$TGT/split/gex-wgpu-replay.deferred.wasm"
        # The link's features, as web/mesa-split.py names them.
        "$(em-config BINARYEN_ROOT)/bin/wasm-split" --mvp-features --enable-threads --enable-simd \
            --enable-bulk-memory --enable-bulk-memory-opt --enable-nontrapping-float-to-int \
            --enable-sign-ext --enable-mutable-globals --enable-multivalue \
            --instrument --in-memory -g \
            "$TGT/gex-wgpu-replay.wasm.orig" -o "$TGT/split/gex-wgpu-replay.wasm"
        echo "  profiling build: tools/mesa-hot.py $TGT/gex-wgpu-replay.wasm.orig LOG..."
    else
        python3 "$ROOT/web/mesa-split.py" "$TGT/gex-wgpu-replay.wasm.orig" "$HOT" "$TGT/split"
    fi
    # The glue's loader (web/gex-wgpu-split.js) prefetches a cold part only
    # where there is one.
    if [ -f "$TGT/split/gex-wgpu-replay.deferred.wasm" ]; then
        cold=YES
    else
        cold=NO
    fi
    # The assignment only: the test beside it names YES too.
    sed -i "s/\\(gexMesaColdSplit *= *[\"']\\)GEX_MESA_COLD_[A-Z]*/\\1GEX_MESA_COLD_$cold/" "$TGT/gex-wgpu-replay.js"
    cp "$TGT/gex-wgpu-replay.js" "$TGT/split/gex-wgpu-replay.js"
    # And over the instrumented one, for the tests that copy from here.
    cp "$TGT/split/gex-wgpu-replay.wasm" "$TGT/gex-wgpu-replay.wasm"
    rm -f "$TGT/gex-wgpu-replay.deferred.wasm"
    [ -f "$TGT/split/gex-wgpu-replay.deferred.wasm" ] &&
        cp "$TGT/split/gex-wgpu-replay.deferred.wasm" "$TGT/gex-wgpu-replay.deferred.wasm"
fi
PUB="$ROOT/web/public/gex"
[ "$THREADS" = 0 ] && [ -d "$PUB" ] && PUB="$PUB/st" && mkdir -p "$PUB"
# A link without -sSPLIT_MODULE=1 (the mesa-wink tree's) leaves no
# .wasm.orig and so no split: its .wasm is the plain module, and that is what
# is published. Before, nothing was, and the page went on loading whatever
# module had last been copied by hand -- a fix to web/mesa/gexgl-mesa.cpp
# rebuilt here and never reached a test.
FROM=$TGT/split
[ -f "$FROM/gex-wgpu-replay.wasm" ] || FROM=$TGT
if [ -d "$PUB" ] && [ -f "$FROM/gex-wgpu-replay.wasm" ]; then
    rm -f "$PUB/gex-wgpu-replay.deferred.wasm"
    for f in gex-wgpu-replay.js gex-wgpu-replay.wasm gex-wgpu-replay.deferred.wasm; do
        [ -f "$FROM/$f" ] || continue
        cp "$FROM/$f" "$PUB/$f"
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
