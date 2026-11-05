#!/bin/sh
# Build Mesa's desktop OpenGL state tracker and a software Gallium reference
# driver for wasm32.  Softpipe is a bootstrap/reference path; the production
# renderer is the WebGPU Gallium driver built in the same Mesa tree.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)
MESA=$ROOT/third_party/mesa
SANITIZE=${GEX_MESA_SANITIZE:-0}
# GEX_MESA_THREADS=0: the single-thread build's Mesa,
# without -pthread -- a plain memory, no glthread -- and Wink's `st` Tint.
# Built in its own directory and published under page/public/gex/st/, beside
# the threaded one.
# GEX_MESA_PAGE_ONLY=1: link only the page's gex-wgpu-replay (a page-only web build), not
# the softpipe replay and smokes the Mesa tests use.
# GEX_MESA_DIRK=1: the D3D device too (ports/mesa/src/gexd3d-mesa.cpp), on dirk:
# DXVK's D3D11/DXGI front end on gallium, built here into a package the
# replay links (-Dgex-dirk-dir). Mesa pins it (subprojects/dxvk.wrap); the
# build fetches it into third_party/mesa/subprojects/dxvk at that commit. Off,
# the module is GL only, as before.
DIRK=${GEX_MESA_DIRK:-0}
THREADS=${GEX_MESA_THREADS:-1}
VARIANT=mt
[ "$THREADS" = 0 ] && VARIANT=st
if [ "$SANITIZE" = 1 ]; then
    DEFAULT_OUT=${TMPDIR:-/tmp}/gex-mesa-wasm-sanitize
    CROSS=$ROOT/ports/mesa/mesa-wasm-sanitize-cross.ini
    BUILD_TYPE=debugoptimized
    GEX_SANITIZE=true
else
    DEFAULT_OUT=${TMPDIR:-/tmp}/gex-mesa-wasm
    CROSS=$ROOT/ports/mesa/mesa-wasm-cross.ini
    if [ "$THREADS" = 0 ]; then
        DEFAULT_OUT=${TMPDIR:-/tmp}/gex-mesa-wasm-st
        CROSS=$ROOT/ports/mesa/mesa-wasm-st-cross.ini
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
    EMDAWN=${GEX_EMDAWN_DIR:-$($ROOT/ports/mesa/fetch-emdawnwebgpu.sh)}
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
# ports/tint/build-tint.sh silences.
if [ -z "${GEX_MESA_TINT:-}" ] && [ -f "$MESA/wink/lib/tint.sh" ]; then
    WINK_CACHE=${GEX_MESA_WINK_CACHE:-${TMPDIR:-/tmp}/gex-mesa-wink-cache}
    EMSDK_ROOT=$(CDPATH='' cd -- "$(dirname "$(command -v emcc)")/../.." && pwd)
    # The cache's and the tree's paths as fixed names: Tint's assertion and
    # file strings ship in the replay module.
    tint_log=$(EMCC_CFLAGS="-Wno-c2y-extensions -Wno-deprecated-pragma -Wno-lifetime-safety-intra-tu-suggestions -ffile-prefix-map=$WINK_CACHE=/tint -ffile-prefix-map=$ROOT=/gex" \
        WINK_ALLOW_ANY_EMSDK=1 bash "$MESA/wink/build.sh" --variant=$VARIANT --until=tint \
        --cache="$WINK_CACHE" --build-dir="$WINK_CACHE/build" --jobs="${JOBS:-$(nproc)}" \
        --emsdk="$EMSDK_ROOT" 2>&1) || { echo "$tint_log" | tail -20 >&2; exit 1; }
    TINT_OUT=$(echo "$tint_log" | sed -n "s/^\[tint\/$VARIANT\] \(ready\|cached\) at //p" | tail -1)
    [ -f "$TINT_OUT/libtint.a" ] || { echo "$tint_log" | tail -20 >&2; echo "no Tint from wink/build.sh" >&2; exit 1; }
fi
[ -f "$TINT_OUT/libtint.a" ] || {
    WT=true; [ "$THREADS" = 0 ] && WT=false
    OUTPUT_DIR="$TINT_OUT" TMP_DIR="$TINT_TMP-$VARIANT" KEEP_TMP=true WASM_THREADS=$WT \
        "$ROOT/ports/tint/build-tint.sh"
}
# Mesa's own files by fixed names too: the replay includes Tint's headers,
# whose assertion strings would otherwise carry the build machine's paths.
# emcc's flags from the environment are not ninja's to track, so a build
# directory compiled with other ones is cleaned below.
export EMCC_CFLAGS="${EMCC_CFLAGS:+$EMCC_CFLAGS }-ffile-prefix-map=${WINK_CACHE:-$(dirname "$TINT_OUT")}=/tint -ffile-prefix-map=$ROOT=/gex"

# A build directory set up from files that are no longer there (its cross or
# native file, or the source tree, at an old path, as after the tree's files
# moved) cannot be reconfigured: it is set up afresh.
if [ -f "$BUILD/build.ninja" ]; then
    gone=$(python3 -I - "$BUILD" <<'PY'
import ast, configparser, json, os, sys
b = sys.argv[1]
paths = []
try:
    paths.append(json.load(open(os.path.join(b, 'meson-info', 'meson-info.json')))['directories']['source'])
except (OSError, ValueError, KeyError):
    paths.append(os.path.join(b, 'meson-info', 'meson-info.json'))
c = configparser.ConfigParser()
c.read(os.path.join(b, 'meson-private', 'cmd_line.txt'))
for k in ('cross_file', 'native_file'):
    v = c.get('properties', k, fallback='[]')
    try:
        paths += ast.literal_eval(v)
    except (ValueError, SyntaxError):
        paths.append(v)
print(' '.join(p for p in paths if not os.path.exists(p)))
PY
)
    if [ -n "$gone" ]; then
        echo "$BUILD was set up from files no longer there ($gone): setting it up again" >&2
        rm -rf "${BUILD:?}"
    fi
fi
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
# Objects compiled with other emcc flags (the prefix maps above) are compiled
# again: ninja would keep them, and their strings with them.
FLAGS_STAMP=$BUILD/.gex-emcc-cflags
if [ "$(cat "$FLAGS_STAMP" 2>/dev/null)" != "$EMCC_CFLAGS" ]; then
    ninja -C "$BUILD" -t clean > /dev/null
    printf '%s' "$EMCC_CFLAGS" > "$FLAGS_STAMP"
fi
DIRK_PKG=
if [ "$DIRK" = 1 ]; then
    DXVK=$MESA/subprojects/dxvk
    [ -f "$MESA/subprojects/dxvk.wrap" ] || {
        echo "third_party/mesa has no subprojects/dxvk.wrap: a Mesa without the D3D front end" >&2
        exit 1
    }
    want=$(sed -n 's/^revision *= *//p' "$MESA/subprojects/dxvk.wrap")
    if [ ! -d "$DXVK" ]; then
        (cd "$MESA" && meson subprojects download dxvk)
    elif [ "$(git -C "$DXVK" rev-parse HEAD)" != "$want" ]; then
        (cd "$MESA" && meson subprojects update --reset dxvk)
    fi
    [ -f "$DXVK/tests/dirk/objects.mk" ] &&
        [ -f "$DXVK/include/vulkan/include/vulkan/vulkan.h" ] &&
        [ -f "$DXVK/include/native/directx/dxgi1_6.h" ] || {
        echo "$DXVK is incomplete: (cd third_party/mesa && meson subprojects update --reset dxvk)" >&2
        exit 1
    }
    # Its objects include Mesa's generated headers: those are built first.
    ninja -C "$BUILD" src/gallium/auxiliary/libgallium.a src/mesa/libmesa.a -j"${JOBS:-$(nproc)}"
    DIRK_PKG=$OUT/dirk
    EMSDK_ROOT=$(CDPATH='' cd -- "$(dirname "$(command -v emcc)")/../.." && pwd)
    make -f "$DXVK/tests/dirk/objects.mk" O="$OUT/dirk-obj" PKG="$DIRK_PKG" \
        MESA="$MESA" MESA_BUILD="$BUILD" WEBGPU_INC="$EMDAWN/webgpu/include" \
        EMSDK="$EMSDK_ROOT" THREADS="$THREADS" -j"${JOBS:-$(nproc)}"
fi
# A Mesa from before the option has no D3D device to switch.
if grep -q "'gex-dirk-dir'" "$MESA/meson.options"; then
    meson configure "$BUILD" -Dgex-dirk-dir="$DIRK_PKG"
fi

if [ "$SANITIZE" = 1 ]; then
    ninja -C "$BUILD" \
        src/gallium/targets/gex-wasm/gex-wgpu-smoke.js \
        src/gallium/targets/gex-wasm/gex-gallium-softpipe-conformance.js \
        -j"${JOBS:-$(nproc)}"
elif [ "${GEX_MESA_PAGE_ONLY:-0}" = 1 ]; then
    # Only what the page loads. The softpipe replay and the smokes are for the
    # Mesa tests; each of their links (wasm-opt, SPLIT_MODULE) costs
    # minutes, which a page-only build doesn't need to spend.
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
# which ports/mesa/split.py splits by ports/mesa/hot.txt into what the page loads
# and a cold part (gex-wgpu-replay.deferred.wasm) it fetches the first time a
# function there runs. GEX_MESA_HOT=none ships it
# whole; GEX_MESA_HOT=FILE splits by another list. GEX_MESA_PROFILE=1 ships
# it whole and instrumented instead, to measure which functions a workload
# runs (wasm-split --instrument --in-memory: a byte per function at address
# 0 up, below the main stack, which --stack-first puts first; the glue prints
# the set, and ports/mesa/mesa-hot.py turns logs of it into ports/mesa/hot.txt).
TGT=$BUILD/src/gallium/targets/gex-wasm
if [ "$SANITIZE" != 1 ] && [ -f "$TGT/gex-wgpu-replay.wasm.orig" ]; then
    HOT=${GEX_MESA_HOT:-$ROOT/ports/mesa/hot.txt}
    if [ "${GEX_MESA_PROFILE:-0}" = 1 ]; then
        mkdir -p "$TGT/split"
        rm -f "$TGT/split/gex-wgpu-replay.deferred.wasm"
        # The link's features, as ports/mesa/split.py names them.
        "$(em-config BINARYEN_ROOT)/bin/wasm-split" --mvp-features --enable-threads --enable-simd \
            --enable-bulk-memory --enable-bulk-memory-opt --enable-nontrapping-float-to-int \
            --enable-sign-ext --enable-mutable-globals --enable-multivalue --enable-exception-handling \
            --instrument --in-memory -g \
            "$TGT/gex-wgpu-replay.wasm.orig" -o "$TGT/split/gex-wgpu-replay.wasm"
        echo "  profiling build: ports/mesa/mesa-hot.py $TGT/gex-wgpu-replay.wasm.orig LOG..."
    else
        python3 "$ROOT/ports/mesa/split.py" "$TGT/gex-wgpu-replay.wasm.orig" "$HOT" "$TGT/split"
    fi
    # The glue's loader (ports/mesa/gex-wgpu-split.js) prefetches a cold part only
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
PUB="$ROOT/page/public/gex"
[ "$THREADS" = 0 ] && [ -d "$PUB" ] && PUB="$PUB/st" && mkdir -p "$PUB"
# A link without -sSPLIT_MODULE=1 (the mesa-wink tree's) leaves no
# .wasm.orig and so no split: its .wasm is the plain module, and that is what
# is published. Before, nothing was, and the page went on loading whatever
# module had last been copied by hand -- a fix to ports/mesa/src/gexgl-mesa.cpp
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
    # The licences of what the replay is built from, beside it, for the npm
    # package's THIRD-PARTY (page/runtime/build.sh): Mesa's, Dawn's (Tint is
    # Dawn's) and emdawnwebgpu's.
    L=$PUB/licenses
    rm -rf "$L/mesa" "$L/dawn" "$L/emdawnwebgpu"
    mkdir -p "$L/mesa" "$L/dawn" "$L/emdawnwebgpu"
    for f in docs/license.rst licenses/MIT licenses/Apache-2.0 licenses/BSL-1.0 licenses/SGI-B-2.0; do
        [ -f "$MESA/$f" ] || { echo "no $MESA/$f: Mesa's licence for the replay" >&2; exit 1; }
        cp "$MESA/$f" "$L/mesa/$(basename "$f")"
    done
    DAWN_LICENSE=${GEX_DAWN_LICENSE:-}
    [ -n "$DAWN_LICENSE" ] || [ -z "${WINK_CACHE:-}" ] ||
        DAWN_LICENSE=$(ls -t "$WINK_CACHE"/dawn/src-*/LICENSE 2>/dev/null | head -1)
    [ -n "$DAWN_LICENSE" ] || DAWN_LICENSE=$TINT_TMP-$VARIANT/dawn/LICENSE
    [ -f "$DAWN_LICENSE" ] || { echo "no Dawn LICENSE ($DAWN_LICENSE; GEX_DAWN_LICENSE names one)" >&2; exit 1; }
    cp "$DAWN_LICENSE" "$L/dawn/LICENSE"
    [ -f "$EMDAWN/webgpu_cpp/LICENSE" ] || { echo "no $EMDAWN/webgpu_cpp/LICENSE" >&2; exit 1; }
    cp "$EMDAWN/webgpu_cpp/LICENSE" "$L/emdawnwebgpu/LICENSE"
fi

printf '%s\n' \
    "$BUILD/src/gallium/targets/gex-wasm/gex-mesa-smoke.js" \
    "$BUILD/src/gallium/targets/gex-wasm/gex-mesa-replay.js" \
    "$BUILD/src/gallium/targets/gex-wasm/gex-wgpu-smoke.js" \
    "$BUILD/src/mesa/libmesa.a" \
    "$BUILD/src/mesa/glapi/shared-glapi/libglapi.a" \
    "$BUILD/src/gallium/auxiliary/libgallium.a" \
    "$BUILD/src/gallium/drivers/softpipe/libsoftpipe.a"
